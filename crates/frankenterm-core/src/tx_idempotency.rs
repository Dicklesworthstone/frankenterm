//! Durable idempotency, deduplication, and resume invariants for the tx substrate (ft-1i2ge.8.7).
//!
//! Guarantees restart-safe idempotency and resume semantics across prepare/commit/compensation
//! paths. Integrates with the tx plan compiler's [`TxPlan`] / [`TxStep`] types and uses
//! collision-resistant, domain-separated content-addressed keys.
//!
//! # Key Components
//!
//! - [`IdempotencyKey`]: Content-addressed key derived from plan ID + step ID + action content.
//! - [`StepOutcome`]: Canonical outcome of executing a tx step.
//! - [`StepExecutionRecord`]: Immutable record of a step execution with hash-chain linkage.
//! - [`TxExecutionLedger`]: Ordered ledger of execution records for a single tx instance.
//! - [`DeduplicationGuard`]: Prevents double-commit and double-compensation.
//! - [`ResumeContext`]: Reconstructs tx state from a persisted ledger for restart recovery.
//! - [`IdempotencyPolicy`]: Configuration for key generation, dedup windows, and resume behavior.

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt};
#[cfg(any(unix, windows))]
use cap_std::fs::OpenOptionsExt as CapOpenOptionsExt;
use cap_std::fs::{Dir, File as CapFile, OpenOptions as CapOpenOptions};
use fs2::FileExt;
use serde::de;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::tx_plan_compiler::{StepRisk, TxPlan};

// ── Idempotency Key ──────────────────────────────────────────────────────────

/// Content-addressed idempotency key for a tx step execution.
///
/// Generated deterministically from (kind, plan_id, step_id, fingerprint) so
/// replaying the same operation produces the same key. The persisted v2 format
/// uses a domain-separated SHA-256 digest over length-delimited tuple
/// components; delimiter placement and action/compensation namespaces therefore
/// cannot alias distinct effects.
///
/// # Persisted-key integrity (br-ft-f4vta)
///
/// Both `key` and `plan_id`/`step_id` were previously trusted across
/// the serde boundary even though `key` is documented as derived
/// from the other two plus an action fingerprint. The custom
/// `Deserialize` impl below validates the wire-format of `key`
/// (must be `txk:v2:` + exactly 64 lowercase-hex characters), rejects
/// empty `plan_id` or `step_id`, AND re-derives the canonical hash
/// from the persisted `(key_kind, plan_id, step_id, hash_input)` tuple to
/// confirm it matches the persisted `key`. This catches the cross-
/// step alias attack flagged by the bead body: a malformed ledger
/// carrying a `txk:HASH` from step A while claiming `step_id = B`
/// is rejected at deserialize time, before the forged form can
/// reach dedup decisions or resume accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IdempotencyKeyKind {
    Action,
    Compensation,
}

impl IdempotencyKeyKind {
    const fn domain_tag(self) -> &'static [u8] {
        match self {
            Self::Action => b"action",
            Self::Compensation => b"compensation",
        }
    }

    const fn fingerprint_domain(self) -> &'static [u8] {
        match self {
            Self::Action => b"frankenterm.tx.action-fingerprint.v1",
            Self::Compensation => b"frankenterm.tx.compensation-fingerprint.v1",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct IdempotencyKey {
    /// The raw key string, format: `txk:v2:{sha256_hex}`.
    key: String,
    /// Plan ID this key belongs to.
    plan_id: String,
    /// Step ID within the plan.
    step_id: String,
    /// Semantic namespace for the external effect. Action and compensation
    /// keys must remain distinct even when caller-controlled fingerprints have
    /// identical text.
    key_kind: IdempotencyKeyKind,
    /// SHA-256 digest of the caller-provided action fingerprint. Persisting the
    /// digest instead of the raw preimage avoids retaining commands, action
    /// arguments, or secrets in every durable ledger while still allowing the
    /// custom `Deserialize` implementation to re-derive the canonical key.
    hash_input: String,
}

impl IdempotencyKey {
    /// Compute the canonical v2 key from the complete persisted tuple.
    fn compute_key(
        key_kind: IdempotencyKeyKind,
        plan_id: &str,
        step_id: &str,
        hash_input_digest: &str,
    ) -> String {
        format!(
            "txk:v2:{}",
            sha256_domain_digest(
                b"frankenterm.tx.idempotency-key.v2",
                &[
                    key_kind.domain_tag(),
                    plan_id.as_bytes(),
                    step_id.as_bytes(),
                    hash_input_digest.as_bytes(),
                ],
            )
        )
    }

    fn compute_hash_input_digest(key_kind: IdempotencyKeyKind, hash_input: &str) -> String {
        format!(
            "sha256:{}",
            sha256_domain_digest(key_kind.fingerprint_domain(), &[hash_input.as_bytes()])
        )
    }

    /// Create a new idempotency key from plan + step + action content.
    #[must_use]
    pub fn new(plan_id: &str, step_id: &str, action_fingerprint: &str) -> Self {
        let key_kind = IdempotencyKeyKind::Action;
        let hash_input = Self::compute_hash_input_digest(key_kind, action_fingerprint);
        Self {
            key: Self::compute_key(key_kind, plan_id, step_id, &hash_input),
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
            key_kind,
            hash_input,
        }
    }

    /// Create a key for a compensation execution.
    #[must_use]
    pub fn for_compensation(plan_id: &str, step_id: &str, compensation_kind: &str) -> Self {
        let key_kind = IdempotencyKeyKind::Compensation;
        let hash_input = Self::compute_hash_input_digest(key_kind, compensation_kind);
        Self {
            key: Self::compute_key(key_kind, plan_id, step_id, &hash_input),
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
            key_kind,
            hash_input,
        }
    }

    /// The string representation of this key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.key
    }

    /// The plan this key belongs to.
    #[must_use]
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    /// The step this key targets.
    #[must_use]
    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    const fn is_compensation(&self) -> bool {
        matches!(self.key_kind, IdempotencyKeyKind::Compensation)
    }
}

impl std::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key)
    }
}

/// br-ft-f4vta: validate that `raw` is a well-formed idempotency
/// key string. Returns `true` iff `raw` starts with `"txk:v2:"`
/// followed by exactly 64 lowercase-hex characters — the format
/// produced by [`IdempotencyKey::compute_key`]
/// and `for_compensation`. Rejects any other shape (wrong prefix,
/// uppercase hex, padding chars, length drift, embedded
/// whitespace).
fn is_well_formed_idempotency_key(raw: &str) -> bool {
    let Some(hex) = raw.strip_prefix("txk:v2:") else {
        return false;
    };
    is_lower_hex_digest(hex)
}

fn is_well_formed_hash_input_digest(raw: &str) -> bool {
    raw.strip_prefix("sha256:").is_some_and(is_lower_hex_digest)
}

fn is_lower_hex_digest(hex: &str) -> bool {
    hex.len() == 64
        && hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// br-ft-f4vta: shape struct used by `IdempotencyKey`'s custom
/// `Deserialize`. Mirrors the field layout 1:1 so we get serde's
/// derived parser for free; the outer impl below validates the
/// resulting fields against the documented format invariants AND
/// the canonical hash derivation from the persisted source tuple.
#[derive(Deserialize)]
struct IdempotencyKeyShape {
    key: String,
    plan_id: String,
    step_id: String,
    key_kind: IdempotencyKeyKind,
    hash_input: String,
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let shape = IdempotencyKeyShape::deserialize(deserializer)?;
        if !is_well_formed_idempotency_key(&shape.key) {
            return Err(serde::de::Error::custom(format!(
                "br-ft-f4vta: malformed IdempotencyKey raw key `{}`; expected format `txk:v2:` + 64 lowercase hex chars",
                shape.key
            )));
        }
        if shape.plan_id.is_empty() {
            return Err(serde::de::Error::custom(
                "br-ft-f4vta: IdempotencyKey.plan_id must not be empty",
            ));
        }
        if shape.step_id.is_empty() {
            return Err(serde::de::Error::custom(
                "br-ft-f4vta: IdempotencyKey.step_id must not be empty",
            ));
        }
        if !is_well_formed_hash_input_digest(&shape.hash_input) {
            return Err(serde::de::Error::custom(
                "br-ft-f4vta: IdempotencyKey.hash_input must be `sha256:` plus 64 lowercase hex chars",
            ));
        }
        // br-ft-f4vta cross-step alias check: re-derive the canonical
        // key from the persisted (key_kind, plan_id, step_id, hash_input) tuple
        // and confirm it matches the persisted `key`. Pre-fix the
        // attacker could swap any of (key, plan_id, step_id) without
        // detection — the format check alone catches malformed wire-
        // format strings but not a valid `txk:HASH` aliased to the
        // wrong step's metadata.
        let expected = IdempotencyKey::compute_key(
            shape.key_kind,
            &shape.plan_id,
            &shape.step_id,
            &shape.hash_input,
        );
        if expected != shape.key {
            return Err(serde::de::Error::custom(format!(
                "br-ft-f4vta: IdempotencyKey.key `{}` does not match \
                 hash(length-delimited key_kind, plan_id, step_id, hash_input) — persisted form is \
                 forged or aliased from a different step (expected `{}`)",
                shape.key, expected
            )));
        }
        Ok(Self {
            key: shape.key,
            plan_id: shape.plan_id,
            step_id: shape.step_id,
            key_kind: shape.key_kind,
            hash_input: shape.hash_input,
        })
    }
}

// ── Step Outcome ─────────────────────────────────────────────────────────────

/// Canonical outcome of executing a single tx step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// Step executed successfully.
    Success {
        /// Optional result payload (JSON-serializable).
        result: Option<String>,
    },
    /// Step failed with an error.
    Failed {
        error_code: String,
        error_message: String,
        /// Whether compensation was triggered.
        compensated: bool,
    },
    /// Step was skipped (e.g., precondition not met, already completed).
    Skipped { reason: String },
    /// Step was compensated (rollback executed).
    Compensated {
        original_outcome: Box<StepOutcome>,
        compensation_result: String,
    },
    /// Step is pending (not yet executed in this tx instance).
    Pending,
}

impl StepOutcome {
    /// Whether this outcome represents a terminal state (no more execution needed).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Success { .. } | Self::Skipped { .. } | Self::Compensated { .. }
        )
    }

    /// Whether this outcome represents a failure.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Whether execution is still pending.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Whether this outcome is durable replay proof that must not age out by
    /// wall-clock TTL alone.
    ///
    /// `Pending` is an ambiguity marker: expiration could turn an uncertain
    /// external dispatch into permission to dispatch again. Successful and
    /// compensated effects likewise remain authoritative until bounded
    /// capacity or an explicit reconciliation policy retires them. Failures
    /// may expire to permit a deliberate retry with the same key.
    #[must_use]
    fn is_sticky_replay_proof(&self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Success { .. } | Self::Compensated { .. }
        )
    }

    /// Whether the outcome is insufficient to prove whether an external
    /// effect should be considered complete.
    #[must_use]
    fn is_ambiguous_replay_outcome(&self) -> bool {
        matches!(self, Self::Pending | Self::Skipped { .. })
    }
}

// ── Step Execution Record ────────────────────────────────────────────────────

/// Record of a single step execution within a tx instance.
///
/// Records form a hash chain: each record includes the domain-separated
/// SHA-256 of the previous record (consistent with `recorder_audit.rs`).
/// Appended records are stable except for the write-ahead path, which may
/// upgrade `StepOutcome::Pending` to the terminal outcome and then re-hash the
/// affected suffix.
///
/// Scope of the guarantee (ft-vtdk4): the chain is **unkeyed**, so it detects
/// truncation, partial writes, reordering, and edits that do not recompute the
/// chain — including the serde-roundtrip tip mutation `verify_chain` checks
/// for. It is not evidence against an adversary who can write the spool, since
/// that adversary can recompute every hash. Same-UID write access to the
/// workspace `.ft` directory is outside the guarantee, exactly as it is for the
/// contract store (see the residual-risk note on `save_tx_contract_atomic`).
/// Claiming more would require an authenticated MAC or signature, which needs a
/// key-management story this crate does not have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepExecutionRecord {
    /// Monotonic ordinal within this tx ledger.
    pub ordinal: u64,
    /// The idempotency key for this execution.
    pub idem_key: IdempotencyKey,
    /// Execution instance ID (unique per tx run).
    pub execution_id: String,
    /// Timestamp in milliseconds since epoch.
    pub timestamp_ms: u64,
    /// The outcome of this execution.
    pub outcome: StepOutcome,
    /// Risk level of the step (from the plan).
    pub risk: StepRisk,
    /// SHA-256 hash of the previous record's canonical form (empty string for first).
    pub prev_hash: String,
    /// Agent that executed this step.
    pub agent_id: String,
}

impl StepExecutionRecord {
    /// Compute a domain-separated SHA-256 hash of this record's canonical form.
    ///
    /// **Chain-integrity contract**: the hash is load-bearing for
    /// the idempotency-resume protocol's chain-of-records dedup.
    /// Two records with the same hash are treated as identical;
    /// hash collisions corrupt the chain.
    ///
    /// br-ft-jyywz follow-up: previously this called
    /// `serde_json::to_string(&self.outcome).unwrap_or_default()`
    /// which silently produced an empty string on failure,
    /// collapsing distinct serialization-failure outcomes into
    /// the same hash. `StepOutcome` is a typed enum deriving
    /// `Serialize` from primitive `String` / `bool` / `Box`
    /// fields — `serde_json::to_string` on it is infallible, so
    /// the original `unwrap_or_default` was masking a contract
    /// the type system already enforces. Replaced with `expect`
    /// so a future refactor that adds a fallible-Serialize field
    /// trips the panic loudly at the call site rather than
    /// silently corrupting the chain.
    ///
    /// br-ft-e9r75: the canonical form now includes `risk` and
    /// `agent_id` (was previously omitted, leaving those fields
    /// unauthenticated by the hash chain). tx_observability builds
    /// forensic bundles from ledger records and chain verification
    /// (tx_observability.rs:705-718); without these fields in the
    /// hash, an attacker mutating risk or agent attribution after
    /// a record was written would be invisible to verify_chain
    /// while green-stamping the forensic evidence. Including all
    /// safety/forensic fields in the hash makes any post-write
    /// mutation detectable. NOTE: this is a chain-format change —
    /// ledgers serialized before this commit will report
    /// `chain_intact = false` after upgrade because the recomputed
    /// hashes will diverge from the embedded `prev_hash` chain.
    #[must_use]
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_vec(self).expect(
            "StepExecutionRecord serialization is infallible for its typed primitive fields",
        );
        sha256_domain_digest(
            b"frankenterm.tx.execution-record.v2",
            &[canonical.as_slice()],
        )
    }
}

// ── Execution Phase ──────────────────────────────────────────────────────────

/// Phase of tx execution for the resume protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxPhase {
    /// Transaction has been planned but not started.
    Planned,
    /// Prepare phase: validating preconditions, acquiring reservations.
    Preparing,
    /// Commit phase: executing steps in dependency order.
    Committing,
    /// Compensation phase: rolling back after a failure.
    Compensating,
    /// Transaction completed (success or fully compensated).
    Completed,
    /// Transaction aborted (unrecoverable failure).
    Aborted,
}

impl TxPhase {
    /// Whether this phase is terminal (no further transitions expected).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Aborted)
    }

    /// Valid next phases from the current phase.
    #[must_use]
    pub fn valid_transitions(self) -> &'static [TxPhase] {
        match self {
            Self::Planned => &[Self::Preparing, Self::Aborted],
            Self::Preparing => &[Self::Committing, Self::Aborted],
            Self::Committing => &[Self::Compensating, Self::Completed, Self::Aborted],
            Self::Compensating => &[Self::Completed, Self::Aborted],
            Self::Completed | Self::Aborted => &[],
        }
    }

    /// Whether transitioning to `next` is valid.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self.valid_transitions().contains(&next)
    }
}

// ── Tx Execution Ledger ──────────────────────────────────────────────────────

/// Kind of terminal disposition certified for a completed or aborted transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalDispositionKind {
    /// All plan steps were successfully committed.
    Committed,
    /// Transaction failed or was aborted, and all committed steps were fully compensated.
    RolledBack,
    /// Transaction aborted or failed without full commit or rollback compensation.
    Aborted,
}

/// Typed certificate proving terminal disposition of a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalDispositionCertificate {
    /// Certified disposition kind.
    pub disposition: TerminalDispositionKind,
    /// Unique execution instance ID.
    pub execution_id: String,
    /// Plan ID.
    pub plan_id: String,
    /// Plan hash.
    pub plan_hash: u64,
    /// Sorted list of step IDs that succeeded in the commit phase.
    pub completed_step_ids: Vec<String>,
    /// Sorted list of step IDs that failed in the commit phase.
    pub failed_step_ids: Vec<String>,
    /// Sorted list of step IDs that were compensated.
    pub compensated_step_ids: Vec<String>,
    /// Timestamp (ms) when the certificate was issued.
    pub certified_at_ms: u64,
}

/// Ordered ledger of execution records for a single tx instance.
///
/// Maintains a hash chain and provides lookup by idempotency key for dedup.
#[derive(Debug, Clone, Serialize)]
pub struct TxExecutionLedger {
    /// Unique execution instance ID.
    execution_id: String,
    /// The plan this ledger tracks.
    plan_id: String,
    /// Plan hash for integrity verification.
    plan_hash: u64,
    /// Current execution phase.
    phase: TxPhase,
    /// Ordered execution records (append-only).
    records: Vec<StepExecutionRecord>,
    /// Hash of the last appended record (empty string if no records).
    last_hash: String,
    /// Next ordinal to assign.
    next_ordinal: u64,
    /// Index: idem_key → record ordinal for O(1) dedup lookup.
    #[serde(skip)]
    key_index: HashMap<IdempotencyKey, u64>,
    /// Terminal disposition certificate, if in a terminal phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_certificate: Option<TerminalDispositionCertificate>,
}

#[derive(Deserialize)]
struct TxExecutionLedgerSerde {
    execution_id: String,
    plan_id: String,
    plan_hash: u64,
    phase: TxPhase,
    records: Vec<StepExecutionRecord>,
    last_hash: String,
    next_ordinal: u64,
    #[serde(default)]
    terminal_certificate: Option<TerminalDispositionCertificate>,
}

fn build_ledger_key_index(
    records: &[StepExecutionRecord],
) -> Result<HashMap<IdempotencyKey, u64>, String> {
    let mut key_index = HashMap::with_capacity(records.len());

    for record in records {
        let key = record.idem_key.clone();
        match key_index.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(record.ordinal);
            }
            Entry::Occupied(entry) => {
                return Err(format!(
                    "duplicate idempotency key in tx ledger: {} at ordinals {} and {}",
                    entry.key(),
                    entry.get(),
                    record.ordinal
                ));
            }
        }
    }

    Ok(key_index)
}

impl<'de> Deserialize<'de> for TxExecutionLedger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let serialized = TxExecutionLedgerSerde::deserialize(deserializer)?;
        if !is_valid_execution_id(&serialized.execution_id) {
            return Err(de::Error::custom(format!(
                "TxExecutionLedger execution_id is unsafe or malformed: {:?}",
                serialized.execution_id
            )));
        }
        if serialized.plan_id.is_empty() {
            return Err(de::Error::custom(
                "TxExecutionLedger plan_id must not be empty",
            ));
        }
        let mut timestamp_high_water = None;
        for record in &serialized.records {
            if record.execution_id != serialized.execution_id {
                return Err(de::Error::custom(format!(
                    "TxExecutionLedger record {} execution_id {:?} does not match ledger execution_id {:?}",
                    record.ordinal, record.execution_id, serialized.execution_id
                )));
            }
            if record.idem_key.plan_id() != serialized.plan_id {
                return Err(de::Error::custom(format!(
                    "TxExecutionLedger record {} idempotency plan_id {:?} does not match ledger plan_id {:?}",
                    record.ordinal,
                    record.idem_key.plan_id(),
                    serialized.plan_id
                )));
            }
            if let Some(previous) = timestamp_high_water
                && record.timestamp_ms < previous
            {
                return Err(de::Error::custom(format!(
                    "TxExecutionLedger record {} timestamp {} is below prior high-water {}",
                    record.ordinal, record.timestamp_ms, previous
                )));
            }
            timestamp_high_water = Some(record.timestamp_ms);
        }
        if serialized.phase.is_terminal() {
            let ambiguous_ordinals: Vec<u64> = serialized
                .records
                .iter()
                .filter(|record| record.outcome.is_ambiguous_replay_outcome())
                .map(|record| record.ordinal)
                .collect();
            if !ambiguous_ordinals.is_empty() {
                return Err(de::Error::custom(format!(
                    "terminal TxExecutionLedger contains ambiguous Pending/Skipped records at ordinals {ambiguous_ordinals:?}"
                )));
            }
            let cert = serialized.terminal_certificate.as_ref().ok_or_else(|| {
                de::Error::custom(format!(
                    "terminal TxExecutionLedger in phase {:?} lacks a terminal disposition certificate",
                    serialized.phase
                ))
            })?;
            let temp_ledger = Self {
                execution_id: serialized.execution_id.clone(),
                plan_id: serialized.plan_id.clone(),
                plan_hash: serialized.plan_hash,
                phase: serialized.phase,
                records: serialized.records.clone(),
                last_hash: serialized.last_hash.clone(),
                next_ordinal: serialized.next_ordinal,
                key_index: HashMap::new(),
                terminal_certificate: None,
            };
            temp_ledger
                .validate_certificate_against_ledger(cert, serialized.phase)
                .map_err(de::Error::custom)?;
        }
        let key_index = build_ledger_key_index(&serialized.records).map_err(de::Error::custom)?;

        // br-ft-ddm8k: validate trust-boundary invariants before
        // installing the deserialized state. Pre-fix the impl
        // accepted any (next_ordinal, last_hash, ordinal sequence)
        // combination as long as idem_keys were unique. A forged
        // persisted ledger could:
        //   (a) set next_ordinal to a value already present in
        //       records, causing the next append() to reuse the
        //       ordinal and corrupt order-by-ordinal queries;
        //   (b) set last_hash to anything (or empty) regardless
        //       of the actual records vec, breaking the chain-of-
        //       hashes anchor so subsequent appends compute
        //       prev_hash from the wrong tip;
        //   (c) supply non-monotonic / sparse ordinals, breaking
        //       resume / dedup / forensic-bundle semantics that
        //       all assume strict 0..records.len() ordinals.
        // Each of these is rejected fail-closed below.

        // (1) Ordinal density + monotonicity: 0..records.len().
        for (expected, record) in serialized.records.iter().enumerate() {
            let expected_u64 = u64::try_from(expected).map_err(|_| {
                de::Error::custom("br-ft-ddm8k: TxExecutionLedger record count exceeds u64 range")
            })?;
            if record.ordinal != expected_u64 {
                return Err(de::Error::custom(format!(
                    "br-ft-ddm8k: TxExecutionLedger record at index {} has ordinal {} (expected dense 0..len, must equal index)",
                    expected, record.ordinal
                )));
            }
        }

        // (2) next_ordinal must equal records.len() (strictly
        // greater than every record ordinal; equal to len because
        // ordinals are dense 0..len).
        let expected_next = u64::try_from(serialized.records.len()).map_err(|_| {
            de::Error::custom("br-ft-ddm8k: TxExecutionLedger record count exceeds u64 range")
        })?;
        if serialized.next_ordinal != expected_next {
            return Err(de::Error::custom(format!(
                "br-ft-ddm8k: TxExecutionLedger next_ordinal {} does not match records.len() {} — append would reuse an existing ordinal or skip into a gap",
                serialized.next_ordinal, expected_next
            )));
        }

        // (3) last_hash must equal records.last().hash() — empty
        // when records is empty, otherwise the last record's
        // hash. The chain-of-hashes anchor is the load-bearing
        // dedup primitive; a detached anchor lets append() compute
        // prev_hash from the wrong tip and silently break chain
        // continuity (verify_chain_continuity then fails AFTER
        // the corruption is committed, instead of at load time).
        let expected_last_hash = serialized
            .records
            .last()
            .map(StepExecutionRecord::hash)
            .unwrap_or_default();
        if serialized.last_hash != expected_last_hash {
            return Err(de::Error::custom(format!(
                "br-ft-ddm8k: TxExecutionLedger last_hash `{}` does not match records.last().hash() `{}` — chain anchor is detached from the record vec",
                serialized.last_hash, expected_last_hash
            )));
        }

        Ok(Self {
            execution_id: serialized.execution_id,
            plan_id: serialized.plan_id,
            plan_hash: serialized.plan_hash,
            phase: serialized.phase,
            records: serialized.records,
            last_hash: serialized.last_hash,
            next_ordinal: serialized.next_ordinal,
            key_index,
            terminal_certificate: serialized.terminal_certificate,
        })
    }
}

impl TxExecutionLedger {
    /// Create a new empty ledger for a tx execution.
    #[must_use]
    pub fn new(execution_id: &str, plan_id: &str, plan_hash: u64) -> Self {
        Self {
            execution_id: execution_id.to_string(),
            plan_id: plan_id.to_string(),
            plan_hash,
            phase: TxPhase::Planned,
            records: Vec::new(),
            last_hash: String::new(),
            next_ordinal: 0,
            key_index: HashMap::new(),
        }
    }

    /// The execution instance ID.
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// The plan ID this ledger tracks.
    #[must_use]
    pub fn plan_id(&self) -> &str {
        &self.plan_id
    }

    /// The deterministic plan hash.
    #[must_use]
    pub fn plan_hash(&self) -> u64 {
        self.plan_hash
    }

    /// The hash chain tip (hash of the last appended record).
    #[must_use]
    pub fn last_hash(&self) -> &str {
        &self.last_hash
    }

    /// Current phase of this tx execution.
    #[must_use]
    pub fn phase(&self) -> TxPhase {
        self.phase
    }

    /// Number of records in the ledger.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// All records in order.
    #[must_use]
    pub fn records(&self) -> &[StepExecutionRecord] {
        &self.records
    }

    /// Terminal disposition certificate, if the ledger is in a terminal phase.
    #[must_use]
    pub fn terminal_certificate(&self) -> Option<&TerminalDispositionCertificate> {
        self.terminal_certificate.as_ref()
    }

    /// Set an explicit terminal certificate before transitioning to a terminal phase.
    pub fn set_terminal_certificate(
        &mut self,
        cert: TerminalDispositionCertificate,
    ) -> Result<(), IdempotencyError> {
        self.validate_certificate_against_ledger(&cert, self.phase)
            .map_err(|reason| IdempotencyError::InvalidTerminalCertificate { reason })?;
        self.terminal_certificate = Some(cert);
        Ok(())
    }

    /// Validate a terminal certificate against this ledger's records.
    pub fn validate_certificate_against_ledger(
        &self,
        cert: &TerminalDispositionCertificate,
        target_phase: TxPhase,
    ) -> Result<(), String> {
        if cert.execution_id != self.execution_id {
            return Err(format!(
                "certificate execution_id {:?} does not match ledger execution_id {:?}",
                cert.execution_id, self.execution_id
            ));
        }
        if cert.plan_id != self.plan_id {
            return Err(format!(
                "certificate plan_id {:?} does not match ledger plan_id {:?}",
                cert.plan_id, self.plan_id
            ));
        }
        if cert.plan_hash != self.plan_hash {
            return Err(format!(
                "certificate plan_hash {} does not match ledger plan_hash {}",
                cert.plan_hash, self.plan_hash
            ));
        }

        let mut actual_completed = Vec::new();
        let mut actual_failed = Vec::new();
        let mut actual_compensated = Vec::new();

        for record in &self.records {
            let step_id = record.idem_key.step_id().to_string();
            match &record.outcome {
                StepOutcome::Success { .. } => {
                    if !actual_completed.contains(&step_id) {
                        actual_completed.push(step_id);
                    }
                }
                StepOutcome::Failed { .. } => {
                    if !actual_failed.contains(&step_id) {
                        actual_failed.push(step_id);
                    }
                }
                StepOutcome::Compensated { .. } => {
                    if !actual_compensated.contains(&step_id) {
                        actual_compensated.push(step_id);
                    }
                }
                _ => {}
            }
        }
        actual_completed.sort();
        actual_failed.sort();
        actual_compensated.sort();

        if cert.completed_step_ids != actual_completed {
            return Err(format!(
                "certificate completed_step_ids {:?} does not match actual completed records {:?}",
                cert.completed_step_ids, actual_completed
            ));
        }
        if cert.failed_step_ids != actual_failed {
            return Err(format!(
                "certificate failed_step_ids {:?} does not match actual failed records {:?}",
                cert.failed_step_ids, actual_failed
            ));
        }
        if cert.compensated_step_ids != actual_compensated {
            return Err(format!(
                "certificate compensated_step_ids {:?} does not match actual compensated records {:?}",
                cert.compensated_step_ids, actual_compensated
            ));
        }

        match cert.disposition {
            TerminalDispositionKind::Committed => {
                if target_phase != TxPhase::Completed {
                    return Err(format!(
                        "Committed disposition requires Completed phase; target phase is {target_phase:?}"
                    ));
                }
                if !cert.failed_step_ids.is_empty() {
                    return Err(format!(
                        "Committed disposition cannot have failed steps: {:?}",
                        cert.failed_step_ids
                    ));
                }
                if !cert.compensated_step_ids.is_empty() {
                    return Err(format!(
                        "Committed disposition cannot have compensated steps: {:?}",
                        cert.compensated_step_ids
                    ));
                }
            }
            TerminalDispositionKind::RolledBack => {
                if target_phase != TxPhase::Completed && target_phase != TxPhase::Aborted {
                    return Err(format!(
                        "RolledBack disposition requires Completed or Aborted phase; target phase is {target_phase:?}"
                    ));
                }
                if cert.compensated_step_ids.is_empty() {
                    return Err(
                        "RolledBack disposition requires non-empty compensated steps"
                            .to_string(),
                    );
                }
                if cert.completed_step_ids != cert.compensated_step_ids {
                    return Err(format!(
                        "RolledBack disposition requires every completed step to be compensated; completed={:?}, compensated={:?}",
                        cert.completed_step_ids, cert.compensated_step_ids
                    ));
                }
            }
            TerminalDispositionKind::Aborted => {
                if target_phase != TxPhase::Aborted {
                    return Err(format!(
                        "Aborted disposition requires Aborted phase; target phase is {target_phase:?}"
                    ));
                }
            }
        }

        Ok(())
    }

    /// Synthesize the canonical terminal certificate for this ledger in `target_phase`.
    pub fn synthesize_terminal_certificate(
        &self,
        target_phase: TxPhase,
    ) -> Result<TerminalDispositionCertificate, IdempotencyError> {
        let mut actual_completed = Vec::new();
        let mut actual_failed = Vec::new();
        let mut actual_compensated = Vec::new();

        for record in &self.records {
            let step_id = record.idem_key.step_id().to_string();
            match &record.outcome {
                StepOutcome::Success { .. } => {
                    if !actual_completed.contains(&step_id) {
                        actual_completed.push(step_id);
                    }
                }
                StepOutcome::Failed { .. } => {
                    if !actual_failed.contains(&step_id) {
                        actual_failed.push(step_id);
                    }
                }
                StepOutcome::Compensated { .. } => {
                    if !actual_compensated.contains(&step_id) {
                        actual_compensated.push(step_id);
                    }
                }
                _ => {}
            }
        }
        actual_completed.sort();
        actual_failed.sort();
        actual_compensated.sort();

        let disposition = match target_phase {
            TxPhase::Completed => {
                if actual_failed.is_empty() && actual_compensated.is_empty() {
                    TerminalDispositionKind::Committed
                } else if !actual_compensated.is_empty() && actual_compensated == actual_completed {
                    TerminalDispositionKind::RolledBack
                } else {
                    return Err(IdempotencyError::InvalidTerminalCertificate {
                        reason: format!(
                            "cannot transition to Completed without proving all steps committed or all completed steps compensated on execution {}; completed={:?}, failed={:?}, compensated={:?}",
                            self.execution_id, actual_completed, actual_failed, actual_compensated
                        ),
                    });
                }
            }
            TxPhase::Aborted => {
                if !actual_compensated.is_empty() && actual_compensated == actual_completed {
                    TerminalDispositionKind::RolledBack
                } else {
                    TerminalDispositionKind::Aborted
                }
            }
            _ => {
                return Err(IdempotencyError::InvalidTerminalCertificate {
                    reason: format!(
                        "cannot synthesize terminal certificate for non-terminal phase {target_phase:?}"
                    ),
                });
            }
        };

        let certified_at_ms = self.records.last().map(|r| r.timestamp_ms).unwrap_or(0);
        let cert = TerminalDispositionCertificate {
            disposition,
            execution_id: self.execution_id.clone(),
            plan_id: self.plan_id.clone(),
            plan_hash: self.plan_hash,
            completed_step_ids: actual_completed,
            failed_step_ids: actual_failed,
            compensated_step_ids: actual_compensated,
            certified_at_ms,
        };
        Ok(cert)
    }

    /// Transition to a new phase. Returns `Err` if the transition is invalid.
    pub fn transition_phase(&mut self, next: TxPhase) -> Result<TxPhase, IdempotencyError> {
        if !self.phase.can_transition_to(next) {
            return Err(IdempotencyError::InvalidPhaseTransition {
                from: self.phase,
                to: next,
            });
        }
        if next.is_terminal() {
            let ambiguous_ordinals: Vec<u64> = self
                .records
                .iter()
                .filter(|record| record.outcome.is_ambiguous_replay_outcome())
                .map(|record| record.ordinal)
                .collect();
            if !ambiguous_ordinals.is_empty() {
                return Err(IdempotencyError::AmbiguousTerminalTransition {
                    execution_id: self.execution_id.clone(),
                    ambiguous_ordinals,
                });
            }
            if self.terminal_certificate.is_none() {
                let cert = self.synthesize_terminal_certificate(next)?;
                self.terminal_certificate = Some(cert);
            } else if let Some(ref cert) = self.terminal_certificate {
                self.validate_certificate_against_ledger(cert, next)
                    .map_err(|reason| IdempotencyError::InvalidTerminalCertificate { reason })?;
            }
        }
        let prev = self.phase;
        self.phase = next;
        Ok(prev)
    }

    /// Check if a step has already been executed (dedup check).
    #[must_use]
    pub fn is_executed(&self, idem_key: &IdempotencyKey) -> bool {
        self.key_index.contains_key(idem_key)
    }

    /// Get the record for a previously executed step, if any.
    #[must_use]
    pub fn get_record(&self, idem_key: &IdempotencyKey) -> Option<&StepExecutionRecord> {
        self.key_index
            .get(idem_key)
            .and_then(|&ordinal| self.records.iter().find(|r| r.ordinal == ordinal))
    }

    /// Get the outcome of a previously executed step.
    #[must_use]
    pub fn get_outcome(&self, idem_key: &IdempotencyKey) -> Option<&StepOutcome> {
        self.get_record(idem_key).map(|r| &r.outcome)
    }

