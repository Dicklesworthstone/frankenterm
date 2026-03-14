//! MCP parameter, response, and envelope types.
//!
//! Extracted from `mcp.rs` as part of Wave 4A migration (ft-1fv0u).

use serde::{Deserialize, Serialize};

use crate::config::PaneFilterConfig;
use crate::policy::{InjectionResult, PaneCapabilities};
use crate::query_contract::UnifiedSearchMode;
use crate::wezterm::PaneInfo;

pub(super) const MCP_VERSION: &str = "v1";

pub(super) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| u64::try_from(dur.as_millis()).unwrap_or(u64::MAX))
}

// ── Core tool params ─────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub(super) struct StateParams {
    pub domain: Option<String>,
    pub agent: Option<String>,
    pub pane_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GetTextParams {
    pub pane_id: u64,
    #[serde(default = "default_tail")]
    pub tail: usize,
    #[serde(default)]
    pub escapes: bool,
}

pub(super) fn default_tail() -> usize {
    500
}

#[derive(Debug, Serialize)]
pub(super) struct McpGetTextData {
    pub pane_id: u64,
    pub text: String,
    pub tail_lines: usize,
    pub escapes_included: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation_info: Option<TruncationInfo>,
}

#[derive(Debug, Serialize)]
pub(super) struct TruncationInfo {
    pub original_bytes: usize,
    pub returned_bytes: usize,
    pub original_lines: usize,
    pub returned_lines: usize,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SearchParams {
    pub query: String,
    pub limit: Option<usize>,
    pub pane: Option<u64>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub snippets: Option<bool>,
    pub mode: Option<UnifiedSearchMode>,
}

// ── CASS params ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct CassSearchParams {
    pub query: String,
    #[serde(default = "default_cass_limit")]
    pub limit: usize,
    #[serde(default = "default_cass_offset")]
    pub offset: usize,
    pub agent: Option<String>,
    pub workspace: Option<String>,
    pub days: Option<u32>,
    pub fields: Option<String>,
    pub max_tokens: Option<usize>,
    #[serde(default = "default_cass_timeout_secs")]
    pub timeout_secs: u64,
}

pub(super) fn default_cass_limit() -> usize {
    10
}

pub(super) fn default_cass_offset() -> usize {
    0
}

pub(super) fn default_cass_timeout_secs() -> u64 {
    15
}

#[derive(Debug, Deserialize)]
pub(super) struct CassViewParams {
    pub source_path: String,
    pub line_number: usize,
    #[serde(default = "default_cass_context_lines")]
    pub context_lines: usize,
    #[serde(default = "default_cass_timeout_secs")]
    pub timeout_secs: u64,
}

pub(super) fn default_cass_context_lines() -> usize {
    10
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CassStatusParams {
    #[serde(default = "default_cass_timeout_secs")]
    pub timeout_secs: u64,
}

// ── Search response ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct McpSearchData {
    pub query: String,
    pub results: Vec<McpSearchHit>,
    pub total_hits: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_filter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_filter: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_filter: Option<i64>,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpSearchHit {
    pub segment_id: i64,
    pub pane_id: u64,
    pub seq: u64,
    pub captured_at: i64,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fusion_rank: Option<usize>,
}

// ── Events ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub(super) struct EventsParams {
    #[serde(default = "default_events_limit")]
    pub limit: usize,
    pub pane: Option<u64>,
    pub rule_id: Option<String>,
    pub event_type: Option<String>,
    pub triage_state: Option<String>,
    pub label: Option<String>,
    #[serde(default)]
    pub unhandled: bool,
    pub since: Option<i64>,
}

