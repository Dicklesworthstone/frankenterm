//! Durable idempotency, deduplication, and resume invariants for the tx substrate (ft-1i2ge.8.7).
//!
//! Guarantees restart-safe idempotency and resume semantics across prepare/commit/compensation
//! paths. Integrates with the tx plan compiler's [`TxPlan`] / [`TxStep`] types and uses
//! content-addressed keys (FNV-1a) consistent with the plan hash scheme.
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

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::tx_plan_compiler::{StepRisk, TxPlan};

// ── Idempotency Key ──────────────────────────────────────────────────────────

/// Content-addressed idempotency key for a tx step execution.
///
/// Generated deterministically from (plan_id, step_id, action_fingerprint) so that
/// replaying the same plan produces the same keys. The FNV-1a scheme matches
/// `tx_plan_compiler::compute_plan_hash` for consistency.
///
/// # Persisted-key integrity (br-ft-f4vta)
///
/// Both `key` and `plan_id`/`step_id` were previously trusted across
/// the serde boundary even though `key` is documented as derived
/// from the other two plus an action fingerprint. The custom
/// `Deserialize` impl below validates the wire-format of `key`
/// (must be `txk:` + exactly 16 lowercase-hex characters), rejects
/// empty `plan_id` or `step_id`, AND re-derives the canonical hash
/// from the persisted `(plan_id, step_id, hash_input)` tuple to
/// confirm it matches the persisted `key`. This catches the cross-
/// step alias attack flagged by the bead body: a malformed ledger
/// carrying a `txk:HASH` from step A while claiming `step_id = B`
/// is rejected at deserialize time, before the forged form can
/// reach dedup decisions or resume accounting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct IdempotencyKey {
    /// The raw key string, format: `txk:{hash_hex}`.
    key: String,
    /// Plan ID this key belongs to.
    plan_id: String,
    /// Step ID within the plan.
    step_id: String,
    /// br-ft-f4vta: the post-`plan_id|step_id|` segment of the
    /// original FNV-1a hash input — i.e., the action fingerprint
    /// for [`Self::new`] or `comp:{compensation_kind}` for
    /// [`Self::for_compensation`]. Persisted so the custom
    /// `Deserialize` impl can re-derive the canonical key and
    /// verify the persisted form has not been tampered or aliased
    /// from a different step.
    hash_input: String,
}

impl IdempotencyKey {
    /// br-ft-f4vta: compute the canonical key string for
    /// `(plan_id, step_id, hash_input)`. Single source of truth
    /// used by both constructors and the deserialize validator.
    fn compute_key(plan_id: &str, step_id: &str, hash_input: &str) -> String {
        let hash = fnv1a_hash(&format!("{plan_id}|{step_id}|{hash_input}"));
        format!("txk:{hash:016x}")
    }

    /// Create a new idempotency key from plan + step + action content.
    #[must_use]
    pub fn new(plan_id: &str, step_id: &str, action_fingerprint: &str) -> Self {
        Self {
            key: Self::compute_key(plan_id, step_id, action_fingerprint),
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
            hash_input: action_fingerprint.to_string(),
        }
    }

    /// Create a key for a compensation execution.
    #[must_use]
    pub fn for_compensation(plan_id: &str, step_id: &str, compensation_kind: &str) -> Self {
        let hash_input = format!("comp:{compensation_kind}");
        Self {
            key: Self::compute_key(plan_id, step_id, &hash_input),
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
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
}

impl std::fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key)
    }
}