    /// Append an execution record. Returns the record's hash.
    ///
    /// # Errors
    ///
    /// - `DuplicateExecution` if this idem_key was already recorded.
    /// - `InvalidPhaseTransition` if the ledger is in a terminal phase.
    ///
    /// br-ft-738kn: defensive self-heal. Normal deserialization
    /// rebuilds the runtime-only `key_index` before this type is
    /// usable. If another constructor path leaves records populated
    /// but the index empty, rebuild and validate before the
    /// duplicate check so resume never replays a recorded step.
    pub fn append(
        &mut self,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        if self.phase.is_terminal() {
            return Err(IdempotencyError::LedgerSealed { phase: self.phase });
        }
        if idem_key.plan_id() != self.plan_id {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "idempotency key plan_id {:?} does not match ledger plan_id {:?}",
                    idem_key.plan_id(),
                    self.plan_id
                ),
            });
        }
        if let Some(previous) = self.records.last()
            && timestamp_ms < previous.timestamp_ms
        {
            return Err(IdempotencyError::RetrogradeTimestamp {
                observed_ms: timestamp_ms,
                high_water_ms: previous.timestamp_ms,
            });
        }

        if self.key_index.is_empty() && !self.records.is_empty() {
            self.rebuild_index_checked()
                .map_err(|reason| IdempotencyError::LedgerIndexCorrupt { reason })?;
        }

        if self.key_index.contains_key(&idem_key) {
            return Err(IdempotencyError::DuplicateExecution {
                key: idem_key.as_str().to_string(),
            });
        }

        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;

        let record = StepExecutionRecord {
            ordinal,
            idem_key: idem_key.clone(),
            execution_id: self.execution_id.clone(),
            timestamp_ms,
            outcome,
            risk,
            prev_hash: self.last_hash.clone(),
            agent_id: agent_id.to_string(),
        };

        let record_hash = record.hash();
        self.last_hash.clone_from(&record_hash);
        self.key_index.insert(idem_key, ordinal);
        self.records.push(record);

        Ok(record_hash)
    }

    /// Upgrade a previously reserved pending record to its terminal outcome.
    ///
    /// The tx executor writes `StepOutcome::Pending` before dispatching an
    /// external side effect. After dispatch returns, this method rewrites that
    /// reservation in place and re-hashes the affected suffix of the chain so a
    /// crash between reservation and dispatch fails closed on replay, while the
    /// normal path still leaves one canonical record for the idempotency key.
    pub fn complete_pending(
        &mut self,
        idem_key: &IdempotencyKey,
        outcome: StepOutcome,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        if self.phase.is_terminal() {
            return Err(IdempotencyError::LedgerSealed { phase: self.phase });
        }

        if self.key_index.is_empty() && !self.records.is_empty() {
            self.rebuild_index_checked()
                .map_err(|reason| IdempotencyError::LedgerIndexCorrupt { reason })?;
        }

        let ordinal =
            *self
                .key_index
                .get(idem_key)
                .ok_or_else(|| IdempotencyError::LedgerIndexCorrupt {
                    reason: format!(
                        "cannot complete unreserved idempotency key {}",
                        idem_key.as_str()
                    ),
                })?;
        let index = self
            .records
            .iter()
            .position(|record| record.ordinal == ordinal)
            .ok_or_else(|| IdempotencyError::LedgerIndexCorrupt {
                reason: format!(
                    "idempotency key {} points to missing ordinal {}",
                    idem_key.as_str(),
                    ordinal
                ),
            })?;

        if !self.records[index].outcome.is_pending() {
            return Err(IdempotencyError::DuplicateExecution {
                key: idem_key.as_str().to_string(),
            });
        }
        if outcome.is_ambiguous_replay_outcome() {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: "a Pending reservation must complete with an unambiguous outcome"
                    .to_string(),
            });
        }
        let previous_timestamp = index
            .checked_sub(1)
            .and_then(|previous| self.records.get(previous))
            .map(|record| record.timestamp_ms)
            .unwrap_or(0);
        let minimum_timestamp = previous_timestamp.max(self.records[index].timestamp_ms);
        let next_timestamp = self
            .records
            .get(index + 1)
            .map(|record| record.timestamp_ms);
        if timestamp_ms < minimum_timestamp
            || next_timestamp.is_some_and(|next| timestamp_ms > next)
        {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "completion timestamp {timestamp_ms} would violate ledger timestamp order around ordinal {ordinal}"
                ),
            });
        }

        self.records[index].outcome = outcome;
        self.records[index].timestamp_ms = timestamp_ms;
        for idx in index..self.records.len() {
            self.records[idx].prev_hash = if idx == 0 {
                String::new()
            } else {
                self.records[idx - 1].hash()
            };
        }
        self.last_hash = self
            .records
            .last()
            .map(StepExecutionRecord::hash)
            .unwrap_or_default();

        Ok(self.records[index].hash())
    }

    /// Verify the hash chain integrity. Returns details of any breaks.
    ///
    /// br-ft-e9r75: the loop walks `prev_hash` links between consecutive
    /// records, but that only authenticates `records[0..N-1]` against
    /// `records[N-1].prev_hash`. The TIP itself (`records[N-1]`) was
    /// previously not verified against the canonical `self.last_hash`
    /// recorded at append time — so mutating any field of the last
    /// record (outcome, timestamp, key, risk, agent_id) after a serde
    /// roundtrip would leave the prev_hash chain consistent and the
    /// tampering invisible. Post-loop we now compare the recomputed
    /// final hash to the stored tip and flag the last record's ordinal
    /// as the break point if they diverge.
    #[must_use]
    pub fn verify_chain(&self) -> ChainVerification {
        let mut expected_prev = String::new();
        let mut first_break_at = None;
        let mut missing_ordinals = Vec::new();
        let mut expected_ordinal = 0u64;

        for record in &self.records {
            if record.ordinal != expected_ordinal {
                for gap in expected_ordinal..record.ordinal {
                    missing_ordinals.push(gap);
                }
            }
            expected_ordinal = record.ordinal + 1;

            if record.prev_hash != expected_prev && first_break_at.is_none() {
                first_break_at = Some(record.ordinal);
            }
            expected_prev = record.hash();
        }

        // br-ft-e9r75: tip authentication. After the loop, expected_prev
        // == hash(records[N-1]). self.last_hash was set on append from
        // the same hash at insertion time. Divergence means the last
        // record was mutated after append (e.g., via serde-roundtrip
        // tampering) in a way the prev_hash chain cannot detect.
        if first_break_at.is_none() && !self.records.is_empty() && expected_prev != self.last_hash {
            first_break_at = Some(
                self.records
                    .last()
                    .expect("records non-empty checked above")
                    .ordinal,
            );
        }

        ChainVerification {
            chain_intact: first_break_at.is_none() && missing_ordinals.is_empty(),
            first_break_at,
            missing_ordinals,
            total_records: self.records.len(),
        }
    }

    /// Rebuild the key index after deserialization.
    pub fn rebuild_index(&mut self) {
        self.rebuild_index_checked()
            .expect("tx execution ledger contains duplicate idempotency keys");
    }

    fn rebuild_index_checked(&mut self) -> Result<(), String> {
        self.key_index = build_ledger_key_index(&self.records)?;
        Ok(())
    }

    /// Get all step IDs that completed successfully.
    #[must_use]
    pub fn completed_steps(&self) -> HashSet<String> {
        self.records
            .iter()
            .filter(|r| r.outcome.is_terminal() && !r.outcome.is_failure())
            .map(|r| r.idem_key.step_id().to_string())
            .collect()
    }

    /// Get all step IDs that failed.
    #[must_use]
    pub fn failed_steps(&self) -> HashSet<String> {
        self.records
            .iter()
            .filter(|r| r.outcome.is_failure())
            .map(|r| r.idem_key.step_id().to_string())
            .collect()
    }

    /// Get step IDs that still need execution (not in ledger at all).
    #[must_use]
    pub fn pending_step_ids(&self, plan: &TxPlan) -> Vec<String> {
        let executed: HashSet<&str> = self.records.iter().map(|r| r.idem_key.step_id()).collect();
        plan.steps
            .iter()
            .filter(|s| !executed.contains(s.id.as_str()))
            .map(|s| s.id.clone())
            .collect()
    }
}

// ── Chain Verification ───────────────────────────────────────────────────────

/// Result of verifying a ledger's hash chain integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerification {
    /// Whether the entire chain is intact (no breaks, no gaps).
    pub chain_intact: bool,
    /// Ordinal of the first hash break, if any.
    pub first_break_at: Option<u64>,
    /// Missing ordinals (gaps in the sequence).
    pub missing_ordinals: Vec<u64>,
    /// Total number of records checked.
    pub total_records: usize,
}

// ── Deduplication Guard ──────────────────────────────────────────────────────

/// Prevents double-commit and double-compensation across tx instances.
///
/// Maintains a sliding window of recent execution IDs with their outcomes,
/// enabling cross-instance dedup (e.g., if a process restarts mid-tx and
/// replays the same plan).
#[derive(Debug, Clone)]
pub struct DeduplicationGuard {
    /// Maximum number of entries to retain.
    capacity: usize,
    /// Map: idempotency key → (execution_id, outcome, timestamp_ms).
    entries: BTreeMap<IdempotencyKey, DeduplicationEntry>,
    /// Oldest-to-newest record/update order for eviction.
    order: VecDeque<IdempotencyKey>,
}

/// A single dedup entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeduplicationEntry {
    pub execution_id: String,
    pub outcome: StepOutcome,
    pub timestamp_ms: u64,
}

impl DeduplicationGuard {
    /// Create a new guard with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Check if a key has already been executed. Returns the cached outcome if so.
    #[must_use]
    pub fn check(&self, idem_key: &IdempotencyKey) -> Option<&DeduplicationEntry> {
        self.entries.get(idem_key)
    }

    fn latest_timestamp_ms(&self) -> Option<u64> {
        self.entries.values().map(|entry| entry.timestamp_ms).max()
    }

    /// Record a new execution. Evicts oldest entry if at capacity.
    pub fn record(
        &mut self,
        idem_key: &IdempotencyKey,
        execution_id: &str,
        outcome: StepOutcome,
        timestamp_ms: u64,
    ) {
        let key = idem_key.clone();

        // If already present, update in place and refresh eviction order.
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.execution_id = execution_id.to_string();
            entry.outcome = outcome;
            entry.timestamp_ms = timestamp_ms;
            self.order.retain(|existing| existing != &key);
            self.order.push_back(key);
            return;
        }

        // Evict if at capacity.
        if self.entries.len() >= self.capacity {
            if let Some(oldest_key) = self.order.pop_front() {
                self.entries.remove(&oldest_key);
            }
        }

        self.entries.insert(
            key.clone(),
            DeduplicationEntry {
                execution_id: execution_id.to_string(),
                outcome,
                timestamp_ms,
            },
        );
        self.order.push_back(key);
    }

    /// Number of entries currently tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the guard is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    /// Evict entries older than the given timestamp.
    pub fn evict_before(&mut self, cutoff_ms: u64) {
        let expired: HashSet<IdempotencyKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.timestamp_ms < cutoff_ms && !entry.outcome.is_sticky_replay_proof()
            })
            .map(|(k, _)| k.clone())
            .collect();
        self.entries.retain(|key, _| !expired.contains(key));
        self.order.retain(|k| !expired.contains(k));
    }
}

// ── Resume Context ───────────────────────────────────────────────────────────

/// Reconstructed tx state for restart recovery.
///
/// Built from a persisted [`TxExecutionLedger`] and the original [`TxPlan`],
/// this context tells the resume protocol exactly what has been done and what
/// remains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeContext {
    /// The execution ID being resumed.
    pub execution_id: String,
    /// Plan ID.
    pub plan_id: String,
    /// Phase at the time of interruption.
    pub interrupted_phase: TxPhase,
    /// Steps that completed successfully (step IDs).
    pub completed_steps: Vec<String>,
    /// Steps that failed (step IDs).
    pub failed_steps: Vec<String>,
    /// Steps that still need execution (step IDs, in dependency order).
    pub remaining_steps: Vec<String>,
    /// Steps that were compensated (step IDs).
    pub compensated_steps: Vec<String>,
    /// Whether the hash chain is intact.
    pub chain_intact: bool,
    /// Last known good hash.
    pub last_hash: String,
    /// Resume recommendation.
    pub recommendation: ResumeRecommendation,
}

/// What the resume protocol recommends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeRecommendation {
    /// Continue execution from where it left off.
    ContinueFromCheckpoint,
    /// Restart the entire tx (chain corrupted or too stale).
    RestartFresh,
    /// Compensate and abort (unrecoverable partial failure).
    CompensateAndAbort,
    /// Transaction already completed, nothing to do.
    AlreadyComplete,
}

impl ResumeContext {
    /// Build a resume context from a ledger and plan.
    #[must_use]
    pub fn from_ledger(ledger: &TxExecutionLedger, plan: &TxPlan) -> Self {
        Self::from_ledger_with_policy(ledger, plan, &IdempotencyPolicy::default())
    }

    /// Build a resume context while applying store-level resume policy.
    #[must_use]
    pub fn from_ledger_with_policy(
        ledger: &TxExecutionLedger,
        plan: &TxPlan,
        policy: &IdempotencyPolicy,
    ) -> Self {
        let verification = ledger.verify_chain();
        let completed: HashSet<String> = ledger
            .records()
            .iter()
            .filter(|record| matches!(record.outcome, StepOutcome::Success { .. }))
            .map(|record| record.idem_key.step_id().to_string())
            .collect();
        let failed = ledger.failed_steps();

        // Identify compensated steps.
        let compensated: HashSet<String> = ledger
            .records()
            .iter()
            .filter(|r| matches!(r.outcome, StepOutcome::Compensated { .. }))
            .map(|r| r.idem_key.step_id().to_string())
            .collect();
        let has_ambiguous_outcome = ledger
            .records()
            .iter()
            .any(|record| record.outcome.is_ambiguous_replay_outcome());
        let remaining = if failed.is_empty() {
            plan.steps
                .iter()
                .filter(|step| {
                    (!policy.skip_completed_on_resume || !completed.contains(&step.id))
                        && !compensated.contains(&step.id)
                })
                .map(|step| step.id.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let recommendation = if policy.require_chain_integrity && !verification.chain_intact {
            ResumeRecommendation::RestartFresh
        } else if has_ambiguous_outcome {
            ResumeRecommendation::CompensateAndAbort
        } else if ledger.phase().is_terminal() {
            if let Some(cert) = ledger.terminal_certificate() {
                match cert.disposition {
                    TerminalDispositionKind::Committed => {
                        let missing_plan_coverage =
                            plan.steps.iter().any(|s| !completed.contains(&s.id));
                        if missing_plan_coverage {
                            ResumeRecommendation::RestartFresh
                        } else {
                            ResumeRecommendation::AlreadyComplete
                        }
                    }
                    TerminalDispositionKind::RolledBack => ResumeRecommendation::AlreadyComplete,
                    TerminalDispositionKind::Aborted => {
                        if !completed.is_empty() && compensated != completed {
                            ResumeRecommendation::CompensateAndAbort
                        } else {
                            ResumeRecommendation::AlreadyComplete
                        }
                    }
                }
            } else {
                ResumeRecommendation::RestartFresh
            }
        } else if !failed.is_empty() {
            ResumeRecommendation::CompensateAndAbort
        } else if remaining.is_empty() && failed.is_empty() && !completed.is_empty() {
            ResumeRecommendation::AlreadyComplete
        } else {
            ResumeRecommendation::ContinueFromCheckpoint
        };

        let mut completed_steps = completed.into_iter().collect::<Vec<_>>();
        completed_steps.sort();
        let mut failed_steps = failed.into_iter().collect::<Vec<_>>();
        failed_steps.sort();
        let mut compensated_steps = compensated.into_iter().collect::<Vec<_>>();
        compensated_steps.sort();

        Self {
            execution_id: ledger.execution_id().to_string(),
            plan_id: ledger.plan_id().to_string(),
            interrupted_phase: ledger.phase(),
            completed_steps,
            failed_steps,
            remaining_steps: remaining,
            compensated_steps,
            chain_intact: verification.chain_intact,
            last_hash: ledger.last_hash.clone(),
            recommendation,
        }
    }
}

// ── Idempotency Store ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProofBarrierMode {
    Shared,
    Exclusive,
}

/// Plan-scoped cross-process barrier for durable idempotency proof reads.
///
/// Ordinary single-key writers hold this barrier shared before taking their
/// exclusive key lock. Atomic rollback proof preflight holds it exclusive,
/// which freezes every durable outcome for the plan while requiring only one
/// file descriptor regardless of the number of commit/compensation keys.
#[derive(Debug)]
struct ProofBarrierGuard {
    plan_id: String,
    mode: ProofBarrierMode,
    lock_dir: Arc<Dir>,
    lock_name: PathBuf,
    spool_display: PathBuf,
    _lock_file: File,
}

/// Optional per-key component of a durable lease.
///
/// Single-key writers own one of these beneath a shared plan barrier. Batch
/// proof leases need no key file: their shared `Arc<ProofBarrierGuard>` owns
/// the plan's exclusive barrier for the entire operation.
#[derive(Debug)]
struct DurableKeyLockGuard {
    lock_dir: Arc<Dir>,
    lock_name: PathBuf,
    spool_display: PathBuf,
    _lock_file: File,
}

/// Key-only lease used to make durable proof preflight atomic with later
/// execution-bound mutation.
///
/// The live-spool outcome is refreshed after the applicable barrier/lock is
/// acquired and remains stable for as long as this value is alive. The lease
/// intentionally owns no execution lock, so callers can bind selected leases
/// only after the complete proof set has been validated.
#[derive(Debug)]
pub(crate) struct DurableKeyLease {
    idem_key: IdempotencyKey,
    observed_outcome: Option<StepOutcome>,
    key_lock: Option<DurableKeyLockGuard>,
    proof_barrier: Arc<ProofBarrierGuard>,
}

impl DurableKeyLease {
    /// Durable outcome observed while the plan barrier and any required key
    /// lock were continuously held.
    #[must_use]
    pub(crate) fn observed_outcome(&self) -> Option<&StepOutcome> {
        self.observed_outcome.as_ref()
    }

    /// Whether this lease owns the exact requested key.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn authorizes(&self, idem_key: &IdempotencyKey) -> bool {
        &self.idem_key == idem_key
    }
}

/// Canonically ordered logical key leases protected by one plan barrier.
///
/// Construction validates the complete input set before taking the exclusive
/// barrier, then scans every durable ledger exactly once. Individual leases
/// can be removed and bound to an execution while this set retains the barrier
/// independently, so consuming the final logical lease cannot unfreeze proof
/// before the rollback operation returns.
#[derive(Debug)]
pub(crate) struct DurableKeyLeaseSet {
    leases: BTreeMap<IdempotencyKey, DurableKeyLease>,
    proof_barrier: Arc<ProofBarrierGuard>,
}

impl DurableKeyLeaseSet {
    /// Number of logical key leases currently retained by the set.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.leases.len()
    }

    /// Whether every acquired key lease has been consumed or released.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    #[cfg(test)]
    fn key_lock_count(&self) -> usize {
        self.leases
            .values()
            .filter(|lease| lease.key_lock.is_some())
            .count()
    }

    #[cfg(test)]
    fn uses_one_exclusive_barrier(&self) -> bool {
        self.proof_barrier.mode == ProofBarrierMode::Exclusive
            && self.leases.values().all(|lease| {
                Arc::ptr_eq(&self.proof_barrier, &lease.proof_barrier)
                    && lease.proof_barrier.mode == ProofBarrierMode::Exclusive
            })
    }

    /// Inspect a held logical lease without releasing the plan barrier.
    #[must_use]
    pub(crate) fn get(&self, idem_key: &IdempotencyKey) -> Option<&DurableKeyLease> {
        debug_assert_eq!(self.proof_barrier.mode, ProofBarrierMode::Exclusive);
        self.leases.get(idem_key)
    }

    /// Remove one logical lease for execution binding while the set retains
    /// the shared exclusive plan barrier.
    pub(crate) fn take(&mut self, idem_key: &IdempotencyKey) -> Option<DurableKeyLease> {
        debug_assert_eq!(self.proof_barrier.mode, ProofBarrierMode::Exclusive);
        self.leases.remove(idem_key)
    }
}

/// Cross-process mutation lease for one idempotency key.
///
/// Ordinary writers retain a shared plan barrier plus an exclusive key lock;
/// atomic proof batches retain their exclusive plan barrier without opening a
/// per-key lock. Durable tx engines keep the reservation alive from the
/// post-lock dedup decision, through the durable `Pending` write and external
/// dispatch, until the terminal outcome has been durably completed.
#[derive(Debug)]
pub struct IdempotencyReservation {
    idem_key: String,
    execution_id: String,
    observed_outcome: Option<StepOutcome>,
    pending_recorded: bool,
    // Fields drop in declaration order. Release the execution lock first while
    // the optional key lease and plan barrier are still held, preserving the
    // barrier-then-key-then-execution lifecycle through the final instant of
    // reservation ownership.
    execution_lock: ExecutionLedgerLock,
    key_lock: Option<DurableKeyLockGuard>,
    proof_barrier: Arc<ProofBarrierGuard>,
}

impl IdempotencyReservation {
    /// Durable outcome observed while holding the plan barrier and any
    /// required per-key lock.
    ///
    /// `None` permits a new Pending append only when the target execution does
    /// not already contain this key (normally a freshly-created retry
    /// execution). Per-ledger key uniqueness still rejects an expired retry in
    /// the original execution. `Some(Pending)` is an ambiguous predecessor and
    /// must fail closed; other outcomes must be interpreted by the tx engine's
    /// semantic replay rules before it decides whether to dispatch.
    #[must_use]
    pub fn observed_outcome(&self) -> Option<&StepOutcome> {
        self.observed_outcome.as_ref()
    }

    /// Whether this lease authorizes mutation for `idem_key`.
    #[must_use]
    pub fn authorizes(&self, idem_key: &IdempotencyKey) -> bool {
        self.idem_key == idem_key.as_str()
    }
}

/// Exclusive cross-process lock for one execution ledger.
///
/// Durable writer lock order is always plan barrier first, optional
/// idempotency-key lock second, and execution lock last. Operations that do
/// not publish outcomes (create, phase transition, archive, abort) take only
/// the execution lock. This prevents different-key writers from publishing
/// stale whole-ledger snapshots over each other while allowing an atomic batch
/// proof reader to freeze one plan without consuming one descriptor per key.
#[derive(Debug)]
struct ExecutionLedgerLock {
    execution_id: String,
    spool_dir: Arc<Dir>,
    spool_display: PathBuf,
    lock_dir: Arc<Dir>,
    lock_name: PathBuf,
    _lock_file: File,
}

/// A verified durable ledger together with the exact filesystem object that
/// supplied its bytes. Keeping the handle alive until publication prevents
/// inode reuse from making a different leaf look like the ledger that the
/// mutation actually authorized.
struct LockedDurableLedger {
    ledger: TxExecutionLedger,
    pinned_file: CapFile,
}

#[derive(Debug, Clone)]
struct DurableOutcomeObservation {
    timestamp_ms: u64,
    execution_id: String,
    ordinal: u64,
    outcome: StepOutcome,
}

fn select_authoritative_durable_outcome(
    idem_key: &IdempotencyKey,
    mut observations: Vec<DurableOutcomeObservation>,
) -> Result<Option<DurableOutcomeObservation>, IdempotencyError> {
    observations.sort_unstable_by(|left, right| {
        left.timestamp_ms
            .cmp(&right.timestamp_ms)
            .then_with(|| left.execution_id.cmp(&right.execution_id))
            .then_with(|| left.ordinal.cmp(&right.ordinal))
    });

    let mut sticky: Option<DurableOutcomeObservation> = None;
    for observation in observations
        .iter()
        .filter(|observation| observation.outcome.is_sticky_replay_proof())
    {
        if let Some(existing) = &sticky
            && existing.outcome != observation.outcome
        {
            return Err(IdempotencyError::LedgerIndexCorrupt {
                reason: format!(
                    "conflicting sticky durable outcomes for key {idem_key}: {:?} in execution {} and {:?} in execution {}",
                    existing.outcome,
                    existing.execution_id,
                    observation.outcome,
                    observation.execution_id
                ),
            });
        }
        sticky = Some(observation.clone());
    }

    // Pending, Success, and Compensated are durable safety facts. A later
    // retry failure must never downgrade them into permission to dispatch.
    // When no sticky fact exists, the newest failure/skipped observation is
    // the authoritative retry state.
    Ok(sticky.or_else(|| observations.pop()))
}

/// Capability bundle rooted at the caller-declared stable control-plane
/// anchor. The caller must identity-gate that parent (normally the pinned
/// workspace `.ft` directory) before construction. Replacing the anchor's own
/// namespace is outside this store's same-UID threat boundary; every
/// descendant operation here is relative to the pinned capabilities. A ledger
/// replacement revalidates the open destination identity immediately before
/// rename, but POSIX exposes no capability-relative compare-and-swap rename;
/// a same-UID actor swapping that exact leaf in the final check-to-rename
/// interval is therefore also outside the noninterference guarantee.
#[derive(Debug)]
struct DurableSpool {
    parent_dir: Arc<Dir>,
    dir: Arc<Dir>,
    execution_lock_dir: Arc<Dir>,
    key_lock_dir: Arc<Dir>,
    /// Preflighted directory handle used to make ledger namespace mutations
    /// durable on supported Unix-like platforms. Windows store acquisition
    /// fails closed until capability-relative write-through rename exists;
    /// directory `FlushFileBuffers` is not a documented durability primitive.
    sync_file: File,
    display_path: PathBuf,
}

/// Cross-instance idempotency store that tracks execution across multiple tx runs.
///
/// Provides the core dedup + resume API surface.
#[derive(Debug)]
pub struct IdempotencyStore {
    /// Active ledgers by execution ID.
    ledgers: HashMap<String, TxExecutionLedger>,
    /// Global dedup guard across all executions.
    dedup: DeduplicationGuard,
    /// Policy configuration.
    policy: IdempotencyPolicy,
    /// On-disk durability sink (ft-iz1ki). The open directory capabilities
    /// pin the filesystem objects selected at construction time; the display
    /// path is diagnostics-only and is never re-resolved for I/O.
    durable_spool: Option<DurableSpool>,
    #[cfg(test)]
    fail_persist_writes: bool,
    #[cfg(test)]
    durable_refresh_scan_count: usize,
}

const TX_LEDGER_DIR_NAME: &str = "tx_ledgers";
const EXECUTION_LOCK_DIR_NAME: &str = "execution_locks";
const KEY_LOCK_DIR_NAME: &str = "key_locks";
/// Maximum serialized size accepted for one durable execution ledger. Reads
/// use a `MAX + 1` limited reader so a hostile valid-named spool entry cannot
/// drive unbounded allocation before validation fails closed.
const MAX_DURABLE_LEDGER_BYTES: u64 = 16 * 1024 * 1024;

#[cfg(unix)]
fn metadata_identity(metadata: &impl CapMetadataExt) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn open_or_create_dir_nofollow(
    parent: &Dir,
    name: &str,
    display_path: &Path,
) -> Result<Dir, IdempotencyError> {
    match parent.create_dir(name) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!("create durable directory {}: {err}", display_path.display()),
            });
        }
    }
    parent
        .open_dir_nofollow(name)
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!(
                "open durable directory {} without following symlinks: {err}",
                display_path.display()
            ),
        })
}

fn validate_pinned_dir_entry(
    parent: &Dir,
    name: &str,
    pinned: &Dir,
    display_path: &Path,
) -> Result<(), IdempotencyError> {
    let current =
        parent
            .open_dir_nofollow(name)
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "validate pinned durable directory {} without following symlinks: {err}",
                    display_path.display()
                ),
            })?;
    let current_metadata =
        current
            .dir_metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "inspect current durable directory {}: {err}",
                    display_path.display()
                ),
            })?;
    let pinned_metadata = pinned
        .dir_metadata()
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!(
                "inspect pinned durable directory {}: {err}",
                display_path.display()
            ),
        })?;
    #[cfg(windows)]
    let _ = (&current_metadata, &pinned_metadata);
    #[cfg(unix)]
    if metadata_identity(&current_metadata) != metadata_identity(&pinned_metadata) {
        return Err(IdempotencyError::LedgerPersist {
            reason: format!(
                "durable directory namespace entry {} no longer names the pinned filesystem object",
                display_path.display()
            ),
        });
    }
    #[cfg(not(any(unix, windows)))]
    return Err(IdempotencyError::LedgerPersist {
        reason: format!(
            "durable directory identity validation is unsupported for {} on this platform",
            display_path.display()
        ),
    });
    // On Windows cap-std directory handles are deliberately opened without
    // FILE_SHARE_DELETE. While `pinned` is alive the directory entry cannot be
    // renamed or replaced, avoiding the truncated 64-bit ReFS file-id issue in
    // metadata-based comparisons.
    Ok(())
}

impl DurableSpool {
    fn display(&self, relative: &Path) -> PathBuf {
        self.display_path.join(relative)
    }

    fn validate_namespace(&self) -> Result<(), IdempotencyError> {
        validate_pinned_dir_entry(
            &self.parent_dir,
            TX_LEDGER_DIR_NAME,
            &self.dir,
            &self.display_path,
        )?;
        validate_pinned_dir_entry(
            &self.dir,
            EXECUTION_LOCK_DIR_NAME,
            &self.execution_lock_dir,
            &self.display_path.join(EXECUTION_LOCK_DIR_NAME),
        )?;
        validate_pinned_dir_entry(
            &self.dir,
            KEY_LOCK_DIR_NAME,
            &self.key_lock_dir,
            &self.display_path.join(KEY_LOCK_DIR_NAME),
        )
    }
}

fn nofollow_open_options(read: bool, write: bool) -> CapOpenOptions {
    let mut options = CapOpenOptions::new();
    options.read(read).write(write);
    options.follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.nonblock(true);
    options
}

fn lock_open_options() -> CapOpenOptions {
    let mut options = nofollow_open_options(true, true);
    options.create(true);
    #[cfg(unix)]
    options.mode(0o600);
    #[cfg(windows)]
    options.share_mode(0x0000_0001 | 0x0000_0002);
    options
}

fn validate_regular_file(
    is_file: bool,
    link_count: u64,
    display_path: &Path,
) -> Result<(), IdempotencyError> {
    if !is_file {
        return Err(IdempotencyError::LedgerPersist {
            reason: format!(
                "durable leaf {} is not a regular file",
                display_path.display()
            ),
        });
    }
    if link_count != 1 {
        return Err(IdempotencyError::LedgerPersist {
            reason: format!(
                "durable leaf {} has {} hard links; exactly one is required",
                display_path.display(),
                link_count
            ),
        });
    }
    Ok(())
}

fn validate_open_regular_file(
    metadata: &cap_std::fs::Metadata,
    display_path: &Path,
) -> Result<(), IdempotencyError> {
    validate_regular_file(metadata.is_file(), metadata.nlink(), display_path)
}

/// ft-0eby0: try an advisory-lock acquisition with a short bounded grace
/// window before reporting contention. Lease release is fd-close based
/// (the guards have no explicit unlock; the flock drops when the last
/// duplicated fd closes), and that close can lag the LOGICAL completion
/// of the releasing operation — guards drop on blocking-pool threads,
/// and any concurrently forked child holds duplicated fds until its
/// exec. A genuine holder persists far beyond this ~40 ms window, so
/// fail-closed contention semantics (and every contention test) are
/// preserved; only sub-window false positives are absorbed.
fn try_lock_with_grace<F>(mut attempt: F) -> std::io::Result<()>
where
    F: FnMut() -> std::io::Result<()>,
{
    const GRACE_DELAYS_MS: [u64; 4] = [2, 5, 10, 25];
    let mut delays = GRACE_DELAYS_MS.iter();
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => match delays.next() {
                Some(delay_ms) => {
                    std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
                }
                None => return Err(err),
            },
            Err(err) => return Err(err),
        }
    }
}

fn validate_std_open_regular_file(
    metadata: &std::fs::Metadata,
    display_path: &Path,
) -> Result<(), IdempotencyError> {
    validate_regular_file(metadata.is_file(), metadata.nlink(), display_path)
}

fn open_regular_nofollow(
    dir: &Dir,
    relative: &Path,
    display_path: &Path,
) -> Result<CapFile, IdempotencyError> {
    let options = nofollow_open_options(true, false);
    let file =
        dir.open_with(relative, &options)
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "open durable leaf {} without following symlinks: {err}",
                    display_path.display()
                ),
            })?;
    let metadata = file
        .metadata()
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!("inspect durable leaf {}: {err}", display_path.display()),
        })?;
    validate_open_regular_file(&metadata, display_path)?;
    Ok(file)
}

fn read_regular_nofollow_bounded(
    dir: &Dir,
    relative: &Path,
    display_path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, IdempotencyError> {
    let mut file = open_regular_nofollow(dir, relative, display_path)?;
    read_bounded_ledger_with_limit(&mut file, display_path, max_bytes)
}

fn read_bounded_ledger_with_limit(
    reader: &mut impl Read,
    display_path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, IdempotencyError> {
    let mut contents = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!("read durable leaf {}: {err}", display_path.display()),
        })?;
    let actual = u64::try_from(contents.len()).unwrap_or(u64::MAX);
    if actual > max_bytes {
        let execution_id = display_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string();
        return Err(IdempotencyError::LedgerOversized {
            execution_id,
            actual,
            maximum: max_bytes,
        });
    }
    Ok(contents)
}

fn validate_pinned_file_entry(
    dir: &Dir,
    relative: &Path,
    held_file: &File,
    display_path: &Path,
) -> Result<(), IdempotencyError> {
    let current = open_regular_nofollow(dir, relative, display_path)?;
    let current_metadata = current
        .metadata()
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!(
                "inspect current durable leaf {}: {err}",
                display_path.display()
            ),
        })?;
    let held_metadata = held_file
        .metadata()
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!(
                "inspect pinned durable leaf {}: {err}",
                display_path.display()
            ),
        })?;
    validate_std_open_regular_file(&held_metadata, display_path)?;
    #[cfg(windows)]
    let _ = &current_metadata;
    #[cfg(unix)]
    if metadata_identity(&current_metadata) != metadata_identity(&held_metadata) {
        return Err(IdempotencyError::LedgerPersist {
            reason: format!(
                "durable leaf namespace entry {} no longer names the pinned filesystem object",
                display_path.display()
            ),
        });
    }
    #[cfg(not(any(unix, windows)))]
    return Err(IdempotencyError::LedgerPersist {
        reason: format!(
            "durable leaf identity validation is unsupported for {} on this platform",
            display_path.display()
        ),
    });
    // Windows lock handles are opened without FILE_SHARE_DELETE by
    // `lock_open_options`, so a held lock leaf cannot be renamed or replaced.
    Ok(())
}

fn retain_failed_ledger_temp(
    temp_file: CapFile,
    spool: &DurableSpool,
    temp_name: &Path,
    action: &str,
    error: &dyn std::fmt::Display,
) -> IdempotencyError {
    let file_sync_error = temp_file.sync_all().err();
    drop(temp_file);
    let directory_sync_error = sync_ledger_parent(spool).err();
    let retention = if file_sync_error.is_none() && directory_sync_error.is_none() {
        format!(
            "recovery artifact name durably retained in the pinned spool as {} (last-known path {}); contents may be partial",
            temp_name.display(),
            spool.display(temp_name).display()
        )
    } else {
        let file_detail = file_sync_error.map_or_else(String::new, |sync_error| {
            format!("; file sync failed: {sync_error}")
        });
        let directory_detail = directory_sync_error.map_or_else(String::new, |sync_error| {
            format!("; directory sync failed: {sync_error}")
        });
        format!(
            "best-effort recovery artifact may remain in the pinned spool as {} (last-known path {}), but durability was not confirmed{file_detail}{directory_detail}",
            temp_name.display(),
            spool.display(temp_name).display()
        )
    };
    IdempotencyError::LedgerPersist {
        reason: format!("{action}: {error}; {retention}"),
    }
}

fn open_directory_sync_file(dir: &Dir, display_path: &Path) -> Result<File, IdempotencyError> {
    #[cfg(not(windows))]
    let file =
        dir.open(".")
            .map(CapFile::into_std)
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "open pinned directory {} for synchronization: {err}",
                    display_path.display()
                ),
            })?;

    #[cfg(windows)]
    {
        let _ = dir;
        return Err(windows_durable_rename_unsupported(display_path));
    }

    #[cfg(not(windows))]
    Ok(file)
}

#[cfg(windows)]
fn windows_durable_rename_unsupported(display_path: &Path) -> IdempotencyError {
    IdempotencyError::LedgerPersist {
        reason: format!(
            "durable idempotency rename is unsupported on Windows for {} until a capability-relative MOVEFILE_WRITE_THROUGH publication primitive is available",
            display_path.display()
        ),
    }
}

fn ensure_durable_rename_supported(display_path: &Path) -> Result<(), IdempotencyError> {
    #[cfg(windows)]
    {
        Err(windows_durable_rename_unsupported(display_path))
    }
    #[cfg(not(windows))]
    {
        let _ = display_path;
        Ok(())
    }
}

fn sync_ledger_parent(spool: &DurableSpool) -> std::io::Result<()> {
    spool.sync_file.sync_all()
}