pub(super) fn default_events_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
pub(super) struct McpEventsData {
    pub events: Vec<McpEventItem>,
    pub total_count: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_filter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triage_state_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_filter: Option<String>,
    pub unhandled_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_filter: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpEventItem {
    pub id: i64,
    pub pane_id: u64,
    pub rule_id: String,
    pub pack_id: String,
    pub event_type: String,
    pub severity: String,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<crate::storage::EventAnnotations>,
    pub captured_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handled_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

// ── Send / WaitFor ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct SendParams {
    pub pane_id: u64,
    pub text: String,
    #[serde(default)]
    pub dry_run: bool,
    pub wait_for: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub wait_for_regex: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct WaitForParams {
    pub pane_id: u64,
    pub pattern: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_wait_tail")]
    pub tail: usize,
    #[serde(default)]
    pub regex: bool,
}

pub(super) fn default_timeout_secs() -> u64 {
    30
}

pub(super) fn default_wait_tail() -> usize {
    200
}

#[derive(Debug, Serialize)]
pub(super) struct McpWaitForData {
    pub pane_id: u64,
    pub pattern: String,
    pub matched: bool,
    pub elapsed_ms: u64,
    pub polls: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_regex: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct McpSendData {
    pub pane_id: u64,
    pub injection: InjectionResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<McpWaitForData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_error: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
}

// ── Workflow ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct WorkflowRunParams {
    pub name: String,
    pub pane_id: u64,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct McpWorkflowRunData {
    pub workflow_name: String,
    pub pane_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps_executed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

// ── Transaction params/data ──────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub(super) struct TxPlanParams {
    #[serde(default)]
    pub contract_file: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TxRunParams {
    #[serde(default)]
    pub contract_file: Option<String>,
    #[serde(default)]
    pub fail_step: Option<String>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub kill_switch: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TxRollbackParams {
    #[serde(default)]
    pub contract_file: Option<String>,
    #[serde(default)]
    pub fail_compensation_for_step: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TxShowParams {
    #[serde(default)]
    pub contract_file: Option<String>,
    #[serde(default)]
    pub include_contract: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpTxTransitionInfo {
    pub kind: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpTxPlanData {
    pub contract_file: String,
    pub tx_id: String,
    pub plan_id: String,
    pub lifecycle_state: crate::plan::MissionTxState,
    pub step_count: usize,
    pub precondition_count: usize,
    pub compensation_count: usize,
    pub legal_transitions: Vec<McpTxTransitionInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpTxRunData {
    pub contract_file: String,
    pub tx_id: String,
    pub plan_id: String,
    pub prepare_report: crate::plan::TxPrepareReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_report: Option<crate::plan::TxCommitReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compensation_report: Option<crate::plan::TxCompensationReport>,
    pub final_state: crate::plan::MissionTxState,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpTxRollbackData {
    pub contract_file: String,
    pub tx_id: String,
    pub plan_id: String,
    pub compensation_report: crate::plan::TxCompensationReport,
    pub final_state: crate::plan::MissionTxState,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpTxShowData {
    pub contract_file: String,
    pub tx_id: String,
    pub plan_id: String,
    pub lifecycle_state: crate::plan::MissionTxState,
    pub outcome: crate::plan::TxOutcome,
    pub step_count: usize,
    pub precondition_count: usize,
    pub compensation_count: usize,
    pub receipt_count: usize,
    pub legal_transitions: Vec<McpTxTransitionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<crate::plan::MissionTxContract>,
}

// ── Mission MCP params/data types ────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub(super) struct MissionStateParams {
    #[serde(default)]
    pub mission_file: Option<String>,
    #[serde(default)]
    pub mission_state: Option<String>,
    #[serde(default)]
    pub run_state: Option<String>,
    #[serde(default)]
    pub agent_state: Option<String>,
    #[serde(default)]
    pub action_state: Option<String>,
    #[serde(default)]
    pub assignment_id: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MissionExplainParams {
    #[serde(default)]
    pub mission_file: Option<String>,
    #[serde(default)]
    pub assignment_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MissionPauseParams {
    #[serde(default)]
    pub mission_file: Option<String>,
    pub reason: Option<String>,
    #[serde(default = "mcp_default_requested_by")]
    pub requested_by: String,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MissionResumeParams {
    #[serde(default)]
    pub mission_file: Option<String>,
    #[serde(default = "mcp_default_requested_by")]
    pub requested_by: String,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MissionAbortParams {
    #[serde(default)]
    pub mission_file: Option<String>,
    pub reason: Option<String>,
    #[serde(default = "mcp_default_requested_by")]
    pub requested_by: String,
    #[serde(default)]
    pub error_code: Option<String>,
}

pub(super) fn mcp_default_requested_by() -> String {
    "mcp-agent".to_string()
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpMissionTransitionInfo {
    pub kind: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpMissionAssignmentCounters {
    pub pending_approval: usize,
    pub approved: usize,
    pub denied: usize,
    pub expired: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub unresolved: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpMissionAssignmentData {
    pub assignment_id: String,
    pub candidate_id: String,
    pub assignee: String,
    pub run_state: String,
    pub agent_state: String,
    pub action_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpMissionStateData {
    pub mission_file: String,
    pub mission_id: String,
    pub title: String,
    pub mission_hash: String,
    pub lifecycle_state: String,
    pub candidate_count: usize,
    pub assignment_count: usize,
    pub matched_assignment_count: usize,
    pub returned_assignment_count: usize,
    pub assignment_counters: McpMissionAssignmentCounters,
    pub available_transitions: Vec<McpMissionTransitionInfo>,
    pub assignments: Vec<McpMissionAssignmentData>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpMissionExplainData {
    pub mission_file: String,
    pub mission_id: String,
    pub title: String,
    pub lifecycle_state: String,
    pub available_transitions: Vec<McpMissionTransitionInfo>,
    pub failure_catalog: Vec<McpMissionFailureCatalogEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpMissionFailureCatalogEntry {
    pub code: String,
    pub reason_code: String,
    pub error_code: String,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct McpMissionControlData {
    pub command: String,
    pub mission_file: String,
    pub mission_id: String,
    pub lifecycle_from: String,
    pub lifecycle_to: String,
    pub decision_path: String,
    pub reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    pub mission_hash: String,
}

// ── Rules ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub(super) struct RulesListParams {
    pub agent_type: Option<String>,
    #[serde(default)]
    pub verbose: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct McpRulesListData {
    pub rules: Vec<McpRuleItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type_filter: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpRuleItem {
    pub id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub anchor_count: usize,
    pub has_regex: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct RulesTestParams {
    pub text: String,
    #[serde(default)]
    pub trace: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct McpRulesTestData {
    pub text_length: usize,
    pub match_count: usize,
    pub matches: Vec<McpRuleMatchItem>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpRuleMatchItem {
    pub rule_id: String,
    pub agent_type: String,
    pub event_type: String,
    pub severity: String,
    pub confidence: f64,
    pub matched_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<McpRuleTraceInfo>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpRuleTraceInfo {
    pub anchors_checked: bool,
    pub regex_matched: bool,
}

// ── Reservations ─────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub(super) struct ReservationsParams {
    pub pane_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReserveParams {
    pub pane_id: u64,
    pub owner_kind: String,
    pub owner_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default = "default_ttl_ms")]
    pub ttl_ms: i64,
}

pub(super) fn default_ttl_ms() -> i64 {
    300_000 // 5 minutes default
}

#[derive(Debug, Deserialize)]
pub(super) struct ReleaseParams {
    pub reservation_id: i64,
}

#[derive(Debug, Serialize)]
pub(super) struct McpReservationsData {
    pub reservations: Vec<McpReservationInfo>,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_filter: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpReservationInfo {
    pub id: i64,
    pub pane_id: u64,
    pub owner_kind: String,
    pub owner_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_at: Option<i64>,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub(super) struct McpReserveData {
    pub reservation: McpReservationInfo,
}

#[derive(Debug, Serialize)]
pub(super) struct McpReleaseData {
    pub reservation_id: i64,
    pub released: bool,
}

// ── Accounts ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct AccountsParams {
    pub service: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AccountsRefreshParams {
    #[serde(default)]
    pub service: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpAccountsData {
    pub accounts: Vec<McpAccountInfo>,
    pub total: usize,
    pub service: String,
}

#[derive(Debug, Serialize)]
pub(super) struct McpAccountsRefreshData {
    pub service: String,
    pub refreshed_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<String>,
    pub accounts: Vec<McpAccountInfo>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpAccountInfo {
    pub account_id: String,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub percent_remaining: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_remaining: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_limit: Option<i64>,
    pub last_refreshed_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

// ── Generic envelope ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct McpEnvelope<T> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub elapsed_ms: u64,
    pub version: String,
    pub now: u64,
    pub mcp_version: &'static str,
}

impl<T> McpEnvelope<T> {
    pub fn success(data: T, elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms,
            version: crate::VERSION.to_string(),
            now: now_ms(),
            mcp_version: MCP_VERSION,
        }
    }

    pub fn error(
        code: &str,
        msg: impl Into<String>,
        hint: Option<String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
            error_code: Some(code.to_string()),
            hint,
            elapsed_ms,
            version: crate::VERSION.to_string(),
            now: now_ms(),
            mcp_version: MCP_VERSION,
        }
    }
}

// ── Pane state ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct McpPaneState {
    pub pane_id: u64,
    pub pane_uuid: Option<String>,
    pub tab_id: u64,
    pub window_id: u64,
    pub domain: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub observed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpWorkflowsData {
    pub workflows: Vec<McpWorkflowItem>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub(super) struct McpWorkflowItem {
    pub name: String,
    pub description: String,
    pub step_count: usize,
    pub trigger_event_types: Vec<String>,
    pub trigger_rule_ids: Vec<String>,
    pub supported_agent_types: Vec<String>,
    pub requires_pane: bool,
    pub requires_approval: bool,
    pub can_abort: bool,
    pub destructive: bool,
}

impl McpPaneState {
    pub fn from_pane_info(info: &PaneInfo, filter: &PaneFilterConfig) -> Self {
        let domain = info.inferred_domain();
        let title = info.title.clone().unwrap_or_default();
        let cwd = info.cwd.clone().unwrap_or_default();

        let ignore_reason = filter.check_pane(&domain, &title, &cwd);

        Self {
            pane_id: info.pane_id,
            pane_uuid: None,
            tab_id: info.tab_id,
            window_id: info.window_id,
            domain,
            title: info.title.clone(),
            cwd: info.cwd.clone(),
            observed: ignore_reason.is_none(),
            ignore_reason,
        }
    }
}

// ── Tail truncation ──────────────────────────────────────────────────────

pub(super) fn apply_tail_truncation(
    text: &str,
    tail_lines: usize,
) -> (String, bool, Option<TruncationInfo>) {
    let lines: Vec<&str> = text.lines().collect();
    let original_lines = lines.len();
    let original_bytes = text.len();

    if lines.len() <= tail_lines {
        return (text.to_string(), false, None);
    }

    let start_idx = lines.len().saturating_sub(tail_lines);
    let truncated_lines: Vec<&str> = lines[start_idx..].to_vec();
    let truncated_text = truncated_lines.join("\n");
    let returned_bytes = truncated_text.len();
    let returned_lines = truncated_lines.len();

    (
        truncated_text,
        true,
        Some(TruncationInfo {
            original_bytes,
            returned_bytes,
            original_lines,
            returned_lines,
        }),
    )
}

// ── Event mutation types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct EventsAnnotateParams {
    pub event_id: i64,
    pub note: Option<String>,
    #[serde(default)]
    pub clear: bool,
    pub by: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct EventsTriageParams {
    pub event_id: i64,
    pub state: Option<String>,
    #[serde(default)]
    pub clear: bool,
    pub by: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct EventsLabelParams {
    pub event_id: i64,
    pub add: Option<String>,
    pub remove: Option<String>,
    #[serde(default)]
    pub list: bool,
    pub by: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct McpEventMutationData {
    pub event_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    pub annotations: crate::storage::EventAnnotations,
}

// ── IPC pane state (internal) ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct IpcPaneState {
    pub pane_id: u64,
    pub known: bool,
    #[serde(default)]
    pub observed: Option<bool>,
    #[serde(default)]
    pub alt_screen: Option<bool>,
    #[serde(default)]
    pub last_status_at: Option<i64>,
    #[serde(default)]
    pub in_gap: Option<bool>,
    #[serde(default)]
    pub cursor_alt_screen: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub(super) struct CapabilityResolution {
    pub capabilities: PaneCapabilities,
    pub _warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // now_ms Tests
    // ========================================================================

    #[test]
    fn now_ms_returns_reasonable_epoch() {
        let ms = now_ms();
        // Should be after 2024-01-01 (1704067200000ms)
        assert!(
            ms > 1_704_067_200_000,
            "now_ms returned suspiciously low value: {ms}"
        );
        // Should be before 2030-01-01 (1893456000000ms)
        assert!(
            ms < 1_893_456_000_000,
            "now_ms returned suspiciously high value: {ms}"
        );
    }

    #[test]
    fn now_ms_is_monotonic_ish() {
        let a = now_ms();
        let b = now_ms();
        // b should be >= a (may be equal due to millisecond granularity)
        assert!(b >= a, "now_ms went backwards: {a} > {b}");
    }

    // ========================================================================
    // default_tail Tests
    // ========================================================================

    #[test]
    fn default_tail_is_500() {
        assert_eq!(default_tail(), 500);
    }

    // ========================================================================
    // MCP_VERSION Tests
    // ========================================================================

    #[test]
    fn mcp_version_is_v1() {
        assert_eq!(MCP_VERSION, "v1");
    }

    // ========================================================================
    // McpEnvelope Tests
    // ========================================================================

    #[test]
    fn envelope_success_fields() {
        let envelope = McpEnvelope::success("hello", 42);
        assert!(envelope.ok);
        assert_eq!(envelope.data, Some("hello"));
        assert!(envelope.error.is_none());
        assert!(envelope.error_code.is_none());
        assert!(envelope.hint.is_none());
        assert_eq!(envelope.elapsed_ms, 42);
        assert_eq!(envelope.mcp_version, "v1");
        assert!(envelope.now > 0);
    }

    #[test]
    fn envelope_error_fields() {
        let envelope =
            McpEnvelope::<()>::error("FT-MCP-0001", "bad input", Some("fix it".to_string()), 10);
        assert!(!envelope.ok);
        assert!(envelope.data.is_none());
        assert_eq!(envelope.error.as_deref(), Some("bad input"));
        assert_eq!(envelope.error_code.as_deref(), Some("FT-MCP-0001"));
        assert_eq!(envelope.hint.as_deref(), Some("fix it"));
        assert_eq!(envelope.elapsed_ms, 10);
    }

    #[test]
    fn envelope_error_no_hint() {
        let envelope = McpEnvelope::<()>::error("FT-MCP-0005", "storage error", None, 0);
        assert!(!envelope.ok);
        assert!(envelope.hint.is_none());
    }

    #[test]
    fn envelope_success_serializes_to_json() {
        let envelope = McpEnvelope::success(serde_json::json!({"count": 5}), 100);
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["count"], 5);
        assert_eq!(parsed["elapsed_ms"], 100);
        assert_eq!(parsed["mcp_version"], "v1");
        // Optional None fields should be absent
        assert!(parsed.get("error").is_none());
        assert!(parsed.get("error_code").is_none());
        assert!(parsed.get("hint").is_none());
    }

    #[test]
    fn envelope_error_serializes_to_json() {
        let envelope = McpEnvelope::<()>::error("ERR", "fail", Some("try again".to_string()), 5);
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "fail");
        assert_eq!(parsed["error_code"], "ERR");
        assert_eq!(parsed["hint"], "try again");
        assert!(parsed.get("data").is_none());
    }

    // ========================================================================
    // StateParams Deserialization
    // ========================================================================

    #[test]
    fn state_params_default() {
        let params = StateParams::default();
        assert!(params.domain.is_none());
        assert!(params.agent.is_none());
        assert!(params.pane_id.is_none());
    }

    #[test]
    fn state_params_from_json() {
        let json = serde_json::json!({"domain": "local", "pane_id": 42});
        let params: StateParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.domain.as_deref(), Some("local"));
        assert_eq!(params.pane_id, Some(42));
        assert!(params.agent.is_none());
    }

    // ========================================================================
    // GetTextParams Deserialization
    // ========================================================================

    #[test]
    fn get_text_params_defaults() {
        let json = serde_json::json!({"pane_id": 1});
        let params: GetTextParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.pane_id, 1);
        assert_eq!(params.tail, 500); // default_tail()
        assert!(!params.escapes); // default false
    }

    #[test]
    fn get_text_params_override_all() {
        let json = serde_json::json!({"pane_id": 5, "tail": 100, "escapes": true});
        let params: GetTextParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.pane_id, 5);
        assert_eq!(params.tail, 100);
        assert!(params.escapes);
    }

    // ========================================================================
    // IpcPaneState Deserialization
    // ========================================================================

    #[test]
    fn ipc_pane_state_minimal_json() {
        let json = serde_json::json!({"pane_id": 1, "known": true});
        let state: IpcPaneState = serde_json::from_value(json).unwrap();
        assert_eq!(state.pane_id, 1);
        assert!(state.known);
        assert!(state.observed.is_none());
        assert!(state.alt_screen.is_none());
        assert!(state.last_status_at.is_none());
        assert!(state.in_gap.is_none());
        assert!(state.cursor_alt_screen.is_none());
        assert!(state.reason.is_none());
    }

    #[test]
    fn ipc_pane_state_full_json() {
        let json = serde_json::json!({
            "pane_id": 42,
            "known": true,
            "observed": true,
            "alt_screen": false,
            "last_status_at": 1234567890,
            "in_gap": false,
            "cursor_alt_screen": true,
            "reason": "test"
        });
        let state: IpcPaneState = serde_json::from_value(json).unwrap();
        assert_eq!(state.pane_id, 42);
        assert_eq!(state.observed, Some(true));
        assert_eq!(state.alt_screen, Some(false));
        assert_eq!(state.last_status_at, Some(1234567890));
        assert_eq!(state.in_gap, Some(false));
        assert_eq!(state.cursor_alt_screen, Some(true));
        assert_eq!(state.reason.as_deref(), Some("test"));
    }

    // ========================================================================
    // Property-Based Tests
    // ========================================================================

    use proptest::prelude::*;

    fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
        proptest::option::of("[a-zA-Z0-9_]{1,20}")
    }

    fn arb_string() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_]{1,20}".prop_map(|s| s)
    }

    fn arb_finite_f64() -> impl Strategy<Value = f64> {
        any::<f64>().prop_filter("must be finite", |x| x.is_finite())
    }

    // ── Deserialize param types ──────────────────────────────────────────

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        // 1. StateParams
        #[test]
        fn prop_state_params_deser(
            domain in arb_opt_string(),
            agent in arb_opt_string(),
            pane_id in proptest::option::of(any::<u64>()),
        ) {
            let json = serde_json::json!({
                "domain": domain,
                "agent": agent,
                "pane_id": pane_id,
            });
            let p: StateParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.domain, domain);
            prop_assert_eq!(p.agent, agent);
            prop_assert_eq!(p.pane_id, pane_id);
        }

        // 2. GetTextParams
        #[test]
        fn prop_get_text_params_deser(
            pane_id in any::<u64>(),
            tail in any::<usize>(),
            escapes in any::<bool>(),
        ) {
            let json = serde_json::json!({
                "pane_id": pane_id,
                "tail": tail,
                "escapes": escapes,
            });
            let p: GetTextParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.pane_id, pane_id);
            prop_assert_eq!(p.tail, tail);
            prop_assert_eq!(p.escapes, escapes);
        }

        // 3. GetTextParams defaults
        #[test]
        fn prop_get_text_params_defaults(pane_id in any::<u64>()) {
            let json = serde_json::json!({"pane_id": pane_id});
            let p: GetTextParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.pane_id, pane_id);
            prop_assert_eq!(p.tail, 500);
            prop_assert!(!p.escapes);
        }

        // 4. CassSearchParams
        #[test]
        fn prop_cass_search_params_deser(
            query in arb_string(),
            limit in any::<usize>(),
            offset in any::<usize>(),
            agent in arb_opt_string(),
            workspace in arb_opt_string(),
            days in proptest::option::of(any::<u32>()),
            fields in arb_opt_string(),
            max_tokens in proptest::option::of(any::<usize>()),
            timeout_secs in any::<u64>(),
        ) {
            let json = serde_json::json!({
                "query": query,
                "limit": limit,
                "offset": offset,
                "agent": agent,
                "workspace": workspace,
                "days": days,
                "fields": fields,
                "max_tokens": max_tokens,
                "timeout_secs": timeout_secs,
            });
            let p: CassSearchParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.query, &query);
            prop_assert_eq!(p.limit, limit);
            prop_assert_eq!(p.offset, offset);
            prop_assert_eq!(p.agent, agent);
            prop_assert_eq!(p.workspace, workspace);
            prop_assert_eq!(p.days, days);
            prop_assert_eq!(p.fields, fields);
            prop_assert_eq!(p.max_tokens, max_tokens);
            prop_assert_eq!(p.timeout_secs, timeout_secs);
        }

        // 5. CassSearchParams defaults
        #[test]
        fn prop_cass_search_params_defaults(query in arb_string()) {
            let json = serde_json::json!({"query": query});
            let p: CassSearchParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.query, &query);
            prop_assert_eq!(p.limit, 10);
            prop_assert_eq!(p.offset, 0);
            prop_assert_eq!(p.timeout_secs, 15);
        }

        // 6. CassViewParams
        #[test]
        fn prop_cass_view_params_deser(
            source_path in arb_string(),
            line_number in any::<usize>(),
            context_lines in any::<usize>(),
            timeout_secs in any::<u64>(),
        ) {
            let json = serde_json::json!({
                "source_path": source_path,
                "line_number": line_number,
                "context_lines": context_lines,
                "timeout_secs": timeout_secs,
            });
            let p: CassViewParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.source_path, &source_path);
            prop_assert_eq!(p.line_number, line_number);
            prop_assert_eq!(p.context_lines, context_lines);
            prop_assert_eq!(p.timeout_secs, timeout_secs);
        }

        // 7. CassViewParams defaults
        #[test]
        fn prop_cass_view_params_defaults(
            source_path in arb_string(),
            line_number in any::<usize>(),
        ) {
            let json = serde_json::json!({
                "source_path": source_path,
                "line_number": line_number,
            });
            let p: CassViewParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.context_lines, 10);
            prop_assert_eq!(p.timeout_secs, 15);
        }

        // 8. CassStatusParams defaults
        #[test]
        fn prop_cass_status_params_defaults(_dummy in 0u8..1) {
            let json = serde_json::json!({});
            let p: CassStatusParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.timeout_secs, 15);
        }

        // 9. EventsParams
        #[test]
        fn prop_events_params_deser(
            limit in any::<usize>(),
            pane in proptest::option::of(any::<u64>()),
            rule_id in arb_opt_string(),
            event_type in arb_opt_string(),
            triage_state in arb_opt_string(),
            label in arb_opt_string(),
            unhandled in any::<bool>(),
            since in proptest::option::of(any::<i64>()),
        ) {
            let json = serde_json::json!({
                "limit": limit,
                "pane": pane,
                "rule_id": rule_id,
                "event_type": event_type,
                "triage_state": triage_state,
                "label": label,
                "unhandled": unhandled,
                "since": since,
            });
            let p: EventsParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.limit, limit);
            prop_assert_eq!(p.pane, pane);
            prop_assert_eq!(p.rule_id, rule_id);
            prop_assert_eq!(p.event_type, event_type);
            prop_assert_eq!(p.triage_state, triage_state);
            prop_assert_eq!(p.label, label);
            prop_assert_eq!(p.unhandled, unhandled);
            prop_assert_eq!(p.since, since);
        }

        // 10. EventsParams defaults
        #[test]
        fn prop_events_params_defaults(_dummy in 0u8..1) {
            let json = serde_json::json!({});
            let p: EventsParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.limit, 20);
            prop_assert!(!p.unhandled);
        }

        // 11. SendParams
        #[test]
        fn prop_send_params_deser(
            pane_id in any::<u64>(),
            text in arb_string(),
            dry_run in any::<bool>(),
            wait_for in arb_opt_string(),
            timeout_secs in any::<u64>(),
            wait_for_regex in any::<bool>(),
        ) {
            let json = serde_json::json!({
                "pane_id": pane_id,
                "text": text,
                "dry_run": dry_run,
                "wait_for": wait_for,
                "timeout_secs": timeout_secs,
                "wait_for_regex": wait_for_regex,
            });
            let p: SendParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.pane_id, pane_id);
            prop_assert_eq!(&p.text, &text);
            prop_assert_eq!(p.dry_run, dry_run);
            prop_assert_eq!(p.wait_for, wait_for);
            prop_assert_eq!(p.timeout_secs, timeout_secs);
            prop_assert_eq!(p.wait_for_regex, wait_for_regex);
        }

        // 12. SendParams defaults
        #[test]
        fn prop_send_params_defaults(pane_id in any::<u64>(), text in arb_string()) {
            let json = serde_json::json!({"pane_id": pane_id, "text": text});
            let p: SendParams = serde_json::from_value(json).unwrap();
            prop_assert!(!p.dry_run);
            prop_assert_eq!(p.timeout_secs, 30);
            prop_assert!(!p.wait_for_regex);
            prop_assert!(p.wait_for.is_none());
        }

        // 13. WaitForParams
        #[test]
        fn prop_wait_for_params_deser(
            pane_id in any::<u64>(),
            pattern in arb_string(),
            timeout_secs in any::<u64>(),
            tail in any::<usize>(),
            regex in any::<bool>(),
        ) {
            let json = serde_json::json!({
                "pane_id": pane_id,
                "pattern": pattern,
                "timeout_secs": timeout_secs,
                "tail": tail,
                "regex": regex,
            });
            let p: WaitForParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.pane_id, pane_id);
            prop_assert_eq!(&p.pattern, &pattern);
            prop_assert_eq!(p.timeout_secs, timeout_secs);
            prop_assert_eq!(p.tail, tail);
            prop_assert_eq!(p.regex, regex);
        }

        // 14. WaitForParams defaults
        #[test]
        fn prop_wait_for_params_defaults(
            pane_id in any::<u64>(),
            pattern in arb_string(),
        ) {
            let json = serde_json::json!({"pane_id": pane_id, "pattern": pattern});
            let p: WaitForParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.timeout_secs, 30);
            prop_assert_eq!(p.tail, 200);
            prop_assert!(!p.regex);
        }

        // 15. WorkflowRunParams
        #[test]
        fn prop_workflow_run_params_deser(
            name in arb_string(),
            pane_id in any::<u64>(),
            force in any::<bool>(),
            dry_run in any::<bool>(),
        ) {
            let json = serde_json::json!({
                "name": name,
                "pane_id": pane_id,
                "force": force,
                "dry_run": dry_run,
            });
            let p: WorkflowRunParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.name, &name);
            prop_assert_eq!(p.pane_id, pane_id);
            prop_assert_eq!(p.force, force);
            prop_assert_eq!(p.dry_run, dry_run);
        }

        // 16. TxPlanParams
        #[test]
        fn prop_tx_plan_params_deser(contract_file in arb_opt_string()) {
            let json = serde_json::json!({"contract_file": contract_file});
            let p: TxPlanParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.contract_file, contract_file);
        }

        // 17. TxRunParams
        #[test]
        fn prop_tx_run_params_deser(
            contract_file in arb_opt_string(),
            fail_step in arb_opt_string(),
            paused in any::<bool>(),
            kill_switch in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "contract_file": contract_file,
                "fail_step": fail_step,
                "paused": paused,
                "kill_switch": kill_switch,
            });
            let p: TxRunParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.contract_file, contract_file);
            prop_assert_eq!(p.fail_step, fail_step);
            prop_assert_eq!(p.paused, paused);
            prop_assert_eq!(p.kill_switch, kill_switch);
        }

        // 18. TxRollbackParams
        #[test]
        fn prop_tx_rollback_params_deser(
            contract_file in arb_opt_string(),
            fail_compensation_for_step in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "contract_file": contract_file,
                "fail_compensation_for_step": fail_compensation_for_step,
            });
            let p: TxRollbackParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.contract_file, contract_file);
            prop_assert_eq!(p.fail_compensation_for_step, fail_compensation_for_step);
        }