/// br-ft-f4vta: validate that `raw` is a well-formed idempotency
/// key string. Returns `true` iff `raw` starts with `"txk:"`
/// followed by exactly 16 lowercase-hex characters — the format
/// produced by `format!("txk:{:016x}", hash)` in `IdempotencyKey::new`
/// and `for_compensation`. Rejects any other shape (wrong prefix,
/// uppercase hex, padding chars, length drift, embedded
/// whitespace).
fn is_well_formed_idempotency_key(raw: &str) -> bool {
    let Some(hex) = raw.strip_prefix("txk:") else {
        return false;
    };
    hex.len() == 16
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
                "br-ft-f4vta: malformed IdempotencyKey raw key `{}`; expected format `txk:` + 16 lowercase hex chars",
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
        // br-ft-f4vta cross-step alias check: re-derive the canonical
        // key from the persisted (plan_id, step_id, hash_input) tuple
        // and confirm it matches the persisted `key`. Pre-fix the
        // attacker could swap any of (key, plan_id, step_id) without
        // detection — the format check alone catches malformed wire-
        // format strings but not a valid `txk:HASH` aliased to the
        // wrong step's metadata.
        let expected =
            IdempotencyKey::compute_key(&shape.plan_id, &shape.step_id, &shape.hash_input);
        if expected != shape.key {
            return Err(serde::de::Error::custom(format!(
                "br-ft-f4vta: IdempotencyKey.key `{}` does not match \
                 hash(plan_id|step_id|hash_input) — persisted form is \
                 forged or aliased from a different step (expected `{}`)",
                shape.key, expected
            )));
        }
        Ok(Self {
            key: shape.key,
            plan_id: shape.plan_id,
            step_id: shape.step_id,
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
}

// ── Step Execution Record ────────────────────────────────────────────────────

/// Immutable record of a single step execution within a tx instance.
///
/// Records form a hash chain: each record includes the hash of the previous record,
/// enabling tamper detection (consistent with `recorder_audit.rs`).
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
    /// FNV-1a hash of the previous record's canonical form (empty string for first).
    pub prev_hash: String,
    /// Agent that executed this step.
    pub agent_id: String,
}

impl StepExecutionRecord {
    /// Compute the FNV-1a hash of this record's canonical form.
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
        let outcome_json = serde_json::to_string(&self.outcome).expect(
            "StepOutcome::serialize is infallible — the enum derives Serialize from \
             primitive String/bool/Box fields; if this fires, a fallible-Serialize \
             field was added (br-ft-jyywz follow-up)",
        );
        let risk_json = serde_json::to_string(&self.risk).expect(
            "StepRisk::serialize is infallible — the enum derives Serialize from a \
             unit-variant set with #[serde(rename_all = \"snake_case\")]; if this \
             fires, a fallible-Serialize variant was added (br-ft-e9r75)",
        );
        let canonical = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            self.ordinal,
            self.idem_key.as_str(),
            self.execution_id,
            self.timestamp_ms,
            outcome_json,
            risk_json,
            self.agent_id,
            self.prev_hash,
        );
        format!("{:016x}", fnv1a_hash(&canonical))
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
    key_index: HashMap<String, u64>,
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
}