fn sync_pinned_directory(
    dir: &Dir,
    display_path: &Path,
    action: &str,
) -> Result<(), IdempotencyError> {
    open_directory_sync_file(dir, display_path)?
        .sync_all()
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!(
                "{action} by synchronizing pinned directory {}: {err}",
                display_path.display()
            ),
        })
}

fn create_ledger_temp(spool: &DurableSpool) -> Result<(PathBuf, CapFile), IdempotencyError> {
    for _ in 0..128 {
        let temp_name = PathBuf::from(format!(
            ".tx-ledger-{:032x}.recovery.tmp",
            rand::random::<u128>()
        ));
        let mut options = nofollow_open_options(true, true);
        options.create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match spool.dir.open_with(&temp_name, &options) {
            Ok(file) => {
                let metadata = file
                    .metadata()
                    .map_err(|err| IdempotencyError::LedgerPersist {
                        reason: format!(
                            "inspect ledger recovery file {}: {err}",
                            spool.display(&temp_name).display()
                        ),
                    })?;
                validate_open_regular_file(&metadata, &spool.display(&temp_name))?;
                return Ok((temp_name, file));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "create ledger recovery file in pinned spool {}: {err}",
                        spool.display_path.display()
                    ),
                });
            }
        }
    }
    Err(IdempotencyError::LedgerPersist {
        reason: format!(
            "could not allocate a unique ledger recovery file in pinned spool {} after 128 attempts",
            spool.display_path.display()
        ),
    })
}

#[derive(Clone, Copy)]
enum LedgerPersistMode<'a> {
    Create,
    Replace { expected_file: &'a CapFile },
}

#[cfg(test)]
std::thread_local! {
    /// Deterministic seam for exercising the otherwise tiny interval between
    /// the live locked read and the immediate pre-rename identity check.
    static LEDGER_PRE_REPLACE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct LedgerPreReplaceTestHookGuard;

#[cfg(test)]
impl Drop for LedgerPreReplaceTestHookGuard {
    fn drop(&mut self) {
        LEDGER_PRE_REPLACE_TEST_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

#[cfg(test)]
fn set_ledger_pre_replace_test_hook(
    hook: impl FnOnce() + 'static,
) -> LedgerPreReplaceTestHookGuard {
    LEDGER_PRE_REPLACE_TEST_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
    LedgerPreReplaceTestHookGuard
}

#[cfg(test)]
fn run_ledger_pre_replace_test_hook() {
    let hook = LEDGER_PRE_REPLACE_TEST_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

fn validate_ledger_replacement_target(
    spool: &DurableSpool,
    final_name: &Path,
    expected_file: &CapFile,
    final_display: &Path,
) -> Result<CapFile, IdempotencyError> {
    // Revalidate the complete pinned namespace immediately before resolving
    // the destination leaf. A failure here must precede the rename so the
    // synchronized recovery artifact can be retained without touching an
    // untrusted destination.
    spool.validate_namespace()?;
    let current_file = open_regular_nofollow(&spool.dir, final_name, final_display)?;
    let current_metadata =
        current_file
            .metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "inspect current ledger replacement target {}: {err}",
                    final_display.display()
                ),
            })?;
    let expected_metadata =
        expected_file
            .metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "inspect pinned ledger replacement source {}: {err}",
                    final_display.display()
                ),
            })?;
    validate_open_regular_file(&expected_metadata, final_display)?;

    #[cfg(unix)]
    {
        if metadata_identity(&current_metadata) != metadata_identity(&expected_metadata) {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!(
                    "ledger replacement target {} no longer names the exact filesystem object read under the execution lock",
                    final_display.display()
                ),
            });
        }
        Ok(current_file)
    }
    #[cfg(not(unix))]
    {
        let _ = (&current_metadata, &expected_metadata);
        Err(IdempotencyError::LedgerPersist {
            reason: format!(
                "exact ledger replacement identity validation is unsupported for {} on this platform",
                final_display.display()
            ),
        })
    }
}

fn persist_ledger_bytes(
    spool: &DurableSpool,
    final_name: &Path,
    bytes: &[u8],
    mode: LedgerPersistMode<'_>,
) -> Result<(), IdempotencyError> {
    spool.validate_namespace()?;
    let final_display = spool.display(final_name);
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DURABLE_LEDGER_BYTES {
        return Err(IdempotencyError::LedgerPersist {
            reason: format!(
                "refusing to persist ledger {} above the {} byte safety limit",
                final_display.display(),
                MAX_DURABLE_LEDGER_BYTES
            ),
        });
    }
    let existing_permissions = match mode {
        LedgerPersistMode::Create => match spool.dir.symlink_metadata(final_name) {
            Ok(metadata) if metadata.file_type().is_symlink() => None,
            Ok(metadata) if metadata.is_file() && metadata.nlink() == 1 => {
                Some(metadata.permissions())
            }
            Ok(metadata) => {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "inspect existing ledger permissions on {}: expected a single-link regular file, got type {:?} with {} links",
                        final_display.display(),
                        metadata.file_type(),
                        metadata.nlink()
                    ),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "inspect existing ledger permissions on {}: {err}",
                        final_display.display()
                    ),
                });
            }
        },
        LedgerPersistMode::Replace { expected_file } => {
            let metadata =
                expected_file
                    .metadata()
                    .map_err(|err| IdempotencyError::LedgerPersist {
                        reason: format!(
                            "inspect pinned ledger permissions on {}: {err}",
                            final_display.display()
                        ),
                    })?;
            validate_open_regular_file(&metadata, &final_display)?;
            Some(metadata.permissions())
        }
    };

    let (temp_name, mut temp_file) = create_ledger_temp(spool)?;
    let temp_display = spool.display(&temp_name);
    if let Some(permissions) = existing_permissions {
        if let Err(err) = temp_file.set_permissions(permissions) {
            return Err(retain_failed_ledger_temp(
                temp_file,
                spool,
                &temp_name,
                &format!("preserve ledger permissions on {}", temp_display.display()),
                &err,
            ));
        }
    }
    if let Err(err) = temp_file.write_all(bytes) {
        return Err(retain_failed_ledger_temp(
            temp_file,
            spool,
            &temp_name,
            &format!("write ledger recovery file {}", temp_display.display()),
            &err,
        ));
    }
    if let Err(err) = temp_file.sync_all() {
        return Err(retain_failed_ledger_temp(
            temp_file,
            spool,
            &temp_name,
            &format!("sync ledger recovery file {}", temp_display.display()),
            &err,
        ));
    }

    match mode {
        LedgerPersistMode::Create => {
            if let Err(error) = spool.dir.hard_link(&temp_name, &spool.dir, final_name) {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    drop(temp_file);
                    if let Err(cleanup_error) = spool.dir.remove_file(&temp_name) {
                        let directory_sync_error = sync_ledger_parent(spool).err();
                        return Err(IdempotencyError::LedgerPersist {
                            reason: format!(
                                "ledger {} already exists, but collision recovery artifact {} could not be removed: {cleanup_error}{}",
                                final_display.display(),
                                temp_display.display(),
                                directory_sync_error.map_or_else(String::new, |sync_error| format!(
                                    "; additionally failed to synchronize pinned spool: {sync_error}"
                                ))
                            ),
                        });
                    }
                    sync_ledger_parent(spool).map_err(|sync_error| {
                        IdempotencyError::LedgerPersist {
                            reason: format!(
                                "ledger {} already exists and collision recovery artifact {} was removed, but pinned spool cleanup could not be synchronized: {sync_error}",
                                final_display.display(),
                                temp_display.display()
                            ),
                        }
                    })?;
                    spool.validate_namespace()?;
                    let execution_id = final_name
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .map_or_else(|| final_display.display().to_string(), ToString::to_string);
                    return Err(IdempotencyError::DuplicateExecution { key: execution_id });
                }
                return Err(retain_failed_ledger_temp(
                    temp_file,
                    spool,
                    &temp_name,
                    &format!(
                        "atomically create ledger {} from {}",
                        final_display.display(),
                        temp_display.display()
                    ),
                    &error,
                ));
            }
            if let Err(error) = spool.dir.remove_file(&temp_name) {
                let sync_error = sync_ledger_parent(spool).err();
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "ledger {} was created without clobbering, but recovery link {} could not be removed: {error}{}",
                        final_display.display(),
                        temp_display.display(),
                        sync_error.map_or_else(String::new, |sync_error| format!(
                            "; additionally failed to synchronize pinned spool: {sync_error}"
                        ))
                    ),
                });
            }
        }
        LedgerPersistMode::Replace { expected_file } => {
            #[cfg(test)]
            run_ledger_pre_replace_test_hook();
            let current_file = match validate_ledger_replacement_target(
                spool,
                final_name,
                expected_file,
                &final_display,
            ) {
                Ok(current_file) => current_file,
                Err(error) => {
                    return Err(retain_failed_ledger_temp(
                        temp_file,
                        spool,
                        &temp_name,
                        &format!(
                            "refuse to replace ledger {} from {} after destination identity changed",
                            final_display.display(),
                            temp_display.display()
                        ),
                        &error,
                    ));
                }
            };
            if let Err(error) = spool.dir.rename(&temp_name, &spool.dir, final_name) {
                return Err(retain_failed_ledger_temp(
                    temp_file,
                    spool,
                    &temp_name,
                    &format!(
                        "atomically replace ledger {} from {}",
                        final_display.display(),
                        temp_display.display()
                    ),
                    &error,
                ));
            }
            drop(current_file);
        }
    }

    // The namespace mutation is already externally visible. Synchronize its
    // containing directory before any later fallible verification so every
    // post-publication error leaves the create/replace durability state known.
    sync_ledger_parent(spool).map_err(|err| IdempotencyError::LedgerPersist {
        reason: format!(
            "ledger {} was published, but pinned spool directory {} could not be synchronized: {err}",
            final_display.display(),
            spool.display_path.display()
        ),
    })?;
    spool.validate_namespace()?;

    let persisted = open_regular_nofollow(&spool.dir, final_name, &final_display)?;
    #[cfg(unix)]
    let persisted_metadata =
        persisted
            .metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "inspect published ledger {}: {err}",
                    final_display.display()
                ),
            })?;
    #[cfg(unix)]
    let temp_metadata = temp_file
        .metadata()
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!(
                "inspect source handle for published ledger {}: {err}",
                final_display.display()
            ),
        })?;
    #[cfg(unix)]
    if metadata_identity(&persisted_metadata) != metadata_identity(&temp_metadata) {
        return Err(IdempotencyError::LedgerPersist {
            reason: format!(
                "published ledger {} no longer names the synchronized temporary filesystem object",
                final_display.display()
            ),
        });
    }
    #[cfg(windows)]
    {
        // Windows ReFS file identifiers are 128-bit while the metadata APIs
        // exposed by cap-fs-ext are only 64-bit. Verify the bytes through the
        // nofollow-opened destination instead of treating a truncated ID as
        // authoritative.
        let mut persisted_reader = &persisted;
        let persisted_bytes = read_bounded_ledger(&mut persisted_reader, &final_display)?;
        if persisted_bytes != bytes {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!(
                    "published ledger {} does not contain the synchronized bytes",
                    final_display.display()
                ),
            });
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(IdempotencyError::LedgerPersist {
        reason: format!(
            "published ledger identity validation is unsupported for {} on this platform",
            final_display.display()
        ),
    });
    drop(persisted);
    drop(temp_file);
    spool.validate_namespace()?;

    Ok(())
}

impl IdempotencyStore {
    /// Create a new in-memory store with the given policy (no durability).
    #[must_use]
    pub fn new(policy: IdempotencyPolicy) -> Self {
        Self {
            ledgers: HashMap::new(),
            dedup: DeduplicationGuard::new(policy.dedup_capacity),
            policy,
            durable_spool: None,
            #[cfg(test)]
            fail_persist_writes: false,
            #[cfg(test)]
            durable_refresh_scan_count: 0,
        }
    }

    /// Whether this store is backed by the durable ledger spool.
    ///
    /// Transaction execution, rollback, and recovery paths that can cause
    /// external effects must require `true`; an in-memory store cannot provide
    /// restart or cross-process idempotency.
    #[must_use]
    pub fn is_durable(&self) -> bool {
        self.durable_spool.is_some()
    }

    fn durable_spool(&self) -> Result<&DurableSpool, IdempotencyError> {
        let spool = self
            .durable_spool
            .as_ref()
            .ok_or(IdempotencyError::DurableReservationRequired)?;
        spool.validate_namespace()?;
        Ok(spool)
    }

    fn durable_spool_for_write(&self) -> Result<&DurableSpool, IdempotencyError> {
        #[cfg(test)]
        if self.fail_persist_writes {
            return Err(IdempotencyError::LedgerPersist {
                reason: "simulated durable ledger persistence failure".to_string(),
            });
        }
        self.durable_spool()
    }

