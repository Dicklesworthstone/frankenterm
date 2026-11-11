//! Shared transaction substrate for daemon-mediated fleet mutations.
//!
//! `ft robot profile apply`, `ft robot fleet scale`, and
//! `ft robot fleet rebalance` need the same safety properties before
//! any of them should perform live daemon/mux mutations:
//!
//! - policy/approval preflight before side effects;
//! - idempotent replay for retried Robot/MCP requests;
//! - dry-run receipts that use the same plan as commit;
//! - no silent partial success; and
//! - compensation receipts for already-committed steps when a later
//!   step fails.
//!
//! This module is intentionally pure. It defines the plan, receipt,
//! idempotency, and executor contracts that the daemon-side profile,
//! scale, and rebalance handlers can share. Live mux spawning/stopping
//! is supplied by an implementation of [`FleetMutationExecutor`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::robot_idempotency::MutationKey;

const MAX_FLEET_REQUEST_KEY_BYTES: usize = 512;
const MAX_FLEET_REQUEST_ACTION_BYTES: usize = 64;
const MAX_FLEET_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_FLEET_REQUEST_NODES: usize = 65_536;
const MAX_FLEET_REQUEST_DEPTH: usize = 64;

/// Immutable caller intent for durable fleet execution.
///
/// An explicit scale/rebalance request must keep this identity when the live
/// inventory changes. The inventory-derived plan is a separate binding owned
/// by the winning execution claim. Only the key, action, and SHA-256 digest
/// survive construction; request parameters are not retained here.
#[derive(Clone, PartialEq, Eq)]
pub struct FleetMutationRequestBinding {
    idempotency_key: String,
    action: String,
    request_sha256: String,
}

impl std::fmt::Debug for FleetMutationRequestBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FleetMutationRequestBinding")
            .field("action", &self.action)
            .field("request_sha256", &self.request_sha256)
            .finish_non_exhaustive()
    }
}

/// Finite construction errors that never include request parameters or keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FleetMutationRequestBindingError {
    #[error("fleet mutation idempotency key is empty, invalid, or exceeds 512 bytes")]
    InvalidKey,
    #[error("fleet mutation action must be 1-64 lowercase ASCII letters, digits, or underscores")]
    InvalidAction,
    #[error("fleet mutation request exceeds the bounded canonical representation")]
    RequestTooLarge,
    #[error("fleet mutation request cannot be encoded")]
    InvalidEncoding,
}

impl FleetMutationRequestBinding {
    /// Bind a caller key to an action and canonical request parameters.
    ///
    /// Callers normalize provider/strategy aliases before construction. Object
    /// key order is ignored; array order, JSON types, and exact string bytes
    /// remain significant. The digest includes an explicit format version and
    /// length framing, so delimiter-like parameters cannot alias one another.
    pub fn new(
        key: &str,
        action: &str,
        canonical_request: &serde_json::Value,
    ) -> Result<Self, FleetMutationRequestBindingError> {
        if !valid_fleet_request_key(key) {
            return Err(FleetMutationRequestBindingError::InvalidKey);
        }
        if !valid_fleet_request_action(action) {
            return Err(FleetMutationRequestBindingError::InvalidAction);
        }
        let mut hasher = Sha256::new();
        hasher.update(b"ft.fleet-mutation-request.v1\0");
        let mut budget = FleetRequestHashBudget {
            bytes: MAX_FLEET_REQUEST_BYTES,
            nodes: MAX_FLEET_REQUEST_NODES,
        };
        budget.field(&mut hasher, action.as_bytes())?;
        hash_fleet_request_value(&mut hasher, canonical_request, &mut budget, 0)?;
        Ok(Self {
            idempotency_key: key.to_string(),
            action: action.to_string(),
            // sha2 0.11 digests are hybrid-array Arrays, which do not
            // implement LowerHex; hex::encode gives the same lowercase digits.
            request_sha256: hex::encode(hasher.finalize()),
        })
    }

    /// Bind a generated key to its complete plan when no caller key was given.
    ///
    /// Explicit caller requests use [`Self::new`] instead, so retrying them
    /// does not depend on recreating a previous inventory-derived plan.
    pub fn for_plan(
        action: &str,
        plan: &FleetMutationPlan,
    ) -> Result<Self, FleetMutationRequestBindingError> {
        // Refuse oversized plans while encoding, before cloning their strings
        // into a second JSON tree. The subsequent canonical hash enforces the
        // independent depth/node/framing budget as for explicit requests.
        let mut encoded = BoundedFleetRequestBuffer::default();
        serde_json::to_writer(&mut encoded, plan).map_err(|_| {
            if encoded.exceeded {
                FleetMutationRequestBindingError::RequestTooLarge
            } else {
                FleetMutationRequestBindingError::InvalidEncoding
            }
        })?;
        let request = serde_json::from_slice(&encoded.bytes)
            .map_err(|_| FleetMutationRequestBindingError::InvalidEncoding)?;
        Self::new(plan.idempotency_key.as_str(), action, &request)
    }

    #[must_use]
    pub fn idempotency_key(&self) -> &str {
        &self.idempotency_key
    }

    #[must_use]
    pub fn action(&self) -> &str {
        &self.action
    }

    #[must_use]
    pub fn request_sha256(&self) -> &str {
        &self.request_sha256
    }

