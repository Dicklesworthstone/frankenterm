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

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::tx_plan_compiler::{StepRisk, TxPlan};

// ── Idempotency Key ──────────────────────────────────────────────────────────

/// Content-addressed idempotency key for a tx step execution.
///
/// Generated deterministically from (plan_id, step_id, action_fingerprint) so that
/// replaying the same plan produces the same keys. The FNV-1a scheme matches
/// `tx_plan_compiler::compute_plan_hash` for consistency.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey {
    /// The raw key string, format: `txk:{hash_hex}`.
    key: String,
    /// Plan ID this key belongs to.
    plan_id: String,
    /// Step ID within the plan.
    step_id: String,
}

impl IdempotencyKey {
    /// Create a new idempotency key from plan + step + action content.
    #[must_use]
    pub fn new(plan_id: &str, step_id: &str, action_fingerprint: &str) -> Self {
        let hash = fnv1a_hash(&format!("{plan_id}|{step_id}|{action_fingerprint}"));
        Self {
            key: format!("txk:{hash:016x}"),
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
        }
    }

    /// Create a key for a compensation execution.
    #[must_use]
    pub fn for_compensation(plan_id: &str, step_id: &str, compensation_kind: &str) -> Self {
        let hash = fnv1a_hash(&format!("{plan_id}|{step_id}|comp:{compensation_kind}"));
        Self {
            key: format!("txk:{hash:016x}"),
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
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
    #[must_use]
    pub fn hash(&self) -> String {
        let canonical = format!(
            "{}|{}|{}|{}|{}|{}",
            self.ordinal,
            self.idem_key.as_str(),
            self.execution_id,
            self.timestamp_ms,
            serde_json::to_string(&self.outcome).unwrap_or_default(),
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

        ChainVerification {
            chain_intact: first_break_at.is_none() && missing_ordinals.is_empty(),
            first_break_at,
            missing_ordinals,
            total_records: self.records.len(),
        }
    }

    /// Rebuild the key index after deserialization.
    pub fn rebuild_index(&mut self) {
        self.key_index.clear();
        for record in &self.records {
            self.key_index
                .insert(record.idem_key.as_str().to_string(), record.ordinal);
        }
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
                !completed.contains(&step.id)
                    && !failed.contains(&step.id)
                    && !compensated.contains(&step.id)
            })
            .map(|step| step.id.clone())
            .collect::<Vec<_>>();

        let recommendation = if ledger.phase().is_terminal() {
            ResumeRecommendation::AlreadyComplete
        } else if !verification.chain_intact {
            ResumeRecommendation::RestartFresh
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
        // Check the global dedup guard first (cross-instance).
        if let Some(entry) = self.dedup.check(idem_key) {
            return Some(&entry.outcome);
        }
        // Check all active ledgers.
        for ledger in self.ledgers.values() {
            if let Some(outcome) = ledger.get_outcome(idem_key) {
                return Some(outcome);
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
            .map(|ledger| ResumeContext::from_ledger(ledger, plan))
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

    #[error("chain integrity violation at ordinal {ordinal}")]
    ChainIntegrityViolation { ordinal: u64 },
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
    fn ledger_rebuild_index() {
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

        // Simulate deserialization (key_index is skip).
        let json = serde_json::to_string(&ledger).unwrap();
        let mut restored: TxExecutionLedger = serde_json::from_str(&json).unwrap();
        assert!(!restored.is_executed(&key)); // Index not rebuilt yet.

        restored.rebuild_index();
        assert!(restored.is_executed(&key));
    }

    #[test]
    fn ledger_serde_roundtrip() {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 42);
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
        let mut back: TxExecutionLedger = serde_json::from_str(&json).unwrap();
        back.rebuild_index();
        assert_eq!(back.execution_id(), "exec-1");
        assert_eq!(back.record_count(), 1);
        assert_eq!(back.phase(), TxPhase::Preparing);
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