    fn acquire_execution_lock(
        &self,
        execution_id: &str,
    ) -> Result<ExecutionLedgerLock, IdempotencyError> {
        if !is_valid_execution_id(execution_id) {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!("unsafe execution_id for execution lock: {execution_id:?}"),
            });
        }
        let spool = self.durable_spool()?;
        let lock_name = PathBuf::from(format!("{execution_id}.lock"));
        let lock_relative = Path::new(EXECUTION_LOCK_DIR_NAME).join(&lock_name);
        let lock_display = spool.display(&lock_relative);
        let options = lock_open_options();
        let lock_file = spool
            .execution_lock_dir
            .open_with(&lock_name, &options)
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "open execution lock {} for {execution_id} without following symlinks: {err}",
                    lock_display.display()
                ),
            })?;
        let metadata = lock_file
            .metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!("inspect execution lock {}: {err}", lock_display.display()),
            })?;
        validate_open_regular_file(&metadata, &lock_display)?;
        let lock_file = lock_file.into_std();
        match try_lock_with_grace(|| lock_file.try_lock_exclusive()) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(IdempotencyError::ExecutionMutationInProgress {
                    execution_id: execution_id.to_string(),
                });
            }
            Err(err) => {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "acquire execution lock {} for {execution_id}: {err}",
                        lock_display.display()
                    ),
                });
            }
        }
        validate_pinned_file_entry(
            &spool.execution_lock_dir,
            &lock_name,
            &lock_file,
            &lock_display,
        )?;
        Ok(ExecutionLedgerLock {
            execution_id: execution_id.to_string(),
            spool_dir: Arc::clone(&spool.dir),
            spool_display: spool.display_path.clone(),
            lock_dir: Arc::clone(&spool.execution_lock_dir),
            lock_name,
            _lock_file: lock_file,
        })
    }

    fn validate_execution_lock(
        &self,
        execution_lock: &ExecutionLedgerLock,
        execution_id: &str,
    ) -> Result<(), IdempotencyError> {
        if execution_lock.execution_id != execution_id {
            return Err(IdempotencyError::ReservationExecutionMismatch {
                reserved: execution_lock.execution_id.clone(),
                attempted: execution_id.to_string(),
            });
        }
        let spool = self.durable_spool()?;
        if !Arc::ptr_eq(&spool.dir, &execution_lock.spool_dir)
            || !Arc::ptr_eq(&spool.execution_lock_dir, &execution_lock.lock_dir)
        {
            return Err(IdempotencyError::ReservationStoreMismatch {
                reserved_spool: execution_lock.spool_display.display().to_string(),
                attempted_spool: spool.display_path.display().to_string(),
            });
        }
        let lock_relative = Path::new(EXECUTION_LOCK_DIR_NAME).join(&execution_lock.lock_name);
        validate_pinned_file_entry(
            &execution_lock.lock_dir,
            &execution_lock.lock_name,
            &execution_lock._lock_file,
            &spool.display(&lock_relative),
        )?;
        Ok(())
    }

    fn read_durable_ledger_locked(
        &self,
        execution_lock: &ExecutionLedgerLock,
    ) -> Result<LockedDurableLedger, IdempotencyError> {
        self.validate_execution_lock(execution_lock, &execution_lock.execution_id)?;
        let spool = self.durable_spool()?;
        let ledger_name = PathBuf::from(format!("{}.json", execution_lock.execution_id));
        let ledger_display = spool.display(&ledger_name);
        let options = nofollow_open_options(true, false);
        let mut ledger_file = spool.dir.open_with(&ledger_name, &options).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                IdempotencyError::LedgerNotFound {
                    execution_id: execution_lock.execution_id.clone(),
                }
            } else {
                IdempotencyError::LedgerPersist {
                    reason: format!(
                        "open locked ledger {} without following symlinks: {err}",
                        ledger_display.display()
                    ),
                }
            }
        })?;
        let metadata = ledger_file
            .metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!("inspect locked ledger {}: {err}", ledger_display.display()),
            })?;
        validate_open_regular_file(&metadata, &ledger_display)?;
        let contents = read_bounded_ledger_with_limit(
            &mut ledger_file,
            &ledger_display,
            self.policy.max_ledger_bytes,
        )?;
        let ledger = serde_json::from_slice::<TxExecutionLedger>(&contents).map_err(|err| {
            IdempotencyError::LedgerPersist {
                reason: format!(
                    "deserialize locked ledger {}: {err}",
                    ledger_display.display()
                ),
            }
        })?;
        if ledger.execution_id() != execution_lock.execution_id {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!(
                    "locked ledger identity mismatch for {}: expected {:?}, got {:?}",
                    ledger_display.display(),
                    execution_lock.execution_id,
                    ledger.execution_id()
                ),
            });
        }
        let verification = ledger.verify_chain();
        if !verification.chain_intact {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!(
                    "verify locked ledger hash chain for {}: first_break_at={:?}, missing_ordinals={:?}",
                    ledger_display.display(),
                    verification.first_break_at,
                    verification.missing_ordinals
                ),
            });
        }
        Ok(LockedDurableLedger {
            ledger,
            pinned_file: ledger_file,
        })
    }

    /// ft-iz1ki: open a *durable* idempotency store rooted at the workspace
    /// `.ft` directory. Ledgers are persisted under `<ft_dir>/tx_ledgers/`.
    /// Every verified ledger contributes its records to the bounded global
    /// replay index, while only nonterminal ledgers remain active for resume.
    ///
    /// Every valid-named ledger file is read, deserialized, and hash-chain
    /// verified before startup succeeds. Corruption fails closed: silently
    /// omitting a committed ledger would make replay treat its side effects as
    /// unrecorded and could dispatch them twice. Records are replayed into the
    /// bounded dedup guard in deterministic `(timestamp, execution_id,
    /// ordinal, key)` order, oldest first, so capacity eviction retains the
    /// newest proofs. Terminal ledgers remain durable spool evidence but do
    /// not consume active capacity. Every nonterminal ledger is reloaded; if
    /// their count exceeds `max_active_ledgers`, startup fails closed rather
    /// than silently discarding resumable state.
    ///
    /// # Errors
    /// Returns [`IdempotencyError::LedgerPersist`] if the spool directory
    /// cannot be created or listed, or if any valid-named ledger cannot be
    /// read, deserialized, identity-checked, or hash-chain verified.
    pub fn open(ft_dir: &Path, policy: IdempotencyPolicy) -> Result<Self, IdempotencyError> {
        ensure_durable_rename_supported(ft_dir)?;
        let leaf_name = ft_dir
            .file_name()
            .ok_or_else(|| IdempotencyError::LedgerPersist {
                reason: format!(
                    "durable idempotency parent {} has no final path component",
                    ft_dir.display()
                ),
            })?;
        let ambient_parent_path = ft_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let ambient_parent = Dir::open_ambient_dir(
            ambient_parent_path,
            cap_std::ambient_authority(),
        )
        .map_err(|err| IdempotencyError::LedgerPersist {
            reason: format!(
                "bind parent directory {} before opening durable idempotency anchor {}: {err}",
                ambient_parent_path.display(),
                ft_dir.display()
            ),
        })?;
        match ambient_parent.create_dir(leaf_name) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "create durable idempotency anchor {}: {err}",
                        ft_dir.display()
                    ),
                });
            }
        }
        let parent = ambient_parent.open_dir_nofollow(leaf_name).map_err(|err| {
            IdempotencyError::LedgerPersist {
                reason: format!(
                    "bind durable idempotency anchor {} without following its final component: {err}",
                    ft_dir.display()
                ),
            }
        })?;
        Self::open_in_pinned_dir(parent, ft_dir.to_path_buf(), policy)
    }

    /// Open a durable store beneath an already-pinned workspace control
    /// directory. The supplied parent is the caller-declared stable
    /// control-plane anchor (normally `<workspace_root>/.ft`) and must already
    /// have passed the caller's workspace identity gates. Replacing that
    /// anchor's own namespace is outside this store's same-UID threat boundary.
    /// Descendant ledger leaves are identity-checked immediately before atomic
    /// replacement, but a same-UID swap of the exact destination leaf in the
    /// final check-to-rename interval remains outside the guarantee because the
    /// platform does not provide a capability-relative compare-and-swap rename.
    /// `parent_display` is used only for diagnostics; all listing, reads,
    /// locks, publication, and directory synchronization are relative to
    /// `parent` and the descendant capabilities opened here.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyError::LedgerPersist`] when the pinned namespace
    /// cannot be initialized or any durable ledger fails closed validation.
    pub(crate) fn open_in_pinned_dir(
        parent: Dir,
        parent_display: PathBuf,
        policy: IdempotencyPolicy,
    ) -> Result<Self, IdempotencyError> {
        ensure_durable_rename_supported(&parent_display)?;
        let spool_display = parent_display.join(TX_LEDGER_DIR_NAME);
        let spool_dir = open_or_create_dir_nofollow(&parent, TX_LEDGER_DIR_NAME, &spool_display)?;
        sync_pinned_directory(
            &parent,
            &parent_display,
            "persist tx_ledgers namespace entry",
        )?;
        let execution_lock_dir = open_or_create_dir_nofollow(
            &spool_dir,
            EXECUTION_LOCK_DIR_NAME,
            &spool_display.join(EXECUTION_LOCK_DIR_NAME),
        )?;
        let key_lock_dir = open_or_create_dir_nofollow(
            &spool_dir,
            KEY_LOCK_DIR_NAME,
            &spool_display.join(KEY_LOCK_DIR_NAME),
        )?;
        let spool_sync_file = open_directory_sync_file(&spool_dir, &spool_display)?;
        spool_sync_file
            .sync_all()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "persist durable lock namespace entries by synchronizing pinned directory {}: {err}",
                    spool_display.display()
                ),
            })?;

        let mut store = Self {
            ledgers: HashMap::new(),
            dedup: DeduplicationGuard::new(policy.dedup_capacity),
            policy,
            durable_spool: Some(DurableSpool {
                parent_dir: Arc::new(parent),
                dir: Arc::new(spool_dir),
                execution_lock_dir: Arc::new(execution_lock_dir),
                key_lock_dir: Arc::new(key_lock_dir),
                sync_file: spool_sync_file,
                display_path: spool_display,
            }),
            #[cfg(test)]
            fail_persist_writes: false,
            #[cfg(test)]
            durable_refresh_scan_count: 0,
        };
        let spool = store.durable_spool()?;

        // Filename order is used only to make validation/error reporting
        // deterministic. Dedup recency is determined by authenticated record
        // timestamps below, never by an execution-id naming convention.
        let mut candidates: Vec<(String, PathBuf)> = Vec::new();
        let entries = spool
            .dir
            .entries()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "list pinned tx_ledgers directory {}: {err}",
                    spool.display_path.display()
                ),
            })?;
        for entry in entries {
            let entry = entry.map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "read entry in pinned tx_ledgers directory {}: {err}",
                    spool.display_path.display()
                ),
            })?;
            let name = PathBuf::from(entry.file_name());
            if name.extension().and_then(|extension| extension.to_str()) == Some("json")
                && let Some(stem) = name.file_stem().and_then(|stem| stem.to_str())
                && is_valid_execution_id(stem)
            {
                candidates.push((stem.to_string(), name));
            }
        }
        candidates.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        if candidates.len() > store.policy.max_spool_files {
            return Err(IdempotencyError::SpoolFileCountExceeded {
                actual: candidates.len(),
                maximum: store.policy.max_spool_files,
            });
        }
        let mut verified_ledgers = Vec::with_capacity(candidates.len());
        let mut total_spool_bytes: u64 = 0;
        let mut total_records: usize = 0;

        for (stem, name) in candidates {
            let display_path = spool.display(&name);
            let contents = read_regular_nofollow_bounded(
                &spool.dir,
                &name,
                &display_path,
                store.policy.max_ledger_bytes,
            )
            .map_err(|error| IdempotencyError::LedgerPersist {
                reason: format!("read ledger {}: {error}", display_path.display()),
            })?;
            let file_bytes = u64::try_from(contents.len()).unwrap_or(u64::MAX);
            total_spool_bytes = total_spool_bytes.saturating_add(file_bytes);
            if total_spool_bytes > store.policy.max_spool_total_bytes {
                return Err(IdempotencyError::SpoolByteLimitExceeded {
                    actual: total_spool_bytes,
                    maximum: store.policy.max_spool_total_bytes,
                });
            }
            let ledger = serde_json::from_slice::<TxExecutionLedger>(&contents).map_err(|err| {
                IdempotencyError::LedgerPersist {
                    reason: format!("deserialize ledger {}: {err}", display_path.display()),
                }
            })?;
            if ledger.execution_id() != stem {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "ledger identity mismatch for {}: filename execution_id {stem:?}, payload execution_id {:?}",
                        display_path.display(),
                        ledger.execution_id()
                    ),
                });
            }
            let verification = ledger.verify_chain();
            if !verification.chain_intact {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "verify ledger hash chain for {}: first_break_at={:?}, missing_ordinals={:?}",
                        display_path.display(),
                        verification.first_break_at,
                        verification.missing_ordinals
                    ),
                });
            }
            total_records = total_records.saturating_add(ledger.records().len());
            if total_records > store.policy.max_spool_records {
                return Err(IdempotencyError::SpoolRecordCountExceeded {
                    actual: total_records,
                    maximum: store.policy.max_spool_records,
                });
            }
            verified_ledgers.push((stem, ledger));
        }

        let active_count = verified_ledgers
            .iter()
            .filter(|(_, ledger)| !ledger.phase().is_terminal())
            .count();
        if active_count > store.policy.max_active_ledgers {
            return Err(IdempotencyError::ActiveLedgerLimitExceeded {
                max_active_ledgers: store.policy.max_active_ledgers,
            });
        }

        // Rebuild replay proof independently of active-ledger retention. Group
        // by the complete authenticated key so a later failure cannot erase a
        // prior Pending/Success/Compensated fact. Contradictory sticky facts
        // fail startup closed instead of depending on timestamp or HashMap
        // iteration order.
        let mut replay_records: BTreeMap<IdempotencyKey, Vec<DurableOutcomeObservation>> =
            BTreeMap::new();
        for (execution_id, ledger) in &verified_ledgers {
            for record in ledger.records() {
                replay_records
                    .entry(record.idem_key.clone())
                    .or_default()
                    .push(DurableOutcomeObservation {
                        timestamp_ms: record.timestamp_ms,
                        execution_id: execution_id.clone(),
                        ordinal: record.ordinal,
                        outcome: record.outcome.clone(),
                    });
            }
        }
        let mut authoritative_records = Vec::with_capacity(replay_records.len());
        for (idem_key, observations) in replay_records {
            if let Some(authoritative) =
                select_authoritative_durable_outcome(&idem_key, observations)?
            {
                authoritative_records.push((idem_key, authoritative));
            }
        }
        authoritative_records.sort_unstable_by(|(left_key, left), (right_key, right)| {
            left.timestamp_ms
                .cmp(&right.timestamp_ms)
                .then_with(|| left.execution_id.cmp(&right.execution_id))
                .then_with(|| left.ordinal.cmp(&right.ordinal))
                .then_with(|| left_key.cmp(right_key))
        });
        for (idem_key, authoritative) in authoritative_records {
            store.dedup.record(
                &idem_key,
                &authoritative.execution_id,
                authoritative.outcome,
                authoritative.timestamp_ms,
            );
        }

        for (execution_id, ledger) in verified_ledgers {
            if !ledger.phase().is_terminal() {
                store.ledgers.insert(execution_id, ledger);
            }
        }

        Ok(store)
    }

    fn acquire_plan_proof_barrier(
        &self,
        plan_id: &str,
        mode: ProofBarrierMode,
    ) -> Result<Arc<ProofBarrierGuard>, IdempotencyError> {
        let spool = self.durable_spool()?;
        let digest =
            sha256_domain_digest(b"frankenterm.tx.proof-barrier.v1", &[plan_id.as_bytes()]);
        let lock_name = PathBuf::from(format!("plan-{digest}.proof.lock"));
        let lock_display = spool.display_path.join(KEY_LOCK_DIR_NAME).join(&lock_name);
        let options = lock_open_options();
        let lock_file = spool
            .key_lock_dir
            .open_with(&lock_name, &options)
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "open {mode:?} transaction proof barrier {} for plan {plan_id:?} without following symlinks: {err}",
                    lock_display.display()
                ),
            })?;
        let metadata = lock_file
            .metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "inspect transaction proof barrier {}: {err}",
                    lock_display.display()
                ),
            })?;
        validate_open_regular_file(&metadata, &lock_display)?;
        let lock_file = lock_file.into_std();
        let lock_result = try_lock_with_grace(|| match mode {
            ProofBarrierMode::Shared => FileExt::try_lock_shared(&lock_file),
            ProofBarrierMode::Exclusive => FileExt::try_lock_exclusive(&lock_file),
        });
        match lock_result {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(IdempotencyError::ReservationInProgress {
                    key: format!("plan:{plan_id}"),
                });
            }
            Err(err) => {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "acquire {mode:?} transaction proof barrier {} for plan {plan_id:?}: {err}",
                        lock_display.display()
                    ),
                });
            }
        }
        validate_pinned_file_entry(&spool.key_lock_dir, &lock_name, &lock_file, &lock_display)?;
        Ok(Arc::new(ProofBarrierGuard {
            plan_id: plan_id.to_string(),
            mode,
            lock_dir: Arc::clone(&spool.key_lock_dir),
            lock_name,
            spool_display: spool.display_path.clone(),
            _lock_file: lock_file,
        }))
    }

    fn acquire_durable_key_lock(
        &self,
        idem_key: &IdempotencyKey,
    ) -> Result<DurableKeyLockGuard, IdempotencyError> {
        let spool = self.durable_spool()?;
        let key_hash = idem_key
            .as_str()
            .strip_prefix("txk:v2:")
            .expect("IdempotencyKey constructors/deserializer enforce txk:v2 prefix");
        let lock_name = PathBuf::from(format!("{key_hash}.lock"));
        let lock_display = spool.display_path.join(KEY_LOCK_DIR_NAME).join(&lock_name);
        let options = lock_open_options();
        let lock_file = spool
            .key_lock_dir
            .open_with(&lock_name, &options)
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "open idempotency key lock {} for {} without following symlinks: {err}",
                    lock_display.display(),
                    idem_key.as_str()
                ),
            })?;
        let metadata = lock_file
            .metadata()
            .map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!(
                    "inspect idempotency key lock {}: {err}",
                    lock_display.display()
                ),
            })?;
        validate_open_regular_file(&metadata, &lock_display)?;
        let lock_file = lock_file.into_std();
        match try_lock_with_grace(|| FileExt::try_lock_exclusive(&lock_file)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(IdempotencyError::ReservationInProgress {
                    key: idem_key.as_str().to_string(),
                });
            }
            Err(err) => {
                return Err(IdempotencyError::LedgerPersist {
                    reason: format!(
                        "acquire idempotency key lock {} for {}: {err}",
                        lock_display.display(),
                        idem_key.as_str()
                    ),
                });
            }
        }
        validate_pinned_file_entry(&spool.key_lock_dir, &lock_name, &lock_file, &lock_display)?;
        Ok(DurableKeyLockGuard {
            lock_dir: Arc::clone(&spool.key_lock_dir),
            lock_name,
            spool_display: spool.display_path.clone(),
            _lock_file: lock_file,
        })
    }

    /// Acquire one key-only durable lease and refresh its live-spool outcome
    /// while the shared plan barrier and exclusive key lock remain held.
    ///
    /// This is the ordinary single-key reservation primitive. It does not
    /// require an execution ledger to exist and performs no transaction ledger
    /// mutation. Atomic multi-key proof uses one exclusive plan barrier and a
    /// bulk refresh instead. Callers must either bind the returned lease to an
    /// execution or retain it until the proof-protected operation is complete.
    fn acquire_durable_key_lease(
        &mut self,
        idem_key: &IdempotencyKey,
        now_ms: u64,
    ) -> Result<DurableKeyLease, IdempotencyError> {
        self.validate_monotonic_timestamp(now_ms)?;
        let proof_barrier =
            self.acquire_plan_proof_barrier(idem_key.plan_id(), ProofBarrierMode::Shared)?;
        let key_lock = self.acquire_durable_key_lock(idem_key)?;

        // Keep both locks alive during the complete live-spool refresh. Any
        // error drops them in key-then-barrier order; success transfers their
        // ownership into the returned guard.
        let observed_outcome = self.refresh_durable_outcome_for_key(idem_key, now_ms)?;
        Ok(DurableKeyLease {
            idem_key: idem_key.clone(),
            observed_outcome,
            key_lock: Some(key_lock),
            proof_barrier,
        })
    }

    /// Acquire a complete set of durable key leases in canonical key order.
    ///
    /// Sorting makes the logical result deterministic. Duplicate, empty, and
    /// mixed-plan input is rejected before any lock is acquired. The method
    /// then takes one exclusive plan barrier, verifies every ledger exactly
    /// once, and creates descriptor-free logical leases for each requested key.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyError::LedgerRecordInvariant`] for malformed batch
    /// input, or propagates a plan-barrier or live-spool refresh error.
    pub(crate) fn acquire_durable_key_leases(
        &mut self,
        keys: impl IntoIterator<Item = IdempotencyKey>,
        now_ms: u64,
    ) -> Result<DurableKeyLeaseSet, IdempotencyError> {
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        keys.sort_unstable();
        let Some(first_key) = keys.first() else {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: "durable key lease batch must not be empty".to_string(),
            });
        };
        if let Some(duplicate) = keys.windows(2).find_map(|pair| {
            if pair[0] == pair[1] {
                Some(pair[0].clone())
            } else {
                None
            }
        }) {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "durable key lease batch contains duplicate idempotency key {duplicate}"
                ),
            });
        }
        let plan_id = first_key.plan_id().to_string();
        if let Some(mixed) = keys.iter().find(|key| key.plan_id() != plan_id) {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "durable key lease batch mixes plan {plan_id:?} with plan {:?} for key {}",
                    mixed.plan_id(),
                    mixed.as_str()
                ),
            });
        }
        self.validate_monotonic_timestamp(now_ms)?;
        let proof_barrier =
            self.acquire_plan_proof_barrier(&plan_id, ProofBarrierMode::Exclusive)?;
        let mut observed = self.refresh_durable_outcomes_for_keys(&keys, now_ms)?;
        let leases = keys
            .into_iter()
            .map(|idem_key| {
                let observed_outcome = observed
                    .remove(&idem_key)
                    .expect("bulk refresh returns one entry per requested unique key");
                let lease = DurableKeyLease {
                    idem_key: idem_key.clone(),
                    observed_outcome,
                    key_lock: None,
                    proof_barrier: Arc::clone(&proof_barrier),
                };
                (idem_key, lease)
            })
            .collect();
        Ok(DurableKeyLeaseSet {
            leases,
            proof_barrier,
        })
    }

    /// Bind a continuously-held key-only lease to a live execution ledger.
    ///
    /// The key outcome is not refreshed again: no supported durable writer can
    /// change it while `lease` owns its plan barrier (and, for ordinary
    /// writers, its exclusive key lock). This method adds the execution lock
    /// last, preserving the global barrier-then-optional-key-then-execution
    /// order.
    ///
    /// # Errors
    ///
    /// Returns a store mismatch when the lease came from another store, a
    /// sealed-ledger error when the target execution is terminal, or propagates
    /// execution-lock and live-ledger verification errors.
    pub(crate) fn bind_durable_key_lease(
        &mut self,
        execution_id: &str,
        lease: DurableKeyLease,
    ) -> Result<IdempotencyReservation, IdempotencyError> {
        self.validate_proof_barrier_binding(&lease.proof_barrier, &lease.idem_key)?;
        Self::validate_durable_key_lock_shape(lease.proof_barrier.mode, lease.key_lock.as_ref())?;
        if let Some(key_lock) = lease.key_lock.as_ref() {
            self.validate_durable_key_lock(key_lock)?;
        }

        let execution_lock = self.acquire_execution_lock(execution_id)?;
        let current = self.read_durable_ledger_locked(&execution_lock)?;
        if current.ledger.phase().is_terminal() {
            return Err(IdempotencyError::LedgerSealed {
                phase: current.ledger.phase(),
            });
        }
        self.ledgers
            .insert(execution_id.to_string(), current.ledger);

        let DurableKeyLease {
            idem_key,
            observed_outcome,
            key_lock,
            proof_barrier,
        } = lease;
        Ok(IdempotencyReservation {
            idem_key: idem_key.as_str().to_string(),
            execution_id: execution_id.to_string(),
            observed_outcome,
            pending_recorded: false,
            execution_lock,
            key_lock,
            proof_barrier,
        })
    }

    fn validate_proof_barrier_binding(
        &self,
        barrier: &ProofBarrierGuard,
        idem_key: &IdempotencyKey,
    ) -> Result<(), IdempotencyError> {
        if barrier.plan_id != idem_key.plan_id() {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "transaction proof barrier for plan {:?} cannot authorize key {} from plan {:?}",
                    barrier.plan_id,
                    idem_key.as_str(),
                    idem_key.plan_id()
                ),
            });
        }
        let spool = self.durable_spool()?;
        if !Arc::ptr_eq(&spool.key_lock_dir, &barrier.lock_dir) {
            return Err(IdempotencyError::ReservationStoreMismatch {
                reserved_spool: barrier.spool_display.display().to_string(),
                attempted_spool: spool.display_path.display().to_string(),
            });
        }
        let lock_relative = Path::new(KEY_LOCK_DIR_NAME).join(&barrier.lock_name);
        validate_pinned_file_entry(
            &barrier.lock_dir,
            &barrier.lock_name,
            &barrier._lock_file,
            &spool.display(&lock_relative),
        )
    }

    fn validate_durable_key_lock_shape(
        barrier_mode: ProofBarrierMode,
        key_lock: Option<&DurableKeyLockGuard>,
    ) -> Result<(), IdempotencyError> {
        match (barrier_mode, key_lock) {
            (ProofBarrierMode::Shared, Some(_)) | (ProofBarrierMode::Exclusive, None) => Ok(()),
            (ProofBarrierMode::Shared, None) => Err(IdempotencyError::LedgerRecordInvariant {
                reason: "shared transaction proof barrier is missing its exclusive key lock"
                    .to_string(),
            }),
            (ProofBarrierMode::Exclusive, Some(_)) => {
                Err(IdempotencyError::LedgerRecordInvariant {
                    reason: "exclusive transaction proof barrier must use descriptor-free logical key leases"
                        .to_string(),
                })
            }
        }
    }

    fn validate_durable_key_lock(
        &self,
        key_lock: &DurableKeyLockGuard,
    ) -> Result<(), IdempotencyError> {
        let spool = self.durable_spool()?;
        if !Arc::ptr_eq(&spool.key_lock_dir, &key_lock.lock_dir) {
            return Err(IdempotencyError::ReservationStoreMismatch {
                reserved_spool: key_lock.spool_display.display().to_string(),
                attempted_spool: spool.display_path.display().to_string(),
            });
        }
        let lock_relative = Path::new(KEY_LOCK_DIR_NAME).join(&key_lock.lock_name);
        validate_pinned_file_entry(
            &key_lock.lock_dir,
            &key_lock.lock_name,
            &key_lock._lock_file,
            &spool.display(&lock_relative),
        )
    }

    /// Acquire the exclusive durable lease for an idempotency key and refresh
    /// that key's outcome from the live spool while the lease is held.
    ///
    /// The refresh is load-bearing. A process may have opened its store before
    /// another process completed the same key; taking the OS lock without
    /// rereading durable state would still allow the stale process to dispatch
    /// after the first lock holder exits. `now_ms` is the caller's logical
    /// dispatch time. It is checked against both the local and durable spool
    /// high-water marks before it can expire a retryable failure or skip.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyError::DurableReservationRequired`] for an
    /// in-memory store, [`IdempotencyError::ReservationInProgress`] when
    /// another process currently owns the key,
    /// [`IdempotencyError::RetrogradeTimestamp`] when `now_ms` is behind the
    /// local or durable spool high-water mark, or
    /// [`IdempotencyError::LedgerPersist`] when the lock or live-spool refresh
    /// cannot be completed safely.
    pub fn acquire_durable_reservation(
        &mut self,
        execution_id: &str,
        idem_key: &IdempotencyKey,
        now_ms: u64,
    ) -> Result<IdempotencyReservation, IdempotencyError> {
        let lease = self.acquire_durable_key_lease(idem_key, now_ms)?;
        self.bind_durable_key_lease(execution_id, lease)
    }

    /// Re-read one key from every verified durable ledger without mutating the
    /// spool. This is suitable for fail-closed preflight that accepts only a
    /// positive sticky outcome: an atomic concurrent completion may make this
    /// read observe the earlier `Pending` value and reject conservatively, but
    /// it cannot manufacture `Success`. A `None` result never authorizes a new
    /// external effect; dispatch paths must use
    /// [`Self::acquire_durable_reservation`] instead.
    pub(crate) fn refresh_durable_outcome_for_key(
        &mut self,
        idem_key: &IdempotencyKey,
        now_ms: u64,
    ) -> Result<Option<StepOutcome>, IdempotencyError> {
        self.refresh_durable_outcomes_for_keys(std::slice::from_ref(idem_key), now_ms)?
            .remove(idem_key)
            .ok_or_else(|| IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "bulk durable refresh omitted requested idempotency key {}",
                    idem_key.as_str()
                ),
            })
    }

    /// Verify the durable spool once and refresh every requested key from that
    /// single authenticated snapshot.
    fn refresh_durable_outcomes_for_keys(
        &mut self,
        idem_keys: &[IdempotencyKey],
        now_ms: u64,
    ) -> Result<BTreeMap<IdempotencyKey, Option<StepOutcome>>, IdempotencyError> {
        let mut matching_records = idem_keys
            .iter()
            .cloned()
            .map(|key| (key, Vec::new()))
            .collect::<BTreeMap<_, _>>();
        if matching_records.len() != idem_keys.len() {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: "bulk durable refresh contains duplicate idempotency keys".to_string(),
            });
        }
        #[cfg(test)]
        {
            self.durable_refresh_scan_count = self.durable_refresh_scan_count.saturating_add(1);
        }

        let durable_high_water_ms = {
            let spool = self.durable_spool()?;
            let entries = spool
                .dir
                .entries()
                .map_err(|err| IdempotencyError::LedgerPersist {
                    reason: format!(
                        "list pinned tx_ledgers directory {} during key refresh: {err}",
                        spool.display_path.display()
                    ),
                })?;
            let mut candidates: Vec<(String, PathBuf)> = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|err| IdempotencyError::LedgerPersist {
                    reason: format!(
                        "read entry in pinned tx_ledgers directory {}: {err}",
                        spool.display_path.display()
                    ),
                })?;
                let name = PathBuf::from(entry.file_name());
                if name.extension().and_then(|extension| extension.to_str()) != Some("json") {
                    continue;
                }
                let Some(stem) = name.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                if !is_valid_execution_id(stem) {
                    continue;
                }
                candidates.push((stem.to_string(), name));
            }
            if candidates.len() > self.policy.max_spool_files {
                return Err(IdempotencyError::SpoolFileCountExceeded {
                    actual: candidates.len(),
                    maximum: self.policy.max_spool_files,
                });
            }
            candidates.sort_unstable_by(|left, right| left.0.cmp(&right.0));

            let mut durable_high_water_ms = 0;
            let mut total_spool_bytes: u64 = 0;
            let mut total_records: usize = 0;
            for (stem, name) in candidates {
                let display_path = spool.display(&name);
                let contents = read_regular_nofollow_bounded(
                    &spool.dir,
                    &name,
                    &display_path,
                    self.policy.max_ledger_bytes,
                )?;
                let file_bytes = u64::try_from(contents.len()).unwrap_or(u64::MAX);
                total_spool_bytes = total_spool_bytes.saturating_add(file_bytes);
                if total_spool_bytes > self.policy.max_spool_total_bytes {
                    return Err(IdempotencyError::SpoolByteLimitExceeded {
                        actual: total_spool_bytes,
                        maximum: self.policy.max_spool_total_bytes,
                    });
                }
                let ledger =
                    serde_json::from_slice::<TxExecutionLedger>(&contents).map_err(|err| {
                        IdempotencyError::LedgerPersist {
                            reason: format!(
                                "deserialize ledger {} during key refresh: {err}",
                                display_path.display()
                            ),
                        }
                    })?;
                if ledger.execution_id() != stem {
                    return Err(IdempotencyError::LedgerPersist {
                        reason: format!(
                            "ledger identity mismatch for {} during key refresh: filename execution_id {stem:?}, payload execution_id {:?}",
                            display_path.display(),
                            ledger.execution_id()
                        ),
                    });
                }
                let verification = ledger.verify_chain();
                if !verification.chain_intact {
                    return Err(IdempotencyError::LedgerPersist {
                        reason: format!(
                            "verify ledger hash chain for {} during key refresh: first_break_at={:?}, missing_ordinals={:?}",
                            display_path.display(),
                            verification.first_break_at,
                            verification.missing_ordinals
                        ),
                    });
                }
                total_records = total_records.saturating_add(ledger.records().len());
                if total_records > self.policy.max_spool_records {
                    return Err(IdempotencyError::SpoolRecordCountExceeded {
                        actual: total_records,
                        maximum: self.policy.max_spool_records,
                    });
                }
                for record in ledger.records() {
                    durable_high_water_ms = durable_high_water_ms.max(record.timestamp_ms);
                    if let Some(observations) = matching_records.get_mut(&record.idem_key) {
                        observations.push(DurableOutcomeObservation {
                            timestamp_ms: record.timestamp_ms,
                            execution_id: stem.clone(),
                            ordinal: record.ordinal,
                            outcome: record.outcome.clone(),
                        });
                    }
                }
            }
            durable_high_water_ms
        };

        if now_ms < durable_high_water_ms {
            return Err(IdempotencyError::RetrogradeTimestamp {
                observed_ms: now_ms,
                high_water_ms: durable_high_water_ms,
            });
        }

        let mut outcomes = BTreeMap::new();
        for (idem_key, observations) in matching_records {
            let outcome = match select_authoritative_durable_outcome(&idem_key, observations)? {
                Some(authoritative)
                    if self.is_fresh_for_dedup(
                        &authoritative.outcome,
                        authoritative.timestamp_ms,
                        now_ms,
                    ) =>
                {
                    self.dedup.record(
                        &idem_key,
                        &authoritative.execution_id,
                        authoritative.outcome.clone(),
                        authoritative.timestamp_ms,
                    );
                    Some(authoritative.outcome)
                }
                Some(_) | None => None,
            };
            outcomes.insert(idem_key, outcome);
        }
        Ok(outcomes)
    }

    /// Flush an explicit ledger snapshot to the durable spool (no-op when
    /// in-memory). Durable replacement requires the still-open file handle from
    /// the mutation's locked live read, so a substituted leaf cannot be
    /// mistaken for the object that was authorized. Taking the snapshot by
    /// reference lets compound mutations use copy-on-write: publish to disk
    /// first, then install the same state in the in-memory map only after
    /// persistence succeeds.
    fn persist_ledger_snapshot(
        &self,
        ledger: &TxExecutionLedger,
        expected_file: Option<&CapFile>,
    ) -> Result<(), IdempotencyError> {
        if self.durable_spool.is_none() {
            return Ok(());
        }
        let spool = self.durable_spool_for_write()?;
        let execution_id = ledger.execution_id();
        if !is_valid_execution_id(execution_id) {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!("unsafe execution_id for ledger filename: {execution_id:?}"),
            });
        }
        let json =
            serde_json::to_vec_pretty(ledger).map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!("serialize ledger {execution_id}: {err}"),
            })?;
        let actual = u64::try_from(json.len()).unwrap_or(u64::MAX);
        if actual > self.policy.max_ledger_bytes {
            return Err(IdempotencyError::LedgerOversized {
                execution_id: execution_id.to_string(),
                actual,
                maximum: self.policy.max_ledger_bytes,
            });
        }
        let final_name = PathBuf::from(format!("{execution_id}.json"));
        let expected_file = expected_file.ok_or_else(|| IdempotencyError::LedgerPersist {
            reason: format!(
                "refusing to replace durable ledger {execution_id} without the exact file handle used by its locked live read"
            ),
        })?;
        persist_ledger_bytes(
            spool,
            &final_name,
            &json,
            LedgerPersistMode::Replace { expected_file },
        )
    }

    fn persist_new_ledger_snapshot(
        &self,
        ledger: &TxExecutionLedger,
    ) -> Result<(), IdempotencyError> {
        let spool = self.durable_spool_for_write()?;
        let execution_id = ledger.execution_id();
        if !is_valid_execution_id(execution_id) {
            return Err(IdempotencyError::LedgerPersist {
                reason: format!("unsafe execution_id for ledger filename: {execution_id:?}"),
            });
        }
        let json =
            serde_json::to_vec_pretty(ledger).map_err(|err| IdempotencyError::LedgerPersist {
                reason: format!("serialize new ledger {execution_id}: {err}"),
            })?;
        let actual = u64::try_from(json.len()).unwrap_or(u64::MAX);
        if actual > self.policy.max_ledger_bytes {
            return Err(IdempotencyError::LedgerOversized {
                execution_id: execution_id.to_string(),
                actual,
                maximum: self.policy.max_ledger_bytes,
            });
        }
        persist_ledger_bytes(
            spool,
            &PathBuf::from(format!("{execution_id}.json")),
            &json,
            LedgerPersistMode::Create,
        )
    }

    /// Create a new ledger for a tx execution. Returns error if execution ID already exists.
    pub fn create_ledger(
        &mut self,
        execution_id: &str,
        plan: &TxPlan,
    ) -> Result<(), IdempotencyError> {
        let execution_lock = if self.is_durable() {
            Some(self.acquire_execution_lock(execution_id)?)
        } else {
            None
        };
        if self.ledgers.contains_key(execution_id) {
            return Err(IdempotencyError::DuplicateExecution {
                key: execution_id.to_string(),
            });
        }
        if self.ledgers.len() >= self.policy.max_active_ledgers {
            return Err(IdempotencyError::ActiveLedgerLimitExceeded {
                max_active_ledgers: self.policy.max_active_ledgers,
            });
        }
        let ledger = TxExecutionLedger::new(execution_id, &plan.plan_id, plan.plan_hash);
        // ft-iz1ki: durably record the freshly-created ledger so a crash
        // before the first step still leaves a recoverable execution record.
        // Publish before installing the active value so a persistence failure
        // leaves no half-created in-memory execution behind.
        if execution_lock.is_some() {
            self.persist_new_ledger_snapshot(&ledger)?;
        } else {
            self.persist_ledger_snapshot(&ledger, None)?;
        }
        self.ledgers.insert(execution_id.to_string(), ledger);
        Ok(())
    }

    /// Get an immutable reference to a ledger.
    #[must_use]
    pub fn get_ledger(&self, execution_id: &str) -> Option<&TxExecutionLedger> {
        self.ledgers.get(execution_id)
    }

    /// Transition a tracked ledger and durably publish the new phase.
    ///
    /// The transition is copy-on-write: the candidate ledger is validated and
    /// atomically persisted before it replaces the in-memory value. A failed
    /// validation or write therefore leaves the previously visible phase
    /// unchanged. In-memory stores still use the same API, with persistence as
    /// a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`IdempotencyError::LedgerNotFound`] when `execution_id` is not
    /// tracked, [`IdempotencyError::InvalidPhaseTransition`] when `next` is not
    /// reachable from the current phase, or [`IdempotencyError::LedgerPersist`]
    /// when the durable snapshot cannot be published.
    pub fn transition_phase(
        &mut self,
        execution_id: &str,
        next: TxPhase,
    ) -> Result<TxPhase, IdempotencyError> {
        let execution_lock = if self.is_durable() {
            Some(self.acquire_execution_lock(execution_id)?)
        } else {
            None
        };
        let (mut candidate, pinned_file) = if let Some(lock) = &execution_lock {
            let locked = self.read_durable_ledger_locked(lock)?;
            (locked.ledger, Some(locked.pinned_file))
        } else {
            (
                self.ledgers.get(execution_id).cloned().ok_or_else(|| {
                    IdempotencyError::LedgerNotFound {
                        execution_id: execution_id.to_string(),
                    }
                })?,
                None,
            )
        };
        let previous = candidate.transition_phase(next)?;
        self.persist_ledger_snapshot(&candidate, pinned_file.as_ref())?;
        self.ledgers.insert(execution_id.to_string(), candidate);
        Ok(previous)
    }

    /// Get a mutable reference to a ledger.
    ///
    /// This escape hatch is intentionally unavailable for durable stores:
    /// mutating a stale in-memory snapshot would bypass execution locking and
    /// could overwrite another process's proof. Durable callers must use the
    /// explicit mutation APIs.
    #[must_use]
    pub fn get_ledger_mut(&mut self, execution_id: &str) -> Option<&mut TxExecutionLedger> {
        if self.is_durable() {
            None
        } else {
            self.ledgers.get_mut(execution_id)
        }
    }

    /// Peek at the bounded in-memory cache for an outcome that is fresh at
    /// `now_ms`.
    ///
    /// This method is advisory only. In a durable store it neither acquires the
    /// cross-process key lease nor refreshes the live spool, so `None` never
    /// authorizes an external effect. Effectful callers must use
    /// [`Self::acquire_durable_reservation`] and inspect the outcome observed
    /// while that reservation is held.
    #[must_use]
    pub fn peek_cached_outcome(
        &self,
        idem_key: &IdempotencyKey,
        now_ms: u64,
    ) -> Option<&StepOutcome> {
        // Check the global dedup guard first (cross-instance).
        if let Some(entry) = self.dedup.check(idem_key) {
            if self.is_fresh_for_dedup(&entry.outcome, entry.timestamp_ms, now_ms) {
                return Some(&entry.outcome);
            }
        }
        // Check all active ledgers.
        for ledger in self.ledgers.values() {
            if let Some(record) = ledger.get_record(idem_key) {
                if self.is_fresh_for_dedup(&record.outcome, record.timestamp_ms, now_ms) {
                    return Some(&record.outcome);
                }
            }
        }
        None
    }

    /// Record a step execution in an in-memory ledger and dedup guard.
    ///
    /// Durable callers must use [`Self::record_execution_reserved`] before an
    /// external dispatch, or [`Self::record_recovered_execution`] when linking
    /// an already-proven sticky outcome into a recovery ledger. Allowing this
    /// unchecked API on a durable store would make the key lease optional.
    pub fn record_execution(
        &mut self,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        if self.is_durable() {
            return Err(IdempotencyError::DurableMutationRequiresReservation {
                operation: "record_execution".to_string(),
            });
        }
        let mut candidate = self.ledgers.get(execution_id).cloned().ok_or_else(|| {
            IdempotencyError::LedgerNotFound {
                execution_id: execution_id.to_string(),
            }
        })?;
        self.validate_monotonic_timestamp(timestamp_ms)?;

        let hash = candidate.append(
            idem_key.clone(),
            outcome.clone(),
            risk,
            agent_id,
            timestamp_ms,
        )?;

        // Persist the candidate before publishing either the in-memory ledger
        // or the global dedup entry. A failed write therefore cannot make this
        // process believe a reservation/outcome exists when restart cannot
        // prove it.
        self.persist_ledger_snapshot(&candidate, None)?;

        self.evict_stale(timestamp_ms.saturating_sub(self.policy.dedup_ttl_ms));
        self.dedup
            .record(&idem_key, execution_id, outcome, timestamp_ms);
        self.ledgers.insert(execution_id.to_string(), candidate);

        Ok(hash)
    }

    /// Durably reserve a key while its cross-process lease is held.
    ///
    /// Durable tx engines should use this wrapper instead of
    /// [`Self::record_execution`] for the pre-dispatch `Pending` record. It
    /// binds the mutation to the exact locked key and refuses to overwrite an
    /// outcome observed by the live-spool refresh performed during lease
    /// acquisition. The one deliberate exception is a failed or skipped
    /// compensation: retries reuse the same semantic compensation key while
    /// holding its lease, so a later durable `Compensated` or ambiguous
    /// `Pending` fact can never be bypassed through a parallel attempt key.
    pub fn record_execution_reserved(
        &mut self,
        reservation: &mut IdempotencyReservation,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        self.validate_reservation_binding(reservation, &idem_key)?;
        self.validate_execution_lock(&reservation.execution_lock, execution_id)?;
        if reservation.execution_id != execution_id {
            return Err(IdempotencyError::ReservationExecutionMismatch {
                reserved: reservation.execution_id.clone(),
                attempted: execution_id.to_string(),
            });
        }
        if reservation.pending_recorded {
            return Err(IdempotencyError::ReservationAlreadyUsed {
                key: reservation.idem_key.clone(),
                execution_id: reservation.execution_id.clone(),
            });
        }
        let retries_failed_compensation = idem_key.is_compensation()
            && matches!(
                reservation.observed_outcome.as_ref(),
                Some(
                    StepOutcome::Failed {
                        compensated: false,
                        ..
                    } | StepOutcome::Skipped { .. }
                )
            );
        if reservation.observed_outcome.is_some() && !retries_failed_compensation {
            return Err(IdempotencyError::DuplicateExecution {
                key: idem_key.as_str().to_string(),
            });
        }
        if !outcome.is_pending() {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: "a new durable reservation must begin with Pending before dispatch"
                    .to_string(),
            });
        }
        self.validate_monotonic_timestamp(timestamp_ms)?;
        let locked = self.read_durable_ledger_locked(&reservation.execution_lock)?;
        let mut candidate = locked.ledger;
        let hash = candidate.append(
            idem_key.clone(),
            outcome.clone(),
            risk,
            agent_id,
            timestamp_ms,
        )?;
        self.persist_ledger_snapshot(&candidate, Some(&locked.pinned_file))?;
        self.evict_stale(timestamp_ms.saturating_sub(self.policy.dedup_ttl_ms));
        self.dedup
            .record(&idem_key, execution_id, outcome, timestamp_ms);
        self.ledgers.insert(execution_id.to_string(), candidate);
        reservation.pending_recorded = true;
        Ok(hash)
    }

    /// Link an already-proven durable outcome into the current recovery
    /// ledger without dispatching the external effect again.
    ///
    /// This method acquires the normal shared plan barrier, key lock, then
    /// execution lock; refreshes the authoritative outcome from the complete
    /// durable spool; and appends only when that proof exactly matches
    /// `outcome`. Only Success and Compensated facts are linkable; Pending,
    /// failure, and skipped states cannot certify that a prior effect completed.
    pub fn record_recovered_execution(
        &mut self,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        if !matches!(
            &outcome,
            StepOutcome::Success { .. } | StepOutcome::Compensated { .. }
        ) {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "recovered proof link for key {idem_key} requires Success or Compensated, got {outcome:?}"
                ),
            });
        }
        let reservation =
            self.acquire_durable_reservation(execution_id, &idem_key, timestamp_ms)?;
        self.record_recovered_execution_reserved(
            reservation,
            execution_id,
            idem_key,
            outcome,
            risk,
            agent_id,
            timestamp_ms,
        )
    }

    /// Link an already-proven sticky outcome using a pre-acquired reservation.
    ///
    /// This is the recovery counterpart to [`Self::record_execution_reserved`]
    /// for callers that acquired a complete key-only lease set before the
    /// execution ledger existed. The reservation is consumed so its key and
    /// execution locks remain held through durable publication.
    ///
    /// # Errors
    ///
    /// Returns an invariant, binding, or proof-mismatch error when the supplied
    /// reservation cannot certify `outcome`, or propagates durable ledger
    /// publication errors.
    pub(crate) fn record_recovered_execution_reserved(
        &mut self,
        reservation: IdempotencyReservation,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        if !matches!(
            &outcome,
            StepOutcome::Success { .. } | StepOutcome::Compensated { .. }
        ) {
            return Err(IdempotencyError::LedgerRecordInvariant {
                reason: format!(
                    "recovered proof link for key {idem_key} requires Success or Compensated, got {outcome:?}"
                ),
            });
        }
        self.validate_reservation_binding(&reservation, &idem_key)?;
        self.validate_execution_lock(&reservation.execution_lock, execution_id)?;
        if reservation.execution_id != execution_id {
            return Err(IdempotencyError::ReservationExecutionMismatch {
                reserved: reservation.execution_id.clone(),
                attempted: execution_id.to_string(),
            });
        }
        if reservation.pending_recorded {
            return Err(IdempotencyError::ReservationAlreadyUsed {
                key: reservation.idem_key.clone(),
                execution_id: reservation.execution_id.clone(),
            });
        }
        if reservation.observed_outcome.as_ref() != Some(&outcome) {
            return Err(IdempotencyError::RecoveredProofMismatch {
                key: idem_key.as_str().to_string(),
                expected: format!("{outcome:?}"),
                observed: format!("{:?}", reservation.observed_outcome),
            });
        }
        self.validate_monotonic_timestamp(timestamp_ms)?;
        let locked = self.read_durable_ledger_locked(&reservation.execution_lock)?;
        let mut candidate = locked.ledger;
        let hash = candidate.append(
            idem_key.clone(),
            outcome.clone(),
            risk,
            agent_id,
            timestamp_ms,
        )?;
        self.persist_ledger_snapshot(&candidate, Some(&locked.pinned_file))?;
        self.evict_stale(timestamp_ms.saturating_sub(self.policy.dedup_ttl_ms));
        self.dedup
            .record(&idem_key, execution_id, outcome, timestamp_ms);
        self.ledgers.insert(execution_id.to_string(), candidate);
        Ok(hash)
    }

    /// Complete a pending reservation in an in-memory store.
    ///
    /// Durable callers must retain and consume the token through
    /// [`Self::complete_execution_reserved`].
    pub fn complete_execution(
        &mut self,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        if self.is_durable() {
            return Err(IdempotencyError::DurableMutationRequiresReservation {
                operation: "complete_execution".to_string(),
            });
        }
        let mut candidate = self.ledgers.get(execution_id).cloned().ok_or_else(|| {
            IdempotencyError::LedgerNotFound {
                execution_id: execution_id.to_string(),
            }
        })?;
        self.validate_monotonic_timestamp(timestamp_ms)?;
        let hash = candidate.complete_pending(&idem_key, outcome.clone(), timestamp_ms)?;

        self.persist_ledger_snapshot(&candidate, None)?;

        self.evict_stale(timestamp_ms.saturating_sub(self.policy.dedup_ttl_ms));
        self.dedup
            .record(&idem_key, execution_id, outcome, timestamp_ms);
        self.ledgers.insert(execution_id.to_string(), candidate);

        Ok(hash)
    }

    /// Complete a pending reservation while retaining the same cross-process
    /// key lease that authorized the pre-dispatch write.
    pub fn complete_execution_reserved(
        &mut self,
        reservation: IdempotencyReservation,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        self.validate_reservation_binding(&reservation, &idem_key)?;
        self.validate_execution_lock(&reservation.execution_lock, execution_id)?;
        if reservation.execution_id != execution_id {
            return Err(IdempotencyError::ReservationExecutionMismatch {
                reserved: reservation.execution_id,
                attempted: execution_id.to_string(),
            });
        }
        if !reservation.pending_recorded {
            return Err(IdempotencyError::ReservationNotPending {
                key: reservation.idem_key,
                execution_id: execution_id.to_string(),
            });
        }
        self.validate_monotonic_timestamp(timestamp_ms)?;
        let locked = self.read_durable_ledger_locked(&reservation.execution_lock)?;
        let mut candidate = locked.ledger;
        let hash = candidate.complete_pending(&idem_key, outcome.clone(), timestamp_ms)?;
        self.persist_ledger_snapshot(&candidate, Some(&locked.pinned_file))?;
        self.evict_stale(timestamp_ms.saturating_sub(self.policy.dedup_ttl_ms));
        self.dedup
            .record(&idem_key, execution_id, outcome, timestamp_ms);
        self.ledgers.insert(execution_id.to_string(), candidate);
        Ok(hash)
    }

    fn validate_reservation_binding(
        &self,
        reservation: &IdempotencyReservation,
        idem_key: &IdempotencyKey,
    ) -> Result<(), IdempotencyError> {
        if !reservation.authorizes(idem_key) {
            return Err(IdempotencyError::ReservationKeyMismatch {
                reserved: reservation.idem_key.clone(),
                attempted: idem_key.as_str().to_string(),
            });
        }
        let spool = self.durable_spool()?;
        if !Arc::ptr_eq(&spool.dir, &reservation.execution_lock.spool_dir) {
            return Err(IdempotencyError::ReservationStoreMismatch {
                reserved_spool: reservation
                    .execution_lock
                    .spool_display
                    .display()
                    .to_string(),
                attempted_spool: spool.display_path.display().to_string(),
            });
        }
        self.validate_proof_barrier_binding(&reservation.proof_barrier, idem_key)?;
        Self::validate_durable_key_lock_shape(
            reservation.proof_barrier.mode,
            reservation.key_lock.as_ref(),
        )?;
        if let Some(key_lock) = reservation.key_lock.as_ref() {
            self.validate_durable_key_lock(key_lock)?;
        }
        Ok(())
    }

    /// Build a resume context for a given execution.
    #[must_use]
    pub fn resume_context(&self, execution_id: &str, plan: &TxPlan) -> Option<ResumeContext> {
        self.ledgers
            .get(execution_id)
            .map(|ledger| ResumeContext::from_ledger_with_policy(ledger, plan, &self.policy))
    }

    /// Abort and archive every active execution for an exact compiled plan.
    ///
    /// This is the safe handoff primitive for a caller that has completed its
    /// contract/dedup recovery preflight and is about to create a superseding
    /// execution. Matching is deliberately on both `plan_id` and `plan_hash`:
    /// a reused human-readable plan ID must never retire an execution whose
    /// action graph differs.
    ///
    /// The operation is copy-on-write with respect to memory. Candidate
    /// ledgers are cloned, nonterminal candidates transition to `Aborted`, and
    /// every snapshot is durably published before any active entry is removed.
    /// If validation or persistence fails, the in-memory map is unchanged.
    /// Multiple spool files cannot be atomically replaced as one filesystem
    /// transaction, so a mid-batch failure may leave an already-published
    /// candidate durably `Aborted`; that is fail-closed and will be treated as
    /// terminal on restart.
    ///
    /// Returns retired execution IDs in lexical order. The retired count is
    /// `result.len()`.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transition error if a matching nonterminal ledger
    /// cannot safely transition to `Aborted`, or a persistence error if any
    /// terminal snapshot cannot be durably published.
    pub fn abort_and_archive_matching_ledgers(
        &mut self,
        plan_id: &str,
        plan_hash: u64,
    ) -> Result<Vec<String>, IdempotencyError> {
        let mut execution_ids: Vec<String> = self
            .ledgers
            .iter()
            .filter(|(_, ledger)| ledger.plan_id() == plan_id && ledger.plan_hash() == plan_hash)
            .map(|(execution_id, _)| execution_id.clone())
            .collect();
        execution_ids.sort_unstable();

        let mut execution_locks = Vec::with_capacity(execution_ids.len());
        if self.is_durable() {
            for execution_id in &execution_ids {
                execution_locks.push(self.acquire_execution_lock(execution_id)?);
            }
        }

        let mut terminal_snapshots = Vec::with_capacity(execution_ids.len());
        for execution_id in &execution_ids {
            let (mut candidate, pinned_file) = if let Some(lock) = execution_locks
                .iter()
                .find(|lock| lock.execution_id == *execution_id)
            {
                let locked = self.read_durable_ledger_locked(lock)?;
                (locked.ledger, Some(locked.pinned_file))
            } else {
                (
                    self.ledgers
                        .get(execution_id)
                        .expect("execution ID collected from active ledger map")
                        .clone(),
                    None,
                )
            };
            if !candidate.phase().is_terminal() {
                candidate.transition_phase(TxPhase::Aborted)?;
            }
            terminal_snapshots.push((candidate, pinned_file));
        }

        for (candidate, pinned_file) in &terminal_snapshots {
            self.persist_ledger_snapshot(candidate, pinned_file.as_ref())?;
        }

        for execution_id in &execution_ids {
            self.ledgers
                .remove(execution_id)
                .expect("candidate remained active until all snapshots persisted");
        }

        Ok(execution_ids)
    }

    /// Remove a completed/aborted ledger from active tracking while retaining
    /// its durable spool file as replay proof and audit evidence.
    ///
    /// Durable callers must invoke this after publishing a terminal phase;
    /// terminal ledgers are deliberately not auto-removed by
    /// [`Self::transition_phase`] because callers commonly still need the
    /// returned ledger to build their final report.
    pub fn archive_ledger(
        &mut self,
        execution_id: &str,
    ) -> Result<TxExecutionLedger, IdempotencyError> {
        let execution_lock = if self.is_durable() {
            Some(self.acquire_execution_lock(execution_id)?)
        } else {
            None
        };
        let (ledger, pinned_file) = if let Some(lock) = &execution_lock {
            let locked = self.read_durable_ledger_locked(lock)?;
            (locked.ledger, Some(locked.pinned_file))
        } else {
            (
                self.ledgers.get(execution_id).cloned().ok_or_else(|| {
                    IdempotencyError::LedgerNotFound {
                        execution_id: execution_id.to_string(),
                    }
                })?,
                None,
            )
        };

        if !ledger.phase().is_terminal() {
            return Err(IdempotencyError::LedgerNotTerminal {
                execution_id: execution_id.to_string(),
                phase: ledger.phase(),
            });
        }

        // Re-publish the terminal snapshot before removing active state. This
        // also makes archival safe for callers that reached the terminal phase
        // through the legacy mutable-ledger API.
        self.persist_ledger_snapshot(&ledger, pinned_file.as_ref())?;
        self.ledgers.remove(execution_id);
        Ok(ledger)
    }

    /// Number of active ledgers.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.ledgers.len()
    }

    /// Current policy.
    #[must_use]
    pub fn policy(&self) -> &IdempotencyPolicy {
        &self.policy
    }

    /// Evict stale dedup entries.
    pub fn evict_stale(&mut self, cutoff_ms: u64) {
        self.dedup.evict_before(cutoff_ms);
    }

    fn logical_clock_ms(&self) -> u64 {
        let ledger_latest = self
            .ledgers
            .values()
            .flat_map(|ledger| ledger.records().iter().map(|record| record.timestamp_ms))
            .max();
        ledger_latest
            .into_iter()
            .chain(self.dedup.latest_timestamp_ms())
            .max()
            .unwrap_or(0)
    }

    fn validate_monotonic_timestamp(&self, timestamp_ms: u64) -> Result<(), IdempotencyError> {
        let high_water_ms = self.logical_clock_ms();
        if timestamp_ms < high_water_ms {
            return Err(IdempotencyError::RetrogradeTimestamp {
                observed_ms: timestamp_ms,
                high_water_ms,
            });
        }
        Ok(())
    }

    fn is_fresh_for_dedup(&self, outcome: &StepOutcome, timestamp_ms: u64, now_ms: u64) -> bool {
        outcome.is_sticky_replay_proof()
            || timestamp_ms.saturating_add(self.policy.dedup_ttl_ms) >= now_ms
    }
}

// ── Idempotency Policy ──────────────────────────────────────────────────────

