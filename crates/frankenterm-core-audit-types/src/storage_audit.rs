//! Storage audit records and query contracts.
//!
//! SQLite connection ownership, migrations, and writer confinement stay in
//! `frankenterm-core::storage`; this module owns the portable audit records
//! and query shapes passed across that boundary.

use serde::{Deserialize, Serialize};

/// Redaction adapter used by storage audit records.
pub trait AuditFieldRedactor {
    /// Return a redacted copy of `value`.
    fn redact(&self, value: &str) -> String;
}

/// Workflow step log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStepLogRecord {
    /// Step log ID.
    pub id: i64,
    /// Workflow execution ID.
    pub workflow_id: String,
    /// Linked audit action ID, if available.
    pub audit_action_id: Option<i64>,
    /// Step index within workflow.
    pub step_index: usize,
    /// Step name.
    pub step_name: String,
    /// Step idempotency key from plan, if available.
    pub step_id: Option<String>,
    /// Step action kind, if available.
    pub step_kind: Option<String>,
    /// Result type: continue, done, retry, abort, wait_for.
    pub result_type: String,
    /// Result data as JSON.
    pub result_data: Option<String>,
    /// Policy decision summary as JSON.
    pub policy_summary: Option<String>,
    /// Verification evidence references as JSON.
    pub verification_refs: Option<String>,
    /// Stable error code, if any.
    pub error_code: Option<String>,
    /// Started timestamp in epoch milliseconds.
    pub started_at: i64,
    /// Completed timestamp in epoch milliseconds.
    pub completed_at: i64,
    /// Duration in milliseconds.
    pub duration_ms: i64,
}

/// Audit action record for policy decisions and outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditActionRecord {
    /// Audit record ID.
    pub id: i64,
    /// Timestamp in epoch milliseconds.
    pub ts: i64,
    /// Actor kind: human, robot, mcp, workflow.
    pub actor_kind: String,
    /// Optional actor identifier: workflow ID, MCP client ID, etc.
    pub actor_id: Option<String>,
    /// Optional correlation identifier for prepare/approve/commit chains.
    pub correlation_id: Option<String>,
    /// Pane ID, if the action targeted a pane.
    pub pane_id: Option<u64>,
    /// Domain name, if applicable.
    pub domain: Option<String>,
    /// Action kind: send_text, workflow_run, etc.
    pub action_kind: String,
    /// Policy decision: allow, deny, require_approval.
    pub policy_decision: String,
    /// Policy decision reason, redacted before persistence/export.
    pub decision_reason: Option<String>,
    /// Policy rule ID, if any.
    pub rule_id: Option<String>,
    /// Redacted input summary.
    pub input_summary: Option<String>,
    /// Redacted verification summary.
    pub verification_summary: Option<String>,
    /// Decision context as JSON, if available.
    pub decision_context: Option<String>,
    /// Result: success, denied, failed, timeout.
    pub result: String,
}

impl AuditActionRecord {
    /// Redact sensitive fields before persistence or export.
    pub fn redact_fields<R: AuditFieldRedactor>(&mut self, redactor: &R) {
        self.decision_reason = self
            .decision_reason
            .as_ref()
            .map(|value| redactor.redact(value));
        self.input_summary = self
            .input_summary
            .as_ref()
            .map(|value| redactor.redact(value));
        self.verification_summary = self
            .verification_summary
            .as_ref()
            .map(|value| redactor.redact(value));
        self.decision_context = self
            .decision_context
            .as_ref()
            .map(|value| redactor.redact(value));
    }
}

/// Persistent audit record for policy-denied MCP mutations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyDeniedAuditRecord {
    /// Auto-assigned row ID; set to 0 on insert and populated on read.
    pub id: i64,
    /// Timestamp the denial landed, in epoch milliseconds.
    pub ts_ms: i64,
    /// Requesting agent or MCP client identifier, if known.
    pub agent_id: Option<String>,
    /// Stable tool identifier, for example "wa.tx_run".
    pub tool_name: String,
    /// Caller-computed fingerprint of the intent.
    pub intent_hash: Option<String>,
    /// Human-readable decision reason, already redacted upstream.
    pub reason: String,
    /// Stable machine code: policy_denied or require_approval.
    pub reason_code: String,
    /// Policy rule ID that produced the decision, if any.
    pub rule_id: Option<String>,
    /// Raw policy decision kind: denied or require_approval.
    pub decision: String,
}