fn build_ledger_key_index(records: &[StepExecutionRecord]) -> Result<HashMap<String, u64>, String> {
    let mut key_index = HashMap::with_capacity(records.len());

    for record in records {
        let key = record.idem_key.as_str().to_string();
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
        let key_index = build_ledger_key_index(&serialized.records).map_err(de::Error::custom)?;

        Ok(Self {
            execution_id: serialized.execution_id,
            plan_id: serialized.plan_id,
            plan_hash: serialized.plan_hash,
            phase: serialized.phase,
            records: serialized.records,
            last_hash: serialized.last_hash,
            next_ordinal: serialized.next_ordinal,
            key_index,
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

    /// Transition to a new phase. Returns `Err` if the transition is invalid.
    pub fn transition_phase(&mut self, next: TxPhase) -> Result<TxPhase, IdempotencyError> {
        if !self.phase.can_transition_to(next) {
            return Err(IdempotencyError::InvalidPhaseTransition {
                from: self.phase,
                to: next,
            });
        }
        let prev = self.phase;
        self.phase = next;
        Ok(prev)
    }

    /// Check if a step has already been executed (dedup check).
    #[must_use]
    pub fn is_executed(&self, idem_key: &IdempotencyKey) -> bool {
        self.key_index.contains_key(idem_key.as_str())
    }

    /// Get the record for a previously executed step, if any.
    #[must_use]
    pub fn get_record(&self, idem_key: &IdempotencyKey) -> Option<&StepExecutionRecord> {
        self.key_index
            .get(idem_key.as_str())
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

        if self.key_index.is_empty() && !self.records.is_empty() {
            self.rebuild_index_checked()
                .map_err(|reason| IdempotencyError::LedgerIndexCorrupt { reason })?;
        }

        if self.key_index.contains_key(idem_key.as_str()) {
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
        self.key_index
            .insert(idem_key.as_str().to_string(), ordinal);
        self.records.push(record);

        Ok(record_hash)
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
    entries: BTreeMap<String, DeduplicationEntry>,
    /// FIFO order for eviction.
    order: VecDeque<String>,
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
        self.entries.get(idem_key.as_str())
    }

    fn latest_timestamp_ms(&self) -> Option<u64> {
        self.entries
            .values()
            .map(|entry| entry.timestamp_ms)
            .max()
    }

    /// Record a new execution. Evicts oldest entry if at capacity.
    pub fn record(
        &mut self,
        idem_key: &IdempotencyKey,
        execution_id: &str,
        outcome: StepOutcome,
        timestamp_ms: u64,
    ) {
        let key_str = idem_key.as_str().to_string();

        // If already present, update in place (no eviction needed).
        if let Some(entry) = self.entries.get_mut(&key_str) {
            entry.execution_id = execution_id.to_string();
            entry.outcome = outcome;
            entry.timestamp_ms = timestamp_ms;
            return;
        }

        // Evict if at capacity.
        if self.entries.len() >= self.capacity {
            if let Some(oldest_key) = self.order.pop_front() {
                self.entries.remove(&oldest_key);
            }
        }

        self.entries.insert(
            key_str.clone(),
            DeduplicationEntry {
                execution_id: execution_id.to_string(),
                outcome,
                timestamp_ms,
            },
        );
        self.order.push_back(key_str);
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
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.timestamp_ms < cutoff_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &expired {
            self.entries.remove(key);
        }
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
        let remaining = plan
            .steps
            .iter()
            .filter(|step| {
                (!policy.skip_completed_on_resume || !completed.contains(&step.id))
                    && !failed.contains(&step.id)
                    && !compensated.contains(&step.id)
            })
            .map(|step| step.id.clone())
            .collect::<Vec<_>>();

        let recommendation = if policy.require_chain_integrity && !verification.chain_intact {
            ResumeRecommendation::RestartFresh
        } else if ledger.phase().is_terminal() {
            ResumeRecommendation::AlreadyComplete
        } else if !failed.is_empty() && ledger.phase() == TxPhase::Compensating {
            ResumeRecommendation::CompensateAndAbort
        } else if remaining.is_empty() && failed.is_empty() {
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
}

impl IdempotencyStore {
    /// Create a new store with the given policy.
    #[must_use]
    pub fn new(policy: IdempotencyPolicy) -> Self {
        Self {
            ledgers: HashMap::new(),
            dedup: DeduplicationGuard::new(policy.dedup_capacity),
            policy,
        }
    }

    /// Create a new ledger for a tx execution. Returns error if execution ID already exists.
    pub fn create_ledger(
        &mut self,
        execution_id: &str,
        plan: &TxPlan,
    ) -> Result<(), IdempotencyError> {
        if self.ledgers.contains_key(execution_id) {
            return Err(IdempotencyError::DuplicateExecution {
                key: execution_id.to_string(),
            });
        }
        if self.ledgers.len() >= self.policy.max_active_ledgers.max(1) {
            return Err(IdempotencyError::ActiveLedgerLimitExceeded {
                max_active_ledgers: self.policy.max_active_ledgers.max(1),
            });
        }
        let ledger = TxExecutionLedger::new(execution_id, &plan.plan_id, plan.plan_hash);
        self.ledgers.insert(execution_id.to_string(), ledger);
        Ok(())
    }

    /// Get an immutable reference to a ledger.
    #[must_use]
    pub fn get_ledger(&self, execution_id: &str) -> Option<&TxExecutionLedger> {
        self.ledgers.get(execution_id)
    }

    /// Get a mutable reference to a ledger.
    #[must_use]
    pub fn get_ledger_mut(&mut self, execution_id: &str) -> Option<&mut TxExecutionLedger> {
        self.ledgers.get_mut(execution_id)
    }

    /// Execute-or-skip: check dedup, and if already done, return cached outcome.
    /// Otherwise return `None` so the caller knows to execute.
    #[must_use]
    pub fn check_dedup(&self, idem_key: &IdempotencyKey) -> Option<&StepOutcome> {
        let now_ms = self.logical_clock_ms();
        // Check the global dedup guard first (cross-instance).
        if let Some(entry) = self.dedup.check(idem_key) {
            if self.is_fresh_for_dedup(entry.timestamp_ms, now_ms) {
                return Some(&entry.outcome);
            }
        }
        // Check all active ledgers.
        for ledger in self.ledgers.values() {
            if let Some(record) = ledger.get_record(idem_key) {
                if self.is_fresh_for_dedup(record.timestamp_ms, now_ms) {
                    return Some(&record.outcome);
                }
            }
        }
        None
    }

    /// Record a step execution in a ledger and in the global dedup guard.
    pub fn record_execution(
        &mut self,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) -> Result<String, IdempotencyError> {
        let ledger =
            self.ledgers
                .get_mut(execution_id)
                .ok_or_else(|| IdempotencyError::LedgerNotFound {
                    execution_id: execution_id.to_string(),
                })?;

        let hash = ledger.append(
            idem_key.clone(),
            outcome.clone(),
            risk,
            agent_id,
            timestamp_ms,
        )?;

        self.evict_stale(timestamp_ms.saturating_sub(self.policy.dedup_ttl_ms));

        // Also record in the global dedup guard.
        self.dedup
            .record(&idem_key, execution_id, outcome, timestamp_ms);

        Ok(hash)
    }

    /// Build a resume context for a given execution.
    #[must_use]
    pub fn resume_context(&self, execution_id: &str, plan: &TxPlan) -> Option<ResumeContext> {
        self.ledgers
            .get(execution_id)
            .map(|ledger| ResumeContext::from_ledger_with_policy(ledger, plan, &self.policy))
    }

    /// Remove a completed/aborted ledger from active tracking.
    /// Returns the ledger for archival if it was terminal.
    pub fn archive_ledger(
        &mut self,
        execution_id: &str,
    ) -> Result<TxExecutionLedger, IdempotencyError> {
        let ledger =
            self.ledgers
                .get(execution_id)
                .ok_or_else(|| IdempotencyError::LedgerNotFound {
                    execution_id: execution_id.to_string(),
                })?;

        if !ledger.phase().is_terminal() {
            return Err(IdempotencyError::LedgerNotTerminal {
                execution_id: execution_id.to_string(),
                phase: ledger.phase(),
            });
        }

        Ok(self.ledgers.remove(execution_id).expect("checked above"))
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

    fn is_fresh_for_dedup(&self, timestamp_ms: u64, now_ms: u64) -> bool {
        timestamp_ms
            .saturating_add(self.policy.dedup_ttl_ms)
            >= now_ms
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
    /// Maximum number of active ledgers before oldest is archived.
    pub max_active_ledgers: usize,
}

impl Default for IdempotencyPolicy {
    fn default() -> Self {
        Self {
            dedup_capacity: 10_000,
            skip_completed_on_resume: true,
            dedup_ttl_ms: 3_600_000, // 1 hour
            require_chain_integrity: true,
            max_active_ledgers: 100,
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

    #[error("ledger index corrupt: {reason}")]
    LedgerIndexCorrupt { reason: String },

    #[error("chain integrity violation at ordinal {ordinal}")]
    ChainIntegrityViolation { ordinal: u64 },

    #[error("active ledger limit exceeded: max_active_ledgers={max_active_ledgers}")]
    ActiveLedgerLimitExceeded { max_active_ledgers: usize },
}

// ── FNV-1a Hash ──────────────────────────────────────────────────────────────

/// FNV-1a hash (consistent with `tx_plan_compiler::compute_plan_hash`).
fn fnv1a_hash(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_plan_compiler::{CompilerConfig, PlannerAssignment, compile_tx_plan};

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
    fn key_format_prefix() {
        let k = IdempotencyKey::new("p1", "s1", "action");
        assert!(k.as_str().starts_with("txk:"));
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
        let json = r#"{"key":"sha256:0123456789abcdef","plan_id":"p1","step_id":"s1","hash_input":"action"}"#;
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
        let json = r#"{"key":"txk:0123","plan_id":"p1","step_id":"s1","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("short hex must reject");
        assert!(err.to_string().contains("br-ft-f4vta"));
    }

    #[test]
    fn key_deserialize_rejects_long_hex() {
        let json = r#"{"key":"txk:0123456789abcdef0","plan_id":"p1","step_id":"s1","hash_input":"action"}"#;
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
        let json =
            r#"{"key":"txk:0123456789ABCDEF","plan_id":"p1","step_id":"s1","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("uppercase hex must reject");
        assert!(err.to_string().contains("br-ft-f4vta"));
    }

    #[test]
    fn key_deserialize_rejects_non_hex_chars() {
        let json =
            r#"{"key":"txk:0123456789abcdeg","plan_id":"p1","step_id":"s1","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("non-hex char must reject");
        assert!(err.to_string().contains("br-ft-f4vta"));
    }

    #[test]
    fn key_deserialize_rejects_empty_plan_id() {
        let json =
            r#"{"key":"txk:0123456789abcdef","plan_id":"","step_id":"s1","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
        let err = result.expect_err("empty plan_id must reject");
        assert!(err.to_string().contains("plan_id"));
    }

    #[test]
    fn key_deserialize_rejects_empty_step_id() {
        let json =
            r#"{"key":"txk:0123456789abcdef","plan_id":"p1","step_id":"","hash_input":"action"}"#;
        let result: Result<IdempotencyKey, _> = serde_json::from_str(json);
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
        let forged_json = format!(
            r#"{{"key":"{}","plan_id":"p1","step_id":"step-b","hash_input":"action-a"}}"#,
            real_a.as_str()
        );
        let result: Result<IdempotencyKey, _> = serde_json::from_str(&forged_json);
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
        let forged_json = format!(
            r#"{{"key":"{}","plan_id":"p1","step_id":"s1","hash_input":"FORGED-action"}}"#,
            real.as_str()
        );
        let result: Result<IdempotencyKey, _> = serde_json::from_str(&forged_json);
        assert!(
            result.is_err(),
            "tampered hash_input must reject (got Ok = forge undetected)"
        );
    }

    /// br-ft-f4vta: tampering `key` while keeping plan_id/step_id/
    /// hash_input must reject. Format check passes if the tampered
    /// key is still `txk:16hex`, but the hash-match gate fires.
    #[test]
    fn key_deserialize_rejects_tampered_key_with_valid_format_ft_f4vta() {
        let real = IdempotencyKey::new("p1", "s1", "real-action");
        // Use a different valid txk format string (different hex)
        // that does NOT match the legitimate hash for these inputs.
        let forged_json = format!(
            r#"{{"key":"txk:0000000000000000","plan_id":"{}","step_id":"{}","hash_input":"{}"}}"#,
            real.plan_id(),
            real.step_id(),
            "real-action",
        );
        let result: Result<IdempotencyKey, _> = serde_json::from_str(&forged_json);
        let err = result.expect_err("forged key must reject even with valid format");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-f4vta") && msg.contains("does not match"),
            "error must reference the hash-match contract; got {msg:?}"
        );
    }

    /// br-ft-f4vta: a constructor-built key always serializes to
    /// a form that round-trips through the strict Deserialize.
    /// Pinned via proptest over arbitrary plan_id, step_id, and
    /// action_fingerprint inputs.
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
        /// match the txk:16hex format are rejected by the
        /// Deserialize gate. Universal quantifier over the
        /// reject path.
        #[test]
        fn key_deserialize_rejects_arbitrary_malformed_key(
            bad_prefix in "[a-z]{1,8}:",
            tail in "[0-9a-f]{0,32}",
        ) {
            // Skip valid `txk:16hex` outputs from this generator.
            let bad_key = format!("{bad_prefix}{tail}");
            let is_valid = bad_key.starts_with("txk:")
                && bad_key.len() == 20
                && bad_key[4..].chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
            prop_assume!(!is_valid);

            let json = format!(
                r#"{{"key":"{bad_key}","plan_id":"p","step_id":"s","hash_input":"a"}}"#
            );
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
            let forged_json = format!(
                r#"{{"key":"{}","plan_id":"{}","step_id":"{}","hash_input":"{}"}}"#,
                key_a.as_str(),
                plan_b,
                step_b,
                action_b
            );
            let result: Result<IdempotencyKey, _> =
                serde_json::from_str(&forged_json);
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
        match outcome {
            Err(IdempotencyError::DuplicateExecution { key: k }) => {
                assert_eq!(k, key.as_str());
            }
            other => panic!("expected DuplicateExecution after deserialize+append, got {other:?}"),
        }

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
    fn dedup_evict_before() {
        let mut guard = DeduplicationGuard::new(10);
        for i in 0..5 {
            let key = make_key("p1", &format!("s{i}"));
            guard.record(
                &key,
                "exec-1",
                StepOutcome::Success { result: None },
                i as u64 * 1000,
            );
        }
        guard.evict_before(2500);
        assert_eq!(guard.len(), 2); // s3 (3000) and s4 (4000) remain.
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
    fn resume_skipped_steps_remain_pending() {
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
        assert_eq!(
            ctx.recommendation,
            ResumeRecommendation::ContinueFromCheckpoint
        );
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
        assert_eq!(
            ctx.recommendation,
            ResumeRecommendation::ContinueFromCheckpoint
        );
        assert!(ctx.remaining_steps.is_empty());
        assert_eq!(ctx.failed_steps, vec![plan.steps[0].id.clone()]);
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
        assert!(store.check_dedup(&key).is_none());

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
        assert_eq!(store.check_dedup(&key), Some(&outcome));
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
        assert!(store.check_dedup(&key).is_some());

        // Evict entries older than 2000.
        store.evict_stale(2000);

        // Still in ledger (not evicted from there), but global dedup evicted.
        // The check_dedup also looks at ledgers, so it will still find it.
        assert!(store.check_dedup(&key).is_some());
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
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: IdempotencyPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dedup_capacity, 500);
        assert!(!back.skip_completed_on_resume);
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

    // ── FNV-1a hash tests ──

    #[test]
    fn fnv1a_deterministic() {
        assert_eq!(fnv1a_hash("hello"), fnv1a_hash("hello"));
    }

    #[test]
    fn fnv1a_different_inputs() {
        assert_ne!(fnv1a_hash("hello"), fnv1a_hash("world"));
    }

    #[test]
    fn fnv1a_empty() {
        // FNV-1a of empty string is the offset basis.
        assert_eq!(fnv1a_hash(""), 0xcbf29ce484222325);
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
            assert!(store.check_dedup(&key).is_none());

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
            assert!(store.check_dedup(&key).is_some());
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

        // Resume context: should continue.
        let ctx = store.resume_context("exec-1", &plan).unwrap();
        assert_eq!(
            ctx.recommendation,
            ResumeRecommendation::ContinueFromCheckpoint
        );
        assert_eq!(ctx.completed_steps.len(), 1);
        assert_eq!(ctx.failed_steps.len(), 1);
        assert_eq!(ctx.remaining_steps.len(), 1);
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
        let dedup = store.check_dedup(&key);
        assert!(dedup.is_some());
        assert!(matches!(dedup.unwrap(), StepOutcome::Success { .. }));
    }
}