/// Configuration for idempotency behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyPolicy {
    /// Maximum entries in the dedup guard.
    pub dedup_capacity: usize,
    /// Whether to skip already-completed steps on resume (vs re-execute).
    pub skip_completed_on_resume: bool,
    /// Maximum age (ms) for a dedup entry to be considered valid.
    pub dedup_ttl_ms: u64,
    /// Whether to require chain integrity for resume (vs restart fresh).
    pub require_chain_integrity: bool,
    /// Maximum number of nonterminal ledgers retained for resume.
    ///
    /// Terminal ledgers do not count after explicit archival or durable
    /// reopen, but their spool files remain replay proof. Startup fails closed
    /// if verified nonterminal ledgers exceed this limit; it never evicts
    /// resumable state to manufacture capacity.
    pub max_active_ledgers: usize,
    /// Maximum number of ledger files allowed in the durable spool (ft-u61ij).
    pub max_spool_files: usize,
    /// Maximum cumulative byte size allowed for all ledger files in the durable spool (ft-u61ij).
    pub max_spool_total_bytes: u64,
    /// Maximum number of total records allowed across the durable spool (ft-u61ij).
    pub max_spool_records: usize,
    /// Maximum byte size accepted for any single ledger file (ft-u61ij).
    pub max_ledger_bytes: u64,
}

impl Default for IdempotencyPolicy {
    fn default() -> Self {
        Self {
            dedup_capacity: 10_000,
            skip_completed_on_resume: true,
            dedup_ttl_ms: 3_600_000, // 1 hour
            require_chain_integrity: true,
            max_active_ledgers: 100,
            max_spool_files: 10_000,
            max_spool_total_bytes: 512 * 1024 * 1024, // 512 MB
            max_spool_records: 1_000_000,
            max_ledger_bytes: MAX_DURABLE_LEDGER_BYTES, // 16 MB
        }
    }
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Errors from idempotency operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum IdempotencyError {
    #[error("duplicate execution: key {key} already recorded")]
    DuplicateExecution { key: String },

    #[error("invalid phase transition: {from:?} → {to:?}")]
    InvalidPhaseTransition { from: TxPhase, to: TxPhase },

    #[error("ledger sealed in phase {phase:?}, cannot append")]
    LedgerSealed { phase: TxPhase },

    #[error("ledger not found for execution {execution_id}")]
    LedgerNotFound { execution_id: String },

    #[error("ledger {execution_id} not in terminal phase ({phase:?})")]
    LedgerNotTerminal {
        execution_id: String,
        phase: TxPhase,
    },

    #[error(
        "ledger {execution_id} cannot become terminal with ambiguous records at ordinals {ambiguous_ordinals:?}"
    )]
    AmbiguousTerminalTransition {
        execution_id: String,
        ambiguous_ordinals: Vec<u64>,
    },

    #[error("terminal disposition certificate invalid: {reason}")]
    InvalidTerminalCertificate { reason: String },

    #[error("ledger index corrupt: {reason}")]
    LedgerIndexCorrupt { reason: String },

    #[error("ledger record invariant violated: {reason}")]
    LedgerRecordInvariant { reason: String },

    #[error("chain integrity violation at ordinal {ordinal}")]
    ChainIntegrityViolation { ordinal: u64 },

    #[error("active ledger limit exceeded: max_active_ledgers={max_active_ledgers}")]
    ActiveLedgerLimitExceeded { max_active_ledgers: usize },

    #[error("spool file limit exceeded: {actual} files exceeds max {maximum}")]
    SpoolFileCountExceeded { actual: usize, maximum: usize },

    #[error("spool total byte limit exceeded: {actual} bytes exceeds max {maximum}")]
    SpoolByteLimitExceeded { actual: u64, maximum: u64 },

    #[error("spool record limit exceeded: {actual} records exceeds max {maximum}")]
    SpoolRecordCountExceeded { actual: usize, maximum: usize },

    #[error("ledger file {execution_id} is oversized: {actual} bytes exceeds max {maximum}")]
    LedgerOversized {
        execution_id: String,
        actual: u64,
        maximum: u64,
    },

    #[error("durable idempotency reservation requires a spool-backed store")]
    DurableReservationRequired,

    #[error("durable {operation} requires an execution-bound idempotency reservation")]
    DurableMutationRequiresReservation { operation: String },

    #[error("idempotency reservation already in progress for key {key}")]
    ReservationInProgress { key: String },

    #[error("execution ledger mutation already in progress for execution {execution_id}")]
    ExecutionMutationInProgress { execution_id: String },

    #[error(
        "idempotency reservation key mismatch: reserved {reserved}, attempted mutation for {attempted}"
    )]
    ReservationKeyMismatch { reserved: String, attempted: String },

    #[error(
        "idempotency reservation execution mismatch: reserved {reserved}, attempted mutation for {attempted}"
    )]
    ReservationExecutionMismatch { reserved: String, attempted: String },

    #[error("idempotency reservation for key {key} in execution {execution_id} was already used")]
    ReservationAlreadyUsed { key: String, execution_id: String },

    #[error(
        "idempotency reservation for key {key} in execution {execution_id} has no durable Pending record"
    )]
    ReservationNotPending { key: String, execution_id: String },

    #[error(
        "recovered proof mismatch for key {key}: expected durable {expected}, observed {observed}"
    )]
    RecoveredProofMismatch {
        key: String,
        expected: String,
        observed: String,
    },

    #[error(
        "idempotency reservation store mismatch: reserved spool {reserved_spool}, attempted spool {attempted_spool}"
    )]
    ReservationStoreMismatch {
        reserved_spool: String,
        attempted_spool: String,
    },

    #[error(
        "retrograde idempotency timestamp: observed {observed_ms}ms below store high-water {high_water_ms}ms"
    )]
    RetrogradeTimestamp {
        observed_ms: u64,
        high_water_ms: u64,
    },

    /// ft-iz1ki: the durable ledger spool could not be opened or flushed.
    /// Surfaced fail-closed so a tx never reports commit success on a step
    /// whose execution record was not made durable.
    #[error("ledger persistence failed: {reason}")]
    LedgerPersist { reason: String },
}

/// ft-iz1ki: filename safety guard for durable ledger files. Execution IDs are
/// timestamp-sortable and nonce-qualified when engine-generated, but validate defensively before they reach
/// the filesystem so a malformed/operator-influenced id can never traverse out
/// of the `tx_ledgers/` spool. Mirrors `steer_receipt_store::is_valid_receipt_id`.
#[must_use]
pub fn is_valid_execution_id(execution_id: &str) -> bool {
    !execution_id.is_empty()
        && execution_id.len() <= 128
        && execution_id != "."
        && execution_id != ".."
        && execution_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !execution_id.contains("..")
}

// ── Collision-resistant hashing ─────────────────────────────────────────────