impl PolicyDeniedAuditRecord {
    /// Stable `reason_code` for denied policy decisions.
    pub const REASON_CODE_DENIED: &'static str = "policy_denied";
    /// Stable `reason_code` for require-approval policy decisions.
    pub const REASON_CODE_REQUIRE_APPROVAL: &'static str = "require_approval";
    /// br-ft-6h1rv: stable `reason_code` for require-approval decisions
    /// returned by an MCP tool surface that does NOT support the
    /// allow-once approval flow (no `ApprovalStore::attach_to_decision`
    /// plumbing). Pre-fix these audit rows used `REASON_CODE_REQUIRE_APPROVAL`
    /// and the client got a misleading "obtain an allow-once token and
    /// retry" hint with no path to actually obtain such a token.
    /// Operators querying the audit table can now distinguish this
    /// dead-end class from approval-supported require-approval rows.
    pub const REASON_CODE_REQUIRE_APPROVAL_UNSUPPORTED: &'static str =
        "require_approval_unsupported";
    /// Stable decision string for denied policy decisions.
    pub const DECISION_DENIED: &'static str = "denied";
    /// Stable decision string for require-approval policy decisions.
    pub const DECISION_REQUIRE_APPROVAL: &'static str = "require_approval";
    /// br-ft-6h1rv: stable decision string for require-approval decisions
    /// on tool surfaces that don't support the approval flow. See
    /// [`Self::REASON_CODE_REQUIRE_APPROVAL_UNSUPPORTED`].
    pub const DECISION_REQUIRE_APPROVAL_UNSUPPORTED: &'static str = "require_approval_unsupported";
}

/// Redacted audit record for JSONL streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditStreamRecord {
    /// Monotonic record ID used as a cursor.
    pub id: i64,
    /// Timestamp in epoch milliseconds.
    pub ts: i64,
    /// Actor kind: human, robot, mcp, workflow.
    pub actor_kind: String,
    /// Actor identifier, if any.
    pub actor_id: Option<String>,
    /// Correlation identifier, if any.
    pub correlation_id: Option<String>,
    /// Pane ID, if applicable.
    pub pane_id: Option<u64>,
    /// Domain name, if applicable.
    pub domain: Option<String>,
    /// Action kind: send_text, workflow_run, etc.
    pub action_kind: String,
    /// Policy decision: allow, deny, require_approval.
    pub policy_decision: String,
    /// Redacted decision reason, if any.
    pub decision_reason: Option<String>,
    /// Policy rule ID, if any.
    pub rule_id: Option<String>,
    /// Redacted input summary, if any.
    pub input_summary: Option<String>,
    /// Redacted verification summary, if any.
    pub verification_summary: Option<String>,
    /// Redacted decision context as JSON, if any.
    pub decision_context: Option<String>,
    /// Result: success, denied, failed, timeout.
    pub result: String,
}

impl AuditStreamRecord {
    /// Build a redacted stream record from an audit action.
    pub fn from_action<R: AuditFieldRedactor>(mut action: AuditActionRecord, redactor: &R) -> Self {
        action.redact_fields(redactor);
        Self {
            id: action.id,
            ts: action.ts,
            actor_kind: action.actor_kind,
            actor_id: action.actor_id,
            correlation_id: action.correlation_id,
            pane_id: action.pane_id,
            domain: action.domain,
            action_kind: action.action_kind,
            policy_decision: action.policy_decision,
            decision_reason: action.decision_reason,
            rule_id: action.rule_id,
            input_summary: action.input_summary,
            verification_summary: action.verification_summary,
            decision_context: action.decision_context,
            result: action.result,
        }
    }
}

/// Undo metadata for an audit action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionUndoRecord {
    /// Audit action ID.
    pub audit_action_id: i64,
    /// Whether the action is undoable.
    pub undoable: bool,
    /// Undo strategy: none, manual, workflow_abort, pane_close, custom.
    pub undo_strategy: String,
    /// Redacted guidance for humans.
    pub undo_hint: Option<String>,
    /// Redacted JSON payload for executor.
    pub undo_payload: Option<String>,
    /// When the action was undone, in epoch milliseconds.
    pub undone_at: Option<i64>,
    /// Who performed the undo.
    pub undone_by: Option<String>,
}

impl ActionUndoRecord {
    /// Redact sensitive fields before persistence or export.
    pub fn redact_fields<R: AuditFieldRedactor>(&mut self, redactor: &R) {
        self.undo_hint = self.undo_hint.as_ref().map(|value| redactor.redact(value));
        self.undo_payload = self
            .undo_payload
            .as_ref()
            .map(|value| redactor.redact(value));
    }
}