    /// Validate the bounded authority representation before a store operation.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        valid_fleet_request_key(&self.idempotency_key)
            && valid_fleet_request_action(&self.action)
            && self.request_sha256.len() == 64
            && self
                .request_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

fn valid_fleet_request_key(key: &str) -> bool {
    !key.trim().is_empty()
        && key.len() <= MAX_FLEET_REQUEST_KEY_BYTES
        && !key.as_bytes().contains(&0)
}

fn valid_fleet_request_action(action: &str) -> bool {
    !action.is_empty()
        && action.len() <= MAX_FLEET_REQUEST_ACTION_BYTES
        && action
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Default)]
struct BoundedFleetRequestBuffer {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl std::io::Write for BoundedFleetRequestBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_FLEET_REQUEST_BYTES {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "fleet request encoding exceeds its bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FleetRequestHashBudget {
    bytes: usize,
    nodes: usize,
}

impl FleetRequestHashBudget {
    fn field(
        &mut self,
        hasher: &mut Sha256,
        bytes: &[u8],
    ) -> Result<(), FleetMutationRequestBindingError> {
        self.bytes = self
            .bytes
            .checked_sub(bytes.len().saturating_add(8))
            .ok_or(FleetMutationRequestBindingError::RequestTooLarge)?;
        let length = u64::try_from(bytes.len())
            .map_err(|_| FleetMutationRequestBindingError::RequestTooLarge)?;
        hasher.update(length.to_be_bytes());
        hasher.update(bytes);
        Ok(())
    }
}

fn hash_fleet_request_value(
    hasher: &mut Sha256,
    value: &serde_json::Value,
    budget: &mut FleetRequestHashBudget,
    depth: usize,
) -> Result<(), FleetMutationRequestBindingError> {
    if depth > MAX_FLEET_REQUEST_DEPTH {
        return Err(FleetMutationRequestBindingError::RequestTooLarge);
    }
    budget.nodes = budget
        .nodes
        .checked_sub(1)
        .ok_or(FleetMutationRequestBindingError::RequestTooLarge)?;
    budget.bytes = budget
        .bytes
        .checked_sub(1)
        .ok_or(FleetMutationRequestBindingError::RequestTooLarge)?;
    match value {
        serde_json::Value::Null => hasher.update(b"n"),
        serde_json::Value::Bool(value) => hasher.update(if *value { b"t" } else { b"f" }),
        serde_json::Value::Number(value) => {
            hasher.update(b"d");
            budget.field(hasher, value.to_string().as_bytes())?;
        }
        serde_json::Value::String(value) => {
            hasher.update(b"s");
            budget.field(hasher, value.as_bytes())?;
        }
        serde_json::Value::Array(values) => {
            if values.len() > budget.nodes {
                return Err(FleetMutationRequestBindingError::RequestTooLarge);
            }
            hasher.update(b"a");
            budget.field(hasher, values.len().to_string().as_bytes())?;
            for value in values {
                hash_fleet_request_value(hasher, value, budget, depth + 1)?;
            }
        }
        serde_json::Value::Object(values) => {
            if values.len() > budget.nodes {
                return Err(FleetMutationRequestBindingError::RequestTooLarge);
            }
            hasher.update(b"o");
            budget.field(hasher, values.len().to_string().as_bytes())?;
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (key, value) in entries {
                budget.field(hasher, key.as_bytes())?;
                hash_fleet_request_value(hasher, value, budget, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// One daemon-side fleet/profile mutation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetMutationAction {
    /// Spawn an agent pane from a resolved profile/program.
    SpawnAgent {
        profile_name: String,
        program: String,
        cwd: Option<String>,
        domain: Option<String>,
        env: BTreeMap<String, String>,
    },
    /// Stop an existing agent pane.
    StopAgent { pane_id: u64, reason: String },
    /// Reassign one work item from one agent to another.
    ReassignWork {
        work_item_id: String,
        from_agent: Option<String>,
        to_agent: String,
        reason: String,
    },
}

impl FleetMutationAction {
    /// Stable, human-readable action family.
    #[must_use]
    pub const fn family(&self) -> &'static str {
        match self {
            Self::SpawnAgent { .. } => "spawn_agent",
            Self::StopAgent { .. } => "stop_agent",
            Self::ReassignWork { .. } => "reassign_work",
        }
    }

    /// Stable target identifier used in receipts and forensic logs.
    #[must_use]
    pub fn target_id(&self) -> String {
        match self {
            Self::SpawnAgent {
                profile_name,
                program,
                ..
            } => format!("profile:{profile_name}:program:{program}"),
            Self::StopAgent { pane_id, .. } => format!("pane:{pane_id}"),
            Self::ReassignWork { work_item_id, .. } => format!("work:{work_item_id}"),
        }
    }
}

/// Policy/approval decision attached to a mutation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum FleetMutationPolicyDecision {
    Allow,
    Deny {
        reason_code: String,
        message: String,
    },
    RequireApproval {
        approval_id: String,
        reason_code: String,
        message: String,
    },
}

impl FleetMutationPolicyDecision {
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// A single planned mutation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationStep {
    pub step_id: String,
    pub action: FleetMutationAction,
    pub policy: FleetMutationPolicyDecision,
    /// Optional action that compensates this step after it has
    /// succeeded and a later step fails.
    pub compensation: Option<FleetMutationAction>,
}

impl FleetMutationStep {
    #[must_use]
    pub fn mutation_key(&self, plan_id: &str) -> MutationKey {
        let fingerprint = stable_json(self);
        MutationKey::derive(
            "fleet_mutation_step",
            &format!("{plan_id}:{}:{fingerprint}", self.step_id),
        )
    }
}

/// A complete mutation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationPlan {
    pub plan_id: String,
    pub idempotency_key: MutationKey,
    pub payload_fingerprint: MutationKey,
    pub dry_run: bool,
    pub steps: Vec<FleetMutationStep>,
}

impl FleetMutationPlan {
    /// Build a plan and derive a content-addressed idempotency key
    /// from the stable plan payload.
    #[must_use]
    pub fn new(plan_id: impl Into<String>, dry_run: bool, steps: Vec<FleetMutationStep>) -> Self {
        let plan_id = plan_id.into();
        let payload_fingerprint = plan_payload_fingerprint(&plan_id, dry_run, &steps);
        let idempotency_key =
            MutationKey::derive("fleet_mutation_plan", payload_fingerprint.as_str());
        Self {
            plan_id,
            idempotency_key,
            payload_fingerprint,
            dry_run,
            steps,
        }
    }

    /// Build a plan with a caller-supplied idempotency key.
    #[must_use]
    pub fn with_client_key(
        plan_id: impl Into<String>,
        dry_run: bool,
        steps: Vec<FleetMutationStep>,
        client_key: &str,
    ) -> Self {
        let plan_id = plan_id.into();
        let payload_fingerprint = plan_payload_fingerprint(&plan_id, dry_run, &steps);
        Self {
            plan_id,
            idempotency_key: MutationKey::from_client("fleet_mutation_plan", client_key),
            payload_fingerprint,
            dry_run,
            steps,
        }
    }
}

/// Executor-provided output for a mutation or compensation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FleetMutationStepOutput {
    pub pane_id: Option<u64>,
    pub work_item_id: Option<String>,
    pub details: BTreeMap<String, String>,
}

/// Executor failure with stable machine code plus operator message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationExecutionError {
    pub error_code: String,
    pub message: String,
}

impl FleetMutationExecutionError {
    #[must_use]
    pub fn new(error_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error_code: error_code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FleetMutationExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_code, self.message)
    }
}

impl std::error::Error for FleetMutationExecutionError {}

