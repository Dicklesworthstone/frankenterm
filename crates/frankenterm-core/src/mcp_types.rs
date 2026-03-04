//! MCP parameter, response, and envelope types.
//!
//! Extracted from `mcp.rs` as part of Wave 4A migration (ft-1fv0u).

#[allow(clippy::wildcard_imports)]
use super::*;

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
}