        // 19. TxShowParams
        #[test]
        fn prop_tx_show_params_deser(
            contract_file in arb_opt_string(),
            include_contract in any::<bool>(),
        ) {
            let json = serde_json::json!({
                "contract_file": contract_file,
                "include_contract": include_contract,
            });
            let p: TxShowParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.contract_file, contract_file);
            prop_assert_eq!(p.include_contract, include_contract);
        }

        // 20. MissionStateParams
        #[test]
        fn prop_mission_state_params_deser(
            mission_file in arb_opt_string(),
            mission_state in arb_opt_string(),
            run_state in arb_opt_string(),
            agent_state in arb_opt_string(),
            action_state in arb_opt_string(),
            assignment_id in arb_opt_string(),
            assignee in arb_opt_string(),
            limit in proptest::option::of(any::<usize>()),
        ) {
            let json = serde_json::json!({
                "mission_file": mission_file,
                "mission_state": mission_state,
                "run_state": run_state,
                "agent_state": agent_state,
                "action_state": action_state,
                "assignment_id": assignment_id,
                "assignee": assignee,
                "limit": limit,
            });
            let p: MissionStateParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.mission_file, mission_file);
            prop_assert_eq!(p.mission_state, mission_state);
            prop_assert_eq!(p.run_state, run_state);
            prop_assert_eq!(p.agent_state, agent_state);
            prop_assert_eq!(p.action_state, action_state);
            prop_assert_eq!(p.assignment_id, assignment_id);
            prop_assert_eq!(p.assignee, assignee);
            prop_assert_eq!(p.limit, limit);
        }

        // 21. MissionExplainParams
        #[test]
        fn prop_mission_explain_params_deser(
            mission_file in arb_opt_string(),
            assignment_id in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "mission_file": mission_file,
                "assignment_id": assignment_id,
            });
            let p: MissionExplainParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.mission_file, mission_file);
            prop_assert_eq!(p.assignment_id, assignment_id);
        }

        // 22. MissionPauseParams
        #[test]
        fn prop_mission_pause_params_deser(
            mission_file in arb_opt_string(),
            reason in arb_opt_string(),
            requested_by in arb_string(),
        ) {
            let json = serde_json::json!({
                "mission_file": mission_file,
                "reason": reason,
                "requested_by": requested_by,
            });
            let p: MissionPauseParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.mission_file, mission_file);
            prop_assert_eq!(p.reason, reason);
            prop_assert_eq!(&p.requested_by, &requested_by);
        }

        // 23. MissionPauseParams defaults
        #[test]
        fn prop_mission_pause_params_defaults(_dummy in 0u8..1) {
            let json = serde_json::json!({});
            let p: MissionPauseParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.requested_by, "mcp-agent");
        }

        // 24. MissionResumeParams
        #[test]
        fn prop_mission_resume_params_deser(
            mission_file in arb_opt_string(),
            requested_by in arb_string(),
        ) {
            let json = serde_json::json!({
                "mission_file": mission_file,
                "requested_by": requested_by,
            });
            let p: MissionResumeParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.mission_file, mission_file);
            prop_assert_eq!(&p.requested_by, &requested_by);
        }

        // 25. MissionAbortParams
        #[test]
        fn prop_mission_abort_params_deser(
            mission_file in arb_opt_string(),
            reason in arb_opt_string(),
            requested_by in arb_string(),
            error_code in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "mission_file": mission_file,
                "reason": reason,
                "requested_by": requested_by,
                "error_code": error_code,
            });
            let p: MissionAbortParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.mission_file, mission_file);
            prop_assert_eq!(p.reason, reason);
            prop_assert_eq!(&p.requested_by, &requested_by);
            prop_assert_eq!(p.error_code, error_code);
        }

        // 26. RulesListParams
        #[test]
        fn prop_rules_list_params_deser(
            agent_type in arb_opt_string(),
            verbose in any::<bool>(),
        ) {
            let json = serde_json::json!({
                "agent_type": agent_type,
                "verbose": verbose,
            });
            let p: RulesListParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.agent_type, agent_type);
            prop_assert_eq!(p.verbose, verbose);
        }

        // 27. RulesTestParams
        #[test]
        fn prop_rules_test_params_deser(
            text in arb_string(),
            trace in any::<bool>(),
        ) {
            let json = serde_json::json!({
                "text": text,
                "trace": trace,
            });
            let p: RulesTestParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.text, &text);
            prop_assert_eq!(p.trace, trace);
        }

        // 28. ReservationsParams
        #[test]
        fn prop_reservations_params_deser(
            pane_id in proptest::option::of(any::<u64>()),
        ) {
            let json = serde_json::json!({"pane_id": pane_id});
            let p: ReservationsParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.pane_id, pane_id);
        }

        // 29. ReserveParams
        #[test]
        fn prop_reserve_params_deser(
            pane_id in any::<u64>(),
            owner_kind in arb_string(),
            owner_id in arb_string(),
            reason in arb_opt_string(),
            ttl_ms in any::<i64>(),
        ) {
            let json = serde_json::json!({
                "pane_id": pane_id,
                "owner_kind": owner_kind,
                "owner_id": owner_id,
                "reason": reason,
                "ttl_ms": ttl_ms,
            });
            let p: ReserveParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.pane_id, pane_id);
            prop_assert_eq!(&p.owner_kind, &owner_kind);
            prop_assert_eq!(&p.owner_id, &owner_id);
            prop_assert_eq!(p.reason, reason);
            prop_assert_eq!(p.ttl_ms, ttl_ms);
        }

        // 30. ReserveParams defaults
        #[test]
        fn prop_reserve_params_defaults(
            pane_id in any::<u64>(),
            owner_kind in arb_string(),
            owner_id in arb_string(),
        ) {
            let json = serde_json::json!({
                "pane_id": pane_id,
                "owner_kind": owner_kind,
                "owner_id": owner_id,
            });
            let p: ReserveParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.ttl_ms, 300_000);
        }

        // 31. ReleaseParams
        #[test]
        fn prop_release_params_deser(reservation_id in any::<i64>()) {
            let json = serde_json::json!({"reservation_id": reservation_id});
            let p: ReleaseParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.reservation_id, reservation_id);
        }

        // 32. AccountsParams
        #[test]
        fn prop_accounts_params_deser(service in arb_string()) {
            let json = serde_json::json!({"service": service});
            let p: AccountsParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.service, &service);
        }

        // 33. AccountsRefreshParams
        #[test]
        fn prop_accounts_refresh_params_deser(service in arb_opt_string()) {
            let json = serde_json::json!({"service": service});
            let p: AccountsRefreshParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.service, service);
        }

        // 34. EventsAnnotateParams
        #[test]
        fn prop_events_annotate_params_deser(
            event_id in any::<i64>(),
            note in arb_opt_string(),
            clear in any::<bool>(),
            by in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "event_id": event_id,
                "note": note,
                "clear": clear,
                "by": by,
            });
            let p: EventsAnnotateParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.event_id, event_id);
            prop_assert_eq!(p.note, note);
            prop_assert_eq!(p.clear, clear);
            prop_assert_eq!(p.by, by);
        }

        // 35. EventsTriageParams
        #[test]
        fn prop_events_triage_params_deser(
            event_id in any::<i64>(),
            state in arb_opt_string(),
            clear in any::<bool>(),
            by in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "event_id": event_id,
                "state": state,
                "clear": clear,
                "by": by,
            });
            let p: EventsTriageParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.event_id, event_id);
            prop_assert_eq!(p.state, state);
            prop_assert_eq!(p.clear, clear);
            prop_assert_eq!(p.by, by);
        }

        // 36. EventsLabelParams
        #[test]
        fn prop_events_label_params_deser(
            event_id in any::<i64>(),
            add in arb_opt_string(),
            remove in arb_opt_string(),
            list in any::<bool>(),
            by in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "event_id": event_id,
                "add": add,
                "remove": remove,
                "list": list,
                "by": by,
            });
            let p: EventsLabelParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.event_id, event_id);
            prop_assert_eq!(p.add, add);
            prop_assert_eq!(p.remove, remove);
            prop_assert_eq!(p.list, list);
            prop_assert_eq!(p.by, by);
        }

        // 37. IpcPaneState
        #[test]
        fn prop_ipc_pane_state_deser(
            pane_id in any::<u64>(),
            known in any::<bool>(),
            observed in proptest::option::of(any::<bool>()),
            alt_screen in proptest::option::of(any::<bool>()),
            last_status_at in proptest::option::of(any::<i64>()),
            in_gap in proptest::option::of(any::<bool>()),
            cursor_alt_screen in proptest::option::of(any::<bool>()),
            reason in arb_opt_string(),
        ) {
            let json = serde_json::json!({
                "pane_id": pane_id,
                "known": known,
                "observed": observed,
                "alt_screen": alt_screen,
                "last_status_at": last_status_at,
                "in_gap": in_gap,
                "cursor_alt_screen": cursor_alt_screen,
                "reason": reason,
            });
            let p: IpcPaneState = serde_json::from_value(json).unwrap();
            prop_assert_eq!(p.pane_id, pane_id);
            prop_assert_eq!(p.known, known);
            prop_assert_eq!(p.observed, observed);
            prop_assert_eq!(p.alt_screen, alt_screen);
            prop_assert_eq!(p.last_status_at, last_status_at);
            prop_assert_eq!(p.in_gap, in_gap);
            prop_assert_eq!(p.cursor_alt_screen, cursor_alt_screen);
            prop_assert_eq!(p.reason, reason);
        }

        // 38. SearchParams (without UnifiedSearchMode since it's external)
        #[test]
        fn prop_search_params_deser(
            query in arb_string(),
            limit in proptest::option::of(any::<usize>()),
            pane in proptest::option::of(any::<u64>()),
            since in proptest::option::of(any::<i64>()),
            until in proptest::option::of(any::<i64>()),
            snippets in proptest::option::of(any::<bool>()),
        ) {
            let json = serde_json::json!({
                "query": query,
                "limit": limit,
                "pane": pane,
                "since": since,
                "until": until,
                "snippets": snippets,
            });
            let p: SearchParams = serde_json::from_value(json).unwrap();
            prop_assert_eq!(&p.query, &query);
            prop_assert_eq!(p.limit, limit);
            prop_assert_eq!(p.pane, pane);
            prop_assert_eq!(p.since, since);
            prop_assert_eq!(p.until, until);
            prop_assert_eq!(p.snippets, snippets);
        }
    }

    // ── Serialize data types ─────────────────────────────────────────────

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        // 39. TruncationInfo
        #[test]
        fn prop_truncation_info_ser(
            original_bytes in any::<usize>(),
            returned_bytes in any::<usize>(),
            original_lines in any::<usize>(),
            returned_lines in any::<usize>(),
        ) {
            let info = TruncationInfo {
                original_bytes,
                returned_bytes,
                original_lines,
                returned_lines,
            };
            let v = serde_json::to_value(&info).unwrap();
            prop_assert_eq!(v["original_bytes"].as_u64().unwrap(), original_bytes as u64);
            prop_assert_eq!(v["returned_bytes"].as_u64().unwrap(), returned_bytes as u64);
            prop_assert_eq!(v["original_lines"].as_u64().unwrap(), original_lines as u64);
            prop_assert_eq!(v["returned_lines"].as_u64().unwrap(), returned_lines as u64);
        }

        // 40. McpRuleTraceInfo
        #[test]
        fn prop_rule_trace_info_ser(
            anchors_checked in any::<bool>(),
            regex_matched in any::<bool>(),
        ) {
            let info = McpRuleTraceInfo {
                anchors_checked,
                regex_matched,
            };
            let v = serde_json::to_value(&info).unwrap();
            prop_assert_eq!(v["anchors_checked"].as_bool().unwrap(), anchors_checked);
            prop_assert_eq!(v["regex_matched"].as_bool().unwrap(), regex_matched);
        }

        // 41. McpTxTransitionInfo
        #[test]
        fn prop_tx_transition_info_ser(
            kind in arb_string(),
            to in arb_string(),
        ) {
            let info = McpTxTransitionInfo {
                kind: kind.clone(),
                to: to.clone(),
            };
            let v = serde_json::to_value(&info).unwrap();
            prop_assert_eq!(v["kind"].as_str().unwrap(), kind.as_str());
            prop_assert_eq!(v["to"].as_str().unwrap(), to.as_str());
        }

        // 42. McpMissionTransitionInfo
        #[test]
        fn prop_mission_transition_info_ser(
            kind in arb_string(),
            from in arb_string(),
            to in arb_string(),
        ) {
            let info = McpMissionTransitionInfo {
                kind: kind.clone(),
                from: from.clone(),
                to: to.clone(),
            };
            let v = serde_json::to_value(&info).unwrap();
            prop_assert_eq!(v["kind"].as_str().unwrap(), kind.as_str());
            prop_assert_eq!(v["from"].as_str().unwrap(), from.as_str());
            prop_assert_eq!(v["to"].as_str().unwrap(), to.as_str());
        }

        // 43. McpMissionAssignmentCounters
        #[test]
        fn prop_mission_assignment_counters_ser(
            pending_approval in any::<usize>(),
            approved in any::<usize>(),
            denied in any::<usize>(),
            expired in any::<usize>(),
            succeeded in any::<usize>(),
            failed in any::<usize>(),
            cancelled in any::<usize>(),
            unresolved in any::<usize>(),
        ) {
            let c = McpMissionAssignmentCounters {
                pending_approval,
                approved,
                denied,
                expired,
                succeeded,
                failed,
                cancelled,
                unresolved,
            };
            let v = serde_json::to_value(&c).unwrap();
            prop_assert_eq!(v["pending_approval"].as_u64().unwrap(), pending_approval as u64);
            prop_assert_eq!(v["approved"].as_u64().unwrap(), approved as u64);
            prop_assert_eq!(v["denied"].as_u64().unwrap(), denied as u64);
            prop_assert_eq!(v["expired"].as_u64().unwrap(), expired as u64);
            prop_assert_eq!(v["succeeded"].as_u64().unwrap(), succeeded as u64);
            prop_assert_eq!(v["failed"].as_u64().unwrap(), failed as u64);
            prop_assert_eq!(v["cancelled"].as_u64().unwrap(), cancelled as u64);
            prop_assert_eq!(v["unresolved"].as_u64().unwrap(), unresolved as u64);
        }

        // 44. McpMissionAssignmentData
        #[test]
        fn prop_mission_assignment_data_ser(
            assignment_id in arb_string(),
            candidate_id in arb_string(),
            assignee in arb_string(),
            run_state in arb_string(),
            agent_state in arb_string(),
            action_state in arb_string(),
            reason_code in arb_opt_string(),
            error_code in arb_opt_string(),
        ) {
            let d = McpMissionAssignmentData {
                assignment_id: assignment_id.clone(),
                candidate_id: candidate_id.clone(),
                assignee: assignee.clone(),
                run_state: run_state.clone(),
                agent_state: agent_state.clone(),
                action_state: action_state.clone(),
                reason_code: reason_code.clone(),
                error_code: error_code.clone(),
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["assignment_id"].as_str().unwrap(), assignment_id.as_str());
            prop_assert_eq!(v["candidate_id"].as_str().unwrap(), candidate_id.as_str());
            prop_assert_eq!(v["assignee"].as_str().unwrap(), assignee.as_str());
            if let Some(ref rc) = reason_code {
                prop_assert_eq!(v["reason_code"].as_str().unwrap(), rc.as_str());
            } else {
                prop_assert!(v.get("reason_code").is_none());
            }
        }

        // 45. McpMissionFailureCatalogEntry
        #[test]
        fn prop_mission_failure_catalog_entry_ser(
            code in arb_string(),
            reason_code in arb_string(),
            error_code in arb_string(),
        ) {
            let e = McpMissionFailureCatalogEntry {
                code: code.clone(),
                reason_code: reason_code.clone(),
                error_code: error_code.clone(),
            };
            let v = serde_json::to_value(&e).unwrap();
            prop_assert_eq!(v["code"].as_str().unwrap(), code.as_str());
            prop_assert_eq!(v["reason_code"].as_str().unwrap(), reason_code.as_str());
            prop_assert_eq!(v["error_code"].as_str().unwrap(), error_code.as_str());
        }

        // 46. McpMissionControlData
        #[test]
        fn prop_mission_control_data_ser(
            command in arb_string(),
            mission_file in arb_string(),
            mission_id in arb_string(),
            lifecycle_from in arb_string(),
            lifecycle_to in arb_string(),
            decision_path in arb_string(),
            reason_code in arb_string(),
            error_code in arb_opt_string(),
            checkpoint_id in arb_opt_string(),
            mission_hash in arb_string(),
        ) {
            let d = McpMissionControlData {
                command: command.clone(),
                mission_file: mission_file.clone(),
                mission_id: mission_id.clone(),
                lifecycle_from: lifecycle_from.clone(),
                lifecycle_to: lifecycle_to.clone(),
                decision_path: decision_path.clone(),
                reason_code: reason_code.clone(),
                error_code: error_code.clone(),
                checkpoint_id: checkpoint_id.clone(),
                mission_hash: mission_hash.clone(),
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["command"].as_str().unwrap(), command.as_str());
            prop_assert_eq!(v["mission_file"].as_str().unwrap(), mission_file.as_str());
            prop_assert_eq!(v["lifecycle_from"].as_str().unwrap(), lifecycle_from.as_str());
            prop_assert_eq!(v["lifecycle_to"].as_str().unwrap(), lifecycle_to.as_str());
            if let Some(ref ec) = error_code {
                prop_assert_eq!(v["error_code"].as_str().unwrap(), ec.as_str());
            } else {
                prop_assert!(v.get("error_code").is_none());
            }
            if let Some(ref ci) = checkpoint_id {
                prop_assert_eq!(v["checkpoint_id"].as_str().unwrap(), ci.as_str());
            } else {
                prop_assert!(v.get("checkpoint_id").is_none());
            }
        }

        // 47. McpRuleItem
        #[test]
        fn prop_rule_item_ser(
            id in arb_string(),
            agent_type in arb_string(),
            event_type in arb_string(),
            severity in arb_string(),
            description in arb_opt_string(),
            workflow in arb_opt_string(),
            anchor_count in any::<usize>(),
            has_regex in any::<bool>(),
        ) {
            let item = McpRuleItem {
                id: id.clone(),
                agent_type: agent_type.clone(),
                event_type: event_type.clone(),
                severity: severity.clone(),
                description: description.clone(),
                workflow: workflow.clone(),
                anchor_count,
                has_regex,
            };
            let v = serde_json::to_value(&item).unwrap();
            prop_assert_eq!(v["id"].as_str().unwrap(), id.as_str());
            prop_assert_eq!(v["agent_type"].as_str().unwrap(), agent_type.as_str());
            prop_assert_eq!(v["anchor_count"].as_u64().unwrap(), anchor_count as u64);
            prop_assert_eq!(v["has_regex"].as_bool().unwrap(), has_regex);
        }

        // 48. McpReleaseData
        #[test]
        fn prop_release_data_ser(
            reservation_id in any::<i64>(),
            released in any::<bool>(),
        ) {
            let d = McpReleaseData {
                reservation_id,
                released,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["reservation_id"].as_i64().unwrap(), reservation_id);
            prop_assert_eq!(v["released"].as_bool().unwrap(), released);
        }

        // 49. McpReservationInfo
        #[test]
        fn prop_reservation_info_ser(
            id in any::<i64>(),
            pane_id in any::<u64>(),
            owner_kind in arb_string(),
            owner_id in arb_string(),
            reason in arb_opt_string(),
            created_at in any::<i64>(),
            expires_at in any::<i64>(),
            released_at in proptest::option::of(any::<i64>()),
            status in arb_string(),
        ) {
            let info = McpReservationInfo {
                id,
                pane_id,
                owner_kind: owner_kind.clone(),
                owner_id: owner_id.clone(),
                reason: reason.clone(),
                created_at,
                expires_at,
                released_at,
                status: status.clone(),
            };
            let v = serde_json::to_value(&info).unwrap();
            prop_assert_eq!(v["id"].as_i64().unwrap(), id);
            prop_assert_eq!(v["pane_id"].as_u64().unwrap(), pane_id);
            prop_assert_eq!(v["owner_kind"].as_str().unwrap(), owner_kind.as_str());
            prop_assert_eq!(v["status"].as_str().unwrap(), status.as_str());
            if let Some(ref r) = reason {
                prop_assert_eq!(v["reason"].as_str().unwrap(), r.as_str());
            } else {
                prop_assert!(v.get("reason").is_none());
            }
        }

        // 50. McpAccountInfo
        #[test]
        fn prop_account_info_ser(
            account_id in arb_string(),
            service in arb_string(),
            name in arb_opt_string(),
            percent_remaining in arb_finite_f64(),
            reset_at in arb_opt_string(),
            tokens_used in proptest::option::of(any::<i64>()),
            tokens_remaining in proptest::option::of(any::<i64>()),
            tokens_limit in proptest::option::of(any::<i64>()),
            last_refreshed_at in any::<i64>(),
            last_used_at in proptest::option::of(any::<i64>()),
        ) {
            let info = McpAccountInfo {
                account_id: account_id.clone(),
                service: service.clone(),
                name: name.clone(),
                percent_remaining,
                reset_at: reset_at.clone(),
                tokens_used,
                tokens_remaining,
                tokens_limit,
                last_refreshed_at,
                last_used_at,
            };
            let v = serde_json::to_value(&info).unwrap();
            prop_assert_eq!(v["account_id"].as_str().unwrap(), account_id.as_str());
            prop_assert_eq!(v["service"].as_str().unwrap(), service.as_str());
            prop_assert_eq!(v["last_refreshed_at"].as_i64().unwrap(), last_refreshed_at);
        }

        // 51. McpPaneState
        #[test]
        fn prop_pane_state_ser(
            pane_id in any::<u64>(),
            pane_uuid in arb_opt_string(),
            tab_id in any::<u64>(),
            window_id in any::<u64>(),
            domain in arb_string(),
            title in arb_opt_string(),
            cwd in arb_opt_string(),
            observed in any::<bool>(),
            ignore_reason in arb_opt_string(),
        ) {
            let s = McpPaneState {
                pane_id,
                pane_uuid: pane_uuid.clone(),
                tab_id,
                window_id,
                domain: domain.clone(),
                title: title.clone(),
                cwd: cwd.clone(),
                observed,
                ignore_reason: ignore_reason.clone(),
            };
            let v = serde_json::to_value(&s).unwrap();
            prop_assert_eq!(v["pane_id"].as_u64().unwrap(), pane_id);
            prop_assert_eq!(v["tab_id"].as_u64().unwrap(), tab_id);
            prop_assert_eq!(v["window_id"].as_u64().unwrap(), window_id);
            prop_assert_eq!(v["domain"].as_str().unwrap(), domain.as_str());
            prop_assert_eq!(v["observed"].as_bool().unwrap(), observed);
        }

        // 52. McpWorkflowItem
        #[test]
        fn prop_workflow_item_ser(
            name in arb_string(),
            description in arb_string(),
            step_count in any::<usize>(),
            requires_pane in any::<bool>(),
            requires_approval in any::<bool>(),
            can_abort in any::<bool>(),
            destructive in any::<bool>(),
        ) {
            let item = McpWorkflowItem {
                name: name.clone(),
                description: description.clone(),
                step_count,
                trigger_event_types: vec!["error".to_string()],
                trigger_rule_ids: vec![],
                supported_agent_types: vec!["codex".to_string()],
                requires_pane,
                requires_approval,
                can_abort,
                destructive,
            };
            let v = serde_json::to_value(&item).unwrap();
            prop_assert_eq!(v["name"].as_str().unwrap(), name.as_str());
            prop_assert_eq!(v["description"].as_str().unwrap(), description.as_str());
            prop_assert_eq!(v["step_count"].as_u64().unwrap(), step_count as u64);
            prop_assert_eq!(v["requires_pane"].as_bool().unwrap(), requires_pane);
            prop_assert_eq!(v["requires_approval"].as_bool().unwrap(), requires_approval);
            prop_assert_eq!(v["can_abort"].as_bool().unwrap(), can_abort);
            prop_assert_eq!(v["destructive"].as_bool().unwrap(), destructive);
            prop_assert!(v["trigger_event_types"].is_array());
            prop_assert!(v["supported_agent_types"].is_array());
        }

        // 53. McpSearchHit
        #[test]
        fn prop_search_hit_ser(
            segment_id in any::<i64>(),
            pane_id in any::<u64>(),
            seq in any::<u64>(),
            captured_at in any::<i64>(),
            score in arb_finite_f64(),
            snippet in arb_opt_string(),
            content in arb_opt_string(),
            semantic_score in proptest::option::of(arb_finite_f64()),
            fusion_rank in proptest::option::of(any::<usize>()),
        ) {
            let hit = McpSearchHit {
                segment_id,
                pane_id,
                seq,
                captured_at,
                score,
                snippet: snippet.clone(),
                content: content.clone(),
                semantic_score,
                fusion_rank,
            };
            let v = serde_json::to_value(&hit).unwrap();
            prop_assert_eq!(v["segment_id"].as_i64().unwrap(), segment_id);
            prop_assert_eq!(v["pane_id"].as_u64().unwrap(), pane_id);
            prop_assert_eq!(v["seq"].as_u64().unwrap(), seq);
            prop_assert_eq!(v["captured_at"].as_i64().unwrap(), captured_at);
        }

        // 54. McpWaitForData
        #[test]
        fn prop_wait_for_data_ser(
            pane_id in any::<u64>(),
            pattern in arb_string(),
            matched in any::<bool>(),
            elapsed_ms in any::<u64>(),
            polls in any::<usize>(),
            is_regex in any::<bool>(),
        ) {
            let d = McpWaitForData {
                pane_id,
                pattern: pattern.clone(),
                matched,
                elapsed_ms,
                polls,
                is_regex,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["pane_id"].as_u64().unwrap(), pane_id);
            prop_assert_eq!(v["pattern"].as_str().unwrap(), pattern.as_str());
            prop_assert_eq!(v["matched"].as_bool().unwrap(), matched);
            prop_assert_eq!(v["elapsed_ms"].as_u64().unwrap(), elapsed_ms);
            prop_assert_eq!(v["polls"].as_u64().unwrap(), polls as u64);
            // is_regex uses skip_serializing_if = Not::not
            if is_regex {
                prop_assert_eq!(v["is_regex"].as_bool().unwrap(), true);
            } else {
                prop_assert!(v.get("is_regex").is_none());
            }
        }

        // 55. McpGetTextData
        #[test]
        fn prop_get_text_data_ser(
            pane_id in any::<u64>(),
            text in arb_string(),
            tail_lines in any::<usize>(),
            escapes_included in any::<bool>(),
            truncated in any::<bool>(),
        ) {
            let d = McpGetTextData {
                pane_id,
                text: text.clone(),
                tail_lines,
                escapes_included,
                truncated,
                truncation_info: None,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["pane_id"].as_u64().unwrap(), pane_id);
            prop_assert_eq!(v["text"].as_str().unwrap(), text.as_str());
            prop_assert_eq!(v["tail_lines"].as_u64().unwrap(), tail_lines as u64);
            prop_assert_eq!(v["escapes_included"].as_bool().unwrap(), escapes_included);
            // truncated uses skip_serializing_if = Not::not
            if truncated {
                prop_assert_eq!(v["truncated"].as_bool().unwrap(), true);
            } else {
                prop_assert!(v.get("truncated").is_none());
            }
            prop_assert!(v.get("truncation_info").is_none());
        }

        // 56. McpGetTextData with TruncationInfo
        #[test]
        fn prop_get_text_data_with_truncation_ser(
            pane_id in any::<u64>(),
            text in arb_string(),
            tail_lines in any::<usize>(),
            orig_bytes in any::<usize>(),
            ret_bytes in any::<usize>(),
            orig_lines in any::<usize>(),
            ret_lines in any::<usize>(),
        ) {
            let d = McpGetTextData {
                pane_id,
                text: text.clone(),
                tail_lines,
                escapes_included: false,
                truncated: true,
                truncation_info: Some(TruncationInfo {
                    original_bytes: orig_bytes,
                    returned_bytes: ret_bytes,
                    original_lines: orig_lines,
                    returned_lines: ret_lines,
                }),
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert!(v.get("truncation_info").is_some());
            prop_assert_eq!(v["truncation_info"]["original_bytes"].as_u64().unwrap(), orig_bytes as u64);
            prop_assert_eq!(v["truncation_info"]["returned_bytes"].as_u64().unwrap(), ret_bytes as u64);
        }

        // 57. McpRulesListData (composite)
        #[test]
        fn prop_rules_list_data_ser(
            agent_type_filter in arb_opt_string(),
        ) {
            let d = McpRulesListData {
                rules: vec![McpRuleItem {
                    id: "r1".to_string(),
                    agent_type: "codex".to_string(),
                    event_type: "error".to_string(),
                    severity: "high".to_string(),
                    description: None,
                    workflow: None,
                    anchor_count: 1,
                    has_regex: false,
                }],
                agent_type_filter: agent_type_filter.clone(),
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert!(v["rules"].is_array());
            prop_assert_eq!(v["rules"].as_array().unwrap().len(), 1);
            if let Some(ref atf) = agent_type_filter {
                prop_assert_eq!(v["agent_type_filter"].as_str().unwrap(), atf.as_str());
            } else {
                prop_assert!(v.get("agent_type_filter").is_none());
            }
        }

        // 58. McpRulesTestData (composite)
        #[test]
        fn prop_rules_test_data_ser(
            text_length in any::<usize>(),
            match_count in any::<usize>(),
        ) {
            let d = McpRulesTestData {
                text_length,
                match_count,
                matches: vec![],
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["text_length"].as_u64().unwrap(), text_length as u64);
            prop_assert_eq!(v["match_count"].as_u64().unwrap(), match_count as u64);
            prop_assert!(v["matches"].is_array());
        }

        // 59. McpReservationsData (composite)
        #[test]
        fn prop_reservations_data_ser(
            total in any::<usize>(),
            pane_filter in proptest::option::of(any::<u64>()),
        ) {
            let d = McpReservationsData {
                reservations: vec![],
                total,
                pane_filter,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["total"].as_u64().unwrap(), total as u64);
            prop_assert!(v["reservations"].is_array());
        }

        // 60. McpReserveData (composite)
        #[test]
        fn prop_reserve_data_ser(
            id in any::<i64>(),
            pane_id in any::<u64>(),
        ) {
            let d = McpReserveData {
                reservation: McpReservationInfo {
                    id,
                    pane_id,
                    owner_kind: "agent".to_string(),
                    owner_id: "a1".to_string(),
                    reason: None,
                    created_at: 100,
                    expires_at: 200,
                    released_at: None,
                    status: "active".to_string(),
                },
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["reservation"]["id"].as_i64().unwrap(), id);
            prop_assert_eq!(v["reservation"]["pane_id"].as_u64().unwrap(), pane_id);
        }

        // 61. McpAccountsData (composite)
        #[test]
        fn prop_accounts_data_ser(
            total in any::<usize>(),
            service in arb_string(),
        ) {
            let d = McpAccountsData {
                accounts: vec![],
                total,
                service: service.clone(),
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["total"].as_u64().unwrap(), total as u64);
            prop_assert_eq!(v["service"].as_str().unwrap(), service.as_str());
            prop_assert!(v["accounts"].is_array());
        }

        // 62. McpWorkflowsData (composite)
        #[test]
        fn prop_workflows_data_ser(total in any::<usize>()) {
            let d = McpWorkflowsData {
                workflows: vec![],
                total,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["total"].as_u64().unwrap(), total as u64);
            prop_assert!(v["workflows"].is_array());
        }

        // 63. McpEventsData (composite)
        #[test]
        fn prop_events_data_ser(
            total_count in any::<usize>(),
            limit in any::<usize>(),
            pane_filter in proptest::option::of(any::<u64>()),
            rule_id_filter in arb_opt_string(),
            unhandled_only in any::<bool>(),
        ) {
            let d = McpEventsData {
                events: vec![],
                total_count,
                limit,
                pane_filter,
                rule_id_filter: rule_id_filter.clone(),
                event_type_filter: None,
                triage_state_filter: None,
                label_filter: None,
                unhandled_only,
                since_filter: None,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["total_count"].as_u64().unwrap(), total_count as u64);
            prop_assert_eq!(v["limit"].as_u64().unwrap(), limit as u64);
            prop_assert_eq!(v["unhandled_only"].as_bool().unwrap(), unhandled_only);
        }

        // 64. McpWorkflowRunData
        #[test]
        fn prop_workflow_run_data_ser(
            workflow_name in arb_string(),
            pane_id in any::<u64>(),
            status in arb_string(),
            message in arb_opt_string(),
            steps_executed in proptest::option::of(any::<usize>()),
            step_index in proptest::option::of(any::<usize>()),
            elapsed_ms in proptest::option::of(any::<u64>()),
        ) {
            let d = McpWorkflowRunData {
                workflow_name: workflow_name.clone(),
                pane_id,
                execution_id: None,
                status: status.clone(),
                message: message.clone(),
                result: None,
                steps_executed,
                step_index,
                elapsed_ms,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["workflow_name"].as_str().unwrap(), workflow_name.as_str());
            prop_assert_eq!(v["pane_id"].as_u64().unwrap(), pane_id);
            prop_assert_eq!(v["status"].as_str().unwrap(), status.as_str());
        }

        // 65. McpSearchData
        #[test]
        fn prop_search_data_ser(
            query in arb_string(),
            total_hits in any::<usize>(),
            limit in any::<usize>(),
            mode in arb_string(),
        ) {
            let d = McpSearchData {
                query: query.clone(),
                results: vec![],
                total_hits,
                limit,
                pane_filter: None,
                since_filter: None,
                until_filter: None,
                mode: mode.clone(),
                metrics: None,
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["query"].as_str().unwrap(), query.as_str());
            prop_assert_eq!(v["total_hits"].as_u64().unwrap(), total_hits as u64);
            prop_assert_eq!(v["limit"].as_u64().unwrap(), limit as u64);
            prop_assert_eq!(v["mode"].as_str().unwrap(), mode.as_str());
            prop_assert!(v["results"].is_array());
        }

        // 66. McpAccountsRefreshData (composite)
        #[test]
        fn prop_accounts_refresh_data_ser(
            service in arb_string(),
            refreshed_count in any::<usize>(),
            refreshed_at in arb_opt_string(),
        ) {
            let d = McpAccountsRefreshData {
                service: service.clone(),
                refreshed_count,
                refreshed_at: refreshed_at.clone(),
                accounts: vec![],
            };
            let v = serde_json::to_value(&d).unwrap();
            prop_assert_eq!(v["service"].as_str().unwrap(), service.as_str());
            prop_assert_eq!(v["refreshed_count"].as_u64().unwrap(), refreshed_count as u64);
            prop_assert!(v["accounts"].is_array());
        }
    }

    // ── apply_tail_truncation property tests ─────────────────────────────

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        // 67. Tail truncation: no truncation when lines fit
        #[test]
        fn prop_tail_truncation_no_truncate(
            lines in proptest::collection::vec("[a-z]{1,20}", 1..=10),
            tail in 10usize..100,
        ) {
            let text = lines.join("\n");
            let (result, truncated, info) = apply_tail_truncation(&text, tail);
            prop_assert!(!truncated);
            prop_assert!(info.is_none());
            prop_assert_eq!(result, text);
        }

        // 68. Tail truncation: returns correct number of lines
        #[test]
        fn prop_tail_truncation_returns_tail_lines(
            lines in proptest::collection::vec("[a-z]{1,10}", 5..=20),
            tail in 1usize..5,
        ) {
            let text = lines.join("\n");
            let (result, truncated, info) = apply_tail_truncation(&text, tail);
            prop_assert!(truncated);
            prop_assert!(info.is_some());
            let info = info.unwrap();
            prop_assert_eq!(info.returned_lines, tail);
            prop_assert_eq!(info.original_lines, lines.len());
            let result_lines: Vec<&str> = result.lines().collect();
            prop_assert_eq!(result_lines.len(), tail);
        }
    }
}