/// Live side-effect adapter supplied by the daemon/mux layer.
pub trait FleetMutationExecutor {
    fn execute_step(
        &mut self,
        step: &FleetMutationStep,
    ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError>;

    fn compensate_step(
        &mut self,
        original: &FleetMutationStep,
        compensation: &FleetMutationAction,
    ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError>;
}

/// Terminal status for a whole plan receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMutationReceiptStatus {
    DryRun,
    Succeeded,
    Denied,
    ApprovalRequired,
    Failed,
    Compensated,
    CompensationFailed,
}

/// Terminal status for one step receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMutationStepStatus {
    Planned,
    Skipped,
    Succeeded,
    Failed,
    Compensated,
}

/// Terminal status for compensating a successful step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMutationCompensationStatus {
    Succeeded,
    Failed,
    NotAvailable,
}

/// Compensation evidence attached to a previously succeeded step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationCompensationReceipt {
    pub status: FleetMutationCompensationStatus,
    pub action: Option<FleetMutationAction>,
    pub output: Option<FleetMutationStepOutput>,
    pub error: Option<FleetMutationExecutionError>,
}

/// Receipt for one mutation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationStepReceipt {
    pub step_id: String,
    pub action: FleetMutationAction,
    pub target_id: String,
    pub status: FleetMutationStepStatus,
    pub policy: FleetMutationPolicyDecision,
    pub idempotency_key: MutationKey,
    pub output: Option<FleetMutationStepOutput>,
    pub error: Option<FleetMutationExecutionError>,
    pub compensation: Option<FleetMutationCompensationReceipt>,
}

/// Durable receipt for a whole mutation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMutationReceipt {
    pub plan_id: String,
    pub idempotency_key: MutationKey,
    pub payload_fingerprint: MutationKey,
    pub dry_run: bool,
    pub status: FleetMutationReceiptStatus,
    pub idempotent_replay: bool,
    pub completed_count: usize,
    pub failed_count: usize,
    pub compensated_count: usize,
    pub skipped_count: usize,
    pub steps: Vec<FleetMutationStepReceipt>,
}

impl FleetMutationReceipt {
    /// Record an admitted request that needs no mutations at its planning point.
    ///
    /// Persisting this result binds an explicit caller key even when inventory
    /// already meets its target. A later retry cannot turn that same request
    /// into a new mutation after inventory changes.
    pub fn no_op(plan: &FleetMutationPlan) -> Result<Self, FleetMutationPlanError> {
        if plan.plan_id.trim().is_empty() {
            return Err(FleetMutationPlanError::EmptyPlanId);
        }
        if !plan.steps.is_empty() {
            return Err(FleetMutationPlanError::NonEmptyNoOp);
        }
        Ok(receipt_with_counts(
            plan,
            if plan.dry_run {
                FleetMutationReceiptStatus::DryRun
            } else {
                FleetMutationReceiptStatus::Succeeded
            },
            false,
            Vec::new(),
        ))
    }

    #[must_use]
    pub fn replay(mut self) -> Self {
        self.idempotent_replay = true;
        self
    }

    fn refresh_counts(&mut self) {
        self.completed_count = self
            .steps
            .iter()
            .filter(|step| matches!(step.status, FleetMutationStepStatus::Succeeded))
            .count();
        self.failed_count = self
            .steps
            .iter()
            .filter(|step| matches!(step.status, FleetMutationStepStatus::Failed))
            .count();
        self.compensated_count = self
            .steps
            .iter()
            .filter(|step| matches!(step.status, FleetMutationStepStatus::Compensated))
            .count();
        self.skipped_count = self
            .steps
            .iter()
            .filter(|step| matches!(step.status, FleetMutationStepStatus::Skipped))
            .count();
    }
}

/// Plan-level validation error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetMutationPlanError {
    EmptyPlanId,
    EmptyPlan,
    NonEmptyNoOp,
    EmptyStepId {
        step_index: usize,
    },
    DuplicateStepId {
        step_id: String,
        first_index: usize,
        duplicate_index: usize,
    },
    IdempotencyKeyConflict {
        idempotency_key: String,
        existing_payload_fingerprint: String,
        incoming_payload_fingerprint: String,
    },
}

impl std::fmt::Display for FleetMutationPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPlanId => f.write_str("fleet mutation plan_id must not be empty"),
            Self::EmptyPlan => f.write_str("fleet mutation plan must contain at least one step"),
            Self::NonEmptyNoOp => {
                f.write_str("fleet mutation no-op receipt must not contain mutation steps")
            }
            Self::EmptyStepId { step_index } => {
                write!(
                    f,
                    "fleet mutation step at index {step_index} has an empty step_id"
                )
            }
            Self::DuplicateStepId {
                step_id,
                first_index,
                duplicate_index,
            } => write!(
                f,
                "fleet mutation step_id `{step_id}` appears at indexes {first_index} and {duplicate_index}"
            ),
            Self::IdempotencyKeyConflict {
                idempotency_key,
                existing_payload_fingerprint,
                incoming_payload_fingerprint,
            } => write!(
                f,
                "fleet mutation idempotency key `{idempotency_key}` was reused for a different plan payload \
                 (existing {existing_payload_fingerprint}, incoming {incoming_payload_fingerprint})"
            ),
        }
    }
}

impl std::error::Error for FleetMutationPlanError {}

/// In-memory receipt ledger keyed by plan idempotency key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetMutationLedger {
    receipts: BTreeMap<String, FleetMutationReceipt>,
}

impl FleetMutationLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn get(&self, key: &MutationKey) -> Option<&FleetMutationReceipt> {
        self.receipts.get(key.as_str())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    /// Execute a plan or return the cached receipt for an idempotent replay.
    pub fn execute_plan<E: FleetMutationExecutor>(
        &mut self,
        plan: &FleetMutationPlan,
        executor: &mut E,
    ) -> Result<FleetMutationReceipt, FleetMutationPlanError> {
        validate_plan(plan)?;
        if let Some(receipt) = self.receipts.get(plan.idempotency_key.as_str()) {
            if receipt.payload_fingerprint != plan.payload_fingerprint {
                return Err(FleetMutationPlanError::IdempotencyKeyConflict {
                    idempotency_key: plan.idempotency_key.as_str().to_string(),
                    existing_payload_fingerprint: receipt.payload_fingerprint.as_str().to_string(),
                    incoming_payload_fingerprint: plan.payload_fingerprint.as_str().to_string(),
                });
            }
            return Ok(receipt.clone().replay());
        }

        let receipt = if plan.dry_run {
            dry_run_receipt(plan)
        } else if let Some(status) = preflight_block_status(plan) {
            preflight_blocked_receipt(plan, status)
        } else {
            commit_receipt(plan, executor)
        };

        self.receipts
            .insert(plan.idempotency_key.as_str().to_string(), receipt.clone());
        Ok(receipt)
    }
}