/// Hash a versioned domain plus a sequence of unambiguous, length-delimited
/// components. Lengths are encoded as big-endian `u64` values so concatenation
/// boundaries cannot be moved by caller-controlled bytes.
fn sha256_domain_digest(domain: &[u8], components: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    let domain_len = u64::try_from(domain.len()).expect("hash domain length fits u64");
    hasher.update(domain_len.to_be_bytes());
    hasher.update(domain);
    for component in components {
        let component_len = u64::try_from(component.len()).expect("hash component length fits u64");
        hasher.update(component_len.to_be_bytes());
        hasher.update(component);
    }
    hex::encode(hasher.finalize())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_plan_compiler::{CompilerConfig, PlannerAssignment, compile_tx_plan};
    use proptest::prelude::*;
    #[cfg(not(windows))]
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(not(windows))]
    static DURABLE_TEST_NONCE: AtomicU64 = AtomicU64::new(0);

    #[cfg(not(windows))]
    #[test]
    fn directory_sync_handle_is_synchronizable_for_idempotency_store() {
        let workspace = tempfile::tempdir().expect("create idempotency sync workspace");
        let pinned = Dir::open_ambient_dir(workspace.path(), cap_std::ambient_authority())
            .expect("pin idempotency sync workspace");
        let sync_file = open_directory_sync_file(&pinned, workspace.path())
            .expect("open synchronizable idempotency directory handle");

        sync_file
            .sync_all()
            .expect("synchronize idempotency directory handle");
    }

    fn make_key(plan: &str, step: &str) -> IdempotencyKey {
        IdempotencyKey::new(plan, step, "action-content")
    }

    fn make_plan(n: usize) -> TxPlan {
        let assignments: Vec<PlannerAssignment> = (0..n)
            .map(|i| PlannerAssignment {
                bead_id: format!("b{i}"),
                agent_id: format!("a{}", i % 3),
                score: 0.8,
                tags: Vec::new(),
                dependency_bead_ids: Vec::new(),
            })
            .collect();
        compile_tx_plan("test-plan", &assignments, &CompilerConfig::default())
    }

    #[cfg(not(windows))]
    fn durable_test_dir(label: &str) -> PathBuf {
        let nonce = DURABLE_TEST_NONCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "tx-idempotency-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[cfg(unix)]
    fn ledger_recovery_artifacts(spool: &Path) -> Vec<PathBuf> {
        let mut artifacts: Vec<PathBuf> = std::fs::read_dir(spool)
            .expect("list durable ledger spool")
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                (name.starts_with(".tx-ledger-") && name.ends_with(".recovery.tmp"))
                    .then(|| entry.path())
            })
            .collect();
        artifacts.sort_unstable();
        artifacts
    }

    fn record_durable_outcome(
        store: &mut IdempotencyStore,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) {
        let mut reservation = store
            .acquire_durable_reservation(execution_id, &idem_key, timestamp_ms)
            .expect("acquire durable reservation");
        assert!(reservation.observed_outcome().is_none());
        store
            .record_execution_reserved(
                &mut reservation,
                execution_id,
                idem_key.clone(),
                StepOutcome::Pending,
                risk,
                agent_id,
                timestamp_ms.saturating_sub(1),
            )
            .expect("persist durable pending record");
        store
            .complete_execution_reserved(reservation, execution_id, idem_key, outcome, timestamp_ms)
            .expect("persist durable terminal outcome");
    }

    // ── IdempotencyKey tests ──

    #[test]
    fn key_deterministic() {
        let k1 = IdempotencyKey::new("p1", "s1", "action");
        let k2 = IdempotencyKey::new("p1", "s1", "action");
        assert_eq!(k1, k2);
        assert_eq!(k1.as_str(), k2.as_str());
    }

    #[test]
    fn key_different_inputs() {
        let k1 = IdempotencyKey::new("p1", "s1", "action-a");
        let k2 = IdempotencyKey::new("p1", "s1", "action-b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_different_plans() {
        let k1 = IdempotencyKey::new("p1", "s1", "action");
        let k2 = IdempotencyKey::new("p2", "s1", "action");
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_different_steps() {
        let k1 = IdempotencyKey::new("p1", "s1", "action");
        let k2 = IdempotencyKey::new("p1", "s2", "action");
        assert_ne!(k1, k2);
    }

    #[test]
    fn action_and_compensation_key_namespaces_cannot_alias() {
        let action = IdempotencyKey::new("p1", "s1", "comp:rollback");
        let compensation = IdempotencyKey::for_compensation("p1", "s1", "rollback");
        assert_ne!(action, compensation);
        assert_ne!(action.as_str(), compensation.as_str());
    }

    #[test]
    fn key_format_prefix() {
        let k = IdempotencyKey::new("p1", "s1", "action");
        assert!(k.as_str().starts_with("txk:v2:"));
        assert_eq!(k.as_str().len(), "txk:v2:".len() + 64);
    }

    #[test]
    fn key_length_delimiting_prevents_pipe_tuple_alias() {
        let left = IdempotencyKey::new("a|b", "c", "d");
        let right = IdempotencyKey::new("a", "b", "c|d");

        assert_ne!(left, right);
        assert_ne!(left.as_str(), right.as_str());
    }

    #[test]
    fn key_compensation_different_from_normal() {
        let normal = IdempotencyKey::new("p1", "s1", "rollback");
        let comp = IdempotencyKey::for_compensation("p1", "s1", "rollback");
        assert_ne!(normal, comp);
    }

    #[test]
    fn key_display() {
        let k = IdempotencyKey::new("p1", "s1", "action");
        let display = format!("{k}");
        assert_eq!(display, k.as_str());
    }

    #[test]
    fn key_serde_roundtrip() {
        let k = IdempotencyKey::new("p1", "s1", "action");
        let json = serde_json::to_string(&k).unwrap();
        let back: IdempotencyKey = serde_json::from_str(&json).unwrap();
        assert_eq!(k, back);
    }

    // br-ft-f4vta: persisted-key validation tests. The custom
    // Deserialize impl rejects malformed `key` formats, empty
    // `plan_id`, and empty `step_id` — without these gates a
    // tampered ledger could alias a raw key from one step while
    // claiming completion for a different step.

    #[test]
    fn key_deserialize_rejects_missing_txk_prefix() {
        // br-ft-f4vta: format check fires before the hash-match check,
        // so hash_input value here is arbitrary.
        let json = r#"{"key":"sha256:0123456789abcdef","plan_id":"p1","step_id":"s1","key_kind":"action","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("malformed prefix must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-f4vta") && msg.contains("txk"),
            "error should reference the malformed-key contract; got {msg:?}"
        );
    }

    #[test]
    fn key_deserialize_rejects_short_hex() {
        let json = r#"{"key":"txk:v2:0123","plan_id":"p1","step_id":"s1","key_kind":"action","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("short hex must reject");
        assert!(err.to_string().contains("br-ft-f4vta"));
    }

    #[test]
    fn key_deserialize_rejects_long_hex() {
        let json = r#"{"key":"txk:v2:00000000000000000000000000000000000000000000000000000000000000000","plan_id":"p1","step_id":"s1","key_kind":"action","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("long hex must reject");
        assert!(err.to_string().contains("br-ft-f4vta"));
    }

    #[test]
    fn key_deserialize_rejects_uppercase_hex() {
        // Constructor produces lowercase; validator pins the
        // canonical case so a tampered ledger using uppercase
        // (which Deserialize_was_not_required to reject pre-fix)
        // is flagged.
        let json = r#"{"key":"txk:v2:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","plan_id":"p1","step_id":"s1","key_kind":"action","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("uppercase hex must reject");
        assert!(err.to_string().contains("br-ft-f4vta"));
    }

    #[test]
    fn key_deserialize_rejects_non_hex_chars() {
        let json = r#"{"key":"txk:v2:000000000000000000000000000000000000000000000000000000000000000g","plan_id":"p1","step_id":"s1","key_kind":"action","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("non-hex char must reject");
        assert!(err.to_string().contains("br-ft-f4vta"));
    }

    #[test]
    fn key_deserialize_rejects_empty_plan_id() {
        let mut value = serde_json::to_value(IdempotencyKey::new("p1", "s1", "action")).unwrap();
        value["plan_id"] = serde_json::Value::String(String::new());
        let result: Result<IdempotencyKey, _> = serde_json::from_value(value);
        let err = result.expect_err("empty plan_id must reject");
        assert!(err.to_string().contains("plan_id"));
    }

    #[test]
    fn key_deserialize_rejects_empty_step_id() {
        let mut value = serde_json::to_value(IdempotencyKey::new("p1", "s1", "action")).unwrap();
        value["step_id"] = serde_json::Value::String(String::new());
        let result: Result<IdempotencyKey, _> = serde_json::from_value(value);
        let err = result.expect_err("empty step_id must reject");
        assert!(err.to_string().contains("step_id"));
    }

    /// br-ft-f4vta: cross-step alias attack. Construct a JSON
    /// carrying a valid `txk:HASH` from step_a but with the
    /// step_id field set to step_b's id (so plan_id/step_id/
    /// hash_input no longer hash to `key`). Pre-fix this slipped
    /// through the format-only check; post-fix the hash-match
    /// gate rejects it before it reaches the ledger.
    #[test]
    fn key_deserialize_rejects_cross_step_alias_ft_f4vta() {
        let real_a = IdempotencyKey::new("p1", "step-a", "action-a");
        // Forge: keep step-a's `key` and `hash_input`, but alias the
        // step_id to "step-b" — this is the smoking-gun cross-step
        // alias the bead body warns about.
        let mut forged = serde_json::to_value(real_a).unwrap();
        forged["step_id"] = serde_json::Value::String("step-b".to_string());
        let result: Result<IdempotencyKey, _> = serde_json::from_value(forged);
        let err = result.expect_err("cross-step alias must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-f4vta") && msg.contains("does not match"),
            "error must reference the hash-match contract; got {msg:?}"
        );
    }

    /// br-ft-f4vta: tampering the `hash_input` field while keeping
    /// the original `key` and metadata must also reject. Equivalent
    /// to the cross-step alias from the other direction (attacker
    /// claims the action fingerprint differs from the one that
    /// produced the hash).
    #[test]
    fn key_deserialize_rejects_tampered_hash_input_ft_f4vta() {
        let real = IdempotencyKey::new("p1", "s1", "real-action");
        let mut forged = serde_json::to_value(real).unwrap();
        forged["hash_input"] = serde_json::Value::String(format!("sha256:{}", "0".repeat(64)));
        let result: Result<IdempotencyKey, _> = serde_json::from_value(forged);
        assert!(
            result.is_err(),
            "tampered hash_input must reject (got Ok = forge undetected)"
        );
    }

    /// br-ft-f4vta: tampering `key` while keeping plan_id/step_id/
    /// hash_input must reject. Format check passes if the tampered
    /// key is still a valid v2 SHA-256 form, but the hash-match gate fires.
    #[test]
    fn key_deserialize_rejects_tampered_key_with_valid_format_ft_f4vta() {
        let real = IdempotencyKey::new("p1", "s1", "real-action");
        // Use a different valid txk format string (different hex)
        // that does NOT match the legitimate hash for these inputs.
        let mut forged = serde_json::to_value(real).unwrap();
        forged["key"] = serde_json::Value::String(format!("txk:v2:{}", "0".repeat(64)));
        let result: Result<IdempotencyKey, _> = serde_json::from_value(forged);
        let err = result.expect_err("forged key must reject even with valid format");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-f4vta") && msg.contains("does not match"),
            "error must reference the hash-match contract; got {msg:?}"
        );
    }

    // br-ft-f4vta: a constructor-built key always serializes to a form that
    // round-trips through the strict Deserialize. Pinned via proptest over
    // arbitrary plan_id, step_id, and action_fingerprint inputs.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn key_constructor_output_always_round_trips(
            plan_id in "[a-zA-Z0-9_-]{1,32}",
            step_id in "[a-zA-Z0-9_-]{1,32}",
            action in "[a-zA-Z0-9 _-]{0,64}",
        ) {
            let k = IdempotencyKey::new(&plan_id, &step_id, &action);
            let json = serde_json::to_string(&k).expect("serialize");
            let back: IdempotencyKey = serde_json::from_str(&json)
                .expect("constructor output must round-trip through validator");
            prop_assert_eq!(k, back);
        }

        /// br-ft-f4vta: arbitrary `key` strings that DO NOT
        /// match the txk:v2:64hex format are rejected by the
        /// Deserialize gate. Universal quantifier over the
        /// reject path.
        #[test]
        fn key_deserialize_rejects_arbitrary_malformed_key(
            bad_prefix in "[a-z]{1,8}:",
            tail in "[0-9a-f]{0,32}",
        ) {
            // Skip valid v2 outputs from this generator.
            let bad_key = format!("{bad_prefix}{tail}");
            let is_valid = is_well_formed_idempotency_key(&bad_key);
            prop_assume!(!is_valid);

            let json = format!(r#"{{"key":"{bad_key}","plan_id":"p","step_id":"s","key_kind":"action","hash_input":"sha256:{}"}}"#, "0".repeat(64));
            let result: Result<IdempotencyKey, _> =
                serde_json::from_str(&json);
            let is_err = result.is_err();
            prop_assert!(is_err, "malformed key {bad_key:?} must reject");
        }

        /// br-ft-f4vta: cross-step alias attack at scale. For any
        /// pair of distinct (plan_id, step_id, action) tuples,
        /// constructing a JSON that mixes one tuple's `key` with
        /// another tuple's source fields must reject — the hash
        /// won't match.
        #[test]
        fn key_deserialize_rejects_cross_tuple_alias_property(
            plan_a in "[a-z]{2,6}-[0-9]{1,3}",
            step_a in "[a-z]{2,6}-[0-9]{1,3}",
            action_a in "[a-zA-Z0-9_]{1,16}",
            plan_b in "[a-z]{2,6}-[0-9]{1,3}",
            step_b in "[a-z]{2,6}-[0-9]{1,3}",
            action_b in "[a-zA-Z0-9_]{1,16}",
        ) {
            let key_a = IdempotencyKey::new(&plan_a, &step_a, &action_a);
            let key_b = IdempotencyKey::new(&plan_b, &step_b, &action_b);
            // Skip the (vanishingly rare) hash-collision case where
            // the two tuples happen to produce the same `key` —
            // those are NOT the cross-step alias attack.
            prop_assume!(key_a.as_str() != key_b.as_str());

            // Forge: take A's key with B's full source tuple. The
            // re-derivation will yield key_b, mismatching the
            // persisted key_a → reject.
            let mut forged = serde_json::to_value(&key_b).expect("serialize key B");
            forged["key"] = serde_json::Value::String(key_a.as_str().to_string());
            let result: Result<IdempotencyKey, _> = serde_json::from_value(forged);
            prop_assert!(
                result.is_err(),
                "cross-tuple alias must reject: A.key={} with B.tuple=({}, {}, {})",
                key_a.as_str(), plan_b, step_b, action_b
            );
        }
    }

    #[test]
    fn key_accessors() {
        let k = IdempotencyKey::new("my-plan", "my-step", "action");
        assert_eq!(k.plan_id(), "my-plan");
        assert_eq!(k.step_id(), "my-step");
    }

    // ── StepOutcome tests ──

    #[test]
    fn outcome_success_is_terminal() {
        let o = StepOutcome::Success { result: None };
        assert!(o.is_terminal());
        assert!(!o.is_failure());
        assert!(!o.is_pending());
    }

    #[test]
    fn outcome_failed_not_terminal() {
        let o = StepOutcome::Failed {
            error_code: "E001".into(),
            error_message: "oops".into(),
            compensated: false,
        };
        assert!(!o.is_terminal());
        assert!(o.is_failure());
    }

    #[test]
    fn outcome_skipped_is_terminal() {
        let o = StepOutcome::Skipped {
            reason: "already done".into(),
        };
        assert!(o.is_terminal());
    }

    #[test]
    fn outcome_compensated_is_terminal() {
        let o = StepOutcome::Compensated {
            original_outcome: Box::new(StepOutcome::Failed {
                error_code: "E001".into(),
                error_message: "oops".into(),
                compensated: true,
            }),
            compensation_result: "rolled back".into(),
        };
        assert!(o.is_terminal());
    }

    #[test]
    fn outcome_pending_not_terminal() {
        assert!(StepOutcome::Pending.is_pending());
        assert!(!StepOutcome::Pending.is_terminal());
    }

    #[test]
    fn outcome_serde_roundtrip() {
        let outcomes = vec![
            StepOutcome::Success {
                result: Some("ok".into()),
            },
            StepOutcome::Failed {
                error_code: "E001".into(),
                error_message: "fail".into(),
                compensated: false,
            },
            StepOutcome::Skipped {
                reason: "done".into(),
            },
            StepOutcome::Pending,
        ];
        for o in &outcomes {
            let json = serde_json::to_string(o).unwrap();
            let back: StepOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, o);
        }
    }

    // ── TxPhase tests ──

    #[test]
    fn phase_planned_transitions() {
        assert!(TxPhase::Planned.can_transition_to(TxPhase::Preparing));
        assert!(TxPhase::Planned.can_transition_to(TxPhase::Aborted));
        assert!(!TxPhase::Planned.can_transition_to(TxPhase::Committing));
        assert!(!TxPhase::Planned.can_transition_to(TxPhase::Completed));
    }

    #[test]
    fn phase_preparing_transitions() {
        assert!(TxPhase::Preparing.can_transition_to(TxPhase::Committing));
        assert!(TxPhase::Preparing.can_transition_to(TxPhase::Aborted));
        assert!(!TxPhase::Preparing.can_transition_to(TxPhase::Planned));
    }

    #[test]
    fn phase_committing_transitions() {
        assert!(TxPhase::Committing.can_transition_to(TxPhase::Compensating));
        assert!(TxPhase::Committing.can_transition_to(TxPhase::Completed));
        assert!(TxPhase::Committing.can_transition_to(TxPhase::Aborted));
    }

    #[test]
    fn phase_terminal_no_transitions() {
        assert!(TxPhase::Completed.valid_transitions().is_empty());
        assert!(TxPhase::Aborted.valid_transitions().is_empty());
        assert!(TxPhase::Completed.is_terminal());
        assert!(TxPhase::Aborted.is_terminal());
    }

    #[test]
    fn phase_non_terminal() {
        assert!(!TxPhase::Planned.is_terminal());
        assert!(!TxPhase::Preparing.is_terminal());
        assert!(!TxPhase::Committing.is_terminal());
        assert!(!TxPhase::Compensating.is_terminal());
    }

    #[test]
    fn phase_serde_roundtrip() {
        let phases = [
            TxPhase::Planned,
            TxPhase::Preparing,
            TxPhase::Committing,
            TxPhase::Compensating,
            TxPhase::Completed,
            TxPhase::Aborted,
        ];
        for p in &phases {
            let json = serde_json::to_string(p).unwrap();
            let back: TxPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, p);
        }
    }

    // ── TxExecutionLedger tests ──

    #[test]
    fn ledger_new_empty() {
        let ledger = TxExecutionLedger::new("exec-1", "plan-1", 12345);
        assert_eq!(ledger.execution_id(), "exec-1");
        assert_eq!(ledger.plan_id(), "plan-1");
        assert_eq!(ledger.phase(), TxPhase::Planned);
        assert_eq!(ledger.record_count(), 0);
    }

    #[test]
    fn ledger_append_and_lookup() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let key = make_key("plan-1", "step-b0");
        let outcome = StepOutcome::Success { result: None };
        let hash = ledger
            .append(key.clone(), outcome.clone(), StepRisk::Low, "agent-0", 1000)
            .unwrap();

        assert!(!hash.is_empty());
        assert!(ledger.is_executed(&key));
        assert_eq!(ledger.get_outcome(&key), Some(&outcome));
        assert_eq!(ledger.record_count(), 1);
    }

    #[test]
    fn ledger_duplicate_rejected() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        let key = make_key("plan-1", "step-b0");
        ledger
            .append(
                key.clone(),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let err = ledger
            .append(key, StepOutcome::Pending, StepRisk::Low, "a", 2000)
            .unwrap_err();
        assert!(matches!(err, IdempotencyError::DuplicateExecution { .. }));
    }

    #[test]
    fn ledger_sealed_rejects_append() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();
        ledger.transition_phase(TxPhase::Completed).unwrap();

        let key = make_key("plan-1", "step-b0");
        let err = ledger
            .append(key, StepOutcome::Pending, StepRisk::Low, "a", 1000)
            .unwrap_err();
        assert!(matches!(err, IdempotencyError::LedgerSealed { .. }));
    }

    #[test]
    fn ledger_rejects_terminal_phase_with_ambiguous_records() {
        for outcome in [
            StepOutcome::Pending,
            StepOutcome::Skipped {
                reason: "dispatch state unknown".to_string(),
            },
        ] {
            let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
            ledger.transition_phase(TxPhase::Preparing).unwrap();
            ledger
                .append(
                    make_key("plan-1", "step-b0"),
                    outcome,
                    StepRisk::Low,
                    "agent",
                    1_000,
                )
                .unwrap();
            let error = ledger
                .transition_phase(TxPhase::Aborted)
                .expect_err("ambiguous execution proof cannot be sealed terminal");
            assert!(matches!(
                error,
                IdempotencyError::AmbiguousTerminalTransition { .. }
            ));
            assert_eq!(ledger.phase(), TxPhase::Preparing);
        }
    }

    #[test]
    fn ledger_deserialize_rejects_header_record_identity_mismatch() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger
            .append(
                make_key("plan-1", "step-b0"),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent",
                1_000,
            )
            .unwrap();

        let mut wrong_execution = serde_json::to_value(&ledger).unwrap();
        wrong_execution["records"][0]["execution_id"] = serde_json::json!("exec-other");
        let error = serde_json::from_value::<TxExecutionLedger>(wrong_execution).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match ledger execution_id")
        );

        let mut wrong_plan = serde_json::to_value(&ledger).unwrap();
        wrong_plan["records"][0]["idem_key"] =
            serde_json::to_value(make_key("plan-other", "step-b0")).unwrap();
        let error = serde_json::from_value::<TxExecutionLedger>(wrong_plan).unwrap_err();
        assert!(error.to_string().contains("does not match ledger plan_id"));
    }

    #[test]
    fn ledger_deserialize_rejects_terminal_ambiguous_outcome() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger
            .append(
                make_key("plan-1", "step-b0"),
                StepOutcome::Pending,
                StepRisk::Low,
                "agent",
                1_000,
            )
            .unwrap();
        let mut value = serde_json::to_value(&ledger).unwrap();
        value["phase"] = serde_json::json!("aborted");

        let error = serde_json::from_value::<TxExecutionLedger>(value).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("terminal TxExecutionLedger contains ambiguous")
        );
    }

    #[test]
    fn ledger_hash_chain_integrity() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        for i in 0..5 {
            let key = make_key("plan-1", &format!("step-{i}"));
            ledger
                .append(
                    key,
                    StepOutcome::Success { result: None },
                    StepRisk::Low,
                    "a",
                    1000 + i,
                )
                .unwrap();
        }

        let verification = ledger.verify_chain();
        assert!(verification.chain_intact);
        assert_eq!(verification.total_records, 5);
        assert!(verification.missing_ordinals.is_empty());
    }

    #[test]
    fn ledger_phase_transitions() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        assert_eq!(ledger.phase(), TxPhase::Planned);

        ledger.transition_phase(TxPhase::Preparing).unwrap();
        assert_eq!(ledger.phase(), TxPhase::Preparing);

        ledger.transition_phase(TxPhase::Committing).unwrap();
        assert_eq!(ledger.phase(), TxPhase::Committing);

        ledger.transition_phase(TxPhase::Completed).unwrap();
        assert_eq!(ledger.phase(), TxPhase::Completed);
    }

    #[test]
    fn ledger_invalid_phase_transition() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        let err = ledger.transition_phase(TxPhase::Committing).unwrap_err();
        assert!(matches!(
            err,
            IdempotencyError::InvalidPhaseTransition { .. }
        ));
    }

    #[test]
    fn ledger_completed_and_failed_steps() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let k1 = make_key("plan-1", "step-ok");
        let k2 = make_key("plan-1", "step-fail");

        ledger
            .append(
                k1,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();
        ledger
            .append(
                k2,
                StepOutcome::Failed {
                    error_code: "E1".into(),
                    error_message: "bad".into(),
                    compensated: false,
                },
                StepRisk::High,
                "a",
                2000,
            )
            .unwrap();

        assert!(ledger.completed_steps().contains("step-ok"));
        assert!(ledger.failed_steps().contains("step-fail"));
    }

    #[test]
    fn ledger_pending_step_ids() {
        let plan = make_plan(3);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        // Execute first step only.
        let key = make_key("test-plan", &plan.steps[0].id);
        ledger
            .append(
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let pending = ledger.pending_step_ids(&plan);
        assert_eq!(pending.len(), 2);
        assert!(!pending.contains(&plan.steps[0].id));
    }

    #[test]
    fn ledger_deserialize_rebuilds_index_ft_738kn() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        let key = make_key("plan-1", "s1");
        ledger
            .append(
                key.clone(),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let json = serde_json::to_string(&ledger).unwrap();
        let mut restored: TxExecutionLedger = serde_json::from_str(&json).unwrap();
        assert!(restored.is_executed(&key));

        restored.key_index.clear();
        assert!(!restored.is_executed(&key));
        restored.rebuild_index();
        assert!(restored.is_executed(&key));
    }

    /// br-ft-738kn: a deserialized ledger MUST reject a duplicate
    /// append even without an explicit `rebuild_index` call. Pre-fix,
    /// `key_index` was `#[serde(skip)]` and `append`'s
    /// `contains_key` check ran against an empty map post-deserialize
    /// → duplicate steps could re-execute at the resume boundary.
    /// Post-fix, deserialization rebuilds the index before the
    /// ledger is usable.
    #[test]
    fn ledger_append_after_deserialize_rejects_duplicate_without_explicit_rebuild() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        let key = make_key("plan-1", "s1");
        ledger
            .append(
                key.clone(),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let json = serde_json::to_string(&ledger).unwrap();
        let mut restored: TxExecutionLedger = serde_json::from_str(&json).unwrap();
        assert!(restored.is_executed(&key));

        // Simulate any future constructor path that leaves records
        // populated but the runtime-only index empty. Append must
        // self-heal before the duplicate check, without an explicit
        // rebuild_index() call from its caller.
        restored.key_index.clear();
        let outcome = restored.append(
            key.clone(),
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "a",
            2000,
        );
        assert!(
            matches!(
                &outcome,
                Err(IdempotencyError::DuplicateExecution { key: k }) if k == key.as_str()
            ),
            "expected DuplicateExecution after deserialize+append, got {outcome:?}"
        );

        // After the self-heal, an unrelated key must still
        // succeed — the guard should not block legitimate appends.
        let key2 = make_key("plan-1", "s2");
        let recorded = restored.append(
            key2,
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "a",
            3000,
        );
        assert!(
            recorded.is_ok(),
            "unrelated key must still append after index self-heal: {recorded:?}"
        );
    }

    /// br-ft-738kn: a malformed serialized ledger containing the
    /// same idempotency key more than once is ambiguous and must be
    /// rejected instead of collapsing the skipped runtime index to
    /// whichever record was seen last.
    #[test]
    fn ledger_deserialize_rejects_duplicate_keys_ft_738kn() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        let key = make_key("plan-1", "s1");
        ledger
            .append(
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let json = serde_json::to_string(&ledger).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let records = value
            .get_mut("records")
            .and_then(serde_json::Value::as_array_mut)
            .unwrap();
        let mut duplicate = records.first().unwrap().clone();
        duplicate["ordinal"] = serde_json::json!(1);
        records.push(duplicate);

        let err = serde_json::from_value::<TxExecutionLedger>(value).unwrap_err();
        assert!(
            err.to_string()
                .contains("duplicate idempotency key in tx ledger")
        );
    }

    /// br-ft-738kn: lazy rebuild must NOT misfire on a fresh ledger
    /// (records empty, index empty) — i.e. the very first append on a
    /// new ledger must work without going through rebuild_index.
    #[test]
    fn ledger_first_append_on_fresh_ledger_unaffected_by_lazy_guard() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        let key = make_key("plan-1", "s1");
        let result = ledger.append(
            key,
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "a",
            1000,
        );
        assert!(
            result.is_ok(),
            "fresh-ledger first append failed: {result:?}"
        );
        assert_eq!(ledger.record_count(), 1);
    }

    #[test]
    fn ledger_serde_roundtrip() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 42);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        let key = make_key("plan-1", "s1");
        ledger
            .append(
                key.clone(),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let json = serde_json::to_string(&ledger).unwrap();
        let back: TxExecutionLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.execution_id(), "exec-1");
        assert_eq!(back.record_count(), 1);
        assert_eq!(back.phase(), TxPhase::Preparing);
        assert!(back.is_executed(&key));
    }

    // ── br-ft-ddm8k: TxExecutionLedger deserialize trust-boundary ────────

    /// Build a JSON ledger string with explicit (records, next_ordinal,
    /// last_hash) so tests can plant forged combinations.
    fn ledger_json(records: &[StepExecutionRecord], next_ordinal: u64, last_hash: &str) -> String {
        let records_json = serde_json::to_string(records).unwrap();
        format!(
            r#"{{"execution_id":"e","plan_id":"p","plan_hash":42,"phase":"preparing","records":{records},"last_hash":"{last_hash}","next_ordinal":{next_ordinal}}}"#,
            records = records_json
        )
    }

    fn build_record(
        plan_id: &str,
        step_id: &str,
        ordinal: u64,
        prev_hash: &str,
    ) -> StepExecutionRecord {
        StepExecutionRecord {
            ordinal,
            idem_key: make_key(plan_id, step_id),
            execution_id: "e".to_string(),
            timestamp_ms: 1000 + ordinal,
            outcome: StepOutcome::Success { result: None },
            risk: StepRisk::Low,
            prev_hash: prev_hash.to_string(),
            agent_id: "a".to_string(),
        }
    }

    #[test]
    fn deserialize_rejects_next_ordinal_below_records_len_ft_ddm8k() {
        // Forged: records=[r0, r1] but next_ordinal=1.
        // Pre-fix accepted; next append() would reuse ordinal 1.
        let r0 = build_record("p", "s0", 0, "");
        let r0_hash = r0.hash();
        let r1 = build_record("p", "s1", 1, &r0_hash);
        let r1_hash = r1.hash();
        let json = ledger_json(&[r0, r1], 1, &r1_hash);
        let err = serde_json::from_str::<TxExecutionLedger>(&json)
            .expect_err("br-ft-ddm8k: forged next_ordinal must be rejected");
        assert!(
            err.to_string().contains("next_ordinal"),
            "br-ft-ddm8k: error message must reference next_ordinal; got {err}"
        );
        assert!(err.to_string().contains("br-ft-ddm8k"));
    }

    #[test]
    fn deserialize_rejects_non_monotonic_ordinals_ft_ddm8k() {
        // Forged: records have ordinals 0, 5, 2 (not 0..len). Keep the
        // timestamps independently monotonic so this fixture isolates the
        // ordinal-density trust-boundary check instead of being rejected first
        // by the stricter timestamp high-water invariant.
        let r0 = build_record("p", "s0", 0, "");
        let mut r5 = build_record("p", "s5", 5, "anyhash");
        r5.timestamp_ms = 1_001;
        let mut r2 = build_record("p", "s2", 2, "anyhash");
        r2.timestamp_ms = 1_002;
        let json = ledger_json(&[r0, r5, r2], 6, "anyhash");
        let err = serde_json::from_str::<TxExecutionLedger>(&json)
            .expect_err("br-ft-ddm8k: non-monotonic ordinals must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("br-ft-ddm8k") && message.contains("dense 0..len"),
            "br-ft-ddm8k: error must reference ordinal density; got {err}"
        );
    }

    #[test]
    fn deserialize_rejects_detached_last_hash_ft_ddm8k() {
        // Forged: records=[r0] with valid ordinal/next_ordinal,
        // but last_hash="" (detached from r0.hash()).
        let r0 = build_record("p", "s0", 0, "");
        let json = ledger_json(&[r0], 1, "");
        let err = serde_json::from_str::<TxExecutionLedger>(&json)
            .expect_err("br-ft-ddm8k: detached last_hash must be rejected");
        assert!(
            err.to_string().contains("last_hash") || err.to_string().contains("detached"),
            "br-ft-ddm8k: error must reference last_hash; got {err}"
        );
    }

    #[test]
    fn deserialize_accepts_well_formed_empty_ledger_ft_ddm8k() {
        // Empty ledger: records=[], next_ordinal=0, last_hash="".
        // All three invariants trivially satisfied.
        let json = r#"{"execution_id":"e","plan_id":"p","plan_hash":42,"phase":"preparing","records":[],"last_hash":"","next_ordinal":0}"#;
        let ledger = serde_json::from_str::<TxExecutionLedger>(json)
            .expect("empty well-formed ledger must deserialize");
        assert_eq!(ledger.record_count(), 0);
        assert_eq!(ledger.next_ordinal, 0);
    }

    #[test]
    fn deserialize_accepts_well_formed_populated_ledger_ft_ddm8k() {
        // Negative pin: a normal append() roundtrip ledger MUST
        // pass all three new gates. If it doesn't, the validator
        // is over-rejecting.
        let mut ledger = TxExecutionLedger::new("e", "p", 42);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        for i in 0..3 {
            let key = make_key("p", &format!("s{i}"));
            ledger
                .append(
                    key,
                    StepOutcome::Success { result: None },
                    StepRisk::Low,
                    "a",
                    1000 + i,
                )
                .unwrap();
        }
        let json = serde_json::to_string(&ledger).unwrap();
        let back: TxExecutionLedger = serde_json::from_str(&json)
            .expect("br-ft-ddm8k: well-formed roundtrip must accept; over-rejection regression");
        assert_eq!(back.record_count(), 3);
        assert_eq!(back.next_ordinal, 3);
    }

    // ── DeduplicationGuard tests ──

    #[test]
    fn dedup_empty() {
        let guard = DeduplicationGuard::new(10);
        assert!(guard.is_empty());
        assert_eq!(guard.len(), 0);
    }

    #[test]
    fn dedup_record_and_check() {
        let mut guard = DeduplicationGuard::new(10);
        let key = make_key("p1", "s1");
        guard.record(&key, "exec-1", StepOutcome::Success { result: None }, 1000);
        assert_eq!(guard.len(), 1);
        let entry = guard.check(&key).unwrap();
        assert_eq!(entry.execution_id, "exec-1");
    }

    #[test]
    fn dedup_miss() {
        let guard = DeduplicationGuard::new(10);
        let key = make_key("p1", "s1");
        assert!(guard.check(&key).is_none());
    }

    #[test]
    fn dedup_eviction_at_capacity() {
        let mut guard = DeduplicationGuard::new(3);
        for i in 0..5 {
            let key = make_key("p1", &format!("s{i}"));
            guard.record(
                &key,
                "exec-1",
                StepOutcome::Success { result: None },
                i as u64 * 1000,
            );
        }
        assert_eq!(guard.len(), 3);
        // Oldest (s0, s1) should be evicted.
        assert!(guard.check(&make_key("p1", "s0")).is_none());
        assert!(guard.check(&make_key("p1", "s1")).is_none());
        assert!(guard.check(&make_key("p1", "s2")).is_some());
    }

    #[test]
    fn dedup_update_in_place() {
        let mut guard = DeduplicationGuard::new(10);
        let key = make_key("p1", "s1");
        guard.record(&key, "exec-1", StepOutcome::Pending, 1000);
        guard.record(&key, "exec-1", StepOutcome::Success { result: None }, 2000);
        assert_eq!(guard.len(), 1);
        let entry = guard.check(&key).unwrap();
        assert!(matches!(entry.outcome, StepOutcome::Success { .. }));
    }

    #[test]
    fn dedup_update_refreshes_eviction_order() {
        let mut guard = DeduplicationGuard::new(2);
        let refreshed = make_key("p1", "refreshed");
        let stale = make_key("p1", "stale");
        let newest = make_key("p1", "newest");

        guard.record(&refreshed, "exec-1", StepOutcome::Pending, 1000);
        guard.record(&stale, "exec-1", StepOutcome::Pending, 2000);
        guard.record(
            &refreshed,
            "exec-2",
            StepOutcome::Success {
                result: Some("refreshed".to_string()),
            },
            3000,
        );
        guard.record(&newest, "exec-1", StepOutcome::Pending, 4000);

        assert_eq!(guard.len(), 2);
        assert!(
            guard.check(&refreshed).is_some(),
            "refreshed key must remain resident under capacity pressure"
        );
        assert!(
            guard.check(&stale).is_none(),
            "least-recently recorded key should evict first"
        );
        assert!(guard.check(&newest).is_some());

        let refreshed_entry = guard.check(&refreshed).unwrap();
        assert_eq!(refreshed_entry.execution_id, "exec-2");
        assert_eq!(refreshed_entry.timestamp_ms, 3000);
        assert!(matches!(
            refreshed_entry.outcome,
            StepOutcome::Success { .. }
        ));
    }

    #[test]
    fn dedup_evict_before() {
        let mut guard = DeduplicationGuard::new(10);
        for i in 0..5 {
            let key = make_key("p1", &format!("s{i}"));
            guard.record(
                &key,
                "exec-1",
                StepOutcome::Failed {
                    error_code: "retryable".to_string(),
                    error_message: "retry after ttl".to_string(),
                    compensated: false,
                },
                i as u64 * 1000,
            );
        }
        guard.evict_before(2500);
        assert_eq!(guard.len(), 2); // s3 (3000) and s4 (4000) remain.
    }

    #[test]
    fn dedup_bulk_eviction_keeps_entry_and_order_indexes_in_sync() {
        const ENTRY_COUNT: usize = 4_096;
        let mut guard = DeduplicationGuard::new(ENTRY_COUNT);
        for index in 0..ENTRY_COUNT {
            let key = make_key("bulk-plan", &format!("step-{index}"));
            guard.record(
                &key,
                "exec-bulk",
                StepOutcome::Failed {
                    error_code: "retryable".to_string(),
                    error_message: "expired".to_string(),
                    compensated: false,
                },
                index as u64,
            );
        }

        guard.evict_before(ENTRY_COUNT as u64);

        assert!(guard.entries.is_empty());
        assert!(guard.order.is_empty());
    }

    #[test]
    fn dedup_ttl_never_expires_pending_or_durable_terminal_proof() {
        let mut guard = DeduplicationGuard::new(10);
        let pending = make_key("p1", "pending");
        let success = make_key("p1", "success");
        let compensated = make_key("p1", "compensated");
        let failed = make_key("p1", "failed");
        guard.record(&pending, "exec-1", StepOutcome::Pending, 1);
        guard.record(&success, "exec-1", StepOutcome::Success { result: None }, 2);
        guard.record(
            &compensated,
            "exec-1",
            StepOutcome::Compensated {
                original_outcome: Box::new(StepOutcome::Success { result: None }),
                compensation_result: "done".to_string(),
            },
            3,
        );
        guard.record(
            &failed,
            "exec-1",
            StepOutcome::Failed {
                error_code: "retryable".to_string(),
                error_message: "retry".to_string(),
                compensated: false,
            },
            4,
        );

        guard.evict_before(10_000);

        assert!(guard.check(&pending).is_some());
        assert!(guard.check(&success).is_some());
        assert!(guard.check(&compensated).is_some());
        assert!(guard.check(&failed).is_none());
    }

    #[test]
    fn dedup_clear() {
        let mut guard = DeduplicationGuard::new(10);
        let key = make_key("p1", "s1");
        guard.record(&key, "exec-1", StepOutcome::Pending, 1000);
        guard.clear();
        assert!(guard.is_empty());
    }

    // ── ResumeContext tests ──

    #[test]
    fn resume_already_complete() {
        let plan = make_plan(2);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();
        ledger.transition_phase(TxPhase::Completed).unwrap();

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::AlreadyComplete);
    }

    #[test]
    fn resume_terminal_corrupt_chain_restarts_fresh_ft_7323v() {
        let plan = make_plan(1);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();
        ledger
            .append(
                make_key("test-plan", &plan.steps[0].id),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-a",
                1000,
            )
            .unwrap();
        ledger.records[0].prev_hash = "tampered".to_string();
        ledger.transition_phase(TxPhase::Completed).unwrap();

        let ctx = ResumeContext::from_ledger(&ledger, &plan);

        assert!(!ctx.chain_intact);
        assert_eq!(ctx.recommendation, ResumeRecommendation::RestartFresh);
    }

    #[test]
    fn resume_continue_from_checkpoint() {
        let plan = make_plan(3);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        // Execute only first step.
        let key = make_key("test-plan", &plan.steps[0].id);
        ledger
            .append(
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(
            ctx.recommendation,
            ResumeRecommendation::ContinueFromCheckpoint
        );
        assert_eq!(ctx.remaining_steps.len(), 2);
        assert_eq!(ctx.completed_steps.len(), 1);
    }

    #[test]
    fn resume_skipped_steps_fail_closed_for_reconciliation() {
        let plan = make_plan(2);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        for step in &plan.steps {
            let key = make_key("test-plan", &step.id);
            ledger
                .append(
                    key,
                    StepOutcome::Skipped {
                        reason: "pause_suspended".to_string(),
                    },
                    StepRisk::Low,
                    "a",
                    1000,
                )
                .unwrap();
        }

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::CompensateAndAbort);
        assert!(ctx.completed_steps.is_empty());
        assert_eq!(ctx.remaining_steps.len(), 2);
    }

    #[test]
    fn resume_all_steps_done_but_not_terminal() {
        let plan = make_plan(1);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let key = make_key("test-plan", &plan.steps[0].id);
        ledger
            .append(
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::AlreadyComplete);
        assert!(ctx.remaining_steps.is_empty());
    }

    #[test]
    fn resume_failed_last_step_is_not_already_complete() {
        let plan = make_plan(1);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let key = make_key("test-plan", &plan.steps[0].id);
        ledger
            .append(
                key,
                StepOutcome::Failed {
                    error_code: "E1".into(),
                    error_message: "bad".into(),
                    compensated: false,
                },
                StepRisk::High,
                "a",
                1000,
            )
            .unwrap();

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::CompensateAndAbort);
        assert!(ctx.remaining_steps.is_empty());
        assert_eq!(ctx.failed_steps, vec![plan.steps[0].id.clone()]);
    }

    #[test]
    fn resume_context_replay_determinism_conformance_matrix() {
        #[derive(Debug, Clone, Copy)]
        enum OutcomeSpec {
            Success,
            Failed,
            Skipped(&'static str),
        }

        #[derive(Debug)]
        struct Case {
            name: &'static str,
            outcomes: &'static [OutcomeSpec],
            expected_recommendation: ResumeRecommendation,
            expected_remaining: usize,
            expected_completed: usize,
            expected_failed: usize,
        }

        let cases = [
            Case {
                name: "partial_commit_failure_compensates_and_aborts",
                outcomes: &[
                    OutcomeSpec::Success,
                    OutcomeSpec::Failed,
                    OutcomeSpec::Skipped("commit_skipped_after_failure"),
                ],
                expected_recommendation: ResumeRecommendation::CompensateAndAbort,
                expected_remaining: 0,
                expected_completed: 1,
                expected_failed: 1,
            },
            Case {
                name: "pause_suspended_skips_require_reconciliation",
                outcomes: &[
                    OutcomeSpec::Skipped("pause_suspended"),
                    OutcomeSpec::Skipped("pause_suspended"),
                    OutcomeSpec::Skipped("pause_suspended"),
                ],
                expected_recommendation: ResumeRecommendation::CompensateAndAbort,
                expected_remaining: 3,
                expected_completed: 0,
                expected_failed: 0,
            },
            Case {
                name: "all_success_nonterminal_is_already_complete",
                outcomes: &[
                    OutcomeSpec::Success,
                    OutcomeSpec::Success,
                    OutcomeSpec::Success,
                ],
                expected_recommendation: ResumeRecommendation::AlreadyComplete,
                expected_remaining: 0,
                expected_completed: 3,
                expected_failed: 0,
            },
        ];

        for case in cases {
            let plan = make_plan(case.outcomes.len());
            let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
            ledger.transition_phase(TxPhase::Preparing).unwrap();
            ledger.transition_phase(TxPhase::Committing).unwrap();

            for (index, outcome) in case.outcomes.iter().copied().enumerate() {
                let step = &plan.steps[index];
                let outcome = match outcome {
                    OutcomeSpec::Success => StepOutcome::Success {
                        result: Some(case.name.to_string()),
                    },
                    OutcomeSpec::Failed => StepOutcome::Failed {
                        error_code: "FTX3999".to_string(),
                        error_message: "commit failed".to_string(),
                        compensated: false,
                    },
                    OutcomeSpec::Skipped(reason) => StepOutcome::Skipped {
                        reason: reason.to_string(),
                    },
                };

                ledger
                    .append(
                        make_key("test-plan", &step.id),
                        outcome,
                        StepRisk::Low,
                        "agent-a",
                        1000 + u64::try_from(index).unwrap(),
                    )
                    .unwrap();
            }

            let replayed: TxExecutionLedger =
                serde_json::from_str(&serde_json::to_string(&ledger).unwrap()).unwrap();
            let ctx = ResumeContext::from_ledger(&ledger, &plan);
            let replayed_ctx = ResumeContext::from_ledger(&replayed, &plan);

            assert_eq!(
                serde_json::to_value(&ctx).unwrap(),
                serde_json::to_value(&replayed_ctx).unwrap(),
                "{}: resume context must be deterministic after ledger replay",
                case.name
            );
            assert_eq!(
                ctx.recommendation, case.expected_recommendation,
                "{}: recommendation",
                case.name
            );
            assert_eq!(
                ctx.remaining_steps.len(),
                case.expected_remaining,
                "{}: remaining steps",
                case.name
            );
            assert_eq!(
                ctx.completed_steps.len(),
                case.expected_completed,
                "{}: completed steps",
                case.name
            );
            assert_eq!(
                ctx.failed_steps.len(),
                case.expected_failed,
                "{}: failed steps",
                case.name
            );
        }
    }

    #[test]
    fn resume_compensate_and_abort() {
        let plan = make_plan(2);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();
        ledger.transition_phase(TxPhase::Compensating).unwrap();

        let key = make_key("test-plan", &plan.steps[0].id);
        ledger
            .append(
                key,
                StepOutcome::Failed {
                    error_code: "E1".into(),
                    error_message: "bad".into(),
                    compensated: false,
                },
                StepRisk::High,
                "a",
                1000,
            )
            .unwrap();

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::CompensateAndAbort);
    }

    #[test]
    fn resume_serde_roundtrip() {
        let plan = make_plan(2);
        let ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        let json = serde_json::to_string(&ctx).unwrap();
        let back: ResumeContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.execution_id, ctx.execution_id);
        assert_eq!(back.recommendation, ctx.recommendation);
    }

    // ── IdempotencyStore tests ──

    #[test]
    fn store_create_and_get_ledger() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();

        assert_eq!(store.active_count(), 1);
        let ledger = store.get_ledger("exec-1").unwrap();
        assert_eq!(ledger.execution_id(), "exec-1");
    }

    #[cfg(not(windows))]
    #[test]
    fn store_reports_whether_cross_process_durability_is_available() {
        let in_memory = IdempotencyStore::new(IdempotencyPolicy::default());
        assert!(!in_memory.is_durable());

        let ft_dir = durable_test_dir("is-durable");
        let durable = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        assert!(durable.is_durable());
        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[test]
    fn store_duplicate_ledger_rejected() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(1);
        store.create_ledger("exec-1", &plan).unwrap();
        let err = store.create_ledger("exec-1", &plan).unwrap_err();
        assert!(matches!(err, IdempotencyError::DuplicateExecution { .. }));
    }

    #[test]
    fn store_record_and_dedup() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();

        // Transition phase.
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(TxPhase::Preparing)
            .unwrap();

        let key = make_key("test-plan", "step-b0");
        let outcome = StepOutcome::Success { result: None };

        // No dedup hit before recording.
        assert!(
            store
                .peek_cached_outcome(&key, store.logical_clock_ms())
                .is_none()
        );

        // Record execution.
        store
            .record_execution(
                "exec-1",
                key.clone(),
                outcome.clone(),
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        // Dedup hit after recording.
        assert_eq!(
            store.peek_cached_outcome(&key, store.logical_clock_ms()),
            Some(&outcome)
        );
    }

    // ── ft-iz1ki: durable store restart-safety ──────────────────────────

    #[cfg(not(windows))]
    #[test]
    fn open_persists_and_reloads_ledger_for_restart_dedup() {
        let ft_dir =
            std::env::temp_dir().join(format!("ft-iz1ki-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ft_dir);

        let plan = make_plan(2);
        let key = make_key("test-plan", "step-b0");
        let outcome = StepOutcome::Success { result: None };

        // First "run": durable store records a committed step.
        {
            let mut store =
                IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
            store
                .create_ledger("txe-1000", &plan)
                .expect("create ledger");
            store
                .transition_phase("txe-1000", TxPhase::Preparing)
                .expect("persist preparing phase");
            record_durable_outcome(
                &mut store,
                "txe-1000",
                key.clone(),
                outcome.clone(),
                StepRisk::Low,
                "agent",
                1_000,
            );
            // Spool file must exist on disk.
            assert!(
                ft_dir.join("tx_ledgers").join("txe-1000.json").is_file(),
                "ledger must be persisted to the spool"
            );
        }

        // Second "run" (process restart): a fresh store reloads the spool and
        // dedups the already-committed step — the ft-iz1ki gap (re-dispatch
        // after crash) is closed.
        {
            let reopened = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
                .expect("reopen store");
            assert_eq!(
                reopened.peek_cached_outcome(&key, reopened.logical_clock_ms()),
                Some(&outcome),
                "reloaded ledger must satisfy dedup after restart"
            );
        }

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn restart_rebuilds_dedup_from_terminal_ledgers_beyond_active_budget() {
        let ft_dir = durable_test_dir("all-terminal-dedup");
        let plan = make_plan(4);
        let policy = IdempotencyPolicy {
            dedup_capacity: 16,
            max_active_ledgers: 1,
            ..IdempotencyPolicy::default()
        };
        let mut expected = Vec::new();

        {
            let mut store = IdempotencyStore::open(&ft_dir, policy.clone()).expect("open store");
            for (index, step) in plan.steps.iter().enumerate() {
                let execution_id = format!("txe-terminal-{index}");
                let key = make_key("test-plan", &step.id);
                let outcome = StepOutcome::Success {
                    result: Some(format!("result-{index}")),
                };
                store
                    .create_ledger(&execution_id, &plan)
                    .expect("create terminal ledger");
                store
                    .transition_phase(&execution_id, TxPhase::Preparing)
                    .expect("transition to preparing");
                record_durable_outcome(
                    &mut store,
                    &execution_id,
                    key.clone(),
                    outcome.clone(),
                    StepRisk::Low,
                    "agent",
                    1_000 + u64::try_from(index).expect("index fits u64"),
                );
                store
                    .transition_phase(&execution_id, TxPhase::Committing)
                    .expect("transition to committing");
                store
                    .transition_phase(&execution_id, TxPhase::Completed)
                    .expect("transition to completed");
                store
                    .archive_ledger(&execution_id)
                    .expect("archive terminal ledger");
                expected.push((key, outcome));
            }
            assert_eq!(store.active_count(), 0);
        }

        let reopened = IdempotencyStore::open(&ft_dir, policy).expect("reopen terminal spool");
        assert_eq!(reopened.active_count(), 0);
        for (key, outcome) in &expected {
            assert_eq!(
                reopened.peek_cached_outcome(key, reopened.logical_clock_ms()),
                Some(outcome),
                "every terminal ledger must rebuild replay proof even beyond the old reload budget"
            );
        }

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn open_rebuilds_bounded_dedup_by_record_timestamp_not_filename() {
        let ft_dir = durable_test_dir("dedup-rebuild-order");
        let spool = ft_dir.join("tx_ledgers");
        std::fs::create_dir_all(&spool).expect("create spool");
        let plan = make_plan(3);
        let fixtures = [
            ("txe-z-old", 100_u64, 0_usize),
            ("txe-a-middle", 200, 1),
            ("txe-m-newest", 300, 2),
        ];
        let mut keys = Vec::new();
        for (execution_id, timestamp_ms, step_index) in fixtures {
            let key = make_key("test-plan", &plan.steps[step_index].id);
            let mut ledger = TxExecutionLedger::new(execution_id, &plan.plan_id, plan.plan_hash);
            ledger.transition_phase(TxPhase::Preparing).unwrap();
            ledger
                .append(
                    key.clone(),
                    StepOutcome::Success {
                        result: Some(execution_id.to_string()),
                    },
                    StepRisk::Low,
                    "agent",
                    timestamp_ms,
                )
                .unwrap();
            ledger.transition_phase(TxPhase::Committing).unwrap();
            ledger.transition_phase(TxPhase::Completed).unwrap();
            std::fs::write(
                spool.join(format!("{execution_id}.json")),
                serde_json::to_vec_pretty(&ledger).unwrap(),
            )
            .expect("write ledger fixture");
            keys.push(key);
        }

        let reopened = IdempotencyStore::open(
            &ft_dir,
            IdempotencyPolicy {
                dedup_capacity: 2,
                max_active_ledgers: 1,
                ..IdempotencyPolicy::default()
            },
        )
        .expect("rebuild bounded replay index");
        let now_ms = reopened.logical_clock_ms();
        assert!(reopened.peek_cached_outcome(&keys[0], now_ms).is_none());
        assert!(reopened.peek_cached_outcome(&keys[1], now_ms).is_some());
        assert!(reopened.peek_cached_outcome(&keys[2], now_ms).is_some());

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_archive_retains_spool_proof_across_reopen() {
        let ft_dir = durable_test_dir("archive-retains-proof");
        let plan = make_plan(1);
        let execution_id = "txe-archive-proof";
        let key = make_key("test-plan", &plan.steps[0].id);
        let outcome = StepOutcome::Compensated {
            original_outcome: Box::new(StepOutcome::Success { result: None }),
            compensation_result: "rolled-back".to_string(),
        };
        let spool_path = ft_dir
            .join("tx_ledgers")
            .join(format!("{execution_id}.json"));

        {
            let mut store =
                IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
            store
                .create_ledger(execution_id, &plan)
                .expect("create ledger");
            store
                .transition_phase(execution_id, TxPhase::Preparing)
                .expect("transition to preparing");
            record_durable_outcome(
                &mut store,
                execution_id,
                key.clone(),
                outcome.clone(),
                StepRisk::Low,
                "agent",
                1_000,
            );
            store
                .transition_phase(execution_id, TxPhase::Committing)
                .expect("transition to committing");
            store
                .transition_phase(execution_id, TxPhase::Completed)
                .expect("transition to completed");
            store
                .archive_ledger(execution_id)
                .expect("archive terminal ledger");
            assert_eq!(store.active_count(), 0);
            assert!(spool_path.is_file(), "archive must retain durable proof");
        }

        let reopened = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("reopen archived spool");
        assert_eq!(reopened.active_count(), 0);
        assert_eq!(
            reopened.peek_cached_outcome(&key, reopened.logical_clock_ms()),
            Some(&outcome)
        );
        assert!(spool_path.is_file(), "reopen must not consume spool proof");

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn open_fails_closed_when_nonterminal_ledgers_exceed_active_policy() {
        let ft_dir = durable_test_dir("active-overflow");
        let plan = make_plan(1);
        {
            let mut store = IdempotencyStore::open(
                &ft_dir,
                IdempotencyPolicy {
                    max_active_ledgers: 2,
                    ..IdempotencyPolicy::default()
                },
            )
            .expect("open wide store");
            store.create_ledger("txe-active-a", &plan).unwrap();
            store.create_ledger("txe-active-b", &plan).unwrap();
        }

        let error = IdempotencyStore::open(
            &ft_dir,
            IdempotencyPolicy {
                max_active_ledgers: 1,
                ..IdempotencyPolicy::default()
            },
        )
        .expect_err("startup must not discard a resumable ledger");
        assert_eq!(
            error,
            IdempotencyError::ActiveLedgerLimitExceeded {
                max_active_ledgers: 1
            }
        );

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_duplicate_create_collision_leaves_no_recovery_artifacts() {
        let ft_dir = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let execution_id = "txe-create-collision";
        let mut winner = IdempotencyStore::open(ft_dir.path(), IdempotencyPolicy::default())
            .expect("open winning store");
        let mut stale_loser = IdempotencyStore::open(ft_dir.path(), IdempotencyPolicy::default())
            .expect("open stale losing store");

        winner
            .create_ledger(execution_id, &plan)
            .expect("publish winning ledger");
        let ledger_path = ft_dir
            .path()
            .join("tx_ledgers")
            .join(format!("{execution_id}.json"));
        let winning_bytes = std::fs::read(&ledger_path).expect("read winning ledger");

        for _ in 0..16 {
            let error = stale_loser
                .create_ledger(execution_id, &plan)
                .expect_err("no-clobber loser must report duplicate execution");
            assert_eq!(
                error,
                IdempotencyError::DuplicateExecution {
                    key: execution_id.to_string()
                }
            );
        }

        assert_eq!(
            std::fs::read(&ledger_path).expect("reread winning ledger"),
            winning_bytes,
            "losing creates must not alter the authoritative ledger"
        );
        let recovery_artifacts = std::fs::read_dir(ft_dir.path().join("tx_ledgers"))
            .expect("list durable spool")
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(".tx-ledger-") && name.ends_with(".recovery.tmp")
            })
            .count();
        assert_eq!(recovery_artifacts, 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_durable_store_fails_before_creating_namespace() {
        let workspace = tempfile::tempdir().expect("create workspace directory");
        let anchor = workspace.path().join(".ft");

        let error = IdempotencyStore::open(&anchor, IdempotencyPolicy::default())
            .expect_err("Windows durability must fail closed before effects");
        let IdempotencyError::LedgerPersist { reason } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(reason.contains("unsupported on Windows"));
        assert!(
            !anchor.exists(),
            "unsupported durable acquisition must not create its namespace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_workspace_anchor_rename_does_not_redirect_ledger_writes() {
        // Replacing the caller-declared `.ft` control-plane anchor is outside
        // the documented same-UID threat boundary. Within that boundary this
        // test proves the compatibility store never redirects I/O by
        // re-resolving its former ambient path.
        let workspace = tempfile::tempdir().expect("create workspace directory");
        let anchor = workspace.path().join(".ft");
        std::fs::create_dir(&anchor).expect("create workspace control directory");
        let pinned = Dir::open_ambient_dir(&anchor, cap_std::ambient_authority())
            .expect("pin workspace control directory");
        let plan = make_plan(1);
        let execution_id = "txe-pinned-anchor";
        let mut store = IdempotencyStore::open_in_pinned_dir(
            pinned,
            anchor.clone(),
            IdempotencyPolicy::default(),
        )
        .expect("open pinned durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create ledger through pinned anchor");

        let moved_anchor = workspace.path().join(".ft-moved");
        std::fs::rename(&anchor, &moved_anchor).expect("move pinned control directory");
        let replacement_spool = anchor.join(TX_LEDGER_DIR_NAME);
        std::fs::create_dir_all(&replacement_spool).expect("create replacement namespace");
        let replacement_ledger = replacement_spool.join(format!("{execution_id}.json"));
        let sentinel = b"replacement namespace sentinel";
        std::fs::write(&replacement_ledger, sentinel).expect("write replacement sentinel");

        store
            .transition_phase(execution_id, TxPhase::Preparing)
            .expect("write remains relative to pinned anchor");

        assert_eq!(
            std::fs::read(&replacement_ledger).expect("read replacement sentinel"),
            sentinel,
            "ambient replacement namespace must remain untouched"
        );
        let moved_ledger = moved_anchor
            .join(TX_LEDGER_DIR_NAME)
            .join(format!("{execution_id}.json"));
        let persisted: TxExecutionLedger = serde_json::from_slice(
            &std::fs::read(&moved_ledger).expect("read ledger through moved anchor path"),
        )
        .expect("deserialize pinned ledger");
        assert_eq!(persisted.phase(), TxPhase::Preparing);
    }

    #[cfg(unix)]
    #[test]
    fn substituted_tx_ledgers_directory_fails_closed_without_touching_replacement() {
        let anchor = tempfile::tempdir().expect("create workspace control directory");
        let plan = make_plan(1);
        let execution_id = "txe-spool-substitution";
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create initial ledger");

        let spool = anchor.path().join(TX_LEDGER_DIR_NAME);
        let moved_spool = anchor.path().join("tx_ledgers-moved");
        std::fs::rename(&spool, &moved_spool).expect("move pinned ledger spool");
        std::fs::create_dir(&spool).expect("install replacement ledger spool");
        let replacement_ledger = spool.join(format!("{execution_id}.json"));
        let sentinel = b"replacement spool sentinel";
        std::fs::write(&replacement_ledger, sentinel).expect("write replacement sentinel");

        let error = store
            .transition_phase(execution_id, TxPhase::Preparing)
            .expect_err("substituted pinned spool must fail closed");
        let IdempotencyError::LedgerPersist { reason } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(
            reason.contains("no longer names the pinned filesystem object"),
            "unexpected reason: {reason}"
        );
        assert_eq!(
            std::fs::read(&replacement_ledger).expect("read replacement sentinel"),
            sentinel,
            "failed mutation must not touch the replacement spool"
        );
        assert!(
            moved_spool.join(format!("{execution_id}.json")).is_file(),
            "original pinned ledger remains available for recovery"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ledger_replace_rejects_substituted_symlink_leaf_without_clobbering_it() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let execution_id = "txe-symlink-substitution";
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create initial ledger");

        let spool = anchor.path().join(TX_LEDGER_DIR_NAME);
        let ledger_path = spool.join(format!("{execution_id}.json"));
        let original_path = spool.join(format!("{execution_id}.original"));
        std::fs::rename(&ledger_path, &original_path).expect("retain original ledger fixture");
        let sentinel_path = anchor.path().join("unrelated-sentinel");
        let sentinel = b"must not be overwritten";
        std::fs::write(&sentinel_path, sentinel).expect("write unrelated sentinel");
        std::os::unix::fs::symlink(&sentinel_path, &ledger_path)
            .expect("substitute ledger symlink");

        let error = store
            .transition_phase(execution_id, TxPhase::Preparing)
            .expect_err("substituted symlink ledger must fail closed");
        assert!(
            matches!(&error, IdempotencyError::LedgerPersist { .. }),
            "unexpected error: {error:?}"
        );

        assert_eq!(
            std::fs::read(&sentinel_path).expect("read unrelated sentinel"),
            sentinel,
            "ledger publication must never write through the substituted symlink"
        );
        let metadata =
            std::fs::symlink_metadata(&ledger_path).expect("inspect substituted ledger leaf");
        assert!(
            metadata.file_type().is_symlink(),
            "failed mutation must preserve the substituted symlink entry"
        );
        assert!(
            original_path.is_file(),
            "original fixture remains inspectable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_lock_rejects_symbolic_link_lock_file() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        let key = make_key("test-plan", &plan.steps[0].id);
        let key_hash = key.as_str().strip_prefix("txk:v2:").expect("key prefix");
        let lock_dir = anchor
            .path()
            .join(TX_LEDGER_DIR_NAME)
            .join(KEY_LOCK_DIR_NAME);
        let lock_path = lock_dir.join(format!("{key_hash}.lock"));
        let target_path = anchor.path().join("unrelated-target");
        std::fs::write(&target_path, b"unrelated target").unwrap();
        std::os::unix::fs::symlink(&target_path, &lock_path).unwrap();

        let err = store.acquire_durable_key_lock(&key).unwrap_err();
        assert!(matches!(err, IdempotencyError::LedgerPersist { .. }));
        assert_eq!(std::fs::read(&target_path).unwrap(), b"unrelated target");
    }

    #[cfg(unix)]
    #[test]
    fn key_lock_rejects_multi_link_lock_file() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        let key = make_key("test-plan", &plan.steps[0].id);
        let key_hash = key.as_str().strip_prefix("txk:v2:").expect("key prefix");
        let lock_dir = anchor
            .path()
            .join(TX_LEDGER_DIR_NAME)
            .join(KEY_LOCK_DIR_NAME);
        let lock_path = lock_dir.join(format!("{key_hash}.lock"));
        std::fs::write(&lock_path, b"").unwrap();
        let second_link = anchor.path().join("extra.link");
        std::fs::hard_link(&lock_path, &second_link).unwrap();

        let err = store.acquire_durable_key_lock(&key).unwrap_err();
        assert!(matches!(err, IdempotencyError::LedgerPersist { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn ledger_replace_rejects_single_link_regular_substitution_after_locked_read() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let execution_id = "txe-regular-substitution";
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create initial ledger");

        let spool = anchor.path().join(TX_LEDGER_DIR_NAME);
        let ledger_path = spool.join(format!("{execution_id}.json"));
        let original_path = spool.join(format!("{execution_id}.original"));
        let hook_ledger_path = ledger_path.clone();
        let hook_original_path = original_path.clone();
        let sentinel = b"single-link foreign ledger sentinel".to_vec();
        let hook_sentinel = sentinel.clone();
        let _hook_guard = set_ledger_pre_replace_test_hook(move || {
            std::fs::rename(&hook_ledger_path, &hook_original_path)
                .expect("move authorized ledger after locked read");
            std::fs::write(&hook_ledger_path, hook_sentinel)
                .expect("install single-link foreign ledger leaf");
        });

        let error = store
            .transition_phase(execution_id, TxPhase::Preparing)
            .expect_err("foreign replacement ledger must fail closed");
        let IdempotencyError::LedgerPersist { reason } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(
            reason.contains(
                "no longer names the exact filesystem object read under the execution lock"
            ),
            "unexpected reason: {reason}"
        );
        assert!(
            reason.contains("recovery artifact name durably retained"),
            "replacement mismatch must retain a synchronized recovery artifact: {reason}"
        );
        assert_eq!(
            std::fs::read(&ledger_path).expect("read foreign ledger sentinel"),
            sentinel,
            "failed publication must not clobber the foreign regular file"
        );
        let original: TxExecutionLedger = serde_json::from_slice(
            &std::fs::read(&original_path).expect("read original authorized ledger"),
        )
        .expect("deserialize original authorized ledger");
        assert_eq!(original.phase(), TxPhase::Planned);
        assert_eq!(
            ledger_recovery_artifacts(&spool).len(),
            1,
            "failed replacement must retain exactly one recovery artifact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ledger_replace_rejects_missing_destination_after_locked_read() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let execution_id = "txe-missing-replacement";
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create initial ledger");

        let spool = anchor.path().join(TX_LEDGER_DIR_NAME);
        let ledger_path = spool.join(format!("{execution_id}.json"));
        let original_path = spool.join(format!("{execution_id}.original"));
        let hook_ledger_path = ledger_path.clone();
        let hook_original_path = original_path.clone();
        let _hook_guard = set_ledger_pre_replace_test_hook(move || {
            std::fs::rename(&hook_ledger_path, &hook_original_path)
                .expect("move authorized ledger after locked read");
        });

        let error = store
            .transition_phase(execution_id, TxPhase::Preparing)
            .expect_err("missing replacement destination must fail closed");
        let IdempotencyError::LedgerPersist { reason } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(
            reason.contains("recovery artifact name durably retained"),
            "missing destination must retain a synchronized recovery artifact: {reason}"
        );
        assert!(
            !ledger_path.exists(),
            "failed publication must not recreate the missing destination"
        );
        let original: TxExecutionLedger = serde_json::from_slice(
            &std::fs::read(&original_path).expect("read original authorized ledger"),
        )
        .expect("deserialize original authorized ledger");
        assert_eq!(original.phase(), TxPhase::Planned);
        assert_eq!(
            ledger_recovery_artifacts(&spool).len(),
            1,
            "failed replacement must retain exactly one recovery artifact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ledger_replace_succeeds_when_locked_identity_is_unchanged() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let execution_id = "txe-unchanged-replacement";
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create initial ledger");

        store
            .transition_phase(execution_id, TxPhase::Preparing)
            .expect("unchanged locked ledger identity must publish");

        let ledger_path = anchor
            .path()
            .join(TX_LEDGER_DIR_NAME)
            .join(format!("{execution_id}.json"));
        let persisted: TxExecutionLedger =
            serde_json::from_slice(&std::fs::read(&ledger_path).expect("read replaced ledger"))
                .expect("deserialize replaced ledger");
        assert_eq!(persisted.phase(), TxPhase::Preparing);
    }

    #[cfg(unix)]
    #[test]
    fn durable_open_rejects_multi_link_ledger_leaf() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let execution_id = "txe-hardlink-substitution";
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create initial ledger");
        drop(store);

        let ledger_path = anchor
            .path()
            .join(TX_LEDGER_DIR_NAME)
            .join(format!("{execution_id}.json"));
        let alias_path = anchor
            .path()
            .join(TX_LEDGER_DIR_NAME)
            .join(format!("{execution_id}.alias"));
        std::fs::hard_link(&ledger_path, &alias_path).expect("add hostile hard link");

        let error = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect_err("multi-link durable leaf must fail closed");
        let IdempotencyError::LedgerPersist { reason } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(reason.contains("hard links"), "unexpected reason: {reason}");
    }

    #[cfg(unix)]
    #[test]
    fn durable_open_rejects_fifo_ledger_leaf_without_blocking() {
        let anchor = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(1);
        let execution_id = "txe-fifo-substitution";
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger(execution_id, &plan)
            .expect("create initial ledger");
        drop(store);

        let ledger_path = anchor
            .path()
            .join(TX_LEDGER_DIR_NAME)
            .join(format!("{execution_id}.json"));
        let original_path = ledger_path.with_extension("original");
        std::fs::rename(&ledger_path, &original_path).expect("retain original ledger fixture");
        let status = std::process::Command::new("mkfifo")
            .arg(&ledger_path)
            .status()
            .expect("run mkfifo for hostile ledger fixture");
        assert!(status.success(), "mkfifo must create the hostile fixture");

        let error = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect_err("FIFO durable leaf must fail closed without waiting for a writer");
        let IdempotencyError::LedgerPersist { reason } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(
            reason.contains("not a regular file"),
            "unexpected reason: {reason}"
        );
        assert!(original_path.is_file());
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_failed_outcome_expires_only_after_ttl_on_reopen() {
        let ft_dir = tempfile::tempdir().expect("create durable store directory");
        let policy = IdempotencyPolicy {
            dedup_ttl_ms: 100,
            ..IdempotencyPolicy::default()
        };
        let plan = make_plan(1);
        let key = make_key("test-plan", &plan.steps[0].id);
        let failed = StepOutcome::Failed {
            error_code: "retryable".to_string(),
            error_message: "try again after ttl".to_string(),
            compensated: false,
        };

        {
            let mut first = IdempotencyStore::open(ft_dir.path(), policy.clone())
                .expect("open first durable store");
            first
                .create_ledger("txe-failed-source", &plan)
                .expect("create source ledger");
            first
                .transition_phase("txe-failed-source", TxPhase::Preparing)
                .expect("prepare source ledger");
            record_durable_outcome(
                &mut first,
                "txe-failed-source",
                key.clone(),
                failed.clone(),
                StepRisk::Low,
                "agent-source",
                1_000,
            );
        }

        let mut restarted = IdempotencyStore::open(ft_dir.path(), policy)
            .expect("reopen durable store after failure");
        restarted
            .create_ledger("txe-failed-retry", &plan)
            .expect("create distinct retry execution");
        let retrograde = restarted
            .acquire_durable_reservation("txe-failed-retry", &key, 999)
            .expect_err("time before the durable high-water mark must fail closed");
        assert_eq!(
            retrograde,
            IdempotencyError::RetrogradeTimestamp {
                observed_ms: 999,
                high_water_ms: 1_000
            }
        );

        let boundary = restarted
            .acquire_durable_reservation("txe-failed-retry", &key, 1_100)
            .expect("ttl boundary reservation");
        assert_eq!(boundary.observed_outcome(), Some(&failed));
        drop(boundary);

        let expired = restarted
            .acquire_durable_reservation("txe-failed-retry", &key, 1_101)
            .expect("post-ttl reservation");
        assert!(
            expired.observed_outcome().is_none(),
            "a retryable failure expires strictly after timestamp + ttl"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_skipped_outcome_expires_only_after_ttl_on_reopen() {
        let ft_dir = tempfile::tempdir().expect("create durable store directory");
        let policy = IdempotencyPolicy {
            dedup_ttl_ms: 100,
            ..IdempotencyPolicy::default()
        };
        let plan = make_plan(1);
        let key = make_key("test-plan", &plan.steps[0].id);
        let skipped = StepOutcome::Skipped {
            reason: "retryable precondition".to_string(),
        };

        {
            let store =
                IdempotencyStore::open(ft_dir.path(), policy.clone()).expect("open fixture store");
            let mut source =
                TxExecutionLedger::new("txe-skipped-source", &plan.plan_id, plan.plan_hash);
            source
                .transition_phase(TxPhase::Preparing)
                .expect("prepare skipped fixture");
            source
                .append(
                    key.clone(),
                    skipped.clone(),
                    StepRisk::Low,
                    "agent-source",
                    1_000,
                )
                .expect("append historical skipped fixture");
            store
                .persist_new_ledger_snapshot(&source)
                .expect("persist skipped fixture");
        }

        let mut restarted =
            IdempotencyStore::open(ft_dir.path(), policy).expect("reopen durable store after skip");
        restarted
            .create_ledger("txe-skipped-retry", &plan)
            .expect("create distinct retry execution");
        let boundary = restarted
            .acquire_durable_reservation("txe-skipped-retry", &key, 1_100)
            .expect("ttl boundary reservation");
        assert_eq!(boundary.observed_outcome(), Some(&skipped));
        drop(boundary);

        let expired = restarted
            .acquire_durable_reservation("txe-skipped-retry", &key, 1_101)
            .expect("post-ttl reservation");
        assert!(
            expired.observed_outcome().is_none(),
            "a retryable skip expires strictly after timestamp + ttl"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_reservation_retries_only_failed_or_skipped_compensation_keys() {
        let ft_dir = tempfile::tempdir().expect("create durable store directory");
        let plan = make_plan(4);
        let failed_compensation =
            IdempotencyKey::for_compensation(&plan.plan_id, &plan.steps[0].id, "undo-failed");
        let skipped_compensation =
            IdempotencyKey::for_compensation(&plan.plan_id, &plan.steps[1].id, "undo-skipped");
        let failed_action = make_key(&plan.plan_id, &plan.steps[2].id);
        let pending_compensation =
            IdempotencyKey::for_compensation(&plan.plan_id, &plan.steps[3].id, "undo-pending");
        let failed = StepOutcome::Failed {
            error_code: "compensation_failed".to_string(),
            error_message: "retry compensation".to_string(),
            compensated: false,
        };
        let skipped = StepOutcome::Skipped {
            reason: "retry compensation".to_string(),
        };
        let mut store = IdempotencyStore::open(ft_dir.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        store
            .create_ledger("txe-compensation-source", &plan)
            .expect("create source ledger");
        store
            .transition_phase("txe-compensation-source", TxPhase::Preparing)
            .expect("prepare source ledger");
        record_durable_outcome(
            &mut store,
            "txe-compensation-source",
            failed_compensation.clone(),
            failed.clone(),
            StepRisk::Low,
            "agent-source",
            1_000,
        );
        let mut skipped_source = TxExecutionLedger::new(
            "txe-compensation-skipped-source",
            &plan.plan_id,
            plan.plan_hash,
        );
        skipped_source
            .transition_phase(TxPhase::Preparing)
            .expect("prepare skipped compensation fixture");
        skipped_source
            .append(
                skipped_compensation.clone(),
                skipped.clone(),
                StepRisk::Low,
                "agent-source",
                1_001,
            )
            .expect("append skipped compensation fixture");
        store
            .persist_new_ledger_snapshot(&skipped_source)
            .expect("persist skipped compensation fixture");
        record_durable_outcome(
            &mut store,
            "txe-compensation-source",
            failed_action.clone(),
            failed.clone(),
            StepRisk::Low,
            "agent-source",
            1_002,
        );
        let mut pending_source = store
            .acquire_durable_reservation("txe-compensation-source", &pending_compensation, 1_003)
            .expect("reserve ambiguous compensation fixture");
        store
            .record_execution_reserved(
                &mut pending_source,
                "txe-compensation-source",
                pending_compensation.clone(),
                StepOutcome::Pending,
                StepRisk::Low,
                "agent-source",
                1_003,
            )
            .expect("persist ambiguous compensation fixture");
        drop(pending_source);

        store
            .create_ledger("txe-compensation-retry", &plan)
            .expect("create retry ledger");
        for (idem_key, expected, timestamp_ms) in [
            (failed_compensation.clone(), &failed, 1_004),
            (skipped_compensation, &skipped, 1_006),
        ] {
            let mut reservation = store
                .acquire_durable_reservation("txe-compensation-retry", &idem_key, timestamp_ms)
                .expect("acquire retryable compensation reservation");
            assert_eq!(reservation.observed_outcome(), Some(expected));
            store
                .record_execution_reserved(
                    &mut reservation,
                    "txe-compensation-retry",
                    idem_key.clone(),
                    StepOutcome::Pending,
                    StepRisk::Low,
                    "agent-retry",
                    timestamp_ms,
                )
                .expect("failed or skipped compensation must admit same-key retry");
            store
                .complete_execution_reserved(
                    reservation,
                    "txe-compensation-retry",
                    idem_key,
                    StepOutcome::Compensated {
                        original_outcome: Box::new(StepOutcome::Success { result: None }),
                        compensation_result: "undo complete".to_string(),
                    },
                    timestamp_ms + 1,
                )
                .expect("complete retried compensation");
        }

        for idem_key in [failed_action, pending_compensation, failed_compensation] {
            let mut reservation = store
                .acquire_durable_reservation("txe-compensation-retry", &idem_key, 1_008)
                .expect("acquire negative-boundary reservation");
            let error = store
                .record_execution_reserved(
                    &mut reservation,
                    "txe-compensation-retry",
                    idem_key.clone(),
                    StepOutcome::Pending,
                    StepRisk::Low,
                    "agent-retry",
                    1_008,
                )
                .expect_err("non-retryable durable outcome must remain deduplicated");
            assert_eq!(
                error,
                IdempotencyError::DuplicateExecution {
                    key: idem_key.as_str().to_string()
                }
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_key_lease_batch_canonicalizes_input_and_rejects_duplicates() {
        let anchor = tempfile::tempdir().expect("create durable key lease workspace");
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        let mut expected = vec![
            IdempotencyKey::for_compensation("test-plan", "step-c", "undo-c"),
            IdempotencyKey::new("test-plan", "step-a", "commit-a"),
            IdempotencyKey::for_compensation("test-plan", "step-b", "undo-b"),
        ];
        expected.sort_unstable();

        let leases = store
            .acquire_durable_key_leases(expected.iter().rev().cloned(), 1_000)
            .expect("reverse caller order must canonicalize before acquisition");
        assert_eq!(leases.len(), expected.len());
        assert_eq!(
            leases.leases.keys().cloned().collect::<Vec<_>>(),
            expected,
            "held lease order must be the canonical IdempotencyKey order"
        );
        assert!(leases.uses_one_exclusive_barrier());
        assert_eq!(leases.key_lock_count(), 0);
        drop(leases);

        let duplicate = IdempotencyKey::new("test-plan", "step-duplicate", "commit");
        let error = store
            .acquire_durable_key_leases(vec![duplicate.clone(), duplicate.clone()], 1_000)
            .expect_err("duplicate batch keys must fail before lock acquisition");
        assert!(matches!(
            error,
            IdempotencyError::LedgerRecordInvariant { ref reason }
                if reason.contains(duplicate.as_str())
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn plan_proof_barrier_serializes_batch_against_same_plan_writers_only() {
        let anchor = tempfile::tempdir().expect("create durable key lease workspace");
        let mut first = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open first store");
        let mut second = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open second store");
        let mut contender = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open contender store");
        let same_plan_a = IdempotencyKey::new("plan-a", "step-a", "commit-a");
        let same_plan_b = IdempotencyKey::new("plan-a", "step-b", "commit-b");
        let same_plan_unlisted = IdempotencyKey::new("plan-a", "step-c", "commit-c");
        let other_plan = IdempotencyKey::new("plan-b", "step-a", "commit-a");

        let shared_a = first
            .acquire_durable_key_lease(&same_plan_a, 1_000)
            .expect("first same-plan writer acquires a shared barrier");
        let shared_b = second
            .acquire_durable_key_lease(&same_plan_b, 1_000)
            .expect("different same-plan key shares the plan barrier");
        let error = contender
            .acquire_durable_key_leases(vec![same_plan_a.clone(), same_plan_b.clone()], 1_000)
            .expect_err("exclusive batch barrier must wait for same-plan writers");
        assert!(matches!(
            error,
            IdempotencyError::ReservationInProgress { ref key }
                if key == "plan:plan-a"
        ));
        drop(shared_b);
        drop(shared_a);

        let batch = contender
            .acquire_durable_key_leases(vec![same_plan_a, same_plan_b], 1_000)
            .expect("batch acquires after shared same-plan writers release");
        let blocked_unlisted = first
            .acquire_durable_key_lease(&same_plan_unlisted, 1_000)
            .expect_err("exclusive plan barrier blocks even an unlisted same-plan key");
        assert!(matches!(
            blocked_unlisted,
            IdempotencyError::ReservationInProgress { ref key }
                if key == "plan:plan-a"
        ));
        let unrelated = second
            .acquire_durable_key_lease(&other_plan, 1_000)
            .expect("plan-scoped barrier must not serialize an unrelated plan");
        drop(unrelated);
        drop(batch);

        first
            .acquire_durable_key_lease(&same_plan_unlisted, 1_000)
            .expect("same-plan writer acquires after exclusive batch releases");
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_key_lease_batch_rejects_mixed_plans_before_lock_or_scan() {
        let anchor = tempfile::tempdir().expect("create durable key lease workspace");
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        let plan_a = IdempotencyKey::new("plan-a", "step-a", "commit-a");
        let plan_b = IdempotencyKey::new("plan-b", "step-b", "commit-b");
        let scans_before = store.durable_refresh_scan_count;

        let error = store
            .acquire_durable_key_leases(vec![plan_a, plan_b], 1_000)
            .expect_err("one atomic proof batch cannot span plan barriers");
        assert!(matches!(
            error,
            IdempotencyError::LedgerRecordInvariant { ref reason }
                if reason.contains("mixes plan")
        ));
        assert_eq!(store.durable_refresh_scan_count, scans_before);

        let empty_error = store
            .acquire_durable_key_leases(Vec::new(), 1_000)
            .expect_err("empty proof batch has no plan barrier to authenticate");
        assert!(matches!(
            empty_error,
            IdempotencyError::LedgerRecordInvariant { ref reason }
                if reason.contains("must not be empty")
        ));
        assert_eq!(store.durable_refresh_scan_count, scans_before);
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_key_lease_batch_uses_one_descriptor_and_one_scan_for_thousand_keys() {
        let anchor = tempfile::tempdir().expect("create durable key lease workspace");
        let mut store = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open durable store");
        let keys = (0..1_000)
            .map(|index| {
                IdempotencyKey::new(
                    "large-plan",
                    &format!("step-{index}"),
                    &format!("commit-{index}"),
                )
            })
            .collect::<Vec<_>>();
        let scans_before = store.durable_refresh_scan_count;

        let leases = store
            .acquire_durable_key_leases(keys, 1_000)
            .expect("large proof set must not consume one descriptor per key");
        assert_eq!(leases.len(), 1_000);
        assert_eq!(leases.key_lock_count(), 0);
        assert!(leases.uses_one_exclusive_barrier());
        assert_eq!(store.durable_refresh_scan_count, scans_before + 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_key_lease_batch_releases_barrier_after_bulk_scan_error() {
        let anchor = tempfile::tempdir().expect("create durable key lease workspace");
        let plan = make_plan(1);
        let key = make_key(&plan.plan_id, &plan.steps[0].id);
        let mut source = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open source store");
        let mut stale = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open stale store before durable high-water advances");
        source
            .create_ledger("txe-barrier-error-source", &plan)
            .expect("create source ledger");
        source
            .transition_phase("txe-barrier-error-source", TxPhase::Preparing)
            .expect("prepare source ledger");
        record_durable_outcome(
            &mut source,
            "txe-barrier-error-source",
            key.clone(),
            StepOutcome::Success {
                result: Some("committed".to_string()),
            },
            StepRisk::Low,
            "agent-source",
            2_000,
        );

        let error = stale
            .acquire_durable_key_leases(vec![key.clone()], 1_000)
            .expect_err("bulk scan must reject a retrograde stale-process timestamp");
        assert_eq!(
            error,
            IdempotencyError::RetrogradeTimestamp {
                observed_ms: 1_000,
                high_water_ms: 2_000,
            }
        );

        let mut probe = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open post-error probe");
        probe
            .acquire_durable_key_leases(vec![key], 2_000)
            .expect("bulk-scan error must release the exclusive plan barrier");
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_key_lease_spans_contention_ledger_creation_and_recovered_link() {
        let anchor = tempfile::tempdir().expect("create durable key lease workspace");
        let plan = make_plan(1);
        let key =
            IdempotencyKey::for_compensation(&plan.plan_id, &plan.steps[0].id, "rollback-complete");
        let outcome = StepOutcome::Compensated {
            original_outcome: Box::new(StepOutcome::Success {
                result: Some("committed".to_string()),
            }),
            compensation_result: "rollback-complete".to_string(),
        };
        let mut first = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open first durable store");
        first
            .create_ledger("txe-key-lease-source", &plan)
            .expect("create source ledger");
        first
            .transition_phase("txe-key-lease-source", TxPhase::Preparing)
            .expect("transition source ledger");
        let mut stale_second = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open stale second store");
        let mut source_reservation = first
            .acquire_durable_reservation("txe-key-lease-source", &key, 1_000)
            .expect("reserve source compensation key");
        first
            .record_execution_reserved(
                &mut source_reservation,
                "txe-key-lease-source",
                key.clone(),
                StepOutcome::Pending,
                StepRisk::High,
                "agent-source",
                1_000,
            )
            .expect("persist source Pending");
        first
            .complete_execution_reserved(
                source_reservation,
                "txe-key-lease-source",
                key.clone(),
                outcome.clone(),
                1_001,
            )
            .expect("persist source compensated outcome");

        let mut leases = first
            .acquire_durable_key_leases(vec![key.clone()], 1_001)
            .expect("acquire proof lease before the recovery ledger exists");
        assert_eq!(
            leases.get(&key).and_then(DurableKeyLease::observed_outcome),
            Some(&outcome)
        );
        let blocked = stale_second
            .acquire_durable_key_leases(vec![key.clone()], 1_001)
            .expect_err("second store must remain blocked by the key-only lease");
        assert!(matches!(
            blocked,
            IdempotencyError::ReservationInProgress { .. }
        ));

        first
            .create_ledger("txe-key-lease-recovery", &plan)
            .expect("create recovery ledger while the proof lease is held");
        first
            .transition_phase("txe-key-lease-recovery", TxPhase::Preparing)
            .expect("transition recovery ledger while the proof lease is held");
        let lease = leases.take(&key).expect("held recovery key lease");
        assert!(lease.authorizes(&key));
        assert!(leases.is_empty());
        let reservation = first
            .bind_durable_key_lease("txe-key-lease-recovery", lease)
            .expect("bind the continuously-held key lease to recovery execution");
        first
            .record_recovered_execution_reserved(
                reservation,
                "txe-key-lease-recovery",
                key.clone(),
                outcome.clone(),
                StepRisk::High,
                "agent-recovery",
                1_001,
            )
            .expect("link recovered proof without reacquiring its key lock");
        assert_eq!(
            first
                .get_ledger("txe-key-lease-recovery")
                .and_then(|ledger| ledger.get_outcome(&key)),
            Some(&outcome)
        );
        drop(leases);

        let refreshed = stale_second
            .acquire_durable_key_leases(vec![key.clone()], 1_001)
            .expect("second store acquires after recovered publication releases the lease");
        assert_eq!(
            refreshed
                .get(&key)
                .and_then(DurableKeyLease::observed_outcome),
            Some(&outcome)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn batch_logical_lease_rejects_reopened_store_binding_and_releases_barrier() {
        let anchor = tempfile::tempdir().expect("create durable key lease workspace");
        let key = IdempotencyKey::new("test-plan", "step-a", "commit-a");
        let mut owner = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open lease-owning store");
        let mut reopened = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open distinct store instance over same durable path");
        let mut probe = IdempotencyStore::open(anchor.path(), IdempotencyPolicy::default())
            .expect("open post-error probe");
        let mut leases = owner
            .acquire_durable_key_leases(vec![key.clone()], 1_000)
            .expect("acquire batch logical lease");
        let lease = leases.take(&key).expect("take logical lease for binding");

        let error = reopened
            .bind_durable_key_lease("txe-foreign-store", lease)
            .expect_err("a lease is bound to the exact pinned store instance that acquired it");
        assert!(matches!(
            error,
            IdempotencyError::ReservationStoreMismatch { .. }
        ));
        let blocked = probe
            .acquire_durable_key_leases(vec![key.clone()], 1_000)
            .expect_err("lease set still owns the batch barrier after a bind error");
        assert!(matches!(
            blocked,
            IdempotencyError::ReservationInProgress { ref key }
                if key == "plan:test-plan"
        ));
        drop(leases);

        probe
            .acquire_durable_key_leases(vec![key], 1_000)
            .expect("dropping the lease set releases its plan barrier after bind failure");
    }

    #[cfg(not(windows))]
    #[test]
    fn durable_key_reservation_refreshes_a_stale_second_store_under_lock() {
        let ft_dir = durable_test_dir("cross-process-reservation");
        let plan = make_plan(1);
        let key = make_key("test-plan", &plan.steps[0].id);
        let outcome = StepOutcome::Success {
            result: Some("committed-once".to_string()),
        };
        let mut first = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("open first process store");
        let mut stale_second = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("open stale second process store");
        first
            .create_ledger("txe-first-process", &plan)
            .expect("create first ledger");
        first
            .transition_phase("txe-first-process", TxPhase::Preparing)
            .expect("transition first ledger");

        let mut reservation = first
            .acquire_durable_reservation("txe-first-process", &key, 1_000)
            .expect("first process acquires key");
        assert!(reservation.observed_outcome().is_none());
        let blocked = stale_second
            .acquire_durable_reservation("txe-first-process", &key, 1_000)
            .expect_err("second process must not acquire a live key lease");
        assert!(matches!(
            blocked,
            IdempotencyError::ReservationInProgress { .. }
        ));
        first
            .record_execution_reserved(
                &mut reservation,
                "txe-first-process",
                key.clone(),
                StepOutcome::Pending,
                StepRisk::Low,
                "agent",
                1_000,
            )
            .expect("durably reserve before dispatch");
        first
            .complete_execution_reserved(
                reservation,
                "txe-first-process",
                key.clone(),
                outcome.clone(),
                1_001,
            )
            .expect("durably complete while lock is held");
        let refreshed = stale_second
            .acquire_durable_reservation("txe-first-process", &key, 1_001)
            .expect("stale process acquires after first completes");
        assert_eq!(refreshed.observed_outcome(), Some(&outcome));
        drop(refreshed);

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn open_fails_closed_when_valid_named_ledger_cannot_be_read() {
        let ft_dir = durable_test_dir("unreadable-ledger");
        let unreadable_path = ft_dir.join("tx_ledgers").join("txe-unreadable.json");
        std::fs::create_dir_all(&unreadable_path).expect("create directory at ledger path");

        let err = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect_err("a valid-named unreadable ledger must block startup");
        let reason = match err {
            IdempotencyError::LedgerPersist { reason } => reason,
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(
            reason.contains("read ledger"),
            "unexpected reason: {reason}"
        );

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn open_fails_closed_on_malformed_valid_named_ledger_with_no_active_capacity() {
        let ft_dir = durable_test_dir("malformed-ledger");
        let spool = ft_dir.join("tx_ledgers");
        std::fs::create_dir_all(&spool).expect("create ledger spool");
        std::fs::write(spool.join("txe-malformed.json"), b"{not-json")
            .expect("write malformed ledger fixture");
        let policy = IdempotencyPolicy {
            // Terminal ledgers would consume no active capacity, but every
            // valid-named spool file must still be integrity-checked.
            max_active_ledgers: 1,
            ..IdempotencyPolicy::default()
        };

        let err = IdempotencyStore::open(&ft_dir, policy)
            .expect_err("a malformed valid-named ledger must block startup");
        let reason = match err {
            IdempotencyError::LedgerPersist { reason } => reason,
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(
            reason.contains("deserialize ledger"),
            "unexpected reason: {reason}"
        );

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn open_fails_closed_on_broken_persisted_hash_chain() {
        let ft_dir = durable_test_dir("broken-chain");
        let plan = make_plan(2);
        {
            let mut store =
                IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
            store
                .create_ledger("txe-chain", &plan)
                .expect("create ledger");
            store
                .transition_phase("txe-chain", TxPhase::Preparing)
                .expect("persist preparing phase");
            for (index, step) in plan.steps.iter().enumerate() {
                record_durable_outcome(
                    &mut store,
                    "txe-chain",
                    make_key("test-plan", &step.id),
                    StepOutcome::Success { result: None },
                    StepRisk::Low,
                    "agent-original",
                    1_000 + u64::try_from(index).expect("step index fits in u64"),
                );
            }
        }

        let path = ft_dir.join("tx_ledgers").join("txe-chain.json");
        let bytes = std::fs::read(&path).expect("read persisted ledger");
        let mut persisted: serde_json::Value =
            serde_json::from_slice(&bytes).expect("parse persisted ledger fixture");
        persisted["records"][0]["agent_id"] =
            serde_json::Value::String("agent-tampered".to_string());
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&persisted).expect("serialize tampered fixture"),
        )
        .expect("write tampered ledger fixture");

        let err = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect_err("a broken persisted hash chain must block startup");
        let reason = match err {
            IdempotencyError::LedgerPersist { reason } => reason,
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(
            reason.contains("deserialize ledger") || reason.contains("verify ledger hash chain"),
            "unexpected reason: {reason}"
        );

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn terminal_phase_remains_durable_but_is_not_active_after_reload() {
        let ft_dir = durable_test_dir("terminal-phase");
        let plan = make_plan(1);
        {
            let mut store =
                IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
            store
                .create_ledger("txe-terminal", &plan)
                .expect("create ledger");
            assert_eq!(
                store
                    .transition_phase("txe-terminal", TxPhase::Preparing)
                    .expect("persist preparing phase"),
                TxPhase::Planned
            );
            assert_eq!(
                store
                    .transition_phase("txe-terminal", TxPhase::Committing)
                    .expect("persist committing phase"),
                TxPhase::Preparing
            );
            assert_eq!(
                store
                    .transition_phase("txe-terminal", TxPhase::Completed)
                    .expect("persist terminal phase"),
                TxPhase::Committing
            );
        }

        let spool_path = ft_dir.join("tx_ledgers").join("txe-terminal.json");
        assert!(spool_path.is_file(), "terminal ledger remains durable");
        let persisted: TxExecutionLedger =
            serde_json::from_slice(&std::fs::read(&spool_path).expect("read terminal ledger"))
                .expect("deserialize terminal ledger");
        assert_eq!(persisted.phase(), TxPhase::Completed);

        let reopened = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("reopen durable store");
        assert_eq!(reopened.active_count(), 0);
        assert!(reopened.get_ledger("txe-terminal").is_none());

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn store_transition_phase_persist_failure_keeps_previous_phase() {
        let ft_dir = durable_test_dir("phase-write-failure");
        let plan = make_plan(1);
        let mut store =
            IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
        store
            .create_ledger("txe-write-failure", &plan)
            .expect("create ledger");
        store.fail_persist_writes = true;

        let err = store
            .transition_phase("txe-write-failure", TxPhase::Preparing)
            .expect_err("phase transition must fail when its snapshot cannot be written");
        assert!(matches!(err, IdempotencyError::LedgerPersist { .. }));
        assert_eq!(
            store
                .get_ledger("txe-write-failure")
                .expect("ledger remains tracked")
                .phase(),
            TxPhase::Planned,
            "a failed copy-on-write publish must not expose the candidate phase"
        );

        store.fail_persist_writes = false;
        drop(store);
        let reopened = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("reopen durable store");
        assert_eq!(
            reopened
                .get_ledger("txe-write-failure")
                .expect("reloaded ledger")
                .phase(),
            TxPhase::Planned
        );

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn record_persist_failure_keeps_ledger_and_dedup_unchanged() {
        let ft_dir = durable_test_dir("record-write-failure");
        let plan = make_plan(1);
        let key = make_key("test-plan", &plan.steps[0].id);
        let mut store =
            IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
        store
            .create_ledger("txe-record-failure", &plan)
            .expect("create ledger");
        store
            .transition_phase("txe-record-failure", TxPhase::Preparing)
            .expect("transition ledger");
        let mut reservation = store
            .acquire_durable_reservation("txe-record-failure", &key, 1_000)
            .expect("acquire reservation before persistence fault");
        store.fail_persist_writes = true;

        let error = store
            .record_execution_reserved(
                &mut reservation,
                "txe-record-failure",
                key.clone(),
                StepOutcome::Pending,
                StepRisk::Low,
                "agent",
                1_000,
            )
            .expect_err("record must fail when candidate cannot persist");
        assert!(matches!(error, IdempotencyError::LedgerPersist { .. }));
        assert_eq!(
            store
                .get_ledger("txe-record-failure")
                .expect("ledger remains active")
                .record_count(),
            0
        );
        assert!(
            store
                .peek_cached_outcome(&key, store.logical_clock_ms())
                .is_none()
        );

        store.fail_persist_writes = false;
        drop(reservation);
        drop(store);
        let reopened = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("reopen durable store");
        assert_eq!(
            reopened
                .get_ledger("txe-record-failure")
                .expect("reloaded ledger")
                .record_count(),
            0
        );

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn completion_persist_failure_preserves_pending_in_memory_and_on_disk() {
        let ft_dir = durable_test_dir("completion-write-failure");
        let plan = make_plan(1);
        let key = make_key("test-plan", &plan.steps[0].id);
        let mut store =
            IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
        store
            .create_ledger("txe-completion-failure", &plan)
            .expect("create ledger");
        store
            .transition_phase("txe-completion-failure", TxPhase::Preparing)
            .expect("transition ledger");
        let mut reservation = store
            .acquire_durable_reservation("txe-completion-failure", &key, 1_000)
            .expect("acquire durable reservation");
        store
            .record_execution_reserved(
                &mut reservation,
                "txe-completion-failure",
                key.clone(),
                StepOutcome::Pending,
                StepRisk::Low,
                "agent",
                1_000,
            )
            .expect("persist pending reservation");
        store.fail_persist_writes = true;

        let error = store
            .complete_execution_reserved(
                reservation,
                "txe-completion-failure",
                key.clone(),
                StepOutcome::Success { result: None },
                1_001,
            )
            .expect_err("completion must fail when candidate cannot persist");
        assert!(matches!(error, IdempotencyError::LedgerPersist { .. }));
        assert_eq!(
            store.peek_cached_outcome(&key, store.logical_clock_ms()),
            Some(&StepOutcome::Pending)
        );
        assert_eq!(
            store
                .get_ledger("txe-completion-failure")
                .and_then(|ledger| ledger.get_outcome(&key)),
            Some(&StepOutcome::Pending)
        );

        store.fail_persist_writes = false;
        drop(store);
        let reopened = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("reopen durable store");
        assert_eq!(
            reopened.peek_cached_outcome(&key, reopened.logical_clock_ms()),
            Some(&StepOutcome::Pending)
        );

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[test]
    fn in_memory_store_does_not_persist() {
        // new() (no dir) keeps legacy behavior: persist_ledger is a no-op and
        // never errors.
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(1);
        store
            .create_ledger("txe-2000", &plan)
            .expect("create ledger");
        store
            .transition_phase("txe-2000", TxPhase::Preparing)
            .expect("transition ledger");
        store
            .record_execution(
                "txe-2000",
                make_key("test-plan", "step-b0"),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent",
                2000,
            )
            .expect("record execution must not fail in-memory");
    }

    #[test]
    fn execution_id_filename_guard_rejects_traversal() {
        assert!(is_valid_execution_id("txe-1700000000000"));
        assert!(is_valid_execution_id("txe-1_abc.def"));
        assert!(!is_valid_execution_id(""));
        assert!(!is_valid_execution_id("."));
        assert!(!is_valid_execution_id(".."));
        assert!(!is_valid_execution_id("../etc/passwd"));
        assert!(!is_valid_execution_id("a/b"));
        assert!(!is_valid_execution_id("txe-../x"));
    }

    #[test]
    fn store_resume_context() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();

        let ctx = store.resume_context("exec-1", &plan).unwrap();
        assert_eq!(ctx.remaining_steps.len(), 2);
        assert_eq!(
            ctx.recommendation,
            ResumeRecommendation::ContinueFromCheckpoint
        );
    }

    #[test]
    fn store_archive_terminal() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(1);
        store.create_ledger("exec-1", &plan).unwrap();

        {
            let ledger = store.get_ledger_mut("exec-1").unwrap();
            ledger.transition_phase(TxPhase::Preparing).unwrap();
            ledger.transition_phase(TxPhase::Committing).unwrap();
            ledger.transition_phase(TxPhase::Completed).unwrap();
        }

        let archived = store.archive_ledger("exec-1").unwrap();
        assert_eq!(archived.phase(), TxPhase::Completed);
        assert_eq!(store.active_count(), 0);
    }

    #[cfg(not(windows))]
    #[test]
    fn abort_and_archive_matching_ledgers_is_plan_hash_scoped_and_durable() {
        let ft_dir = durable_test_dir("retire-matching");
        let matching_plan = make_plan(1);
        let different_plan = make_plan(2);
        let matching_id = "txe-matching";
        let different_id = "txe-different-hash";
        {
            let mut store =
                IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
            store
                .create_ledger(matching_id, &matching_plan)
                .expect("create matching ledger");
            store
                .transition_phase(matching_id, TxPhase::Preparing)
                .expect("transition matching ledger");
            store
                .transition_phase(matching_id, TxPhase::Committing)
                .expect("transition matching ledger to committing");
            store
                .create_ledger(different_id, &different_plan)
                .expect("create different-hash ledger");

            let retired = store
                .abort_and_archive_matching_ledgers(&matching_plan.plan_id, matching_plan.plan_hash)
                .expect("retire matching execution");
            assert_eq!(retired, vec![matching_id.to_string()]);
            assert!(store.get_ledger(matching_id).is_none());
            assert!(store.get_ledger(different_id).is_some());
        }

        let matching_path = ft_dir
            .join("tx_ledgers")
            .join(format!("{matching_id}.json"));
        let matching: TxExecutionLedger =
            serde_json::from_slice(&std::fs::read(&matching_path).expect("read retired ledger"))
                .expect("deserialize retired ledger");
        assert_eq!(matching.phase(), TxPhase::Aborted);

        let reopened = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default())
            .expect("reopen durable store");
        assert!(reopened.get_ledger(matching_id).is_none());
        assert!(reopened.get_ledger(different_id).is_some());

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn abort_and_archive_persist_failure_does_not_mutate_active_map() {
        let ft_dir = durable_test_dir("retire-write-failure");
        let plan = make_plan(1);
        let mut store =
            IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).expect("open store");
        store
            .create_ledger("txe-retire-failure", &plan)
            .expect("create ledger");
        store
            .transition_phase("txe-retire-failure", TxPhase::Preparing)
            .expect("transition ledger");
        store.fail_persist_writes = true;

        let error = store
            .abort_and_archive_matching_ledgers(&plan.plan_id, plan.plan_hash)
            .expect_err("retirement must fail when terminal snapshot cannot persist");
        assert!(matches!(error, IdempotencyError::LedgerPersist { .. }));
        assert_eq!(store.active_count(), 1);
        assert_eq!(
            store
                .get_ledger("txe-retire-failure")
                .expect("ledger remains active")
                .phase(),
            TxPhase::Preparing
        );

        store.fail_persist_writes = false;
        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[test]
    fn store_archived_terminal_ledger_keeps_sticky_replay_dedup() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy {
            dedup_ttl_ms: 10_000,
            ..IdempotencyPolicy::default()
        });
        let plan = make_plan(1);
        store.create_ledger("exec-1", &plan).unwrap();
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(TxPhase::Preparing)
            .unwrap();

        let key = make_key("test-plan", &plan.steps[0].id);
        let outcome = StepOutcome::Success {
            result: Some("archived".to_string()),
        };
        store
            .record_execution(
                "exec-1",
                key.clone(),
                outcome.clone(),
                StepRisk::Low,
                "agent-a",
                1_000,
            )
            .unwrap();
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(TxPhase::Committing)
            .unwrap();
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(TxPhase::Completed)
            .unwrap();

        let archived = store.archive_ledger("exec-1").unwrap();

        assert_eq!(archived.phase(), TxPhase::Completed);
        assert_eq!(store.active_count(), 0);
        assert_eq!(
            store.peek_cached_outcome(&key, store.logical_clock_ms()),
            Some(&outcome)
        );
    }

    #[test]
    fn store_archive_non_terminal_rejected() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(1);
        store.create_ledger("exec-1", &plan).unwrap();

        let err = store.archive_ledger("exec-1").unwrap_err();
        assert!(matches!(err, IdempotencyError::LedgerNotTerminal { .. }));
    }

    #[test]
    fn store_ledger_not_found() {
        let store = IdempotencyStore::new(IdempotencyPolicy::default());
        assert!(store.get_ledger("nonexistent").is_none());
    }

    #[test]
    fn store_record_not_found_ledger() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let key = make_key("p1", "s1");
        let err = store
            .record_execution(
                "nonexistent",
                key,
                StepOutcome::Pending,
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap_err();
        assert!(matches!(err, IdempotencyError::LedgerNotFound { .. }));
    }

    #[test]
    fn store_evict_stale() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(TxPhase::Preparing)
            .unwrap();

        let key = make_key("test-plan", "step-b0");
        store
            .record_execution(
                "exec-1",
                key.clone(),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        // Dedup entry exists.
        assert!(
            store
                .peek_cached_outcome(&key, store.logical_clock_ms())
                .is_some()
        );

        // Evict entries older than 2000.
        store.evict_stale(2000);

        // A successful external effect is sticky replay proof and is not
        // evicted by TTL; active-ledger lookup independently retains it too.
        assert!(
            store
                .peek_cached_outcome(&key, store.logical_clock_ms())
                .is_some()
        );
    }

    #[test]
    fn sticky_replay_proof_does_not_expire_or_erase_resume_evidence() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy {
            dedup_ttl_ms: 100,
            ..IdempotencyPolicy::default()
        });
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();
        store.create_ledger("exec-2", &plan).unwrap();
        for execution_id in ["exec-1", "exec-2"] {
            store
                .get_ledger_mut(execution_id)
                .unwrap()
                .transition_phase(TxPhase::Preparing)
                .unwrap();
        }

        let old_key = make_key("test-plan", &plan.steps[0].id);
        store
            .record_execution(
                "exec-1",
                old_key.clone(),
                StepOutcome::Success {
                    result: Some("old".to_string()),
                },
                StepRisk::Low,
                "agent-a",
                1_000,
            )
            .unwrap();

        let newer_key = make_key("test-plan", &plan.steps[1].id);
        store
            .record_execution(
                "exec-2",
                newer_key.clone(),
                StepOutcome::Success {
                    result: Some("newer".to_string()),
                },
                StepRisk::Low,
                "agent-b",
                1_201,
            )
            .unwrap();

        assert!(
            matches!(
                store.peek_cached_outcome(&old_key, store.logical_clock_ms()),
                Some(StepOutcome::Success { result: Some(result) }) if result == "old"
            ),
            "durable success proof must not age into permission to redispatch"
        );
        assert!(
            store
                .peek_cached_outcome(&newer_key, store.logical_clock_ms())
                .is_some()
        );

        let ctx = store.resume_context("exec-1", &plan).unwrap();
        assert_eq!(
            ctx.recommendation,
            ResumeRecommendation::ContinueFromCheckpoint
        );
        assert_eq!(ctx.completed_steps, vec![plan.steps[0].id.clone()]);
        assert_eq!(ctx.remaining_steps, vec![plan.steps[1].id.clone()]);
    }

    #[test]
    fn policy_default() {
        let p = IdempotencyPolicy::default();
        assert_eq!(p.dedup_capacity, 10_000);
        assert!(p.skip_completed_on_resume);
        assert_eq!(p.dedup_ttl_ms, 3_600_000);
        assert!(p.require_chain_integrity);
        assert_eq!(p.max_active_ledgers, 100);
    }

    #[test]
    fn policy_serde_roundtrip() {
        let p = IdempotencyPolicy {
            dedup_capacity: 500,
            skip_completed_on_resume: false,
            dedup_ttl_ms: 60_000,
            require_chain_integrity: false,
            max_active_ledgers: 10,
            max_spool_files: 50,
            max_spool_total_bytes: 1024 * 1024,
            max_spool_records: 500,
            max_ledger_bytes: 64 * 1024,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: IdempotencyPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dedup_capacity, 500);
        assert!(!back.skip_completed_on_resume);
        assert_eq!(back.max_spool_files, 50);
        assert_eq!(back.max_spool_total_bytes, 1024 * 1024);
        assert_eq!(back.max_spool_records, 500);
        assert_eq!(back.max_ledger_bytes, 64 * 1024);
    }

    proptest! {
        #[test]
        fn policy_max_active_ledgers_is_enforced_ft_u3s72(
            max_active_ledgers in 0usize..8,
            extra_attempts in 1usize..4,
        ) {
            let policy = IdempotencyPolicy {
                max_active_ledgers,
                ..IdempotencyPolicy::default()
            };
            let mut store = IdempotencyStore::new(policy);
            let plan = make_plan(1);

            for idx in 0..max_active_ledgers {
                store
                    .create_ledger(&format!("exec-{idx}"), &plan)
                    .expect("ledger below active limit should be accepted");
            }

            for idx in max_active_ledgers..max_active_ledgers + extra_attempts {
                let result = store.create_ledger(&format!("exec-{idx}"), &plan);
                let observed_limit = match result {
                    Err(IdempotencyError::ActiveLedgerLimitExceeded {
                        max_active_ledgers: observed
                    }) => Some(observed),
                    _ => None,
                };
                prop_assert_eq!(observed_limit, Some(max_active_ledgers));
                prop_assert_eq!(store.active_count(), max_active_ledgers);
            }
        }
    }

    #[test]
    fn policy_dedup_ttl_expires_retryable_failures_ft_u3s72() {
        let policy = IdempotencyPolicy {
            dedup_ttl_ms: 10,
            ..IdempotencyPolicy::default()
        };
        let mut store = IdempotencyStore::new(policy);
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();

        let old_key = IdempotencyKey::new("test-plan", &plan.steps[0].id, "old");
        store
            .record_execution(
                "exec-1",
                old_key.clone(),
                StepOutcome::Failed {
                    error_code: "retryable".to_string(),
                    error_message: "retry after ttl".to_string(),
                    compensated: false,
                },
                StepRisk::Low,
                "agent-a",
                100,
            )
            .unwrap();
        assert!(
            store
                .peek_cached_outcome(&old_key, store.logical_clock_ms())
                .is_some()
        );

        let fresh_key = IdempotencyKey::new("test-plan", &plan.steps[1].id, "fresh");
        store
            .record_execution(
                "exec-1",
                fresh_key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-a",
                111,
            )
            .unwrap();

        assert!(
            store
                .peek_cached_outcome(&old_key, store.logical_clock_ms())
                .is_none(),
            "dedup_ttl_ms must expire retryable failures in active and global lookup"
        );
    }

    #[test]
    fn store_rejects_retrograde_timestamp_before_mutating_ledger() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();
        store.create_ledger("exec-2", &plan).unwrap();
        let first_key = IdempotencyKey::new("test-plan", &plan.steps[0].id, "first");
        store
            .record_execution(
                "exec-1",
                first_key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent",
                100,
            )
            .unwrap();
        let retrograde_key = IdempotencyKey::new("test-plan", &plan.steps[1].id, "retrograde");

        let error = store
            .record_execution(
                "exec-2",
                retrograde_key.clone(),
                StepOutcome::Pending,
                StepRisk::Low,
                "agent",
                99,
            )
            .expect_err("retrograde timestamp must fail closed");
        assert_eq!(
            error,
            IdempotencyError::RetrogradeTimestamp {
                observed_ms: 99,
                high_water_ms: 100
            }
        );
        assert_eq!(store.get_ledger("exec-2").unwrap().record_count(), 0);
        assert!(
            store
                .peek_cached_outcome(&retrograde_key, store.logical_clock_ms())
                .is_none()
        );
    }

    #[test]
    fn policy_resume_can_reexecute_completed_steps_ft_u3s72() {
        let policy = IdempotencyPolicy {
            skip_completed_on_resume: false,
            ..IdempotencyPolicy::default()
        };
        let mut store = IdempotencyStore::new(policy);
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();
        let first_step = plan.steps[0].id.clone();
        let key = IdempotencyKey::new("test-plan", &first_step, "act");
        store
            .record_execution(
                "exec-1",
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-a",
                100,
            )
            .unwrap();

        let ctx = store.resume_context("exec-1", &plan).unwrap();
        assert!(ctx.completed_steps.contains(&first_step));
        assert!(
            ctx.remaining_steps.contains(&first_step),
            "skip_completed_on_resume=false should make completed steps eligible for re-execution"
        );
    }

    #[test]
    fn policy_can_disable_chain_integrity_resume_restart_ft_u3s72() {
        let policy = IdempotencyPolicy {
            require_chain_integrity: false,
            ..IdempotencyPolicy::default()
        };
        let mut store = IdempotencyStore::new(policy);
        let plan = make_plan(2);
        store.create_ledger("exec-1", &plan).unwrap();
        let key = IdempotencyKey::new("test-plan", &plan.steps[0].id, "act");
        store
            .record_execution(
                "exec-1",
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-a",
                100,
            )
            .unwrap();
        store.get_ledger_mut("exec-1").unwrap().records[0].agent_id = "tampered".into();

        let ctx = store.resume_context("exec-1", &plan).unwrap();
        assert!(!ctx.chain_intact);
        assert_ne!(ctx.recommendation, ResumeRecommendation::RestartFresh);
    }

    // ── Error tests ──

    #[test]
    fn error_display() {
        let err = IdempotencyError::DuplicateExecution {
            key: "txk:abc".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("txk:abc"));
    }

    #[test]
    fn error_serde_roundtrip() {
        let errors = vec![
            IdempotencyError::DuplicateExecution { key: "k1".into() },
            IdempotencyError::InvalidPhaseTransition {
                from: TxPhase::Planned,
                to: TxPhase::Completed,
            },
            IdempotencyError::LedgerSealed {
                phase: TxPhase::Completed,
            },
            IdempotencyError::LedgerNotFound {
                execution_id: "e1".into(),
            },
            IdempotencyError::LedgerNotTerminal {
                execution_id: "e1".into(),
                phase: TxPhase::Committing,
            },
            IdempotencyError::LedgerIndexCorrupt {
                reason: "duplicate key".into(),
            },
            IdempotencyError::ChainIntegrityViolation { ordinal: 5 },
        ];
        for e in &errors {
            let json = serde_json::to_string(e).unwrap();
            let back: IdempotencyError = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, e);
        }
    }

    // ── StepExecutionRecord tests ──

    #[test]
    fn record_hash_deterministic() {
        let record = StepExecutionRecord {
            ordinal: 0,
            idem_key: make_key("p1", "s1"),
            execution_id: "exec-1".to_string(),
            timestamp_ms: 1000,
            outcome: StepOutcome::Success { result: None },
            risk: StepRisk::Low,
            prev_hash: String::new(),
            agent_id: "a1".to_string(),
        };
        let h1 = record.hash();
        let h2 = record.hash();
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn record_hash_changes_with_ordinal() {
        let make = |ordinal| StepExecutionRecord {
            ordinal,
            idem_key: make_key("p1", "s1"),
            execution_id: "exec-1".to_string(),
            timestamp_ms: 1000,
            outcome: StepOutcome::Success { result: None },
            risk: StepRisk::Low,
            prev_hash: String::new(),
            agent_id: "a1".to_string(),
        };
        assert_ne!(make(0).hash(), make(1).hash());
    }

    // br-ft-6niwr: chain-tamper-detection tests. Pre-fix the
    // canonical form omitted `risk` and `agent_id`, so a
    // post-write mutation of those fields was invisible to
    // `verify_chain`. Post-fix the hash includes both, so any
    // mutation of either breaks the chain at exactly the
    // tampered ordinal.

    #[test]
    fn record_hash_changes_with_risk() {
        let make = |risk| StepExecutionRecord {
            ordinal: 0,
            idem_key: make_key("p1", "s1"),
            execution_id: "exec-1".to_string(),
            timestamp_ms: 1000,
            outcome: StepOutcome::Success { result: None },
            risk,
            prev_hash: String::new(),
            agent_id: "a1".to_string(),
        };
        assert_ne!(make(StepRisk::Low).hash(), make(StepRisk::Critical).hash());
        assert_ne!(make(StepRisk::Low).hash(), make(StepRisk::Medium).hash());
        assert_ne!(make(StepRisk::Medium).hash(), make(StepRisk::High).hash());
    }

    #[test]
    fn record_hash_changes_with_agent_id() {
        let make = |agent: &str| StepExecutionRecord {
            ordinal: 0,
            idem_key: make_key("p1", "s1"),
            execution_id: "exec-1".to_string(),
            timestamp_ms: 1000,
            outcome: StepOutcome::Success { result: None },
            risk: StepRisk::Low,
            prev_hash: String::new(),
            agent_id: agent.to_string(),
        };
        assert_ne!(make("agent-A").hash(), make("agent-B").hash());
        assert_ne!(make("").hash(), make("agent-A").hash());
    }

    /// br-ft-6niwr: tampering `risk` from Low → Critical on a
    /// persisted record post-write must be detected by
    /// `verify_chain` (chain_intact = false).
    #[test]
    fn verify_chain_detects_risk_tamper() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger
            .append(
                make_key("plan-1", "s1"),
                StepOutcome::Success { result: None },
                StepRisk::Critical,
                "agent-A",
                1000,
            )
            .unwrap();
        ledger
            .append(
                make_key("plan-1", "s2"),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-A",
                2000,
            )
            .unwrap();

        // Pre-tamper: chain intact.
        assert!(ledger.verify_chain().chain_intact);

        // Tamper: simulate persisted-record mutation by
        // serializing, mutating the JSON, and deserializing.
        let json = serde_json::to_string(&ledger).unwrap();
        let mutated = json.replace(r#""risk":"critical""#, r#""risk":"low""#);
        let mut tampered: TxExecutionLedger = serde_json::from_str(&mutated).unwrap();
        tampered.rebuild_index();
        let result = tampered.verify_chain();
        assert!(
            !result.chain_intact,
            "verify_chain must detect risk tamper from Critical→Low; got {result:?}"
        );
    }

    /// br-ft-6niwr: tampering `agent_id` (executor identity)
    /// post-write must be detected by `verify_chain`. This is
    /// the "who ran this" forensic field — operators MUST be
    /// able to trust it.
    #[test]
    fn verify_chain_detects_agent_id_tamper() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger
            .append(
                make_key("plan-1", "s1"),
                StepOutcome::Success { result: None },
                StepRisk::High,
                "agent-original",
                1000,
            )
            .unwrap();
        ledger
            .append(
                make_key("plan-1", "s2"),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-original",
                2000,
            )
            .unwrap();

        assert!(ledger.verify_chain().chain_intact);

        let json = serde_json::to_string(&ledger).unwrap();
        let mutated = json.replacen("agent-original", "agent-impostor", 1);
        let mut tampered: TxExecutionLedger = serde_json::from_str(&mutated).unwrap();
        tampered.rebuild_index();
        let result = tampered.verify_chain();
        assert!(
            !result.chain_intact,
            "verify_chain must detect agent_id tamper; got {result:?}"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// br-ft-6niwr: any change to risk or agent_id flips
        /// the hash — universal quantifier over the
        /// risk/agent_id × hash-input contract.
        #[test]
        fn record_hash_strictly_depends_on_risk_and_agent(
            risk_a in 0u8..4,
            risk_b in 0u8..4,
            agent_a in "[a-zA-Z0-9_-]{1,16}",
            agent_b in "[a-zA-Z0-9_-]{1,16}",
        ) {
            fn lift(r: u8) -> StepRisk {
                match r {
                    0 => StepRisk::Low,
                    1 => StepRisk::Medium,
                    2 => StepRisk::High,
                    _ => StepRisk::Critical,
                }
            }
            let a = StepExecutionRecord {
                ordinal: 0,
                idem_key: make_key("p", "s"),
                execution_id: "e".to_string(),
                timestamp_ms: 1000,
                outcome: StepOutcome::Success { result: None },
                risk: lift(risk_a),
                prev_hash: String::new(),
                agent_id: agent_a.clone(),
            };
            let b = StepExecutionRecord {
                ordinal: 0,
                idem_key: make_key("p", "s"),
                execution_id: "e".to_string(),
                timestamp_ms: 1000,
                outcome: StepOutcome::Success { result: None },
                risk: lift(risk_b),
                prev_hash: String::new(),
                agent_id: agent_b.clone(),
            };
            // Records are equal iff (risk, agent_id) match. If
            // either differs, the hash MUST differ.
            if a.risk == b.risk && a.agent_id == b.agent_id {
                prop_assert_eq!(a.hash(), b.hash());
            } else {
                prop_assert_ne!(
                    a.hash(),
                    b.hash(),
                    "hash must differentiate risk={:?}/{:?} agent={:?}/{:?}",
                    a.risk, b.risk, a.agent_id, b.agent_id
                );
            }
        }
    }

    #[test]
    fn record_serde_roundtrip() {
        let record = StepExecutionRecord {
            ordinal: 42,
            idem_key: make_key("p1", "s1"),
            execution_id: "exec-1".to_string(),
            timestamp_ms: 99999,
            outcome: StepOutcome::Failed {
                error_code: "E1".into(),
                error_message: "fail".into(),
                compensated: true,
            },
            risk: StepRisk::Critical,
            prev_hash: "abcdef".to_string(),
            agent_id: "agent-x".to_string(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: StepExecutionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ordinal, 42);
        assert_eq!(back.risk, StepRisk::Critical);
        assert_eq!(back.agent_id, "agent-x");
    }

    // ── SHA-256 canonical hash tests ──

    #[test]
    fn sha256_domain_digest_is_deterministic() {
        assert_eq!(
            sha256_domain_digest(b"domain", &[b"hello"]),
            sha256_domain_digest(b"domain", &[b"hello"])
        );
    }

    #[test]
    fn sha256_domain_digest_separates_domains_and_inputs() {
        assert_ne!(
            sha256_domain_digest(b"domain-a", &[b"hello"]),
            sha256_domain_digest(b"domain-b", &[b"hello"])
        );
        assert_ne!(
            sha256_domain_digest(b"domain", &[b"hello"]),
            sha256_domain_digest(b"domain", &[b"world"])
        );
    }

    #[test]
    fn sha256_domain_digest_length_delimits_components() {
        assert_ne!(
            sha256_domain_digest(b"domain", &[b"a|b", b"c", b"d"]),
            sha256_domain_digest(b"domain", &[b"a", b"b", b"c|d"])
        );
    }

    // ── Integration: full tx lifecycle ──

    #[test]
    fn full_tx_lifecycle() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(3);
        store.create_ledger("exec-1", &plan).unwrap();

        // Prepare phase.
        let ledger = store.get_ledger_mut("exec-1").unwrap();
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        // Execute all steps.
        for step in &plan.steps {
            let key = IdempotencyKey::new("test-plan", &step.id, &step.description);

            // Check dedup (should be None first time).
            assert!(
                store
                    .peek_cached_outcome(&key, store.logical_clock_ms())
                    .is_none()
            );

            store
                .record_execution(
                    "exec-1",
                    key.clone(),
                    StepOutcome::Success {
                        result: Some(format!("{} done", step.id)),
                    },
                    step.risk,
                    &step.agent_id,
                    1000,
                )
                .unwrap();

            // Dedup should now hit.
            assert!(
                store
                    .peek_cached_outcome(&key, store.logical_clock_ms())
                    .is_some()
            );
        }

        // Complete.
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(TxPhase::Completed)
            .unwrap();

        // Verify chain.
        let verification = store.get_ledger("exec-1").unwrap().verify_chain();
        assert!(verification.chain_intact);

        // Resume context should say "already complete".
        let ctx = store.resume_context("exec-1", &plan).unwrap();
        assert_eq!(ctx.recommendation, ResumeRecommendation::AlreadyComplete);

        // Archive.
        let archived = store.archive_ledger("exec-1").unwrap();
        assert_eq!(archived.record_count(), 3);
        assert_eq!(store.active_count(), 0);
    }

    #[test]
    fn partial_failure_and_resume() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(3);
        store.create_ledger("exec-1", &plan).unwrap();

        let ledger = store.get_ledger_mut("exec-1").unwrap();
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        // First step succeeds.
        let k1 = IdempotencyKey::new("test-plan", &plan.steps[0].id, "act");
        store
            .record_execution(
                "exec-1",
                k1,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        // Second step fails.
        let k2 = IdempotencyKey::new("test-plan", &plan.steps[1].id, "act");
        store
            .record_execution(
                "exec-1",
                k2,
                StepOutcome::Failed {
                    error_code: "E1".into(),
                    error_message: "timeout".into(),
                    compensated: false,
                },
                StepRisk::Medium,
                "a",
                2000,
            )
            .unwrap();

        // Resume context: recorded commit failures must compensate and abort,
        // even if the crash happened before the phase transition was persisted.
        let ctx = store.resume_context("exec-1", &plan).unwrap();
        assert_eq!(ctx.recommendation, ResumeRecommendation::CompensateAndAbort);
        assert_eq!(ctx.completed_steps.len(), 1);
        assert_eq!(ctx.failed_steps.len(), 1);
        assert!(ctx.remaining_steps.is_empty());
    }

    #[test]
    fn cross_instance_dedup() {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(1);

        // First execution.
        store.create_ledger("exec-1", &plan).unwrap();
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(TxPhase::Preparing)
            .unwrap();

        let key = IdempotencyKey::new("test-plan", "step-b0", "act");
        store
            .record_execution(
                "exec-1",
                key.clone(),
                StepOutcome::Success {
                    result: Some("done".into()),
                },
                StepRisk::Low,
                "a",
                1000,
            )
            .unwrap();

        // Second execution (replay). The key should dedup across instances.
        store.create_ledger("exec-2", &plan).unwrap();
        let dedup = store.peek_cached_outcome(&key, store.logical_clock_ms());
        assert!(dedup.is_some());
        assert!(matches!(dedup.unwrap(), StepOutcome::Success { .. }));
    }

    #[cfg(not(windows))]
    #[test]
    fn spool_file_count_limit_exceeded_on_open_and_refresh() {
        let ft_dir = durable_test_dir("spool-file-count-limit");
        let plan = make_plan(1);
        let mut store = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        store.create_ledger("exec-1", &plan).unwrap();
        store.create_ledger("exec-2", &plan).unwrap();
        store.create_ledger("exec-3", &plan).unwrap();
        drop(store);

        let restricted_policy = IdempotencyPolicy {
            max_spool_files: 2,
            ..IdempotencyPolicy::default()
        };
        let open_err = IdempotencyStore::open(&ft_dir, restricted_policy.clone()).unwrap_err();
        assert_eq!(
            open_err,
            IdempotencyError::SpoolFileCountExceeded {
                actual: 3,
                maximum: 2
            }
        );

        let mut store = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        store.policy.max_spool_files = 2;
        let key = make_key("test-plan", &plan.steps[0].id);
        let refresh_err = store
            .refresh_durable_outcome_for_key(&key, 1000)
            .unwrap_err();
        assert_eq!(
            refresh_err,
            IdempotencyError::SpoolFileCountExceeded {
                actual: 3,
                maximum: 2
            }
        );
        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn spool_total_bytes_limit_exceeded_on_open_and_refresh() {
        let ft_dir = durable_test_dir("spool-bytes-limit");
        let plan = make_plan(1);
        let mut store = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        store.create_ledger("exec-1", &plan).unwrap();
        store.create_ledger("exec-2", &plan).unwrap();
        drop(store);

        let restricted_policy = IdempotencyPolicy {
            max_spool_total_bytes: 100,
            ..IdempotencyPolicy::default()
        };
        let open_err = IdempotencyStore::open(&ft_dir, restricted_policy.clone()).unwrap_err();
        assert!(matches!(
            open_err,
            IdempotencyError::SpoolByteLimitExceeded { maximum: 100, .. }
        ));

        let mut store = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        store.policy.max_spool_total_bytes = 100;
        let key = make_key("test-plan", &plan.steps[0].id);
        let refresh_err = store
            .refresh_durable_outcome_for_key(&key, 1000)
            .unwrap_err();
        assert!(matches!(
            refresh_err,
            IdempotencyError::SpoolByteLimitExceeded { maximum: 100, .. }
        ));
        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn spool_record_count_limit_exceeded_on_open_and_refresh() {
        let ft_dir = durable_test_dir("spool-record-count-limit");
        let plan = make_plan(2);
        let mut store = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        store.create_ledger("exec-1", &plan).unwrap();
        store
            .transition_phase("exec-1", TxPhase::Preparing)
            .unwrap();
        let key_1 = make_key("test-plan", &plan.steps[0].id);
        let key_2 = make_key("test-plan", &plan.steps[1].id);
        record_durable_outcome(
            &mut store,
            "exec-1",
            key_1.clone(),
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "agent",
            1000,
        );
        record_durable_outcome(
            &mut store,
            "exec-1",
            key_2,
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "agent",
            1001,
        );
        drop(store);

        let restricted_policy = IdempotencyPolicy {
            max_spool_records: 1,
            ..IdempotencyPolicy::default()
        };
        let open_err = IdempotencyStore::open(&ft_dir, restricted_policy.clone()).unwrap_err();
        assert_eq!(
            open_err,
            IdempotencyError::SpoolRecordCountExceeded {
                actual: 2,
                maximum: 1
            }
        );

        let mut store = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        store.policy.max_spool_records = 1;
        let refresh_err = store
            .refresh_durable_outcome_for_key(&key_1, 2000)
            .unwrap_err();
        assert_eq!(
            refresh_err,
            IdempotencyError::SpoolRecordCountExceeded {
                actual: 2,
                maximum: 1
            }
        );
        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn spool_ledger_oversized_limit_exceeded() {
        let ft_dir = durable_test_dir("spool-ledger-oversized");
        let restricted_policy = IdempotencyPolicy {
            max_ledger_bytes: 100,
            ..IdempotencyPolicy::default()
        };
        let mut store = IdempotencyStore::open(&ft_dir, restricted_policy).unwrap();
        let plan = make_plan(2);
        let err = store.create_ledger("exec-oversized", &plan).unwrap_err();
        assert!(matches!(
            err,
            IdempotencyError::LedgerOversized {
                ref execution_id,
                actual,
                maximum: 100
            } if execution_id == "exec-oversized" && actual > 100
        ));
        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn corrupt_spool_fails_closed_without_oom() {
        let ft_dir = durable_test_dir("spool-corrupt-fail-closed");
        let store = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap();
        let spool_path = ft_dir.join("tx_ledgers");
        drop(store);

        let corrupt_file = spool_path.join("exec-corrupt.json");
        std::fs::write(
            &corrupt_file,
            b"{\"execution_id\": \"exec-corrupt\", broken",
        )
        .unwrap();

        let open_err = IdempotencyStore::open(&ft_dir, IdempotencyPolicy::default()).unwrap_err();
        assert!(matches!(open_err, IdempotencyError::LedgerPersist { .. }));

        let _ = std::fs::remove_dir_all(&ft_dir);
    }

    #[test]
    fn terminal_certificate_synthesized_on_completion() {
        let plan = make_plan(2);
        let mut ledger = TxExecutionLedger::new("exec-cert-1", "test-plan", plan.hash());
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let key0 = make_key("test-plan", "step-0");
        let key1 = make_key("test-plan", "step-1");
        ledger
            .append(
                key0,
                StepOutcome::Success {
                    result: Some("ok0".to_string()),
                },
                StepRisk::Low,
                "agent",
                1000,
            )
            .unwrap();
        ledger
            .append(
                key1,
                StepOutcome::Success {
                    result: Some("ok1".to_string()),
                },
                StepRisk::Low,
                "agent",
                2000,
            )
            .unwrap();

        ledger.transition_phase(TxPhase::Completed).unwrap();
        let cert = ledger
            .terminal_certificate()
            .expect("certificate synthesized on Completed");
        assert_eq!(cert.disposition, TerminalDispositionKind::Committed);
        assert_eq!(cert.completed_step_ids, vec!["step-0", "step-1"]);
        assert!(cert.failed_step_ids.is_empty());
        assert!(cert.compensated_step_ids.is_empty());

        let json = serde_json::to_string(&ledger).unwrap();
        let deserialized: TxExecutionLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deserialized.terminal_certificate(),
            Some(cert)
        );

        let ctx = ResumeContext::from_ledger(&deserialized, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::AlreadyComplete);
    }

    #[test]
    fn terminal_certificate_synthesized_on_rollback() {
        let plan = make_plan(2);
        let mut ledger = TxExecutionLedger::new("exec-cert-rollback", "test-plan", plan.hash());
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let key0 = make_key("test-plan", "step-0");
        let key1 = make_key("test-plan", "step-1");
        ledger
            .append(
                key0.clone(),
                StepOutcome::Success {
                    result: Some("ok0".to_string()),
                },
                StepRisk::Low,
                "agent",
                1000,
            )
            .unwrap();
        ledger
            .append(
                key1,
                StepOutcome::Failed {
                    error_code: "ERR".to_string(),
                    error_message: "boom".to_string(),
                    compensated: true,
                },
                StepRisk::Low,
                "agent",
                2000,
            )
            .unwrap();

        ledger.transition_phase(TxPhase::Compensating).unwrap();
        let comp_key0 = IdempotencyKey::for_compensation("test-plan", "step-0", 1000);
        ledger
            .append(
                comp_key0,
                StepOutcome::Compensated {
                    original_outcome: Box::new(StepOutcome::Success {
                        result: Some("ok0".to_string()),
                    }),
                    compensation_result: "undone".to_string(),
                },
                StepRisk::Low,
                "agent",
                3000,
            )
            .unwrap();

        ledger.transition_phase(TxPhase::Completed).unwrap();
        let cert = ledger
            .terminal_certificate()
            .expect("certificate synthesized on rollback Completed");
        assert_eq!(cert.disposition, TerminalDispositionKind::RolledBack);
        assert_eq!(cert.completed_step_ids, vec!["step-0"]);
        assert_eq!(cert.failed_step_ids, vec!["step-1"]);
        assert_eq!(cert.compensated_step_ids, vec!["step-0"]);

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::AlreadyComplete);
    }

    #[test]
    fn resume_context_detects_missing_plan_coverage_in_partial_completed_ledger() {
        let plan = make_plan(3);
        let mut ledger = TxExecutionLedger::new("exec-partial", "test-plan", plan.hash());
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let key0 = make_key("test-plan", "step-0");
        ledger
            .append(
                key0,
                StepOutcome::Success {
                    result: Some("ok0".to_string()),
                },
                StepRisk::Low,
                "agent",
                1000,
            )
            .unwrap();

        ledger.transition_phase(TxPhase::Completed).unwrap();
        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        assert_eq!(ctx.recommendation, ResumeRecommendation::RestartFresh);
    }

    #[test]
    fn terminal_ledger_deserialization_rejects_missing_or_forged_certificate() {
        let plan = make_plan(1);
        let mut ledger = TxExecutionLedger::new("exec-cert-tamper", "test-plan", plan.hash());
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();
        let key0 = make_key("test-plan", "step-0");
        ledger
            .append(
                key0,
                StepOutcome::Success {
                    result: Some("ok0".to_string()),
                },
                StepRisk::Low,
                "agent",
                1000,
            )
            .unwrap();
        ledger.transition_phase(TxPhase::Completed).unwrap();

        // 1. Deserialization fails if terminal_certificate is missing
        let mut json_val: serde_json::Value = serde_json::to_value(&ledger).unwrap();
        json_val
            .as_object_mut()
            .unwrap()
            .remove("terminal_certificate");
        let err = serde_json::from_value::<TxExecutionLedger>(json_val).unwrap_err();
        assert!(err.to_string().contains("lacks a terminal disposition certificate"));

        // 2. Deserialization fails if certificate execution_id is forged
        let mut json_val: serde_json::Value = serde_json::to_value(&ledger).unwrap();
        json_val
            .get_mut("terminal_certificate")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "execution_id".to_string(),
                serde_json::Value::String("forged-id".to_string()),
            );
        let err = serde_json::from_value::<TxExecutionLedger>(json_val).unwrap_err();
        assert!(err.to_string().contains("execution_id"));

        // 3. Deserialization fails if completed_step_ids is forged
        let mut json_val: serde_json::Value = serde_json::to_value(&ledger).unwrap();
        json_val
            .get_mut("terminal_certificate")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "completed_step_ids".to_string(),
                serde_json::json!(["step-0", "step-unexecuted"]),
            );
        let err = serde_json::from_value::<TxExecutionLedger>(json_val).unwrap_err();
        assert!(err.to_string().contains("completed_step_ids"));
    }
}