/// Read-optimized action history record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionHistoryRecord {
    /// Audit record ID.
    pub id: i64,
    /// Timestamp in epoch milliseconds.
    pub ts: i64,
    /// Actor kind: human, robot, mcp, workflow.
    pub actor_kind: String,
    /// Optional actor identifier.
    pub actor_id: Option<String>,
    /// Optional correlation identifier.
    pub correlation_id: Option<String>,
    /// Pane ID, if the action targeted a pane.
    pub pane_id: Option<u64>,
    /// Domain name, if applicable.
    pub domain: Option<String>,
    /// Action kind.
    pub action_kind: String,
    /// Policy decision.
    pub policy_decision: String,
    /// Policy decision reason.
    pub decision_reason: Option<String>,
    /// Policy rule ID, if any.
    pub rule_id: Option<String>,
    /// Redacted input summary.
    pub input_summary: Option<String>,
    /// Redacted verification summary.
    pub verification_summary: Option<String>,
    /// Decision context as JSON, if available.
    pub decision_context: Option<String>,
    /// Result.
    pub result: String,
    /// Whether the action is undoable.
    pub undoable: Option<bool>,
    /// Undo strategy.
    pub undo_strategy: Option<String>,
    /// Redacted undo hint.
    pub undo_hint: Option<String>,
    /// When the action was undone, in epoch milliseconds.
    pub undone_at: Option<i64>,
    /// Who performed the undo.
    pub undone_by: Option<String>,
    /// Workflow ID associated with the action, if any.
    pub workflow_id: Option<String>,
    /// Workflow step name associated with the action, if any.
    pub step_name: Option<String>,
}

/// Query options for audit actions.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// Maximum number of results; default is 100.
    pub limit: Option<usize>,
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by domain name.
    pub domain: Option<String>,
    /// Filter by actor kind.
    pub actor_kind: Option<String>,
    /// Filter by actor identifier.
    pub actor_id: Option<String>,
    /// Filter by correlation identifier.
    pub correlation_id: Option<String>,
    /// Filter by action kind.
    pub action_kind: Option<String>,
    /// Filter by policy decision.
    pub policy_decision: Option<String>,
    /// Filter by rule ID.
    pub rule_id: Option<String>,
    /// Filter by result.
    pub result: Option<String>,
    /// Filter by time range start, in epoch milliseconds.
    pub since: Option<i64>,
    /// Filter by time range end, in epoch milliseconds.
    pub until: Option<i64>,
}

/// Query options for cursor-based audit streaming.
#[derive(Debug, Clone, Default)]
pub struct AuditStreamQuery {
    /// Resume after this audit action ID, exclusive.
    pub cursor: Option<i64>,
    /// Maximum number of results; default is 100.
    pub limit: Option<usize>,
    /// Optional offset applied after cursor filtering.
    pub offset: Option<usize>,
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by domain name.
    pub domain: Option<String>,
    /// Filter by actor kind.
    pub actor_kind: Option<String>,
    /// Filter by actor identifier.
    pub actor_id: Option<String>,
    /// Filter by correlation identifier.
    pub correlation_id: Option<String>,
    /// Filter by action kind.
    pub action_kind: Option<String>,
    /// Filter by policy decision.
    pub policy_decision: Option<String>,
    /// Filter by rule ID.
    pub rule_id: Option<String>,
    /// Filter by result.
    pub result: Option<String>,
    /// Filter by time range start, in epoch milliseconds.
    pub since: Option<i64>,
    /// Filter by time range end, in epoch milliseconds.
    pub until: Option<i64>,
}

/// Cursor-based audit stream page.
#[derive(Debug, Clone, Default)]
pub struct AuditStreamPage {
    /// Ordered audit records for this page.
    pub records: Vec<AuditActionRecord>,
    /// Cursor to resume from, if any.
    pub next_cursor: Option<i64>,
}

/// Query options for action history view.
#[derive(Debug, Clone, Default)]
pub struct ActionHistoryQuery {
    /// Filter by audit action ID.
    pub audit_action_id: Option<i64>,
    /// Maximum number of results; default is 100.
    pub limit: Option<usize>,
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by domain name.
    pub domain: Option<String>,
    /// Filter by actor kind.
    pub actor_kind: Option<String>,
    /// Filter by actor identifier.
    pub actor_id: Option<String>,
    /// Filter by correlation identifier.
    pub correlation_id: Option<String>,
    /// Filter by action kind.
    pub action_kind: Option<String>,
    /// Filter by policy decision.
    pub policy_decision: Option<String>,
    /// Filter by rule ID.
    pub rule_id: Option<String>,
    /// Filter by result.
    pub result: Option<String>,
    /// Filter by undoable flag.
    pub undoable: Option<bool>,
    /// Filter by time range start, in epoch milliseconds.
    pub since: Option<i64>,
    /// Filter by time range end, in epoch milliseconds.
    pub until: Option<i64>,
}