fn validate_plan(plan: &FleetMutationPlan) -> Result<(), FleetMutationPlanError> {
    if plan.plan_id.trim().is_empty() {
        return Err(FleetMutationPlanError::EmptyPlanId);
    }
    if plan.steps.is_empty() {
        return Err(FleetMutationPlanError::EmptyPlan);
    }

    let mut seen = BTreeMap::<&str, usize>::new();
    for (index, step) in plan.steps.iter().enumerate() {
        if step.step_id.trim().is_empty() {
            return Err(FleetMutationPlanError::EmptyStepId { step_index: index });
        }
        if let Some(first_index) = seen.insert(step.step_id.as_str(), index) {
            return Err(FleetMutationPlanError::DuplicateStepId {
                step_id: step.step_id.clone(),
                first_index,
                duplicate_index: index,
            });
        }
    }
    Ok(())
}

fn preflight_block_status(plan: &FleetMutationPlan) -> Option<FleetMutationReceiptStatus> {
    if plan
        .steps
        .iter()
        .any(|step| matches!(step.policy, FleetMutationPolicyDecision::Deny { .. }))
    {
        Some(FleetMutationReceiptStatus::Denied)
    } else if plan.steps.iter().any(|step| {
        matches!(
            step.policy,
            FleetMutationPolicyDecision::RequireApproval { .. }
        )
    }) {
        Some(FleetMutationReceiptStatus::ApprovalRequired)
    } else {
        None
    }
}

fn dry_run_receipt(plan: &FleetMutationPlan) -> FleetMutationReceipt {
    let steps = plan
        .steps
        .iter()
        .map(|step| step_receipt(plan, step, FleetMutationStepStatus::Planned, None, None))
        .collect::<Vec<_>>();
    receipt_with_counts(plan, FleetMutationReceiptStatus::DryRun, false, steps)
}

fn preflight_blocked_receipt(
    plan: &FleetMutationPlan,
    status: FleetMutationReceiptStatus,
) -> FleetMutationReceipt {
    let steps = plan
        .steps
        .iter()
        .map(|step| step_receipt(plan, step, FleetMutationStepStatus::Skipped, None, None))
        .collect::<Vec<_>>();
    receipt_with_counts(plan, status, false, steps)
}

fn commit_receipt<E: FleetMutationExecutor>(
    plan: &FleetMutationPlan,
    executor: &mut E,
) -> FleetMutationReceipt {
    let mut receipts = Vec::with_capacity(plan.steps.len());
    let mut succeeded = Vec::new();

    for step in &plan.steps {
        match executor.execute_step(step) {
            Ok(output) => {
                succeeded.push(receipts.len());
                receipts.push(step_receipt(
                    plan,
                    step,
                    FleetMutationStepStatus::Succeeded,
                    Some(output),
                    None,
                ));
            }
            Err(err) => {
                receipts.push(step_receipt(
                    plan,
                    step,
                    FleetMutationStepStatus::Failed,
                    None,
                    Some(err),
                ));
                compensate_successes(plan, executor, &mut receipts, &succeeded);
                let status = if receipts
                    .iter()
                    .any(|receipt| compensation_failed(receipt.compensation.as_ref()))
                {
                    FleetMutationReceiptStatus::CompensationFailed
                } else if receipts
                    .iter()
                    .any(|receipt| receipt.compensation.is_some())
                {
                    FleetMutationReceiptStatus::Compensated
                } else {
                    FleetMutationReceiptStatus::Failed
                };
                return receipt_with_counts(plan, status, false, receipts);
            }
        }
    }

    receipt_with_counts(plan, FleetMutationReceiptStatus::Succeeded, false, receipts)
}

fn compensate_successes<E: FleetMutationExecutor>(
    plan: &FleetMutationPlan,
    executor: &mut E,
    receipts: &mut [FleetMutationStepReceipt],
    succeeded_indexes: &[usize],
) {
    for index in succeeded_indexes.iter().rev().copied() {
        let Some(receipt) = receipts.get_mut(index) else {
            continue;
        };
        let Some(step) = plan.steps.get(index) else {
            continue;
        };
        let Some(compensation) = step.compensation.as_ref() else {
            receipt.compensation = Some(FleetMutationCompensationReceipt {
                status: FleetMutationCompensationStatus::NotAvailable,
                action: None,
                output: None,
                error: None,
            });
            continue;
        };

        match executor.compensate_step(step, compensation) {
            Ok(output) => {
                receipt.status = FleetMutationStepStatus::Compensated;
                receipt.compensation = Some(FleetMutationCompensationReceipt {
                    status: FleetMutationCompensationStatus::Succeeded,
                    action: Some(compensation.clone()),
                    output: Some(output),
                    error: None,
                });
            }
            Err(err) => {
                receipt.compensation = Some(FleetMutationCompensationReceipt {
                    status: FleetMutationCompensationStatus::Failed,
                    action: Some(compensation.clone()),
                    output: None,
                    error: Some(err),
                });
            }
        }
    }
}

fn step_receipt(
    plan: &FleetMutationPlan,
    step: &FleetMutationStep,
    status: FleetMutationStepStatus,
    output: Option<FleetMutationStepOutput>,
    error: Option<FleetMutationExecutionError>,
) -> FleetMutationStepReceipt {
    FleetMutationStepReceipt {
        step_id: step.step_id.clone(),
        action: step.action.clone(),
        target_id: step.action.target_id(),
        status,
        policy: step.policy.clone(),
        idempotency_key: step.mutation_key(&plan.plan_id),
        output,
        error,
        compensation: None,
    }
}

fn receipt_with_counts(
    plan: &FleetMutationPlan,
    status: FleetMutationReceiptStatus,
    idempotent_replay: bool,
    steps: Vec<FleetMutationStepReceipt>,
) -> FleetMutationReceipt {
    let completed_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Succeeded))
        .count();
    let failed_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Failed))
        .count();
    let compensated_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Compensated))
        .count();
    let skipped_count = steps
        .iter()
        .filter(|step| matches!(step.status, FleetMutationStepStatus::Skipped))
        .count();

    FleetMutationReceipt {
        plan_id: plan.plan_id.clone(),
        idempotency_key: plan.idempotency_key.clone(),
        payload_fingerprint: plan.payload_fingerprint.clone(),
        dry_run: plan.dry_run,
        status,
        idempotent_replay,
        completed_count,
        failed_count,
        compensated_count,
        skipped_count,
        steps,
    }
}

fn compensation_failed(compensation: Option<&FleetMutationCompensationReceipt>) -> bool {
    compensation.is_some_and(|receipt| receipt.status == FleetMutationCompensationStatus::Failed)
}

/// Compensate successful side effects from an already-built receipt.
///
/// Durable receipt writers call this after a plan executed but the receipt
/// could not be persisted. The receipt remains the forensic payload returned to
/// Robot/MCP callers, with any successful rollback attempts attached to the
/// original step receipts.
#[must_use]
pub fn compensate_committed_receipt<E: FleetMutationExecutor>(
    plan: &FleetMutationPlan,
    mut receipt: FleetMutationReceipt,
    executor: &mut E,
) -> FleetMutationReceipt {
    if receipt.dry_run
        || matches!(
            receipt.status,
            FleetMutationReceiptStatus::DryRun
                | FleetMutationReceiptStatus::Denied
                | FleetMutationReceiptStatus::ApprovalRequired
                | FleetMutationReceiptStatus::Compensated
                | FleetMutationReceiptStatus::CompensationFailed
        )
    {
        return receipt;
    }

    let succeeded_indexes = receipt
        .steps
        .iter()
        .enumerate()
        .filter_map(|(index, step)| {
            (step.status == FleetMutationStepStatus::Succeeded).then_some(index)
        })
        .collect::<Vec<_>>();

    if succeeded_indexes.is_empty() {
        receipt.status = FleetMutationReceiptStatus::Failed;
        receipt.refresh_counts();
        return receipt;
    }

    compensate_successes(plan, executor, &mut receipt.steps, &succeeded_indexes);
    receipt.status = if receipt
        .steps
        .iter()
        .any(|step| compensation_failed(step.compensation.as_ref()))
    {
        FleetMutationReceiptStatus::CompensationFailed
    } else if receipt.steps.iter().any(|step| step.compensation.is_some()) {
        FleetMutationReceiptStatus::Compensated
    } else {
        FleetMutationReceiptStatus::Failed
    };
    receipt.refresh_counts();
    receipt
}

fn stable_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| format!("serde-error:{err}"))
}

fn plan_payload_fingerprint(
    plan_id: &str,
    dry_run: bool,
    steps: &[FleetMutationStep],
) -> MutationKey {
    let fingerprint = stable_json(&serde_json::json!({
        "plan_id": plan_id,
        "dry_run": dry_run,
        "steps": steps,
    }));
    MutationKey::derive("fleet_mutation_plan_payload", &fingerprint)
}

/// Convenience assertion for callers reconstructing receipts from durable
/// storage: every compensation must refer to a step that actually reached a
/// side-effect state.
#[must_use]
pub fn receipt_has_no_orphan_compensation(receipt: &FleetMutationReceipt) -> bool {
    let committed: BTreeSet<&str> = receipt
        .steps
        .iter()
        .filter(|step| {
            matches!(
                step.status,
                FleetMutationStepStatus::Succeeded | FleetMutationStepStatus::Compensated
            )
        })
        .map(|step| step.step_id.as_str())
        .collect();

    receipt
        .steps
        .iter()
        .all(|step| step.compensation.is_none() || committed.contains(step.step_id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeExecutor {
        calls: Vec<String>,
        fail_step: Option<String>,
        fail_compensation_for: Option<String>,
        next_pane_id: u64,
    }

    impl FakeExecutor {
        fn new() -> Self {
            Self {
                next_pane_id: 100,
                ..Self::default()
            }
        }
    }

    impl FleetMutationExecutor for FakeExecutor {
        fn execute_step(
            &mut self,
            step: &FleetMutationStep,
        ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError> {
            self.calls.push(format!("execute:{}", step.step_id));
            if self.fail_step.as_deref() == Some(step.step_id.as_str()) {
                return Err(FleetMutationExecutionError::new(
                    "fake.step_failed",
                    format!("step {} failed", step.step_id),
                ));
            }
            let mut output = FleetMutationStepOutput::default();
            match &step.action {
                FleetMutationAction::SpawnAgent { .. } => {
                    output.pane_id = Some(self.next_pane_id);
                    self.next_pane_id += 1;
                }
                FleetMutationAction::StopAgent { .. } => {}
                FleetMutationAction::ReassignWork {
                    work_item_id,
                    from_agent,
                    to_agent,
                    reason,
                } => {
                    output.work_item_id = Some(work_item_id.clone());
                    if let Some(from_agent) = from_agent {
                        output
                            .details
                            .insert("from_agent".to_string(), from_agent.clone());
                    }
                    output
                        .details
                        .insert("to_agent".to_string(), to_agent.clone());
                    output.details.insert("reason".to_string(), reason.clone());
                }
            }
            Ok(output)
        }

        fn compensate_step(
            &mut self,
            original: &FleetMutationStep,
            compensation: &FleetMutationAction,
        ) -> Result<FleetMutationStepOutput, FleetMutationExecutionError> {
            self.calls.push(format!("compensate:{}", original.step_id));
            if self.fail_compensation_for.as_deref() == Some(original.step_id.as_str()) {
                return Err(FleetMutationExecutionError::new(
                    "fake.compensation_failed",
                    format!("compensation for {} failed", original.step_id),
                ));
            }
            let mut output = FleetMutationStepOutput::default();
            if let FleetMutationAction::ReassignWork {
                work_item_id,
                from_agent,
                to_agent,
                reason,
            } = compensation
            {
                output.work_item_id = Some(work_item_id.clone());
                if let Some(from_agent) = from_agent {
                    output
                        .details
                        .insert("from_agent".to_string(), from_agent.clone());
                }
                output
                    .details
                    .insert("to_agent".to_string(), to_agent.clone());
                output.details.insert("reason".to_string(), reason.clone());
            }
            Ok(output)
        }
    }

    fn spawn_step(step_id: &str) -> FleetMutationStep {
        FleetMutationStep {
            step_id: step_id.to_string(),
            action: FleetMutationAction::SpawnAgent {
                profile_name: "default".to_string(),
                program: "codex".to_string(),
                cwd: Some("/repo".to_string()),
                domain: None,
                env: BTreeMap::new(),
            },
            policy: FleetMutationPolicyDecision::Allow,
            compensation: Some(FleetMutationAction::StopAgent {
                pane_id: 100,
                reason: "rollback spawned agent".to_string(),
            }),
        }
    }

    fn stop_step(step_id: &str, pane_id: u64) -> FleetMutationStep {
        FleetMutationStep {
            step_id: step_id.to_string(),
            action: FleetMutationAction::StopAgent {
                pane_id,
                reason: "scale down".to_string(),
            },
            policy: FleetMutationPolicyDecision::Allow,
            compensation: None,
        }
    }

    fn reassign_step(
        step_id: &str,
        work_item_id: &str,
        from_agent: Option<&str>,
        to_agent: &str,
        reason: &str,
    ) -> FleetMutationStep {
        let compensation = from_agent.map(|from_agent| FleetMutationAction::ReassignWork {
            work_item_id: work_item_id.to_string(),
            from_agent: Some(to_agent.to_string()),
            to_agent: from_agent.to_string(),
            reason: format!("rollback {reason}"),
        });
        FleetMutationStep {
            step_id: step_id.to_string(),
            action: FleetMutationAction::ReassignWork {
                work_item_id: work_item_id.to_string(),
                from_agent: from_agent.map(str::to_string),
                to_agent: to_agent.to_string(),
                reason: reason.to_string(),
            },
            policy: FleetMutationPolicyDecision::Allow,
            compensation,
        }
    }

    #[test]
    fn request_binding_canonicalizes_objects_and_hides_request_material() {
        let left: serde_json::Value =
            serde_json::from_str(r#"{"program":"codex","options":{"force":false,"count":3}}"#)
                .unwrap();
        let right: serde_json::Value =
            serde_json::from_str(r#"{"options":{"count":3,"force":false},"program":"codex"}"#)
                .unwrap();
        let first =
            FleetMutationRequestBinding::new("private-request-key", "scale", &left).unwrap();
        let reordered =
            FleetMutationRequestBinding::new("private-request-key", "scale", &right).unwrap();
        assert_eq!(first, reordered);
        assert!(first.is_valid());
        let debug = format!("{first:?}");
        assert!(!debug.contains("private-request-key"));
        assert!(!debug.contains("codex"));

        let different_key =
            FleetMutationRequestBinding::new(" other-key ", "scale", &left).unwrap();
        assert_eq!(different_key.idempotency_key(), " other-key ");
        assert_eq!(first.request_sha256(), different_key.request_sha256());
        assert_ne!(first, different_key);
        let other_action =
            FleetMutationRequestBinding::new("private-request-key", "rebalance", &left).unwrap();
        assert_ne!(first.request_sha256(), other_action.request_sha256());
    }

    #[test]
    fn request_binding_keeps_types_array_order_and_field_boundaries_distinct() {
        let requests = [
            serde_json::json!(null),
            serde_json::json!(false),
            serde_json::json!(0),
            serde_json::json!("0"),
            serde_json::json!(["a", "b"]),
            serde_json::json!(["b", "a"]),
            serde_json::json!(["a:b", "c"]),
            serde_json::json!(["a", "b:c"]),
            serde_json::json!({"a:b": "c"}),
            serde_json::json!({"a": "b:c"}),
            serde_json::json!({"a": {"b": "c"}}),
        ];
        let hashes: BTreeSet<String> = requests
            .iter()
            .map(|request| {
                FleetMutationRequestBinding::new("same-key", "scale", request)
                    .unwrap()
                    .request_sha256()
                    .to_string()
            })
            .collect();
        assert_eq!(hashes.len(), requests.len());
    }

    #[test]
    fn request_binding_enforces_key_action_depth_node_and_byte_limits() {
        use FleetMutationRequestBindingError::{InvalidAction, InvalidKey, RequestTooLarge};
        let request = serde_json::json!({"target_count": 3});
        for key in [
            String::new(),
            " \n".to_string(),
            "nul\0key".to_string(),
            "k".repeat(513),
        ] {
            assert_eq!(
                FleetMutationRequestBinding::new(&key, "scale", &request),
                Err(InvalidKey)
            );
        }
        for action in ["", "Scale", "scale-up", "é", &"a".repeat(65)] {
            assert_eq!(
                FleetMutationRequestBinding::new("key", action, &request),
                Err(InvalidAction)
            );
        }
        let huge = serde_json::Value::String("x".repeat(MAX_FLEET_REQUEST_BYTES));
        assert_eq!(
            FleetMutationRequestBinding::new("key", "scale", &huge),
            Err(RequestTooLarge)
        );
        let wide = serde_json::Value::Array(vec![serde_json::Value::Null; MAX_FLEET_REQUEST_NODES]);
        assert_eq!(
            FleetMutationRequestBinding::new("key", "scale", &wide),
            Err(RequestTooLarge)
        );
        let mut deep = serde_json::Value::Null;
        for _ in 0..MAX_FLEET_REQUEST_DEPTH {
            deep = serde_json::Value::Array(vec![deep]);
        }
        assert!(FleetMutationRequestBinding::new("key", "scale", &deep).is_ok());
        deep = serde_json::Value::Array(vec![deep]);
        assert_eq!(
            FleetMutationRequestBinding::new("key", "scale", &deep),
            Err(RequestTooLarge)
        );
    }

    #[test]
    fn generated_binding_tracks_the_plan_and_refuses_oversized_encoding() {
        let original = FleetMutationPlan::with_client_key(
            "scale-down",
            false,
            vec![stop_step("stop", 7)],
            "caller-key",
        );
        let changed = FleetMutationPlan::with_client_key(
            "scale-down",
            false,
            vec![stop_step("stop", 8)],
            "caller-key",
        );
        assert_ne!(
            FleetMutationRequestBinding::for_plan("scale", &original).unwrap(),
            FleetMutationRequestBinding::for_plan("scale", &changed).unwrap(),
        );
        let mut oversized = original;
        if let FleetMutationAction::StopAgent { reason, .. } = &mut oversized.steps[0].action {
            *reason = "x".repeat(MAX_FLEET_REQUEST_BYTES);
        }
        assert_eq!(
            FleetMutationRequestBinding::for_plan("scale", &oversized),
            Err(FleetMutationRequestBindingError::RequestTooLarge),
        );
    }

    #[test]
    fn no_op_receipt_binds_only_an_empty_valid_plan() {
        for dry_run in [false, true] {
            let plan =
                FleetMutationPlan::with_client_key("at-target", dry_run, Vec::new(), "caller-key");
            let receipt = FleetMutationReceipt::no_op(&plan).unwrap();
            assert_eq!(receipt.idempotency_key, plan.idempotency_key);
            assert_eq!(receipt.payload_fingerprint, plan.payload_fingerprint);
            assert_eq!(receipt.dry_run, dry_run);
            assert_eq!(
                receipt.status,
                if dry_run {
                    FleetMutationReceiptStatus::DryRun
                } else {
                    FleetMutationReceiptStatus::Succeeded
                }
            );
            assert_eq!(
                (
                    receipt.completed_count,
                    receipt.failed_count,
                    receipt.compensated_count,
                    receipt.skipped_count
                ),
                (0, 0, 0, 0)
            );
            assert!(receipt.steps.is_empty());
            assert!(!receipt.idempotent_replay);
        }
        let nonempty = FleetMutationPlan::new("must-execute", false, vec![spawn_step("one")]);
        assert_eq!(
            FleetMutationReceipt::no_op(&nonempty),
            Err(FleetMutationPlanError::NonEmptyNoOp)
        );
        let invalid = FleetMutationPlan::new(" ", false, Vec::new());
        assert_eq!(
            FleetMutationReceipt::no_op(&invalid),
            Err(FleetMutationPlanError::EmptyPlanId)
        );
    }

    #[test]
    fn dry_run_returns_plan_without_executor_side_effects() {
        let plan = FleetMutationPlan::new("plan-a", true, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("dry-run receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::DryRun);
        assert_eq!(receipt.steps[0].status, FleetMutationStepStatus::Planned);
        assert_eq!(executor.calls, [] as [std::string::String; 0]);
        assert_eq!(receipt.completed_count, 0);
    }

    #[test]
    fn policy_denial_prevents_all_side_effects() {
        let mut denied = spawn_step("spawn-denied");
        denied.policy = FleetMutationPolicyDecision::Deny {
            reason_code: "policy.forbidden".to_string(),
            message: "spawning blocked".to_string(),
        };
        let plan = FleetMutationPlan::new("plan-denied", false, vec![denied, spawn_step("later")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("denied receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::Denied);
        assert_eq!(receipt.skipped_count, 2);
        assert_eq!(executor.calls, [] as [std::string::String; 0]);
    }

    #[test]
    fn approval_required_prevents_all_side_effects() {
        let mut pending = spawn_step("spawn-needs-approval");
        pending.policy = FleetMutationPolicyDecision::RequireApproval {
            approval_id: "appr-123".to_string(),
            reason_code: "policy.approval_required".to_string(),
            message: "operator approval required".to_string(),
        };
        let plan =
            FleetMutationPlan::new("plan-approval", false, vec![pending, spawn_step("later")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("approval-required receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::ApprovalRequired);
        assert_eq!(receipt.skipped_count, 2);
        assert!(matches!(
            receipt.steps[0].policy,
            FleetMutationPolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(executor.calls, [] as [std::string::String; 0]);
    }

    #[test]
    fn reassign_work_receipt_preserves_target_owner_and_reason() {
        let plan = FleetMutationPlan::new(
            "plan-reassign",
            false,
            vec![reassign_step(
                "move-work-a",
                "ft-work-a",
                Some("agent-a"),
                "agent-b",
                "load_based",
            )],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("reassign receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::Succeeded);
        assert_eq!(receipt.completed_count, 1);
        assert_eq!(executor.calls, vec!["execute:move-work-a"]);
        let step = &receipt.steps[0];
        assert_eq!(step.action.family(), "reassign_work");
        assert_eq!(step.target_id, "work:ft-work-a");
        let output = step.output.as_ref().expect("reassign output");
        assert_eq!(output.work_item_id.as_deref(), Some("ft-work-a"));
        assert_eq!(
            output.details.get("from_agent").map(String::as_str),
            Some("agent-a")
        );
        assert_eq!(
            output.details.get("to_agent").map(String::as_str),
            Some("agent-b")
        );
        assert_eq!(
            output.details.get("reason").map(String::as_str),
            Some("load_based")
        );
        assert!(receipt_has_no_orphan_compensation(&receipt));
    }

    #[test]
    fn reassign_policy_denial_prevents_all_side_effects() {
        let mut denied = reassign_step(
            "move-denied",
            "ft-work-denied",
            Some("agent-a"),
            "agent-b",
            "capability_based",
        );
        denied.policy = FleetMutationPolicyDecision::Deny {
            reason_code: "work.reserved".to_string(),
            message: "reserved work cannot be reassigned".to_string(),
        };
        let plan = FleetMutationPlan::new(
            "plan-reassign-denied",
            false,
            vec![
                denied,
                reassign_step(
                    "move-later",
                    "ft-work-later",
                    Some("agent-c"),
                    "agent-d",
                    "round_robin",
                ),
            ],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("denied reassign receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::Denied);
        assert_eq!(receipt.skipped_count, 2);
        assert_eq!(receipt.steps[0].target_id, "work:ft-work-denied");
        assert_eq!(executor.calls, [] as [std::string::String; 0]);
    }

    #[test]
    fn idempotent_replay_returns_cached_receipt_without_reexecution() {
        let plan = FleetMutationPlan::new("plan-replay", false, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let first = ledger.execute_plan(&plan, &mut executor).expect("first");
        assert_eq!(first.status, FleetMutationReceiptStatus::Succeeded);
        assert_eq!(executor.calls, vec!["execute:spawn-1"]);

        let replay = ledger.execute_plan(&plan, &mut executor).expect("replay");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.steps, first.steps);
        assert_eq!(executor.calls, vec!["execute:spawn-1"]);
    }

    #[test]
    fn client_key_reuse_with_different_payload_is_rejected_without_execution() {
        let first_plan = FleetMutationPlan::with_client_key(
            "plan-client",
            false,
            vec![spawn_step("spawn-1")],
            "client-key-1",
        );
        let conflicting_plan = FleetMutationPlan::with_client_key(
            "plan-client",
            false,
            vec![spawn_step("spawn-2")],
            "client-key-1",
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let first = ledger
            .execute_plan(&first_plan, &mut executor)
            .expect("first execution");
        assert_eq!(first.status, FleetMutationReceiptStatus::Succeeded);

        let err = ledger
            .execute_plan(&conflicting_plan, &mut executor)
            .expect_err("same client key with different payload must conflict");

        assert!(matches!(
            err,
            FleetMutationPlanError::IdempotencyKeyConflict { .. }
        ));
        assert_eq!(executor.calls, vec!["execute:spawn-1"]);
    }

    #[test]
    fn partial_failure_compensates_successes_in_reverse_order() {
        let plan = FleetMutationPlan::new(
            "plan-comp",
            false,
            vec![
                spawn_step("spawn-1"),
                spawn_step("spawn-2"),
                stop_step("stop-fails", 42),
            ],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        executor.fail_step = Some("stop-fails".to_string());

        let receipt = ledger.execute_plan(&plan, &mut executor).expect("receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::Compensated);
        assert_eq!(receipt.failed_count, 1);
        assert_eq!(receipt.compensated_count, 2);
        assert_eq!(
            executor.calls,
            vec![
                "execute:spawn-1",
                "execute:spawn-2",
                "execute:stop-fails",
                "compensate:spawn-2",
                "compensate:spawn-1",
            ]
        );
        assert!(receipt_has_no_orphan_compensation(&receipt));
    }

    #[test]
    fn reassign_failure_compensates_prior_moves_in_reverse_order() {
        let plan = FleetMutationPlan::new(
            "plan-reassign-comp",
            false,
            vec![
                reassign_step(
                    "move-a",
                    "ft-work-a",
                    Some("agent-a"),
                    "agent-b",
                    "load_based",
                ),
                reassign_step(
                    "move-b",
                    "ft-work-b",
                    Some("agent-c"),
                    "agent-d",
                    "capability_based",
                ),
                reassign_step(
                    "move-fails",
                    "ft-work-c",
                    Some("agent-e"),
                    "agent-f",
                    "round_robin",
                ),
            ],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        executor.fail_step = Some("move-fails".to_string());

        let receipt = ledger.execute_plan(&plan, &mut executor).expect("receipt");

        assert_eq!(receipt.status, FleetMutationReceiptStatus::Compensated);
        assert_eq!(receipt.failed_count, 1);
        assert_eq!(receipt.compensated_count, 2);
        assert_eq!(
            executor.calls,
            vec![
                "execute:move-a",
                "execute:move-b",
                "execute:move-fails",
                "compensate:move-b",
                "compensate:move-a",
            ]
        );
        assert_eq!(
            receipt.steps[0].status,
            FleetMutationStepStatus::Compensated
        );
        assert_eq!(
            receipt.steps[1].status,
            FleetMutationStepStatus::Compensated
        );
        assert_eq!(receipt.steps[2].status, FleetMutationStepStatus::Failed);
        let compensation = receipt.steps[0]
            .compensation
            .as_ref()
            .expect("move-a compensation");
        assert_eq!(
            compensation
                .output
                .as_ref()
                .and_then(|out| out.work_item_id.as_deref()),
            Some("ft-work-a")
        );
        assert_eq!(
            compensation
                .output
                .as_ref()
                .and_then(|out| out.details.get("to_agent"))
                .map(String::as_str),
            Some("agent-a")
        );
        assert!(receipt_has_no_orphan_compensation(&receipt));
    }

    #[test]
    fn compensation_failure_is_explicit_in_plan_status() {
        let plan = FleetMutationPlan::new(
            "plan-comp-fail",
            false,
            vec![spawn_step("spawn-1"), stop_step("fail", 7)],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        executor.fail_step = Some("fail".to_string());
        executor.fail_compensation_for = Some("spawn-1".to_string());

        let receipt = ledger.execute_plan(&plan, &mut executor).expect("receipt");

        assert_eq!(
            receipt.status,
            FleetMutationReceiptStatus::CompensationFailed
        );
        let compensation = receipt.steps[0]
            .compensation
            .as_ref()
            .expect("compensation receipt");
        assert_eq!(compensation.status, FleetMutationCompensationStatus::Failed);
        assert!(receipt_has_no_orphan_compensation(&receipt));
    }

    #[test]
    fn duplicate_step_ids_are_rejected_before_execution() {
        let plan = FleetMutationPlan::new(
            "plan-dup",
            false,
            vec![spawn_step("same"), stop_step("same", 1)],
        );
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let err = ledger
            .execute_plan(&plan, &mut executor)
            .expect_err("duplicate step ids should fail");

        assert!(matches!(
            err,
            FleetMutationPlanError::DuplicateStepId { .. }
        ));
        assert_eq!(executor.calls, [] as [std::string::String; 0]);
    }

    #[test]
    fn empty_plan_id_is_rejected_before_execution() {
        let plan = FleetMutationPlan::new("  ", false, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let err = ledger
            .execute_plan(&plan, &mut executor)
            .expect_err("whitespace plan_id should fail");

        assert_eq!(err, FleetMutationPlanError::EmptyPlanId);
        assert_eq!(executor.calls, [] as [std::string::String; 0]);
    }

    #[test]
    fn whitespace_step_id_is_rejected_before_execution() {
        let plan = FleetMutationPlan::new("plan-whitespace-step", false, vec![spawn_step("  ")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();

        let err = ledger
            .execute_plan(&plan, &mut executor)
            .expect_err("whitespace step_id should fail");

        assert_eq!(err, FleetMutationPlanError::EmptyStepId { step_index: 0 });
        assert_eq!(executor.calls, [] as [std::string::String; 0]);
    }

    #[test]
    fn receipt_roundtrip_preserves_resume_evidence() {
        let plan = FleetMutationPlan::new("plan-roundtrip", false, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        let receipt = ledger.execute_plan(&plan, &mut executor).expect("receipt");

        let encoded = serde_json::to_string(&receipt).expect("encode receipt");
        let decoded: FleetMutationReceipt = serde_json::from_str(&encoded).expect("decode receipt");

        assert_eq!(decoded, receipt);
        assert!(receipt_has_no_orphan_compensation(&decoded));
    }

    #[test]
    fn orphan_compensation_on_uncommitted_step_is_rejected() {
        let plan = FleetMutationPlan::new("plan-orphan", true, vec![spawn_step("spawn-1")]);
        let mut ledger = FleetMutationLedger::new();
        let mut executor = FakeExecutor::new();
        let mut receipt = ledger
            .execute_plan(&plan, &mut executor)
            .expect("dry-run receipt");

        receipt.steps[0].compensation = Some(FleetMutationCompensationReceipt {
            status: FleetMutationCompensationStatus::NotAvailable,
            action: None,
            output: None,
            error: None,
        });

        assert!(!receipt_has_no_orphan_compensation(&receipt));
    }
}
