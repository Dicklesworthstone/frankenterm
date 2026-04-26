//! Extracted MCP tool handlers (strangler-fig migration slice).

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::sync::OnceLock;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use crate::mcp_error::MCP_ERR_REMOTE_TEXT_UNAVAILABLE;
#[allow(unused_imports)]
use crate::mcp_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkTool as Tool, FrameworkToolHandler as ToolHandler,
};
use crate::policy::PolicySurface;
use crate::runtime_async::{CompatRuntime, RuntimeBuilder as CompatRuntimeBuilder};
use fs2::FileExt;

use super::mcp_missions::mcp_save_mission_tx_contract_to_path;
use super::mcp_types::{
    AccountsParams, AccountsRefreshParams, CassSearchParams, CassStatusParams, CassViewParams,
    EventsAnnotateParams, EventsLabelParams, EventsParams, EventsTriageParams, GetTextParams,
    McpAccountInfo, McpAccountsData, McpAccountsRefreshData, McpEnvelope, McpEventItem,
    McpEventMutationData, McpEventsData, McpGetTextData, McpMissionControlData,
    McpMissionExplainData, McpMissionStateData, McpPaneState, McpReleaseData, McpReservationInfo,
    McpReservationsData, McpReserveData, McpRuleItem, McpRuleMatchItem, McpRuleTraceInfo,
    McpRulesListData, McpRulesTestData, McpSearchData, McpSearchHit, McpSendData, McpTxPlanData,
    McpTxRollbackData, McpTxRunData, McpTxShowData, McpWaitForData, McpWorkflowRunData,
    MissionAbortParams, MissionExplainParams, MissionPauseParams, MissionResumeParams,
    MissionStateParams, ReleaseParams, ReservationsParams, ReserveParams, RulesListParams,
    RulesTestParams, SearchParams, SendParams, StateParams, TxPlanParams, TxRollbackParams,
    TxRunParams, TxShowParams, WaitForParams, WorkflowRunParams, apply_tail_truncation, now_ms,
};
#[allow(unused_imports)]
use super::{
    AccountRecord, ActionKind, ActorKind, AgentProvider, AgentType, ApprovalStore, CassAgent,
    CassClient, CassError, CassSearchOptions, CassSearchResult, CassStatus, CassViewOptions,
    CassViewResult, CautClient, CautService, Config, DecisionContext, EventQuery,
    HandleAuthRequired, HandleClaudeCodeLimits, HandleCompaction, HandleGeminiQuota,
    HandleProcessTriageLifecycle, HandleSessionEnd, HandleUsageLimits, InjectionResult,
    MCP_ERR_CASS, MCP_ERR_CAUT, MCP_ERR_CONFIG, MCP_ERR_FTS_QUERY, MCP_ERR_INVALID_ARGS,
    MCP_ERR_NOT_IMPLEMENTED, MCP_ERR_PANE_NOT_FOUND, MCP_ERR_POLICY, MCP_ERR_STORAGE,
    MCP_ERR_TIMEOUT, MCP_ERR_WEZTERM, MCP_ERR_WORKFLOW, McpToolError, Osc133State,
    PaneCapabilities, PaneFilterConfig, PaneInfo, PaneReservation, PaneWaiter, PatternEngine,
    PolicyDecision, PolicyEngine, PolicyGatedInjector, PolicyInput, SearchQueryDefaults,
    SearchQueryInput, SharedRateLimiter, StorageHandle, UnifiedSearchMode, WaitOptions, WaitResult,
    WeztermError, WeztermHandleSource, Workflow, WorkflowExecutionResult, approval_command,
    build_mcp_shared_rate_limiter, build_mcp_workflow_assembly,
    build_policy_engine_with_shared_rate_limiter, default_wezterm_handle,
    effective_search_fusion_backend, effective_search_fusion_weights,
    effective_search_quality_timeout_ms, effective_search_rrf_k, elapsed_ms, envelope_to_content,
    map_cass_error, map_caut_error, map_mcp_error, mcp_build_mission_assignments,
    mcp_build_tx_compensation_inputs, mcp_load_mission_from_path,
    mcp_load_mission_tx_contract_from_path, mcp_mission_failure_catalog,
    mcp_mission_lifecycle_transitions, mcp_parse_mission_kill_switch,
    mcp_resolve_mission_file_path, mcp_resolve_mission_tx_file_path, mcp_save_mission_to_path,
    mcp_tx_transition_info, parse_cass_agent, parse_caut_service, parse_unified_search_query,
    policy_reason, record_mcp_audit_sync, redact_mcp_args, reservation_to_mcp_info,
    resolve_alt_screen_state, resolve_workspace_id, to_storage_search_options,
};
use super::{
    MCP_REFRESH_COOLDOWN_MS, check_refresh_cooldown, injection_from_decision,
    resolve_pane_capabilities,
};

fn mcp_get_text_policy_input(
    pane_id: u64,
    domain: impl Into<String>,
    capabilities: PaneCapabilities,
    summary: &str,
) -> PolicyInput {
    PolicyInput::new(ActionKind::ReadOutput, ActorKind::Mcp)
        .with_surface(PolicySurface::Mux)
        .with_pane(pane_id)
        .with_domain(domain.into())
        .with_capabilities(capabilities)
        .with_text_summary(summary.to_string())
}

fn mcp_search_output_policy_input(summary: &str) -> PolicyInput {
    PolicyInput::new(ActionKind::SearchOutput, ActorKind::Mcp)
        .with_surface(PolicySurface::Mux)
        .with_text_summary(summary.to_string())
}

/// [ft-05hfm] Hard cap on the `text` payload accepted by `wa.send`.
///
/// `wa.send` is the MCP surface for typing into a live pane — an
/// attacker with MCP client access (stdio transport inherits the
/// operator's uid/gid, but auto-MCP pipelines can ferry third-party
/// input in) could submit a multi-gigabyte `text` field and OOM the
/// watcher before anyone noticed.
///
/// 4 MiB is roughly 40x the largest legitimate paste (a full Claude
/// Code prompt + context). High enough to accommodate uncommon but
/// valid bulk-input scenarios (paste of a long generated artifact,
/// for instance), low enough to reject obvious DoS attempts before
/// the payload reaches the injector / policy / wezterm pipeline.
pub(crate) const MAX_SEND_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Hard cap for MCP pane-output waits.
///
/// `wa.wait_for` and `wa.send --wait_for` run through synchronous MCP tool
/// handlers backed by a current-thread runtime. Keep their operator-tunable
/// wait window bounded so a malformed MCP client cannot pin a handler for
/// hours or days.
pub(crate) const MAX_MCP_WAIT_TIMEOUT_SECS: u64 = 600;

/// ft-<ux-audit>: shared hint string for every `MCP_ERR_POLICY` hard-deny
/// response. Previously every deny site passed `None` as the hint,
/// leaving the MCP client with "Read denied by policy" and nowhere to go.
/// The hint names two actionable things: where the operator looks to
/// understand the deny (`config.safety.rules`) and where the decision
/// context (rule_id + reason) is queryable (`policy_denied_audit`).
///
/// Kept as a single const so all 7 deny sites and the gate helper can
/// diverge-by-accident-proof: one edit here, every hint updates.
pub(crate) const POLICY_DENY_HINT: &str = "Hard policy deny: review `config.safety.rules` for the active deny list, \
     or query `policy_denied_audit` for the decision context (rule_id, reason). \
     Hard denies are not retryable without a policy change.";

fn validate_mcp_wait_timeout_secs(
    tool_name: &str,
    timeout_secs: u64,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if (1..=MAX_MCP_WAIT_TIMEOUT_SECS).contains(&timeout_secs) {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!("timeout_secs must be in 1..={MAX_MCP_WAIT_TIMEOUT_SECS} (got {timeout_secs})"),
        Some(format!(
            "The {tool_name} tool schema declares timeout_secs with minimum: 1 and maximum: \
             {MAX_MCP_WAIT_TIMEOUT_SECS}; omit the field to use the default (30)."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

static MCP_TX_CONTRACT_LOCKS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct McpTxContractLockGuard {
    key: PathBuf,
    _file: File,
}

impl Drop for McpTxContractLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
        if let Ok(mut locks) = MCP_TX_CONTRACT_LOCKS.lock() {
            locks.remove(&self.key);
        }
    }
}

fn canonical_tx_lock_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn tx_contract_lock_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("lock");
    path.with_extension(format!("{extension}.lock"))
}

fn release_mcp_tx_contract_lock_key(key: &Path) {
    if let Ok(mut locks) = MCP_TX_CONTRACT_LOCKS.lock() {
        locks.remove(key);
    }
}

fn acquire_mcp_tx_contract_lock(
    path: &Path,
) -> std::result::Result<McpTxContractLockGuard, McpToolError> {
    let key = canonical_tx_lock_key(path);
    let mut locks = MCP_TX_CONTRACT_LOCKS.lock().map_err(|_| {
        McpToolError::new(
            "robot.tx_lock_failed",
            "Failed to lock tx contract registry".to_string(),
            Some("Retry the tx operation; the in-process lock registry was poisoned.".to_string()),
        )
    })?;

    if !locks.insert(key.clone()) {
        return Err(McpToolError::new(
            "robot.tx_in_progress",
            format!("Tx contract is already being executed: {}", path.display()),
            Some(
                "Wait for the in-flight wa.tx_run or wa.tx_rollback call for this contract to finish."
                    .to_string(),
            ),
        ));
    }

    let lock_path = tx_contract_lock_path(&key);
    let file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(err) => {
            release_mcp_tx_contract_lock_key(&key);
            return Err(McpToolError::new(
                "robot.tx_lock_failed",
                format!(
                    "Failed to open tx contract lock file {}: {err}",
                    lock_path.display()
                ),
                None,
            ));
        }
    };

    if let Err(err) = FileExt::try_lock_exclusive(&file) {
        release_mcp_tx_contract_lock_key(&key);
        return Err(McpToolError::new(
            "robot.tx_in_progress",
            format!(
                "Tx contract is already being executed: {} ({err})",
                path.display()
            ),
            Some(format!(
                "Wait for the process holding {} to finish.",
                lock_path.display()
            )),
        ));
    }

    Ok(McpTxContractLockGuard { key, _file: file })
}

#[cfg(test)]
fn tx_run_test_wezterm_override_slot()
-> &'static std::sync::Mutex<Option<crate::wezterm::WeztermHandle>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<crate::wezterm::WeztermHandle>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

fn tx_run_wezterm_handle() -> crate::wezterm::WeztermHandle {
    #[cfg(test)]
    if let Some(handle) = tx_run_test_wezterm_override_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return handle;
    }

    default_wezterm_handle()
}

fn mcp_tx_outcome_for_state(state: crate::plan::MissionTxState) -> crate::plan::TxOutcome {
    match state {
        crate::plan::MissionTxState::Committed => crate::plan::TxOutcome::Committed,
        crate::plan::MissionTxState::RolledBack | crate::plan::MissionTxState::Compensated => {
            crate::plan::TxOutcome::Compensated
        }
        crate::plan::MissionTxState::Failed => crate::plan::TxOutcome::Failed,
        _ => crate::plan::TxOutcome::Pending,
    }
}

fn mcp_send_text_policy_input(
    pane_id: u64,
    domain: impl Into<String>,
    capabilities: PaneCapabilities,
    summary: &str,
    command_text: &str,
) -> PolicyInput {
    PolicyInput::new(ActionKind::SendText, ActorKind::Mcp)
        .with_surface(PolicySurface::Mux)
        .with_pane(pane_id)
        .with_domain(domain.into())
        .with_capabilities(capabilities)
        .with_text_summary(summary.to_string())
        .with_command_text(command_text.to_string())
}

fn mcp_workflow_run_policy_input(
    pane_id: u64,
    domain: impl Into<String>,
    capabilities: PaneCapabilities,
    summary: &str,
) -> PolicyInput {
    PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Mcp)
        .with_surface(PolicySurface::Workflow)
        .with_pane(pane_id)
        .with_domain(domain.into())
        .with_capabilities(capabilities)
        .with_text_summary(summary.to_string())
}

fn authorize_mcp_policy_call(
    engine: &mut PolicyEngine,
    input: &PolicyInput,
    dry_run: bool,
) -> PolicyDecision {
    if dry_run {
        engine.authorize_preview(input)
    } else {
        engine.authorize(input)
    }
}

fn mcp_reserve_pane_policy_input(pane_id: u64, summary: &str) -> PolicyInput {
    PolicyInput::new(ActionKind::ReservePane, ActorKind::Mcp)
        .with_surface(PolicySurface::Swarm)
        .with_pane(pane_id)
        .with_capabilities(PaneCapabilities::unknown())
        .with_text_summary(summary.to_string())
        .with_command_text("reserve_pane".to_string())
}

fn mcp_release_pane_policy_input(summary: &str, pane_id: Option<u64>) -> PolicyInput {
    let mut input = PolicyInput::new(ActionKind::ReleasePane, ActorKind::Mcp)
        .with_surface(PolicySurface::Swarm)
        .with_capabilities(PaneCapabilities::unknown())
        .with_text_summary(summary.to_string())
        .with_command_text("release_reservation".to_string());
    if let Some(pane_id) = pane_id {
        input = input.with_pane(pane_id);
    }
    input
}

/// ft-x86z2: security gate for mutating MCP tools that don't live on a pane
/// surface (tx / mission control). Runs `PolicyEngine.authorize` against a
/// `PolicySurface::Mcp` + `ActionKind::ExecCommand` input carrying the tool
/// name as summary and a stable `command_text` so policy rules can match per
/// operation. Returns `None` on Allow; `Some(err_envelope)` on Deny or
/// RequireApproval — callers `return` that directly.
///
/// Intentionally lighter than the `wa.workflow_run` pattern at mcp_tools.rs:2242:
/// no `ApprovalStore::attach_to_decision` plumbing here, since these handlers
/// don't have an async storage handle in scope at the gate point and wiring
/// one is a larger refactor. RequireApproval surfaces as `MCP_ERR_POLICY`
/// with a hint telling the caller to obtain an allow-once token. Upgrading
/// to the full `attach_to_decision` flow (issuing the token from this path)
/// is a deliberate follow-up.
fn mcp_authorize_mcp_mutation(
    config: &Config,
    policy_rate_limiter: &SharedRateLimiter,
    summary: &str,
    command_text: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    let input = PolicyInput::new(ActionKind::ExecCommand, ActorKind::Mcp)
        .with_surface(PolicySurface::Mcp)
        .with_text_summary(summary.to_string())
        .with_command_text(command_text.to_string());
    let mut engine = build_policy_engine_with_shared_rate_limiter(
        config,
        config.safety.require_prompt_active,
        Arc::clone(policy_rate_limiter),
    );
    let decision = engine.authorize(&input);
    if decision.is_denied() {
        let reason = policy_reason(&decision)
            .unwrap_or("Policy denied this MCP mutation")
            .to_string();
        // ft-6mmyp: structured observability for denied attempts via
        // tracing. ft-rsqap: ALSO persist to the policy_denied_audit
        // table so operators get SQL-queryable forensics. Best-effort:
        // a failed audit write logs a secondary warn but never blocks
        // the client's denial response.
        tracing::warn!(
            target: "ft::security::policy",
            tool = %summary,
            command = %command_text,
            decision = "denied",
            rule_id = ?decision.rule_id(),
            reason = %reason,
            "MCP mutation denied by policy"
        );
        persist_mcp_policy_denial(
            config,
            summary,
            command_text,
            &reason,
            decision.rule_id(),
            crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
        );
        let envelope = McpEnvelope::<()>::error(
            MCP_ERR_POLICY,
            reason,
            Some(POLICY_DENY_HINT.to_string()),
            elapsed_ms(start),
        );
        return Some(envelope_to_content(envelope));
    }
    if decision.requires_approval() {
        let reason = policy_reason(&decision)
            .unwrap_or("This MCP mutation requires allow-once approval")
            .to_string();
        tracing::warn!(
            target: "ft::security::policy",
            tool = %summary,
            command = %command_text,
            decision = "require_approval",
            rule_id = ?decision.rule_id(),
            reason = %reason,
            "MCP mutation requires allow-once approval"
        );
        persist_mcp_policy_denial(
            config,
            summary,
            command_text,
            &reason,
            decision.rule_id(),
            crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
        );
        let hint = Some(
            "Obtain an allow-once approval token and retry via the approving client.".to_string(),
        );
        let envelope = McpEnvelope::<()>::error(MCP_ERR_POLICY, reason, hint, elapsed_ms(start));
        return Some(envelope_to_content(envelope));
    }
    None
}

/// ft-rsqap: best-effort audit-table write for a denied/require-approval
/// MCP mutation. Resolves the workspace db_path from `config`, builds a
/// `PolicyDeniedAuditRecord`, and calls the sync blocking helper in
/// `storage.rs`. Every failure (layout resolution, connection open, INSERT)
/// logs a secondary `tracing::warn!` and returns — the caller's policy-
/// denied response to the client must never be blocked by an audit-write
/// failure.
fn persist_mcp_policy_denial(
    config: &Config,
    tool_name: &str,
    command_text: &str,
    reason: &str,
    rule_id: Option<&str>,
    decision: &str,
    reason_code: &str,
) {
    let layout = match config.workspace_layout(None) {
        Ok(layout) => layout,
        Err(err) => {
            tracing::warn!(
                target: "ft::security::policy",
                tool = %tool_name,
                error = %err,
                "workspace_layout unavailable; skipping policy_denied_audit write"
            );
            return;
        }
    };
    let record = crate::storage::PolicyDeniedAuditRecord {
        id: 0,
        ts_ms: i64::try_from(now_ms()).unwrap_or(0),
        agent_id: None,
        tool_name: tool_name.to_string(),
        intent_hash: Some(intent_hash_hex(command_text)),
        reason: reason.to_string(),
        reason_code: reason_code.to_string(),
        rule_id: rule_id.map(String::from),
        decision: decision.to_string(),
    };
    if let Err(err) =
        crate::storage::record_policy_denial_audit_blocking(layout.db_path.as_path(), &record)
    {
        tracing::warn!(
            target: "ft::security::policy",
            tool = %tool_name,
            error = %err,
            "policy_denied_audit write failed; tracing emission remains the primary signal"
        );
    }
}

/// ft-mw1zb: async sibling of `persist_mcp_policy_denial`. Use at the 7
/// direct `authorize()` sites (wa.get_text / wa.search / wa.send /
/// wa.workflow_run / wa.reserve / wa.release / wa.accounts_refresh)
/// whose handlers already live inside `runtime.block_on(async { ... })`
/// with a live `StorageHandle` in scope — no blocking bridge needed.
///
/// Best-effort: a failed write logs `tracing::warn!` and returns. Must
/// never block the caller's policy-denied response to the client.
async fn persist_mcp_policy_denial_async(
    storage: &StorageHandle,
    tool_name: &str,
    command_text: &str,
    reason: &str,
    rule_id: Option<&str>,
    decision: &str,
    reason_code: &str,
) {
    let record = crate::storage::PolicyDeniedAuditRecord {
        id: 0,
        ts_ms: i64::try_from(now_ms()).unwrap_or(0),
        agent_id: None,
        tool_name: tool_name.to_string(),
        intent_hash: Some(intent_hash_hex(command_text)),
        reason: reason.to_string(),
        reason_code: reason_code.to_string(),
        rule_id: rule_id.map(String::from),
        decision: decision.to_string(),
    };
    if let Err(err) = storage.record_policy_denial_audit(record).await {
        tracing::warn!(
            target: "ft::security::policy",
            tool = %tool_name,
            error = %err,
            "policy_denied_audit write failed; tracing emission remains the primary signal"
        );
    }
}

/// Short 16-hex-char correlation fingerprint of the command_text so
/// operators can group repeated identical denies without persisting the
/// raw args. DefaultHasher is used deliberately — this is a
/// correlation/grouping aid, not a cryptographic identifier, so
/// collision resistance against an adversary isn't required.
fn intent_hash_hex(input: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn mcp_pane_matches_agent_filter(agent_filter: &str, pane_title: &str) -> bool {
    let title_lower = pane_title.to_lowercase();
    let filter_lower = agent_filter.to_lowercase();
    match filter_lower.as_str() {
        "codex" => title_lower.contains("codex") || title_lower.contains("openai"),
        "claude_code" | "claude" => title_lower.contains("claude"),
        "gemini" => title_lower.contains("gemini"),
        _ => title_lower.contains(&filter_lower),
    }
}

fn mcp_is_distributed_remote_domain(domain: &str) -> bool {
    domain.starts_with("distributed:")
}

async fn load_distributed_remote_panes(
    db_path: &Path,
) -> std::result::Result<Vec<crate::storage::PaneRecord>, crate::Error> {
    // ft-xbnl0.2.3 tick 303: cx-first MCP scratch-handle open + panes read.
    let mcp_panes_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    let storage = StorageHandle::new_with_cx(&mcp_panes_cx, &db_path.to_string_lossy()).await?;
    let panes = storage.get_panes_with_cx(&mcp_panes_cx).await?;
    if let Err(err) = storage.shutdown().await {
        tracing::warn!(error = %err, "Failed to shutdown storage cleanly after MCP pane query");
    }

    Ok(panes
        .into_iter()
        .filter(|pane| mcp_is_distributed_remote_domain(&pane.domain))
        .collect())
}

async fn load_distributed_remote_pane(
    storage: Option<&StorageHandle>,
    pane_id: u64,
) -> std::result::Result<Option<crate::storage::PaneRecord>, crate::Error> {
    let Some(storage) = storage else {
        return Ok(None);
    };

    Ok(storage
        .get_pane(pane_id)
        .await?
        .filter(|pane| mcp_is_distributed_remote_domain(&pane.domain)))
}

fn merge_distributed_remote_mcp_states(
    states: &mut Vec<McpPaneState>,
    remote_records: Vec<crate::storage::PaneRecord>,
    params: &StateParams,
) {
    let mut existing_pane_ids: std::collections::HashSet<u64> =
        states.iter().map(|state| state.pane_id).collect();

    for record in remote_records {
        if !existing_pane_ids.insert(record.pane_id) {
            continue;
        }
        if let Some(pane_id) = params.pane_id {
            if record.pane_id != pane_id {
                continue;
            }
        }
        if let Some(domain) = params.domain.as_deref() {
            if record.domain != domain {
                continue;
            }
        }
        if let Some(agent) = params.agent.as_deref() {
            let title = record.title.as_deref().unwrap_or("");
            if !mcp_pane_matches_agent_filter(agent, title) {
                continue;
            }
        }
        states.push(McpPaneState::from_pane_record(&record));
    }

    states.sort_by_key(|state| state.pane_id);
}

fn serialize_mcp_audit_decision_context(
    context: &crate::policy::DecisionContext,
) -> Option<String> {
    serde_json::to_string(context)
        .inspect_err(
            |e| tracing::warn!(error = %e, "mcp audit decision_context serialization failed"),
        )
        .ok()
}

fn mcp_event_mutation_decision_context(
    tool_name: &str,
    action_kind: &str,
    event_id: i64,
    operation: &str,
    actor_id: Option<&str>,
    input_summary: &str,
    timestamp_ms: i64,
) -> crate::policy::DecisionContext {
    let mut context = crate::policy::DecisionContext::new_audit(
        timestamp_ms,
        crate::policy::ActionKind::ExecCommand,
        crate::policy::ActorKind::Mcp,
        PolicySurface::Mcp,
        None,
        None,
        Some(input_summary.to_string()),
        None,
    );
    let determining_rule = format!("audit.{action_kind}");
    context.record_rule(
        &determining_rule,
        true,
        Some("allow"),
        Some("MCP event mutation recorded".to_string()),
    );
    context.set_determining_rule(&determining_rule);
    context.add_evidence("stage", "event_mutation");
    context.add_evidence("tool", tool_name);
    context.add_evidence("event_action_kind", action_kind);
    context.add_evidence("event_id", event_id.to_string());
    context.add_evidence("operation", operation);
    context.add_evidence("event_surface", PolicySurface::Mcp.as_str());
    if let Some(actor_id) = actor_id {
        context.add_evidence("actor_id", actor_id);
    }
    context
}

fn cass_client_with_timeout(timeout_secs: u64) -> CassClient {
    let client = CassClient::new().with_timeout_secs(timeout_secs);
    #[cfg(all(test, unix))]
    if let Some(binary) = cass_test_binary_override() {
        return client.with_binary(binary);
    }
    client
}

#[cfg(all(test, unix))]
fn cass_test_binary_override_slot() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(all(test, unix))]
fn cass_test_binary_override() -> Option<String> {
    cass_test_binary_override_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(all(test, unix))]
fn set_cass_test_binary_override(binary: Option<String>) {
    *cass_test_binary_override_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = binary;
}

// wa.rules_list tool
pub(super) struct WaRulesListTool;

impl ToolHandler for WaRulesListTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.rules_list".to_string(),
            description: Some(
                "List pattern detection rules in the rule library (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_type": { "type": "string", "description": "Filter by agent type (codex, claude_code, gemini, wezterm)" },
                    "verbose": { "type": "boolean", "default": false, "description": "Include descriptions in output" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "rules".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: RulesListParams = if arguments.is_null() {
            RulesListParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional agent_type, verbose".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let agent_filter: Option<AgentType> = match params.agent_type.as_ref() {
            Some(s) => match s.to_lowercase().as_str() {
                "codex" => Some(AgentType::Codex),
                "claude_code" => Some(AgentType::ClaudeCode),
                "gemini" => Some(AgentType::Gemini),
                "wezterm" => Some(AgentType::Wezterm),
                _ => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Unknown agent_type: {s}"),
                        Some("Valid types: codex, claude_code, gemini, wezterm".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            },
            None => None,
        };

        let engine = PatternEngine::new();
        let rules = engine.rules();

        let rule_items: Vec<McpRuleItem> = rules
            .iter()
            .filter(|rule| match agent_filter {
                Some(filter) => rule.agent_type == filter,
                None => true,
            })
            .map(|rule| McpRuleItem {
                id: rule.id.clone(),
                agent_type: rule.agent_type.to_string(),
                event_type: rule.event_type.clone(),
                severity: format!("{:?}", rule.severity).to_lowercase(),
                description: if params.verbose {
                    Some(rule.description.clone())
                } else {
                    None
                },
                workflow: rule.workflow.clone(),
                anchor_count: rule.anchors.len(),
                has_regex: rule.regex.is_some(),
            })
            .collect();

        let data = McpRulesListData {
            rules: rule_items,
            agent_type_filter: params.agent_type,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.rules_test tool
pub(super) struct WaRulesTestTool;

impl ToolHandler for WaRulesTestTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.rules_test".to_string(),
            description: Some(
                "Test pattern detection rules against provided text (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to test pattern detection against" },
                    "trace": { "type": "boolean", "default": false, "description": "Include trace information in matches" }
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "rules".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: RulesTestParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with text (required), trace".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let engine = PatternEngine::new();
        let detections = engine.detect(&params.text);

        let matches: Vec<McpRuleMatchItem> = detections
            .iter()
            .map(|d| McpRuleMatchItem {
                rule_id: d.rule_id.clone(),
                agent_type: d.agent_type.to_string(),
                event_type: d.event_type.clone(),
                severity: format!("{:?}", d.severity).to_lowercase(),
                confidence: d.confidence,
                matched_text: d.matched_text.clone(),
                extracted: if d.extracted.is_null()
                    || d.extracted
                        .as_object()
                        .is_some_and(serde_json::Map::is_empty)
                {
                    None
                } else {
                    Some(d.extracted.clone())
                },
                trace: if params.trace {
                    Some(McpRuleTraceInfo {
                        anchors_checked: true,
                        regex_matched: !d.matched_text.is_empty(),
                    })
                } else {
                    None
                },
            })
            .collect();

        let data = McpRulesTestData {
            text_length: params.text.len(),
            match_count: matches.len(),
            matches,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.cass_search tool
pub(super) struct WaCassSearchTool;

impl ToolHandler for WaCassSearchTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.cass_search".to_string(),
            description: Some("Search coding agent session history via cass".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query string" },
                    "limit": { "type": "integer", "minimum": 0, "maximum": 1000, "default": 10, "description": "Maximum results (0 = cass default)" },
                    "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Offset into results" },
                    "agent": { "type": "string", "description": "Agent filter: codex|claude_code|gemini|cursor|aider|chatgpt" },
                    "workspace": { "type": "string", "description": "Workspace filter (cass-defined)" },
                    "days": { "type": "integer", "minimum": 0, "description": "Only sessions within the last N days" },
                    "fields": { "type": "string", "description": "Field selection (cass-defined; e.g. minimal)" },
                    "max_tokens": { "type": "integer", "minimum": 0, "description": "Max tokens per hit content (cass-defined)" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 15, "description": "cass timeout override (seconds)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "cass".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: CassSearchParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with query (required) and optional limit/offset/agent/workspace/days/fields/max_tokens/timeout_secs".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.query.trim().is_empty() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "query cannot be empty".to_string(),
                Some("Provide a non-empty search query string".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        // [ft-tzwuw] Enforce schema's "timeout_secs": { "minimum": 1 }
        // bound. serde_json doesn't honour JSON-Schema bounds, so a
        // client sending timeout_secs=0 reaches cass_client_with_timeout(0)
        // → Duration::from_secs(0) → timeout_with_cx fires before the
        // child cass binary ever executes, returning a confusing
        // "cass timeout (0 secs)" error on every call. Same shape as
        // ft-t62hq (wa.wait_for/wa.send) extended to the cass surface.
        //
        // [ft-szuzd] Enforce schema's "timeout_secs": { "maximum": 600 }
        // bound too. serde_json ignores the upper bound the same way it
        // ignores the lower. Without this, a client (hostile or buggy)
        // sending timeout_secs: 3600 blocks the mcp server on cass for
        // up to an hour — well beyond the 10-minute cap the tool
        // schema advertises. Mirror the LIMIT_MIN/LIMIT_MAX pattern in
        // wa.events at mcp_tools.rs:1725-1749.
        const TIMEOUT_SECS_MIN: u64 = 1;
        const TIMEOUT_SECS_MAX: u64 = 600;
        if params.timeout_secs < TIMEOUT_SECS_MIN || params.timeout_secs > TIMEOUT_SECS_MAX {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!(
                    "timeout_secs must be in {TIMEOUT_SECS_MIN}..={TIMEOUT_SECS_MAX} (got {})",
                    params.timeout_secs
                ),
                Some(format!(
                    "The wa.cass_search tool schema declares timeout_secs \
                     ∈ [{TIMEOUT_SECS_MIN}, {TIMEOUT_SECS_MAX}]; clamp your \
                     request or omit the field to use the default (15)."
                )),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        // [ft-szuzd] Enforce schema's "limit": { "minimum": 0, "maximum": 1000 }
        // bound. The 0 case is a cass sentinel ("use cass default") and
        // is already handled at the call site (params.limit != 0), so we
        // only need to cap the upper end. Without this, a client sending
        // limit: u64::MAX stages it straight into CassSearchOptions and
        // triggers an unbounded cass query.
        const LIMIT_MAX: usize = 1000;
        if params.limit > LIMIT_MAX {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!("limit must be in 0..={LIMIT_MAX} (got {})", params.limit),
                Some(format!(
                    "The wa.cass_search tool schema declares limit \
                     ∈ [0, {LIMIT_MAX}] with 0 meaning 'cass default'; \
                     clamp your request or omit the field to use the \
                     default (10)."
                )),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let agent: Option<CassAgent> = if let Some(ref agent_str) = params.agent {
            match parse_cass_agent(agent_str) {
                Some(agent) => Some(agent),
                None => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid agent: {agent_str}"),
                        Some(
                            "Supported: codex, claude_code, gemini, cursor, aider, chatgpt"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        } else {
            None
        };

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<CassSearchResult, CassError> = runtime.block_on(async {
            let client = cass_client_with_timeout(params.timeout_secs);
            let options = CassSearchOptions {
                limit: (params.limit != 0).then_some(params.limit),
                offset: (params.offset != 0).then_some(params.offset),
                agent,
                workspace: params.workspace,
                days: params.days,
                fields: params.fields,
                max_tokens: params.max_tokens,
            };
            client.search(&params.query, &options).await
        });

        match result {
            Ok(result) => {
                let envelope = McpEnvelope::success(result, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_cass_error(&err);
                let envelope = McpEnvelope::<()>::error(
                    code,
                    format!("cass search failed: {err}"),
                    hint,
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.cass_view tool
pub(super) struct WaCassViewTool;

impl ToolHandler for WaCassViewTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.cass_view".to_string(),
            description: Some("View context for a cass search hit".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "source_path": { "type": "string", "description": "Source path returned by cass search" },
                    "line_number": { "type": "integer", "minimum": 0, "description": "Line number returned by cass search" },
                    "context_lines": { "type": "integer", "minimum": 0, "default": 10, "description": "Context lines before/after match" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 15, "description": "cass timeout override (seconds)" }
                },
                "required": ["source_path", "line_number"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "cass".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: CassViewParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with source_path, line_number, optional context_lines, timeout_secs"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.source_path.trim().is_empty() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "source_path cannot be empty".to_string(),
                Some("Provide a valid source_path returned by cass search".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        // [ft-tzwuw] See ca.search call() for context. Same fix applied
        // here to match the ca.view schema's "timeout_secs": { "minimum": 1 }.
        if params.timeout_secs == 0 {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "timeout_secs must be >= 1 (got 0)".to_string(),
                Some(
                    "The ca.view tool schema declares timeout_secs with \
                     minimum: 1; omit the field to use the default (15)."
                        .to_string(),
                ),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<CassViewResult, CassError> = runtime.block_on(async {
            let client = cass_client_with_timeout(params.timeout_secs);
            let options = CassViewOptions {
                context_lines: Some(params.context_lines),
            };
            client
                .query(
                    std::path::Path::new(&params.source_path),
                    params.line_number,
                    &options,
                )
                .await
        });

        match result {
            Ok(result) => {
                let envelope = McpEnvelope::success(result, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_cass_error(&err);
                let envelope = McpEnvelope::<()>::error(
                    code,
                    format!("cass view failed: {err}"),
                    hint,
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.cass_status tool
pub(super) struct WaCassStatusTool;

impl ToolHandler for WaCassStatusTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.cass_status".to_string(),
            description: Some("Check cass index status".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 15, "description": "cass timeout override (seconds)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "cass".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: CassStatusParams = if arguments.is_null() {
            CassStatusParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional timeout_secs".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        // [ft-tzwuw] See ca.search call() for context. Same fix applied
        // here to match the ca.status schema's "timeout_secs": { "minimum": 1 }.
        if params.timeout_secs == 0 {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "timeout_secs must be >= 1 (got 0)".to_string(),
                Some(
                    "The ca.status tool schema declares timeout_secs with \
                     minimum: 1; omit the field to use the default (15)."
                        .to_string(),
                ),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<CassStatus, CassError> = runtime.block_on(async {
            let client = cass_client_with_timeout(params.timeout_secs);
            client.status().await
        });

        match result {
            Ok(result) => {
                let envelope = McpEnvelope::success(result, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_cass_error(&err);
                let envelope = McpEnvelope::<()>::error(
                    code,
                    format!("cass status failed: {err}"),
                    hint,
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaStateTool {
    filter: PaneFilterConfig,
    db_path: Option<Arc<PathBuf>>,
}

fn redact_mcp_pane_state_fields(states: &mut [McpPaneState]) {
    static REDACTOR: LazyLock<crate::redactor::Redactor> =
        LazyLock::new(crate::redactor::Redactor::new);

    for state in states {
        if let Some(title) = state.title.as_mut() {
            let redacted = REDACTOR.redact(title);
            *title = redacted;
        }
        if let Some(cwd) = state.cwd.as_mut() {
            let redacted = REDACTOR.redact(cwd);
            *cwd = redacted;
        }
        if let Some(ignore_reason) = state.ignore_reason.as_mut() {
            let redacted = REDACTOR.redact(ignore_reason);
            *ignore_reason = redacted;
        }
    }
}

impl WaStateTool {
    pub(super) fn new(filter: PaneFilterConfig, db_path: Option<Arc<PathBuf>>) -> Self {
        Self { filter, db_path }
    }
}

impl ToolHandler for WaStateTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.state".to_string(),
            description: Some("Get current pane states (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "domain": { "type": "string" },
                    "agent": { "type": "string" },
                    "pane_id": { "type": "integer", "minimum": 0 }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params = if arguments.is_null() {
            StateParams::default()
        } else {
            match serde_json::from_value::<StateParams>(arguments) {
                Ok(params) => params,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional domain/agent/pane_id".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let db_path = self.db_path.as_ref().map(Arc::clone);

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let wezterm = default_wezterm_handle();
            let panes = wezterm.list_panes().await?;
            let mut states: Vec<McpPaneState> = panes
                .iter()
                .filter(|pane| match params.pane_id {
                    Some(pane_id) => pane.pane_id == pane_id,
                    None => true,
                })
                .filter(|pane| match params.domain.as_ref() {
                    Some(domain) => pane.inferred_domain() == *domain,
                    None => true,
                })
                .filter(|pane| match params.agent.as_ref() {
                    Some(agent) => {
                        let title = pane.title.as_deref().unwrap_or("");
                        mcp_pane_matches_agent_filter(agent, title)
                    }
                    None => true,
                })
                .map(|pane| McpPaneState::from_pane_info(pane, &self.filter))
                .collect();

            if let Some(db_path) = db_path.as_ref() {
                match load_distributed_remote_panes(db_path.as_path()).await {
                    Ok(remote_records) => {
                        merge_distributed_remote_mcp_states(&mut states, remote_records, &params);
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            path = %db_path.display(),
                            "Failed to load distributed panes for wa.state"
                        );
                    }
                }
            }

            redact_mcp_pane_state_fields(&mut states);
            Ok::<Vec<McpPaneState>, crate::Error>(states)
        });

        match result {
            Ok(states) => {
                let envelope = McpEnvelope::success(states, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaGetTextTool {
    config: Arc<Config>,
    db_path: Option<Arc<PathBuf>>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaGetTextTool {
    pub(super) fn new(config: Arc<Config>, db_path: Option<Arc<PathBuf>>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Option<Arc<PathBuf>>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaGetTextTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.get_text".to_string(),
            description: Some("Get text content from a pane (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "The pane ID to read from" },
                    "tail": { "type": "integer", "minimum": 1, "maximum": 10000, "default": 500, "description": "Number of lines to return (from end). Server enforces 1..=10000 (ft-ii8ss); requests outside this range return policy_error." },
                    "escapes": { "type": "boolean", "default": false, "description": "Include escape sequences" }
                },
                "required": ["pane_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: GetTextParams = serde_json::from_value(arguments).map_err(|err| {
            McpError::internal_error(format!(
                "wa.get_text schema/handler mismatch after framework validation: {err}"
            ))
        })?;

        // ft-ii8ss: enforce a server-side bound on `tail` independent of the
        // tool-schema's advertised maximum. The schema is a contract with the
        // client, but many MCP clients don't validate inputs against it before
        // sending — a malicious or buggy caller can otherwise send
        // `tail: usize::MAX` (memory-pressure vector: the downstream
        // scrollback fetch + the `Vec<String>::with_capacity(tail)` both scale
        // with the value). Same LIMIT_MIN/LIMIT_MAX pattern used by wa.events.
        const TAIL_MIN: usize = 1;
        const TAIL_MAX: usize = 10_000;
        if params.tail < TAIL_MIN || params.tail > TAIL_MAX {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!(
                    "tail must be in {TAIL_MIN}..={TAIL_MAX} (got {})",
                    params.tail
                ),
                Some(format!(
                    "The wa.get_text tool schema declares tail ∈ [{TAIL_MIN}, {TAIL_MAX}]; \
                     clamp your request or omit the field to use the default (500)."
                )),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let config = Arc::clone(&self.config);
        let db_path = self.db_path.as_ref().map(Arc::clone);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpGetTextData, McpToolError> =
            runtime.block_on(async move {
                // ft-xbnl0.2.3 tick 303: cx-first MCP get-text storage open.
                let open_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let storage = if let Some(path) = db_path.as_ref() {
                    Some(
                        StorageHandle::new_with_cx(&open_cx, &path.to_string_lossy())
                            .await
                            .map_err(McpToolError::from_error)?,
                    )
                } else {
                    None
                };

                let remote_pane = load_distributed_remote_pane(storage.as_ref(), params.pane_id)
                    .await
                    .map_err(McpToolError::from_error)?;
                let wezterm = default_wezterm_handle();
                // ft-xbnl0.2.3 tick 261: cx-first wezterm pane lookup.
                let wezterm_cx = crate::cx::Cx::current()
                    .unwrap_or_else(crate::cx::for_request);
                let pane_info = match wezterm.get_pane_with_cx(&wezterm_cx, params.pane_id).await {
                    Ok(pane_info) => Some(pane_info),
                    Err(err) => {
                        if remote_pane.is_some() {
                            None
                        } else {
                            return Err(McpToolError::from_error(err));
                        }
                    }
                };
                let domain = pane_info
                    .as_ref()
                    .map(|pane| pane.inferred_domain())
                    .or_else(|| remote_pane.as_ref().map(|pane| pane.domain.clone()))
                    .ok_or_else(|| {
                        McpToolError::new(
                            MCP_ERR_PANE_NOT_FOUND,
                            format!("Pane {} not found", params.pane_id),
                            Some("Use wa.state to list available panes.".to_string()),
                        )
                    })?;
                let resolution =
                    resolve_pane_capabilities(&config, storage.as_ref(), params.pane_id).await;
                let capabilities = resolution.capabilities;

                let mut engine = build_policy_engine_with_shared_rate_limiter(
                    &config,
                    false,
                    Arc::clone(&policy_rate_limiter),
                );
                let summary = format!("wa.get_text pane_id={}", params.pane_id);
                let mut input =
                    mcp_get_text_policy_input(params.pane_id, domain.clone(), capabilities, &summary);
                if let Some(pane_info) = pane_info.as_ref() {
                    if let Some(title) = &pane_info.title {
                        input = input.with_pane_title(title.clone());
                    }
                    if let Some(cwd) = &pane_info.cwd {
                        input = input.with_pane_cwd(cwd.clone());
                    }
                } else if let Some(record) = remote_pane.as_ref() {
                    if let Some(title) = &record.title {
                        input = input.with_pane_title(title.clone());
                    }
                    if let Some(cwd) = &record.cwd {
                        input = input.with_pane_cwd(cwd.clone());
                    }
                }

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Read denied by policy")
                        .to_string();
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    if let Some(storage_ref) = storage.as_ref() {
                        persist_mcp_policy_denial_async(
                            storage_ref,
                            "wa.get_text",
                            &summary,
                            &reason,
                            decision.rule_id(),
                            crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
                        )
                        .await;
                    }
                    return Err(McpToolError::new(
                        MCP_ERR_POLICY,
                        reason,
                        Some(POLICY_DENY_HINT.to_string()),
                    ));
                }
                if decision.requires_approval() {
                    let mut hint = approval_command(&decision);
                    if let Some(storage) = storage.as_ref() {
                        let workspace_id =
                            resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                        let store = ApprovalStore::new(
                            storage,
                            config.safety.approval.clone(),
                            workspace_id,
                        );
                        let updated = store
                            .attach_to_decision(decision, &input, Some(summary.clone()))
                            .await
                            .map_err(McpToolError::from_error)?;
                        hint = approval_command(&updated);
                        let reason = policy_reason(&updated)
                            .unwrap_or("Read requires approval")
                            .to_string();
                        // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                        persist_mcp_policy_denial_async(
                            storage,
                            "wa.get_text",
                            &summary,
                            &reason,
                            updated.rule_id(),
                            crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
                            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
                        )
                        .await;
                        return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                    }
                    let reason = policy_reason(&decision)
                        .unwrap_or("Read requires approval")
                        .to_string();
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                if remote_pane.is_some() {
                    return Err(McpToolError::new(
                        MCP_ERR_REMOTE_TEXT_UNAVAILABLE,
                        "Live get-text is unavailable for distributed panes".to_string(),
                        Some(
                            "Use wa.search, ft search, or ft robot search to inspect persisted remote output."
                                .to_string(),
                        ),
                    ));
                }

                let full_text = wezterm
                    .get_text(params.pane_id, params.escapes)
                    .await
                    .map_err(McpToolError::from_error)?;
                let (text, truncated, truncation_info) =
                    apply_tail_truncation(&full_text, params.tail);

                Ok(McpGetTextData {
                    pane_id: params.pane_id,
                    text: engine.redact_secrets(&text),
                    tail_lines: params.tail,
                    escapes_included: params.escapes,
                    truncated,
                    truncation_info,
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaWaitForTool {
    config: Arc<Config>,
    db_path: Option<Arc<PathBuf>>,
    wezterm: crate::wezterm::WeztermHandle,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaWaitForTool {
    pub(super) fn new(config: Arc<Config>, db_path: Option<Arc<PathBuf>>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Option<Arc<PathBuf>>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            default_wezterm_handle(),
            policy_rate_limiter,
        )
    }

    #[cfg(test)]
    pub(super) fn with_wezterm_handle(
        config: Arc<Config>,
        db_path: Option<Arc<PathBuf>>,
        wezterm: crate::wezterm::WeztermHandle,
    ) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            wezterm,
            policy_rate_limiter,
        )
    }

    pub(super) fn with_wezterm_handle_and_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Option<Arc<PathBuf>>,
        wezterm: crate::wezterm::WeztermHandle,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            wezterm,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaWaitForTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.wait_for".to_string(),
            description: Some("Wait for a pattern match in pane output (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Pane ID to wait on" },
                    "pattern": { "type": "string", "description": "Pattern to match (substring or regex)" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 30, "description": "Timeout in seconds" },
                    "tail": { "type": "integer", "minimum": 0, "default": 200, "description": "Tail lines to search (0 = full buffer)" },
                    "regex": { "type": "boolean", "default": false, "description": "Treat pattern as regex" }
                },
                "required": ["pane_id", "pattern"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: WaitForParams = serde_json::from_value(arguments).map_err(|err| {
            McpError::internal_error(format!(
                "wa.wait_for schema/handler mismatch after framework validation: {err}"
            ))
        })?;

        // Enforce the advertised timeout range server-side. serde accepts any
        // u64, and some MCP clients do not validate against the tool schema.
        if let Some(error) =
            validate_mcp_wait_timeout_secs("wa.wait_for", params.timeout_secs, start)
        {
            return error;
        }

        let matcher = match crate::wezterm::compile_wait_matcher(&params.pattern, params.regex) {
            Ok(matcher) => matcher,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid regex pattern: {err}"),
                    Some("Check the regex syntax".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let config = Arc::clone(&self.config);
        let db_path = self.db_path.as_ref().map(Arc::clone);
        let wezterm = Arc::clone(&self.wezterm);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let pattern = params.pattern.clone();
        let pane_id = params.pane_id;
        let tail = params.tail;
        let timeout_secs = params.timeout_secs;
        let is_regex = params.regex;

        let result = runtime.block_on(async move {
            let open_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let storage = if let Some(path) = db_path.as_ref() {
                Some(
                    StorageHandle::new_with_cx(&open_cx, &path.to_string_lossy())
                        .await
                        .map_err(McpToolError::from_error)?,
                )
            } else {
                None
            };

            let remote_pane = load_distributed_remote_pane(storage.as_ref(), pane_id)
                .await
                .map_err(McpToolError::from_error)?;
            let wezterm_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let pane_info = match wezterm.get_pane_with_cx(&wezterm_cx, pane_id).await {
                Ok(pane_info) => Some(pane_info),
                Err(err) => {
                    if remote_pane.is_some() {
                        None
                    } else {
                        return Err(McpToolError::from_error(err));
                    }
                }
            };
            let domain = pane_info
                .as_ref()
                .map(|pane| pane.inferred_domain())
                .or_else(|| remote_pane.as_ref().map(|pane| pane.domain.clone()))
                .ok_or_else(|| {
                    McpToolError::from_error(WeztermError::PaneNotFound(pane_id).into())
                })?;
            let resolution = resolve_pane_capabilities(&config, storage.as_ref(), pane_id).await;
            let capabilities = resolution.capabilities;

            let mut engine = build_policy_engine_with_shared_rate_limiter(
                &config,
                false,
                Arc::clone(&policy_rate_limiter),
            );
            let summary = format!("wa.wait_for pane_id={pane_id}");
            let mut input = mcp_get_text_policy_input(pane_id, domain, capabilities, &summary);
            if let Some(pane_info) = pane_info.as_ref() {
                if let Some(title) = &pane_info.title {
                    input = input.with_pane_title(title.clone());
                }
                if let Some(cwd) = &pane_info.cwd {
                    input = input.with_pane_cwd(cwd.clone());
                }
            } else if let Some(record) = remote_pane.as_ref() {
                if let Some(title) = &record.title {
                    input = input.with_pane_title(title.clone());
                }
                if let Some(cwd) = &record.cwd {
                    input = input.with_pane_cwd(cwd.clone());
                }
            }

            let decision = engine.authorize(&input);
            if decision.is_denied() {
                let reason = policy_reason(&decision)
                    .unwrap_or("Read denied by policy")
                    .to_string();
                if let Some(storage_ref) = storage.as_ref() {
                    persist_mcp_policy_denial_async(
                        storage_ref,
                        "wa.wait_for",
                        &summary,
                        &reason,
                        decision.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
                    )
                    .await;
                }
                return Err(McpToolError::new(
                    MCP_ERR_POLICY,
                    reason,
                    Some(POLICY_DENY_HINT.to_string()),
                ));
            }
            if decision.requires_approval() {
                let mut hint = approval_command(&decision);
                if let Some(storage) = storage.as_ref() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store =
                        ApprovalStore::new(storage, config.safety.approval.clone(), workspace_id);
                    let updated = store
                        .attach_to_decision(decision, &input, Some(summary.clone()))
                        .await
                        .map_err(McpToolError::from_error)?;
                    hint = approval_command(&updated);
                    let reason = policy_reason(&updated)
                        .unwrap_or("Read requires approval")
                        .to_string();
                    persist_mcp_policy_denial_async(
                        storage,
                        "wa.wait_for",
                        &summary,
                        &reason,
                        updated.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
                    )
                    .await;
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }
                let reason = policy_reason(&decision)
                    .unwrap_or("Read requires approval")
                    .to_string();
                return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
            }

            if pane_info.is_none() {
                return Err(McpToolError::from_error(
                    WeztermError::PaneNotFound(pane_id).into(),
                ));
            }

            let options = WaitOptions {
                tail_lines: tail,
                escapes: false,
                ..WaitOptions::default()
            };
            let source = WeztermHandleSource::new(Arc::clone(&wezterm));
            let waiter = PaneWaiter::new(&source).with_options(options);
            let timeout = std::time::Duration::from_secs(timeout_secs);
            waiter
                .wait_for(pane_id, &matcher, timeout)
                .await
                .map_err(McpToolError::from_error)
        });

        match result {
            Ok(WaitResult::Matched {
                elapsed_ms: wait_elapsed_ms,
                polls,
            }) => {
                let data = McpWaitForData {
                    pane_id,
                    pattern,
                    matched: true,
                    elapsed_ms: wait_elapsed_ms,
                    polls,
                    is_regex,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Ok(WaitResult::TimedOut {
                elapsed_ms: wait_elapsed_ms,
                polls,
                ..
            }) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_TIMEOUT,
                    format!(
                        "Timeout waiting for pattern '{pattern}' after {wait_elapsed_ms}ms ({polls} polls)"
                    ),
                    Some("Increase timeout_secs or verify the pattern.".to_string()),
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
            Ok(WaitResult::Cancelled { reason, polls }) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_TIMEOUT,
                    format!("Wait cancelled: {reason} ({polls} polls)"),
                    Some("Retry with a fresh request. Cancellation is not a timeout.".to_string()),
                    elapsed_ms(start),
                );
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaSearchTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaSearchTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaSearchTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.search".to_string(),
            description: Some(
                "Unified lexical/semantic/hybrid search across captured pane output (CLI/robot/MCP contract)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "FTS5 search query" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 20, "description": "Maximum results" },
                    "pane": { "type": "integer", "minimum": 0, "description": "Filter by pane ID" },
                    "since": { "type": "integer", "description": "Filter by lower bound time (epoch ms, inclusive)" },
                    "until": { "type": "integer", "description": "Filter by upper bound time (epoch ms, inclusive)" },
                    "snippets": { "type": "boolean", "default": true, "description": "Include snippets in results" }
                    ,
                    "mode": { "type": "string", "enum": ["lexical", "semantic", "hybrid"], "default": "lexical", "description": "Search mode (lexical, semantic, or hybrid)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "search".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: SearchParams = serde_json::from_value(arguments).map_err(|err| {
            McpError::internal_error(format!(
                "wa.search schema/handler mismatch after framework validation: {err}"
            ))
        })?;

        let parsed = match parse_unified_search_query(
            SearchQueryInput {
                query: params.query,
                limit: params.limit,
                pane: params.pane,
                since: params.since,
                until: params.until,
                snippets: params.snippets,
                mode: params.mode,
                explain: None,
            },
            SearchQueryDefaults::default(),
        ) {
            Ok(parsed) => parsed,
            Err(err) => {
                let code = if err.is_query_lint_error() {
                    MCP_ERR_FTS_QUERY
                } else {
                    MCP_ERR_INVALID_ARGS
                };
                let envelope =
                    McpEnvelope::<()>::error(code, err.message(), err.hint(), elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let canonical = parsed.query;

        let requested_mode = canonical.mode;
        let search_mode = match requested_mode {
            UnifiedSearchMode::Lexical => crate::search::SearchMode::Lexical,
            UnifiedSearchMode::Semantic => crate::search::SearchMode::Semantic,
            UnifiedSearchMode::Hybrid => crate::search::SearchMode::Hybrid,
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let query_for_storage = canonical.query.clone();
        let search_options = to_storage_search_options(&canonical);
        let snippets_enabled = canonical.snippets;
        let hybrid_rrf_k = effective_search_rrf_k(config.as_ref());
        let (hybrid_lexical_weight, hybrid_semantic_weight) =
            effective_search_fusion_weights(config.as_ref());
        let hybrid_fusion_backend = effective_search_fusion_backend(config.as_ref());
        let semantic_query = if matches!(
            requested_mode,
            UnifiedSearchMode::Semantic | UnifiedSearchMode::Hybrid
        ) {
            use crate::search::Embedder;

            let embedder = crate::search::HashEmbedder::default();
            match embedder.embed(&canonical.query) {
                Ok(vector) => Some((embedder.info().name, vector)),
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_STORAGE,
                        format!("Failed to embed query for semantic search: {err}"),
                        Some(
                            "Try mode=lexical or verify semantic embedding support in this build."
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        } else {
            None
        };

        enum SearchExecution {
            Lexical(Vec<crate::storage::SearchResult>),
            Hybrid(crate::storage::HybridSearchBundle),
        }

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<SearchExecution, McpToolError> =
            runtime.block_on(async move {
                // ft-xbnl0.2.3 tick 303: cx-first MCP search storage open.
                let search_open_cx =
                    crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let storage =
                    StorageHandle::new_with_cx(&search_open_cx, &db_path.to_string_lossy())
                        .await
                        .map_err(McpToolError::from_error)?;
                let mut semantic_budget_config = storage.semantic_budget_snapshot().config;
                semantic_budget_config.max_semantic_latency_ms =
                    effective_search_quality_timeout_ms(config.as_ref());
                storage.set_semantic_budget_config(semantic_budget_config);

                let mut engine = build_policy_engine_with_shared_rate_limiter(
                    &config,
                    false,
                    Arc::clone(&policy_rate_limiter),
                );
                let summary = engine.redact_secrets(&query_for_storage);
                let mut input = mcp_search_output_policy_input(&summary);

                if let Some(pane_id) = search_options.pane_id {
                    let wezterm = default_wezterm_handle();
                    let pane_info = wezterm
                        .get_pane(pane_id)
                        .await
                        .map_err(McpToolError::from_error)?;
                    let domain = pane_info.inferred_domain();
                    let resolution =
                        resolve_pane_capabilities(&config, Some(&storage), pane_id).await;
                    input = input
                        .with_pane(pane_id)
                        .with_domain(domain)
                        .with_capabilities(resolution.capabilities);
                    if let Some(title) = &pane_info.title {
                        input = input.with_pane_title(title.clone());
                    }
                    if let Some(cwd) = &pane_info.cwd {
                        input = input.with_pane_cwd(cwd.clone());
                    }
                } else {
                    input = input.with_capabilities(PaneCapabilities::unknown());
                }

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Search denied by policy")
                        .to_string();
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.search",
                        &summary,
                        &reason,
                        decision.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
                    )
                    .await;
                    return Err(McpToolError::new(
                        MCP_ERR_POLICY,
                        reason,
                        Some(POLICY_DENY_HINT.to_string()),
                    ));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store =
                        ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                    let updated = store
                        .attach_to_decision(decision, &input, Some(summary.clone()))
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Search requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.search",
                        &summary,
                        &reason,
                        updated.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
                    )
                    .await;
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                match requested_mode {
                    UnifiedSearchMode::Lexical => {
                        let results = storage
                            .search_with_results(&query_for_storage, search_options)
                            .await
                            .map_err(McpToolError::from_error)?;
                        Ok(SearchExecution::Lexical(results))
                    }
                    UnifiedSearchMode::Semantic | UnifiedSearchMode::Hybrid => {
                        let (embedder_id, query_vector) = semantic_query.ok_or_else(|| {
                            McpToolError::new(
                                MCP_ERR_STORAGE,
                                "semantic query vector missing for non-lexical wa.search mode"
                                    .to_string(),
                                None,
                            )
                        })?;

                        let bundle = storage
                            .hybrid_search_with_results(
                                &query_for_storage,
                                search_options,
                                &embedder_id,
                                &query_vector,
                                search_mode,
                                hybrid_rrf_k,
                                hybrid_lexical_weight,
                                hybrid_semantic_weight,
                                Some(hybrid_fusion_backend),
                            )
                            .await
                            .map_err(McpToolError::from_error)?;
                        Ok(SearchExecution::Hybrid(bundle))
                    }
                }
            });

        let redactor = crate::redactor::Redactor::new();
        let redacted_query = redactor.redact(&canonical.query);

        match result {
            Ok(SearchExecution::Lexical(results)) => {
                let total_hits = results.len();
                let hits: Vec<McpSearchHit> = results
                    .into_iter()
                    .map(|r| McpSearchHit {
                        segment_id: r.segment.id,
                        pane_id: r.segment.pane_id,
                        seq: r.segment.seq,
                        captured_at: r.segment.captured_at,
                        score: r.score,
                        snippet: r.snippet.map(|snippet| redactor.redact(&snippet)),
                        content: if snippets_enabled {
                            None
                        } else {
                            Some(redactor.redact(&r.segment.content))
                        },
                        semantic_score: None,
                        fusion_rank: None,
                    })
                    .collect();

                let data = McpSearchData {
                    query: redacted_query.clone(),
                    results: hits,
                    total_hits,
                    limit: canonical.limit,
                    pane_filter: canonical.pane,
                    since_filter: canonical.since,
                    until_filter: canonical.until,
                    mode: canonical.mode.as_str().to_string(),
                    metrics: None,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Ok(SearchExecution::Hybrid(bundle)) => {
                let crate::storage::HybridSearchBundle {
                    mode,
                    requested_mode,
                    fallback_reason,
                    rrf_k,
                    lexical_weight,
                    semantic_weight,
                    fusion_backend,
                    lexical_candidates,
                    semantic_candidates,
                    semantic_cache_hit,
                    semantic_latency_ms,
                    semantic_rows_scanned,
                    semantic_budget_state,
                    semantic_backoff_until_ms,
                    results,
                } = bundle;
                let effective_mode = mode.clone();

                let total_hits = results.len();
                let hits: Vec<McpSearchHit> = results
                    .into_iter()
                    .map(|hit| {
                        let result = hit.result;
                        McpSearchHit {
                            segment_id: result.segment.id,
                            pane_id: result.segment.pane_id,
                            seq: result.segment.seq,
                            captured_at: result.segment.captured_at,
                            score: hit.fusion_score,
                            snippet: result.snippet.map(|snippet| redactor.redact(&snippet)),
                            content: if snippets_enabled {
                                None
                            } else {
                                Some(redactor.redact(&result.segment.content))
                            },
                            semantic_score: hit.semantic_score,
                            fusion_rank: Some(hit.fusion_rank),
                        }
                    })
                    .collect();

                let metrics = serde_json::json!({
                    "requested_mode": requested_mode,
                    "effective_mode": effective_mode,
                    "fallback_reason": fallback_reason,
                    "rrf_k": rrf_k,
                    "lexical_weight": lexical_weight,
                    "semantic_weight": semantic_weight,
                    "fusion_backend": fusion_backend,
                    "lexical_candidates": lexical_candidates,
                    "semantic_candidates": semantic_candidates,
                    "semantic_cache_hit": semantic_cache_hit,
                    "semantic_latency_ms": semantic_latency_ms,
                    "semantic_rows_scanned": semantic_rows_scanned,
                    "semantic_budget_state": semantic_budget_state,
                    "semantic_backoff_until_ms": semantic_backoff_until_ms
                });

                let data = McpSearchData {
                    query: redacted_query,
                    results: hits,
                    total_hits,
                    limit: canonical.limit,
                    pane_filter: canonical.pane,
                    since_filter: canonical.since,
                    until_filter: canonical.until,
                    mode: effective_mode,
                    metrics: Some(metrics),
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaEventsTool {
    db_path: Arc<PathBuf>,
}

impl WaEventsTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaEventsTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events".to_string(),
            description: Some("Get pattern detection events (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 20, "description": "Maximum results" },
                    "pane": { "type": "integer", "minimum": 0, "description": "Filter by pane ID" },
                    "rule_id": { "type": "string", "description": "Filter by rule ID (exact match)" },
                    "event_type": { "type": "string", "description": "Filter by event type" },
                    "triage_state": { "type": "string", "description": "Filter by triage state (exact match)" },
                    "label": { "type": "string", "description": "Filter by label (exact match)" },
                    "unhandled": { "type": "boolean", "default": false, "description": "Only return unhandled events" },
                    "since": { "type": "integer", "description": "Filter by time (epoch ms)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsParams = if arguments.is_null() {
            EventsParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional limit, pane, rule_id, event_type, triage_state, label, unhandled, since".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        // Enforce the input schema's advertised `"limit": { "minimum": 1,
        // "maximum": 1000 }` bounds at the server. The tool schema is a
        // contract with the client, but many MCP clients don't validate
        // tool inputs against the schema before sending — a malicious
        // or buggy caller can otherwise send `limit: 0` (silent no-op)
        // or `limit: u64::MAX` (memory-pressure vector: the downstream
        // `storage.get_events_with_cx` query and the subsequent
        // `Vec::with_capacity(events.len())` both scale with the limit).
        const LIMIT_MIN: usize = 1;
        const LIMIT_MAX: usize = 1000;
        if params.limit < LIMIT_MIN || params.limit > LIMIT_MAX {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!(
                    "limit must be in {LIMIT_MIN}..={LIMIT_MAX} (got {})",
                    params.limit
                ),
                Some(format!(
                    "The wa.events tool schema declares limit ∈ [{LIMIT_MIN}, {LIMIT_MAX}]; \
                     clamp your request or omit the field to use the default (20)."
                )),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventsData> = runtime.block_on(async {
            // ft-xbnl0.2.3 tick 303: cx-first MCP events storage open.
            let events_open_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let storage =
                StorageHandle::new_with_cx(&events_open_cx, &db_path.to_string_lossy()).await?;

            let query = EventQuery {
                limit: Some(params.limit),
                pane_id: params.pane,
                rule_id: params.rule_id.clone(),
                event_type: params.event_type.clone(),
                triage_state: params.triage_state.clone(),
                label: params.label.clone(),
                unhandled_only: params.unhandled,
                since: params.since,
                until: None,
            };

            // ft-xbnl0.2.3 tick 258: cx-first MCP event-query + annotation loop.
            let events_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let events = storage.get_events_with_cx(&events_cx, query).await?;
            let total_count = events.len();

            let mut items: Vec<McpEventItem> = Vec::with_capacity(events.len());
            for e in events {
                let pack_id = e.rule_id.split('.').next().map_or_else(
                    || "builtin:unknown".to_string(),
                    |agent| format!("builtin:{agent}"),
                );

                let annotations = match storage
                    .get_event_annotations_with_cx(&events_cx, e.id)
                    .await
                {
                    Ok(Some(a)) => Some(a),
                    Ok(None) => None,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            event_id = e.id,
                            "Failed to load event annotations"
                        );
                        None
                    }
                };

                items.push(McpEventItem {
                    id: e.id,
                    pane_id: e.pane_id,
                    rule_id: e.rule_id,
                    pack_id,
                    event_type: e.event_type,
                    severity: e.severity,
                    confidence: e.confidence,
                    extracted: e.extracted,
                    annotations,
                    captured_at: e.detected_at,
                    handled_at: e.handled_at,
                    workflow_id: e.handled_by_workflow_id,
                });
            }

            Ok(McpEventsData {
                events: items,
                total_count,
                limit: params.limit,
                pane_filter: params.pane,
                rule_id_filter: params.rule_id,
                event_type_filter: params.event_type,
                triage_state_filter: params.triage_state,
                label_filter: params.label,
                unhandled_only: params.unhandled,
                since_filter: params.since,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaSendTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    wezterm: crate::wezterm::WeztermHandle,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaSendTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            default_wezterm_handle(),
            policy_rate_limiter,
        )
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            default_wezterm_handle(),
            policy_rate_limiter,
        )
    }

    #[cfg(test)]
    pub(super) fn with_wezterm_handle(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        wezterm: crate::wezterm::WeztermHandle,
    ) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            wezterm,
            policy_rate_limiter,
        )
    }

    // ft-ljgyr: un-cfg-test. Non-test callers `WaSendTool::new` at :2000 and
    // `WaSendTool::new_with_shared_rate_limiter` at :2010 both delegate here,
    // so gating this behind `#[cfg(test)]` broke every non-test build with
    // E0599. The body has no test-only dependencies — it's a plain struct
    // ctor — so the test gate was a slip during pane 2's ft-eu0no
    // shared-rate-limiter refactor.
    pub(super) fn with_wezterm_handle_and_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        wezterm: crate::wezterm::WeztermHandle,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            wezterm,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaSendTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.send".to_string(),
            description: Some("Send text to a pane with policy gating (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Pane ID to send to" },
                    "text": { "type": "string", "description": "Text to send" },
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview without sending" },
                    "wait_for": { "type": "string", "description": "Wait for a pattern after sending" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 30, "description": "Wait-for timeout (seconds)" },
                    "wait_for_regex": { "type": "boolean", "default": false, "description": "Treat wait_for as regex" }
                },
                "required": ["pane_id", "text"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: SendParams = serde_json::from_value(arguments).map_err(|err| {
            McpError::internal_error(format!(
                "wa.send schema/handler mismatch after framework validation: {err}"
            ))
        })?;

        // Enforce the advertised timeout range server-side. `wa.send` uses
        // this bound for its optional wait_for phase.
        if let Some(error) = validate_mcp_wait_timeout_secs("wa.send", params.timeout_secs, start) {
            return error;
        }

        // [ft-05hfm] Bound the text payload before any downstream
        // buffering, redaction, or dispatch. Without this cap, an
        // MCP client could submit a multi-gigabyte `text` field; the
        // full string would transit the injector → policy → wezterm
        // CLI pipeline before anyone noticed, OOM-ing the watcher.
        if params.text.len() > MAX_SEND_TEXT_BYTES {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!(
                    "text payload is {} bytes; max allowed is {} bytes",
                    params.text.len(),
                    MAX_SEND_TEXT_BYTES,
                ),
                Some(
                    "wa.send is for interactive pane input — split large \
                     payloads into multiple calls or use a file drop instead."
                        .to_string(),
                ),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async move {
            // ft-xbnl0.2.3 tick 303: cx-first MCP pane-state storage open (reuse wezterm_cx).
            let wezterm_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let storage =
                StorageHandle::new_with_cx(&wezterm_cx, &db_path.to_string_lossy()).await?;
            let wezterm = Arc::clone(&self.wezterm);
            let pane_info = wezterm
                .get_pane_with_cx(&wezterm_cx, params.pane_id)
                .await?;
            let domain = pane_info.inferred_domain();

            let resolution =
                resolve_pane_capabilities(&config, Some(&storage), params.pane_id).await;
            let capabilities = resolution.capabilities;

            let mut engine = build_policy_engine_with_shared_rate_limiter(
                &config,
                config.safety.require_prompt_active,
                Arc::clone(&policy_rate_limiter),
            );
            let summary = engine.redact_secrets(&params.text);

            let mut input = mcp_send_text_policy_input(
                params.pane_id,
                domain,
                capabilities.clone(),
                &summary,
                &params.text,
            );

            if let Some(title) = &pane_info.title {
                input = input.with_pane_title(title.clone());
            }
            if let Some(cwd) = &pane_info.cwd {
                input = input.with_pane_cwd(cwd.clone());
            }

            let wait_matcher = match params.wait_for.as_ref() {
                Some(pattern) => Some(crate::wezterm::compile_wait_matcher(
                    pattern,
                    params.wait_for_regex,
                )?),
                None => None,
            };

            if params.dry_run {
                let decision = engine.authorize_preview(&input);
                let injection = injection_from_decision(
                    decision,
                    summary,
                    params.pane_id,
                    ActionKind::SendText,
                );
                return Ok(McpSendData {
                    pane_id: params.pane_id,
                    injection,
                    wait_for: None,
                    verification_error: None,
                    dry_run: true,
                });
            }

            let mut injector =
                PolicyGatedInjector::with_storage(engine, Arc::clone(&wezterm), storage.clone());
            let mut injection = injector
                .send_text(
                    params.pane_id,
                    &params.text,
                    ActorKind::Mcp,
                    &capabilities,
                    None,
                )
                .await;

            if let InjectionResult::RequiresApproval {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id,
            } = injection
            {
                let workspace_id = resolve_workspace_id(&config)?;
                let store =
                    ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                let updated = store
                    .attach_to_decision(decision, &input, Some(summary.clone()))
                    .await?;
                injection = InjectionResult::RequiresApproval {
                    decision: updated,
                    summary,
                    pane_id,
                    action,
                    audit_action_id,
                };
            }

            let mut wait_for_data = None;
            let mut verification_error = None;
            if injection.is_allowed() {
                if let (Some(pattern), Some(matcher)) =
                    (params.wait_for.as_ref(), wait_matcher.as_ref())
                {
                    let options = WaitOptions {
                        tail_lines: 200,
                        escapes: false,
                        ..WaitOptions::default()
                    };
                    let source = WeztermHandleSource::new(Arc::clone(&wezterm));
                    let waiter = PaneWaiter::new(&source).with_options(options);
                    let timeout = std::time::Duration::from_secs(params.timeout_secs);
                    match waiter.wait_for(params.pane_id, matcher, timeout).await {
                        Ok(WaitResult::Matched { elapsed_ms, polls }) => {
                            wait_for_data = Some(McpWaitForData {
                                pane_id: params.pane_id,
                                pattern: pattern.clone(),
                                matched: true,
                                elapsed_ms,
                                polls,
                                is_regex: params.wait_for_regex,
                            });
                        }
                        Ok(WaitResult::TimedOut {
                            elapsed_ms, polls, ..
                        }) => {
                            wait_for_data = Some(McpWaitForData {
                                pane_id: params.pane_id,
                                pattern: pattern.clone(),
                                matched: false,
                                elapsed_ms,
                                polls,
                                is_regex: params.wait_for_regex,
                            });
                            verification_error =
                                Some(format!("Timeout waiting for pattern '{pattern}'"));
                        }
                        Ok(WaitResult::Cancelled { reason, polls }) => {
                            wait_for_data = Some(McpWaitForData {
                                pane_id: params.pane_id,
                                pattern: pattern.clone(),
                                matched: false,
                                elapsed_ms: 0,
                                polls,
                                is_regex: params.wait_for_regex,
                            });
                            verification_error = Some(format!("Wait cancelled: {reason}"));
                        }
                        Err(e) => {
                            verification_error = Some(format!("wait-for failed: {e}"));
                        }
                    }
                }
            }

            Ok(McpSendData {
                pane_id: params.pane_id,
                injection,
                wait_for: wait_for_data,
                verification_error,
                dry_run: false,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaWorkflowRunTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaWorkflowRunTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaWorkflowRunTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.workflow_run".to_string(),
            description: Some("Execute a workflow (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Workflow name" },
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Target pane ID" },
                    "force": { "type": "boolean", "default": false, "description": "Force run (bypass handled guard)" },
                    "dry_run": { "type": "boolean", "default": false, "description": "Preview without executing" }
                },
                "required": ["name", "pane_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "workflow".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: WorkflowRunParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with name, pane_id, force, dry_run".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpWorkflowRunData, McpToolError> =
            runtime.block_on(async move {
                // ft-xbnl0.2.3 tick 303: cx-first MCP workflow run storage open.
                let wf_open_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let storage =
                    StorageHandle::new_with_cx(&wf_open_cx, &db_path.to_string_lossy())
                        .await
                        .map_err(McpToolError::from_error)?;
                let storage = Arc::new(storage);

                let wezterm = default_wezterm_handle();
                let pane_info = wezterm
                    .get_pane(params.pane_id)
                    .await
                    .map_err(McpToolError::from_error)?;
                let domain = pane_info.inferred_domain();

                let resolution =
                    resolve_pane_capabilities(&config, Some(storage.as_ref()), params.pane_id)
                        .await;
                let capabilities = resolution.capabilities;

                let workflow_assembly = build_mcp_workflow_assembly(
                    Arc::clone(&config),
                    Arc::clone(&storage),
                    Arc::clone(&wezterm),
                    Arc::clone(&policy_rate_limiter),
                );
                let mut policy_engine = workflow_assembly.policy_engine();
                let summary = format!("workflow run {}", params.name);

                let mut input = mcp_workflow_run_policy_input(
                    params.pane_id,
                    domain,
                    capabilities.clone(),
                    &summary,
                );

                if let Some(title) = &pane_info.title {
                    input = input.with_pane_title(title.clone());
                }
                if let Some(cwd) = &pane_info.cwd {
                    input = input.with_pane_cwd(cwd.clone());
                }

                let decision =
                    authorize_mcp_policy_call(&mut policy_engine, &input, params.dry_run);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Workflow denied by policy")
                        .to_string();
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    if !params.dry_run {
                        persist_mcp_policy_denial_async(
                            storage.as_ref(),
                            "wa.workflow_run",
                            &summary,
                            &reason,
                            decision.rule_id(),
                            crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
                        )
                        .await;
                    }
                    return Err(McpToolError::new(
                        MCP_ERR_POLICY,
                        reason,
                        Some(POLICY_DENY_HINT.to_string()),
                    ));
                }
                if decision.requires_approval() {
                    if params.dry_run {
                        let reason = policy_reason(&decision)
                            .unwrap_or("Workflow requires approval")
                            .to_string();
                        return Err(McpToolError::new(
                            MCP_ERR_POLICY,
                            reason,
                            Some(
                                "Dry-run preview only: rerun without dry_run to request an allow-once approval token."
                                    .to_string(),
                            ),
                        ));
                    }
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store = ApprovalStore::new(
                        storage.as_ref(),
                        config.safety.approval.clone(),
                        workspace_id,
                    );
                    let updated = store
                        .attach_to_decision(decision, &input, Some(summary.clone()))
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Workflow requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        storage.as_ref(),
                        "wa.workflow_run",
                        &summary,
                        &reason,
                        updated.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
                    )
                    .await;
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                if params.dry_run {
                    return Ok(McpWorkflowRunData {
                        workflow_name: params.name,
                        pane_id: params.pane_id,
                        execution_id: None,
                        status: "dry_run".to_string(),
                        message: Some("Dry-run: workflow not executed".to_string()),
                        result: None,
                        steps_executed: None,
                        step_index: None,
                        elapsed_ms: Some(elapsed_ms(start)),
                    });
                }

                let _ = params.force;
                let runner = workflow_assembly.runner();
                let workflow = runner.find_workflow_by_name(&params.name).ok_or_else(|| {
                    McpToolError::new(
                        MCP_ERR_WORKFLOW,
                        format!("Workflow '{}' not found", params.name),
                        Some(
                            "Ensure workflows are enabled or run ft watch for event-driven workflows."
                                .to_string(),
                        ),
                    )
                })?;

                let execution_id = format!("mcp-{}-{}", params.name, now_ms());
                let result = runner
                    .run_workflow(params.pane_id, workflow, &execution_id, 0)
                    .await;

                let (status, message, result_value, steps_executed, step_index) = match result {
                    WorkflowExecutionResult::Completed {
                        result,
                        steps_executed,
                        ..
                    } => ("completed", None, Some(result), Some(steps_executed), None),
                    WorkflowExecutionResult::Aborted {
                        reason, step_index, ..
                    } => ("aborted", Some(reason), None, None, Some(step_index)),
                    WorkflowExecutionResult::PolicyDenied {
                        reason, step_index, ..
                    } => ("policy_denied", Some(reason), None, None, Some(step_index)),
                    WorkflowExecutionResult::Error { error, .. } => {
                        ("error", Some(error), None, None, None)
                    }
                };

                Ok(McpWorkflowRunData {
                    workflow_name: params.name,
                    pane_id: params.pane_id,
                    execution_id: Some(execution_id),
                    status: status.to_string(),
                    message,
                    result: result_value,
                    steps_executed,
                    step_index,
                    elapsed_ms: Some(elapsed_ms(start)),
                })
            });

        match result {
            Ok(data) => {
                let status = data.status.as_str();
                if status == "completed" || status == "dry_run" {
                    let envelope = McpEnvelope::success(data, elapsed_ms(start));
                    envelope_to_content(envelope)
                } else if status == "policy_denied" {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_POLICY,
                        "Workflow denied by policy".to_string(),
                        Some("Review safety configuration or use dry_run.".to_string()),
                        elapsed_ms(start),
                    );
                    envelope_to_content(envelope)
                } else {
                    let message = data
                        .message
                        .clone()
                        .unwrap_or_else(|| "workflow failed".to_string());
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_WORKFLOW,
                        message,
                        None,
                        elapsed_ms(start),
                    );
                    envelope_to_content(envelope)
                }
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaTxPlanTool {
    config: Arc<Config>,
}

impl WaTxPlanTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaTxPlanTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_plan".to_string(),
            description: Some(
                "Validate and summarize mission transaction contract metadata (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxPlanParams = if arguments.is_null() {
            TxPlanParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional contract_file".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let data = McpTxPlanData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            lifecycle_state: contract.lifecycle_state,
            step_count: contract.plan.steps.len(),
            precondition_count: contract.plan.preconditions.len(),
            compensation_count: contract.plan.compensations.len(),
            legal_transitions: mcp_tx_transition_info(contract.lifecycle_state),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaTxShowTool {
    config: Arc<Config>,
}

impl WaTxShowTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaTxShowTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_show".to_string(),
            description: Some(
                "Inspect mission tx lifecycle, receipts, and legal transitions (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" },
                    "include_contract": { "type": "boolean", "default": false, "description": "Include full contract payload in response" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxShowParams = if arguments.is_null() {
            TxShowParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some(
                            "Expected object with optional contract_file, include_contract"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let data = McpTxShowData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            lifecycle_state: contract.lifecycle_state,
            outcome: contract.outcome.clone(),
            step_count: contract.plan.steps.len(),
            precondition_count: contract.plan.preconditions.len(),
            compensation_count: contract.plan.compensations.len(),
            receipt_count: contract.receipts.len(),
            legal_transitions: mcp_tx_transition_info(contract.lifecycle_state),
            contract: params.include_contract.then_some(contract),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaTxRunTool {
    config: Arc<Config>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaTxRunTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaTxRunTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_run".to_string(),
            description: Some(
                "Execute deterministic tx prepare+commit and compensation on partial failure (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" },
                    "fail_step": { "type": "string", "description": "Deterministic commit failure injection step_id" },
                    "paused": { "type": "boolean", "default": false, "description": "Treat mission as paused; commit returns pause-suspended outcome" },
                    "kill_switch": { "type": "string", "description": "off|safe_mode|hard_stop (safe-mode/hard-stop also accepted)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxRunParams = if arguments.is_null() {
            TxRunParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some(
                            "Expected object with optional contract_file, fail_step, paused, kill_switch"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        // ft-x86z2: policy gate before any side effect (contract load, tx execute).
        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.tx_run",
            "tx.run",
            start,
        ) {
            return deny;
        }

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let _contract_lock = match acquire_mcp_tx_contract_lock(&contract_path) {
            Ok(lock) => lock,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let kill_switch = match mcp_parse_mission_kill_switch(params.kill_switch.as_deref()) {
            Ok(level) => level,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        if let Some(fail_step_id) = params.fail_step.as_deref()
            && !contract
                .plan
                .steps
                .iter()
                .any(|step| step.step_id.0 == fail_step_id)
        {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!("Unknown fail_step: {fail_step_id}"),
                Some("Use step IDs from wa.tx_show(include_contract=true).".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let now_ms = i64::try_from(now_ms()).unwrap_or(0);
        let layout = match self.config.workspace_layout(None) {
            Ok(layout) => layout,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_CONFIG,
                    format!("Failed to resolve workspace layout: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let runtime = match CompatRuntimeBuilder::current_thread().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_STORAGE,
                    format!("Tokio runtime init failed: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let storage = match runtime.block_on(async {
            let tx_run_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            StorageHandle::new_with_cx(&tx_run_cx, &layout.db_path.to_string_lossy()).await
        }) {
            Ok(storage) => storage,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_STORAGE,
                    format!("Failed to open tx storage: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let workspace_id = match resolve_workspace_id(self.config.as_ref()) {
            Ok(workspace_id) => workspace_id,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_CONFIG,
                    format!("Failed to resolve workspace id: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let policy_engine = build_policy_engine_with_shared_rate_limiter(
            self.config.as_ref(),
            false,
            Arc::clone(&self.policy_rate_limiter),
        );
        let prepare_context = crate::plan::TxPrepareEvaluationContext::new(workspace_id)
            .with_surface(PolicySurface::Mcp);
        let approvals = crate::plan::StorageBackedPrepareApprovalChecker::new(Some(&storage));
        let targets = crate::plan::StorageBackedPrepareTargetLookup::new(None, Some(&storage));
        let executor = crate::tx_execution::PaneStepExecutor::new(
            tx_run_wezterm_handle(),
            RefCell::new(policy_engine),
            approvals,
            targets,
            prepare_context,
        );
        let execution_engine = crate::tx_execution::TxExecutionEngine::new(
            executor,
            crate::tx_execution::TxExecutionConfig {
                kill_switch,
                paused: params.paused,
                fail_step: params.fail_step.clone(),
                ..crate::tx_execution::TxExecutionConfig::default()
            },
        );

        let mut persisted_contract = contract.clone();
        let execution = match execution_engine.execute(&mut persisted_contract, now_ms) {
            Ok(execution) => execution,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    "robot.tx_execution_failed",
                    format!("tx execution failed: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        if let Err(err) = mcp_save_mission_tx_contract_to_path(&contract_path, &persisted_contract)
        {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpTxRunData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            prepare_report: execution.prepare_report,
            commit_report: execution.commit_report,
            compensation_report: execution.compensation_report,
            final_state: execution.final_state,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaTxRollbackTool {
    config: Arc<Config>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaTxRollbackTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaTxRollbackTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.tx_rollback".to_string(),
            description: Some(
                "Execute compensation phase for committed tx steps (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "contract_file": { "type": "string", "description": "Optional path to MissionTxContract JSON (default: .ft/mission/tx-active.json)" },
                    "fail_compensation_for_step": { "type": "string", "description": "Deterministic compensation failure injection step_id" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "tx".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: TxRollbackParams = if arguments.is_null() {
            TxRollbackParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some(
                            "Expected object with optional contract_file, fail_compensation_for_step"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        // ft-x86z2: policy gate before any side effect (contract load, compensation).
        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.tx_rollback",
            "tx.rollback",
            start,
        ) {
            return deny;
        }

        let contract_path = match mcp_resolve_mission_tx_file_path(
            self.config.as_ref(),
            params.contract_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let _contract_lock = match acquire_mcp_tx_contract_lock(&contract_path) {
            Ok(lock) => lock,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let contract = match mcp_load_mission_tx_contract_from_path(&contract_path) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let now_ms = i64::try_from(now_ms()).unwrap_or(0);
        let commit_report = match crate::plan::mission_tx_rollback_commit_report(&contract, now_ms)
        {
            Ok(report) => report,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    err,
                    Some(
                        "Use wa.tx_show(include_contract=true) and ensure the contract includes commit receipts for the steps that actually committed."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        if let Some(step_id) = params.fail_compensation_for_step.as_deref()
            && !commit_report
                .step_results
                .iter()
                .any(|result| result.step_id.0 == step_id && result.outcome.is_committed())
        {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!("Unknown fail_compensation_for_step: {step_id}"),
                Some("Use a committed step ID from wa.tx_show(include_contract=true).".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }
        let comp_inputs = mcp_build_tx_compensation_inputs(
            &commit_report,
            params.fail_compensation_for_step.as_deref(),
            now_ms,
        );
        let mut compensating_contract = contract.clone();
        compensating_contract.lifecycle_state = crate::plan::MissionTxState::Compensating;
        compensating_contract
            .receipts
            .clone_from(&contract.receipts);
        let compensation_report = match crate::plan::execute_compensation_phase(
            &compensating_contract,
            &commit_report,
            &comp_inputs,
            now_ms,
        ) {
            Ok(report) => report,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    "robot.tx_execution_failed",
                    format!("rollback compensation failed: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let final_state = compensation_report.outcome.target_tx_state();
        let mut persisted_contract = contract.clone();
        persisted_contract.lifecycle_state = final_state;
        persisted_contract.outcome = mcp_tx_outcome_for_state(final_state);
        persisted_contract
            .receipts
            .extend(compensation_report.receipts.clone());
        if let Err(err) = mcp_save_mission_tx_contract_to_path(&contract_path, &persisted_contract)
        {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpTxRollbackData {
            contract_file: contract_path.display().to_string(),
            tx_id: contract.intent.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            final_state,
            compensation_report,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

pub(super) struct WaReservationsTool {
    db_path: Arc<PathBuf>,
}

impl WaReservationsTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaReservationsTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.reservations".to_string(),
            description: Some("List active pane reservations (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Filter by pane ID" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "reservations".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: ReservationsParams = if arguments.is_null() {
            ReservationsParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional pane_id".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            // ft-xbnl0.2.3 tick 303: cx-first MCP reservation list.
            let res_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let storage = StorageHandle::new_with_cx(&res_cx, &db_path.to_string_lossy()).await?;
            storage.list_active_reservations_with_cx(&res_cx).await
        });

        match result {
            Ok(reservations) => {
                let filtered: Vec<&PaneReservation> = reservations
                    .iter()
                    .filter(|r| match params.pane_id {
                        Some(pane_id) => r.pane_id == pane_id,
                        None => true,
                    })
                    .collect();

                let total = filtered.len();
                let items: Vec<McpReservationInfo> =
                    filtered.into_iter().map(reservation_to_mcp_info).collect();

                let data = McpReservationsData {
                    reservations: items,
                    total,
                    pane_filter: params.pane_id,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaReserveTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaReserveTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaReserveTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.reserve".to_string(),
            description: Some("Create an exclusive pane reservation (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "Pane ID to reserve" },
                    "owner_kind": { "type": "string", "description": "Kind of owner (workflow, agent, mcp, manual)" },
                    "owner_id": { "type": "string", "description": "Unique identifier for the owner" },
                    "reason": { "type": "string", "description": "Human-readable reason for reservation" },
                    "ttl_ms": { "type": "integer", "minimum": 1000, "default": 300000, "description": "Time to live in milliseconds" }
                },
                "required": ["pane_id", "owner_kind", "owner_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "reservations".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: ReserveParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with pane_id, owner_kind, owner_id (required), reason, ttl_ms"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpReserveData, McpToolError> =
            runtime.block_on(async move {
                // ft-xbnl0.2.3 tick 303: cx-first MCP reserve storage open.
                let reserve_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let storage = StorageHandle::new_with_cx(&reserve_cx, &db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;

                let mut engine = build_policy_engine_with_shared_rate_limiter(
                    &config,
                    config.safety.require_prompt_active,
                    Arc::clone(&policy_rate_limiter),
                );
                let summary = format!("reserve pane {}", params.pane_id);
                let input = mcp_reserve_pane_policy_input(params.pane_id, &summary);

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Reservation denied by policy")
                        .to_string();
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.reserve",
                        &summary,
                        &reason,
                        decision.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
                    )
                    .await;
                    return Err(McpToolError::new(
                        MCP_ERR_POLICY,
                        reason,
                        Some(POLICY_DENY_HINT.to_string()),
                    ));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store =
                        ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                    let updated = store
                        .attach_to_decision(decision, &input, None)
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Reservation requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.reserve",
                        &summary,
                        &reason,
                        updated.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
                    )
                    .await;
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                let reservation = storage
                    .create_reservation(
                        params.pane_id,
                        &params.owner_kind,
                        &params.owner_id,
                        params.reason.as_deref(),
                        params.ttl_ms,
                    )
                    .await
                    .map_err(McpToolError::from_error)?;

                Ok(McpReserveData {
                    reservation: reservation_to_mcp_info(&reservation),
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaReleaseTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaReleaseTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaReleaseTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.release".to_string(),
            description: Some("Release a pane reservation by ID (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reservation_id": { "type": "integer", "description": "Reservation ID to release" }
                },
                "required": ["reservation_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "reservations".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: ReleaseParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with reservation_id (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpReleaseData, McpToolError> =
            runtime.block_on(async move {
                // ft-xbnl0.2.3 tick 303: cx-first MCP release storage open.
                let release_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let storage = StorageHandle::new_with_cx(&release_cx, &db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;

                let active = storage
                    .list_active_reservations()
                    .await
                    .map_err(McpToolError::from_error)?;
                let pane_id = active
                    .iter()
                    .find(|r| r.id == params.reservation_id)
                    .map(|r| r.pane_id);

                let mut engine = build_policy_engine_with_shared_rate_limiter(
                    &config,
                    config.safety.require_prompt_active,
                    Arc::clone(&policy_rate_limiter),
                );
                let summary = format!("release reservation {}", params.reservation_id);
                let input = mcp_release_pane_policy_input(&summary, pane_id);

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Release denied by policy")
                        .to_string();
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.release",
                        &summary,
                        &reason,
                        decision.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
                    )
                    .await;
                    return Err(McpToolError::new(
                        MCP_ERR_POLICY,
                        reason,
                        Some(POLICY_DENY_HINT.to_string()),
                    ));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store =
                        ApprovalStore::new(&storage, config.safety.approval.clone(), workspace_id);
                    let updated = store
                        .attach_to_decision(decision, &input, None)
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Release requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.release",
                        &summary,
                        &reason,
                        updated.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
                    )
                    .await;
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                let released = storage
                    .release_reservation(params.reservation_id)
                    .await
                    .map_err(McpToolError::from_error)?;
                Ok(McpReleaseData {
                    reservation_id: params.reservation_id,
                    released,
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaAccountsTool {
    db_path: Arc<PathBuf>,
}

impl WaAccountsTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ToolHandler for WaAccountsTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.accounts".to_string(),
            description: Some(
                "List accounts for a service with usage info (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "Service name (openai, anthropic, google)" }
                },
                "required": ["service"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "accounts".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: AccountsParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with service (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            // ft-xbnl0.2.3 tick 258: cx-first account lookup.
            let accounts_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            storage
                .get_accounts_by_service_with_cx(&accounts_cx, &params.service)
                .await
        });

        match result {
            Ok(accounts) => {
                let total = accounts.len();
                let items: Vec<McpAccountInfo> = accounts
                    .into_iter()
                    .map(|a| McpAccountInfo {
                        account_id: a.account_id,
                        service: a.service,
                        name: a.name,
                        percent_remaining: a.percent_remaining,
                        reset_at: a.reset_at,
                        tokens_used: a.tokens_used,
                        tokens_remaining: a.tokens_remaining,
                        tokens_limit: a.tokens_limit,
                        last_refreshed_at: a.last_refreshed_at,
                        last_used_at: a.last_used_at,
                    })
                    .collect();

                let data = McpAccountsData {
                    accounts: items,
                    total,
                    service: params.service,
                };
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

pub(super) struct WaAccountsRefreshTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaAccountsRefreshTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

fn accounts_refresh_policy_input(summary: &str) -> PolicyInput {
    PolicyInput::new(ActionKind::ExecCommand, ActorKind::Mcp)
        .with_surface(PolicySurface::Mcp)
        .with_text_summary(summary.to_string())
        .with_command_text(summary.to_string())
}

impl ToolHandler for WaAccountsRefreshTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.accounts_refresh".to_string(),
            description: Some("Refresh account usage via caut (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "Service name (openai)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "accounts".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: AccountsRefreshParams = if arguments.is_null() {
            AccountsRefreshParams { service: None }
        } else {
            match serde_json::from_value(arguments) {
                Ok(p) => p,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional service".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: std::result::Result<McpAccountsRefreshData, McpToolError> =
            runtime.block_on(async move {
                let service = params.service.unwrap_or_else(|| "openai".to_string());
                let caut_service = parse_caut_service(&service).ok_or_else(|| {
                    McpToolError::new(
                        MCP_ERR_INVALID_ARGS,
                        format!("Unknown service: {service}"),
                        Some(format!(
                            "Supported services: {}",
                            crate::caut::CautService::supported_cli_inputs().join(", ")
                        )),
                    )
                })?;

                let storage = StorageHandle::new(&db_path.to_string_lossy())
                    .await
                    .map_err(McpToolError::from_error)?;

                let mut engine = build_policy_engine_with_shared_rate_limiter(
                    &config,
                    false,
                    Arc::clone(&policy_rate_limiter),
                );
                let summary = format!("caut refresh {service}");
                let input = accounts_refresh_policy_input(&summary);
                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Refresh denied by policy")
                        .to_string();
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.accounts_refresh",
                        &summary,
                        &reason,
                        decision.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
                    )
                    .await;
                    return Err(McpToolError::new(
                        MCP_ERR_POLICY,
                        reason,
                        Some(POLICY_DENY_HINT.to_string()),
                    ));
                }
                if decision.requires_approval() {
                    let workspace_id =
                        resolve_workspace_id(&config).map_err(McpToolError::from_error)?;
                    let store = ApprovalStore::new(
                        &storage,
                        config.safety.approval.clone(),
                        workspace_id,
                    );
                    let updated = store
                        .attach_to_decision(decision, &input, Some(summary.clone()))
                        .await
                        .map_err(McpToolError::from_error)?;
                    let reason = policy_reason(&updated)
                        .unwrap_or("Refresh requires approval")
                        .to_string();
                    let hint = approval_command(&updated);
                    // ft-mw1zb: persist to policy_denied_audit alongside tracing.
                    persist_mcp_policy_denial_async(
                        &storage,
                        "wa.accounts_refresh",
                        &summary,
                        &reason,
                        updated.rule_id(),
                        crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
                        crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
                    )
                    .await;
                    return Err(McpToolError::new(MCP_ERR_POLICY, reason, hint));
                }

                // ft-xbnl0.2.3 tick 258: cx-first account lookup.
                let refresh_cx = crate::cx::Cx::current()
                    .unwrap_or_else(crate::cx::for_request);
                if let Ok(accounts) = storage
                    .get_accounts_by_service_with_cx(&refresh_cx, &service)
                    .await
                {
                    let now_check = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .and_then(|d| i64::try_from(d.as_millis()).ok())
                        .unwrap_or(0);
                    let most_recent = accounts.iter().map(|a| a.last_refreshed_at).max().unwrap_or(0);
                    if let Some((secs_ago, wait_secs)) =
                        check_refresh_cooldown(most_recent, now_check, MCP_REFRESH_COOLDOWN_MS)
                    {
                        return Err(McpToolError::new(
                            MCP_ERR_POLICY,
                            format!(
                                "Refresh rate limited: last refresh was {secs_ago}s ago (cooldown: {}s)",
                                MCP_REFRESH_COOLDOWN_MS / 1000
                            ),
                            Some(format!(
                                "Wait {wait_secs}s before refreshing again, or use wa.accounts to view cached data."
                            )),
                        ));
                    }
                }

                let caut = CautClient::new();
                let refresh_result = caut
                    .refresh(caut_service)
                    .await
                    .map_err(McpToolError::from_caut_error)?;

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|d| i64::try_from(d.as_millis()).ok())
                    .unwrap_or(0);

                let mut account_infos = Vec::new();
                for usage in &refresh_result.accounts {
                    let record = AccountRecord::from_caut(usage, caut_service, now_ms);
                    if let Err(e) = storage.upsert_account(record.clone()).await {
                        tracing::warn!("Failed to upsert account {}: {e}", record.account_id);
                    }
                    account_infos.push(McpAccountInfo {
                        account_id: record.account_id,
                        service: record.service,
                        name: record.name,
                        percent_remaining: record.percent_remaining,
                        reset_at: record.reset_at,
                        tokens_used: record.tokens_used,
                        tokens_remaining: record.tokens_remaining,
                        tokens_limit: record.tokens_limit,
                        last_refreshed_at: record.last_refreshed_at,
                        last_used_at: record.last_used_at,
                    });
                }

                Ok(McpAccountsRefreshData {
                    service,
                    refreshed_count: account_infos.len(),
                    refreshed_at: refresh_result.refreshed_at,
                    accounts: account_infos,
                })
            });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// ── Mission MCP tools (ft-1i2ge.5.3) ────────────────────────────────────

// wa.mission_state tool
pub(super) struct WaMissionStateTool {
    config: Arc<Config>,
}

impl WaMissionStateTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaMissionStateTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_state".to_string(),
            description: Some(
                "Query mission lifecycle state, assignments, and counters with optional filtering (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "mission_state": { "type": "string", "description": "Filter by lifecycle state (e.g., running, paused, completed)" },
                    "run_state": { "type": "string", "description": "Filter assignments by run state (pending, succeeded, failed, cancelled)" },
                    "agent_state": { "type": "string", "description": "Filter by agent approval state (not_required, pending, approved, denied, expired)" },
                    "action_state": { "type": "string", "description": "Filter by action state (ready, blocked, completed)" },
                    "assignment_id": { "type": "string", "description": "Filter to specific assignment ID" },
                    "assignee": { "type": "string", "description": "Filter by assignee name" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Max assignments to return (default: 100)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "mission".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionStateParams = if arguments.is_null() {
            MissionStateParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional mission_file, filters".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        // Check mission_state filter
        if let Some(ref filter_state) = params.mission_state {
            let current = mission.lifecycle_state.to_string();
            if !current.eq_ignore_ascii_case(filter_state) {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!(
                        "mission_state filter '{}' did not match active mission lifecycle_state '{}'",
                        filter_state, current
                    ),
                    Some(
                        "Use wa.mission_state without mission_state to inspect the active mission, or request the current lifecycle state."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        }

        let (assignments, counters, matched_count) =
            mcp_build_mission_assignments(&mission, &params);
        let returned_count = assignments.len();

        let data = McpMissionStateData {
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            title: mission.title.clone(),
            mission_hash: mission.compute_hash(),
            lifecycle_state: mission.lifecycle_state.to_string(),
            candidate_count: mission.candidates.len(),
            assignment_count: mission.assignments.len(),
            matched_assignment_count: matched_count,
            returned_assignment_count: returned_count,
            assignment_counters: counters,
            available_transitions: mcp_mission_lifecycle_transitions(mission.lifecycle_state),
            assignments,
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_explain tool
pub(super) struct WaMissionExplainTool {
    config: Arc<Config>,
}

impl WaMissionExplainTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaMissionExplainTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_explain".to_string(),
            description: Some(
                "Show legal lifecycle transitions, failure catalog, and optional assignment context (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "assignment_id": { "type": "string", "description": "Optional assignment ID for dispatch context details" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "mission".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionExplainParams = if arguments.is_null() {
            MissionExplainParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional mission_file".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        // Build assignment context if requested
        let assignment_context = if let Some(ref aid) = params.assignment_id {
            let found = mission
                .assignments
                .iter()
                .find(|a| a.assignment_id.0 == *aid);
            found.map(|a| {
                serde_json::json!({
                    "assignment_id": a.assignment_id.0,
                    "candidate_id": a.candidate_id.0,
                    "assignee": a.assignee,
                    "approval_state": a.approval_state.canonical_string(),
                    "outcome": a.outcome.as_ref().map(|o| match o {
                        crate::plan::Outcome::Success { .. } => "success",
                        crate::plan::Outcome::Failed { .. } => "failed",
                        crate::plan::Outcome::Cancelled { .. } => "cancelled",
                    }),
                })
            })
        } else {
            None
        };

        let data = McpMissionExplainData {
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            title: mission.title.clone(),
            lifecycle_state: mission.lifecycle_state.to_string(),
            available_transitions: mcp_mission_lifecycle_transitions(mission.lifecycle_state),
            failure_catalog: mcp_mission_failure_catalog(),
            assignment_context,
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_pause tool
pub(super) struct WaMissionPauseTool {
    config: Arc<Config>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaMissionPauseTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaMissionPauseTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_pause".to_string(),
            description: Some(
                "Pause an active mission, creating a checkpoint (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "reason": { "type": "string", "description": "Reason code for the pause (required)" },
                    "requested_by": { "type": "string", "description": "Who requested the pause (default: mcp-agent)" }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "mission".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionPauseParams = match serde_json::from_value(arguments) {
            Ok(parsed) => parsed,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with reason (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let reason = match &params.reason {
            Some(r) if !r.trim().is_empty() => r.clone(),
            _ => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    "reason is required and must not be empty".to_string(),
                    Some("Provide a reason code for the pause.".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        // ft-x86z2: policy gate before mission load + state transition.
        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.mission_pause",
            "mission.pause",
            start,
        ) {
            return deny;
        }

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mut mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let requested_at_ms = i64::try_from(now_ms()).unwrap_or(0);
        let decision =
            match mission.pause_mission(&params.requested_by, &reason, requested_at_ms, None) {
                Ok(d) => d,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Cannot pause mission: {err}"),
                        Some("Use wa.mission_explain to see valid transitions.".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            };

        if let Err(err) = mcp_save_mission_to_path(&mission_path, &mission) {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpMissionControlData {
            command: "pause".to_string(),
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            lifecycle_from: decision.lifecycle_from.to_string(),
            lifecycle_to: decision.lifecycle_to.to_string(),
            decision_path: decision.decision_path,
            reason_code: decision.reason_code,
            error_code: decision.error_code,
            checkpoint_id: decision.checkpoint_id,
            mission_hash: mission.compute_hash(),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_resume tool
pub(super) struct WaMissionResumeTool {
    config: Arc<Config>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaMissionResumeTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaMissionResumeTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_resume".to_string(),
            description: Some(
                "Resume a paused mission, restoring prior lifecycle state (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "requested_by": { "type": "string", "description": "Who requested the resume (default: mcp-agent)" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "mission".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionResumeParams = if arguments.is_null() {
            MissionResumeParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some("Expected object with optional mission_file".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        // ft-x86z2: policy gate before mission load + state transition.
        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.mission_resume",
            "mission.resume",
            start,
        ) {
            return deny;
        }

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mut mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let requested_at_ms = i64::try_from(now_ms()).unwrap_or(0);
        let decision =
            match mission.resume_mission(&params.requested_by, "mcp_resume", requested_at_ms, None)
            {
                Ok(d) => d,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Cannot resume mission: {err}"),
                        Some("Use wa.mission_explain to see valid transitions.".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            };

        if let Err(err) = mcp_save_mission_to_path(&mission_path, &mission) {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpMissionControlData {
            command: "resume".to_string(),
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            lifecycle_from: decision.lifecycle_from.to_string(),
            lifecycle_to: decision.lifecycle_to.to_string(),
            decision_path: decision.decision_path,
            reason_code: decision.reason_code,
            error_code: decision.error_code,
            checkpoint_id: decision.checkpoint_id,
            mission_hash: mission.compute_hash(),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.mission_abort tool
pub(super) struct WaMissionAbortTool {
    config: Arc<Config>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaMissionAbortTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaMissionAbortTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_abort".to_string(),
            description: Some(
                "Abort a mission, cancelling all in-flight assignments (robot parity)".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mission_file": { "type": "string", "description": "Optional path to mission JSON (default: .ft/mission/active.json)" },
                    "reason": { "type": "string", "description": "Reason code for the abort (required)" },
                    "requested_by": { "type": "string", "description": "Who requested the abort (default: mcp-agent)" },
                    "error_code": { "type": "string", "description": "Optional error code for the abort" }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "mission".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: MissionAbortParams = match serde_json::from_value(arguments) {
            Ok(parsed) => parsed,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected object with reason (required)".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let reason = match &params.reason {
            Some(r) if !r.trim().is_empty() => r.clone(),
            _ => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    "reason is required and must not be empty".to_string(),
                    Some("Provide a reason code for the abort.".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        // ft-x86z2: policy gate before mission load + abort decision.
        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.mission_abort",
            "mission.abort",
            start,
        ) {
            return deny;
        }

        let mission_path = match mcp_resolve_mission_file_path(
            self.config.as_ref(),
            params.mission_file.as_deref(),
        ) {
            Ok(path) => path,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let mut mission = match mcp_load_mission_from_path(&mission_path) {
            Ok(m) => m,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let requested_at_ms = i64::try_from(now_ms()).unwrap_or(0);
        let decision = match mission.abort_mission(
            &params.requested_by,
            &reason,
            params.error_code.clone(),
            requested_at_ms,
            None,
        ) {
            Ok(d) => d,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Cannot abort mission: {err}"),
                    Some("Use wa.mission_explain to see valid transitions.".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if let Err(err) = mcp_save_mission_to_path(&mission_path, &mission) {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }

        let data = McpMissionControlData {
            command: "abort".to_string(),
            mission_file: mission_path.display().to_string(),
            mission_id: mission.mission_id.0.clone(),
            lifecycle_from: decision.lifecycle_from.to_string(),
            lifecycle_to: decision.lifecycle_to.to_string(),
            decision_path: decision.decision_path,
            reason_code: decision.reason_code,
            error_code: decision.error_code,
            checkpoint_id: decision.checkpoint_id,
            mission_hash: mission.compute_hash(),
        };

        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_to_content(envelope)
    }
}

// wa.events_annotate tool (bd-2gce) — extracted from mcp.rs [ft-1fv0u]
pub(super) struct WaEventsAnnotateTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaEventsAnnotateTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaEventsAnnotateTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events_annotate".to_string(),
            description: Some("Set or clear an event note (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event_id": { "type": "integer", "minimum": 1, "description": "Event ID" },
                    "note": { "type": "string", "description": "Note text to set" },
                    "clear": { "type": "boolean", "default": false, "description": "Clear the note" },
                    "by": { "type": "string", "description": "Actor identifier (optional)" }
                },
                "required": ["event_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsAnnotateParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected { event_id, note? | clear=true, by? }".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.clear == params.note.is_some() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Invalid params: specify exactly one of note or clear".to_string(),
                Some("Example: {\"event_id\":123,\"note\":\"Investigating\"}".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.events_annotate",
            "event.annotate",
            start,
        ) {
            return deny;
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            let prior_annotations = storage
                .get_event_annotations(params.event_id)
                .await?
                .ok_or_else(|| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Event {} not found",
                        params.event_id
                    )))
                })?;
            let prior_note = prior_annotations.note.clone();

            storage
                .set_event_note(params.event_id, params.note.clone(), params.by.clone())
                .await?;

            let ts = i64::try_from(now_ms()).unwrap_or(0);
            let input_summary = if params.clear {
                format!("wa.events_annotate event_id={} clear=true", params.event_id)
            } else {
                format!(
                    "wa.events_annotate event_id={} note=<redacted>",
                    params.event_id
                )
            };
            let decision_context = mcp_event_mutation_decision_context(
                "wa.events_annotate",
                "event.annotate",
                params.event_id,
                if params.clear {
                    "clear_note"
                } else {
                    "set_note"
                },
                params.by.as_deref(),
                &input_summary,
                ts,
            );
            let audit = crate::storage::AuditActionRecord {
                id: 0,
                ts,
                actor_kind: "mcp".to_string(),
                actor_id: params.by.clone(),
                correlation_id: None,
                pane_id: None,
                domain: None,
                action_kind: "event.annotate".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: Some("MCP updated event note".to_string()),
                rule_id: None,
                input_summary: Some(input_summary),
                verification_summary: None,
                decision_context: serialize_mcp_audit_decision_context(&decision_context),
                result: "success".to_string(),
            };
            // ft-xbnl0.2.3 tick 258: cx-first MCP audit write.
            let audit_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let _ = storage
                .record_audit_action_redacted_with_cx(&audit_cx, audit)
                .await;

            let annotations = storage
                .get_event_annotations(params.event_id)
                .await?
                .ok_or_else(|| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Event {} not found",
                        params.event_id
                    )))
                })?;
            // ft-xo3u4: derive `changed` from THIS call's requested note vs the prior
            // observation, NOT from the post-write re-read. Under concurrent
            // wa.events_annotate calls on the same event_id (SQLite has no surrounding
            // transaction across get/set/get here), the second read can already reflect
            // another writer's overwrite — in which case `prior != annotations.note`
            // would claim our write "changed" the record to a value we never sent.
            // Comparing against `params.note` keeps `changed` honest about the caller's
            // own intent; `annotations.note` in the envelope still reports the latest
            // on-disk state, so a client that finds `annotations.note != their note`
            // can detect the last-write-wins race explicitly.
            let changed = prior_note != params.note;
            Ok(McpEventMutationData {
                event_id: params.event_id,
                changed: Some(changed),
                annotations,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.events_triage tool — extracted from mcp.rs [ft-1fv0u]
pub(super) struct WaEventsTriageTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaEventsTriageTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaEventsTriageTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events_triage".to_string(),
            description: Some("Set or clear an event triage state (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event_id": { "type": "integer", "minimum": 1, "description": "Event ID" },
                    "state": { "type": "string", "description": "Triage state to set" },
                    "clear": { "type": "boolean", "default": false, "description": "Clear the triage state" },
                    "by": { "type": "string", "description": "Actor identifier (optional)" }
                },
                "required": ["event_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsTriageParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected { event_id, state? | clear=true, by? }".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if params.clear == params.state.is_some() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Invalid params: specify exactly one of state or clear".to_string(),
                Some("Example: {\"event_id\":123,\"state\":\"investigating\"}".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.events_triage",
            "event.triage",
            start,
        ) {
            return deny;
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;

            let changed = storage
                .set_event_triage_state(params.event_id, params.state.clone(), params.by.clone())
                .await?;

            let ts = i64::try_from(now_ms()).unwrap_or(0);
            let input_summary = if params.clear {
                format!("wa.events_triage event_id={} clear=true", params.event_id)
            } else {
                format!(
                    "wa.events_triage event_id={} state={}",
                    params.event_id,
                    params.state.clone().unwrap_or_default()
                )
            };
            let mut decision_context = mcp_event_mutation_decision_context(
                "wa.events_triage",
                "event.triage",
                params.event_id,
                if params.clear {
                    "clear_triage_state"
                } else {
                    "set_triage_state"
                },
                params.by.as_deref(),
                &input_summary,
                ts,
            );
            if let Some(state) = params.state.as_ref() {
                decision_context.add_evidence("state", state);
            }
            decision_context.add_evidence("changed", changed.to_string());
            let audit = crate::storage::AuditActionRecord {
                id: 0,
                ts,
                actor_kind: "mcp".to_string(),
                actor_id: params.by.clone(),
                correlation_id: None,
                pane_id: None,
                domain: None,
                action_kind: "event.triage".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: Some("MCP updated event triage".to_string()),
                rule_id: None,
                input_summary: Some(input_summary),
                verification_summary: None,
                decision_context: serialize_mcp_audit_decision_context(&decision_context),
                result: if changed {
                    "success".to_string()
                } else {
                    "noop".to_string()
                },
            };
            // ft-xbnl0.2.3 tick 258: cx-first MCP audit write.
            let audit_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let _ = storage
                .record_audit_action_redacted_with_cx(&audit_cx, audit)
                .await;

            let annotations = storage
                .get_event_annotations(params.event_id)
                .await?
                .ok_or_else(|| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Event {} not found",
                        params.event_id
                    )))
                })?;
            Ok(McpEventMutationData {
                event_id: params.event_id,
                changed: Some(changed),
                annotations,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

// wa.events_label tool — extracted from mcp.rs [ft-1fv0u]
pub(super) struct WaEventsLabelTool {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaEventsLabelTool {
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        Self {
            config,
            db_path,
            policy_rate_limiter,
        }
    }
}

impl ToolHandler for WaEventsLabelTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.events_label".to_string(),
            description: Some("Add/remove/list event labels (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event_id": { "type": "integer", "minimum": 1, "description": "Event ID" },
                    "add": { "type": "string", "description": "Label to add" },
                    "remove": { "type": "string", "description": "Label to remove" },
                    "list": { "type": "boolean", "default": false, "description": "List labels only" },
                    "by": { "type": "string", "description": "Actor identifier (optional; applies to add)" }
                },
                "required": ["event_id"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "events".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsLabelParams = match serde_json::from_value(arguments) {
            Ok(p) => p,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some("Expected { event_id, add? | remove? | list=true, by? }".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let mut ops = 0;
        if params.add.is_some() {
            ops += 1;
        }
        if params.remove.is_some() {
            ops += 1;
        }
        if params.list {
            ops += 1;
        }
        if ops != 1 {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Invalid params: specify exactly one of add/remove/list".to_string(),
                Some("Example: {\"event_id\":123,\"add\":\"urgent\"}".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        if let Some(deny) = mcp_authorize_mcp_mutation(
            self.config.as_ref(),
            &self.policy_rate_limiter,
            "wa.events_label",
            "event.label",
            start,
        ) {
            return deny;
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Tokio runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            let ts = i64::try_from(now_ms()).unwrap_or(0);

            let changed = if let Some(label) = params.add.clone() {
                let inserted = storage
                    .add_event_label(params.event_id, label.clone(), params.by.clone())
                    .await?;
                let input_summary =
                    format!("wa.events_label event_id={} add={label}", params.event_id);

                let mut decision_context = mcp_event_mutation_decision_context(
                    "wa.events_label",
                    "event.label.add",
                    params.event_id,
                    "add_label",
                    params.by.as_deref(),
                    &input_summary,
                    ts,
                );
                decision_context.add_evidence("label", &label);
                decision_context.add_evidence("changed", inserted.to_string());
                let audit = crate::storage::AuditActionRecord {
                    id: 0,
                    ts,
                    actor_kind: "mcp".to_string(),
                    actor_id: params.by.clone(),
                    correlation_id: None,
                    pane_id: None,
                    domain: None,
                    action_kind: "event.label.add".to_string(),
                    policy_decision: "allow".to_string(),
                    decision_reason: Some("MCP added event label".to_string()),
                    rule_id: None,
                    input_summary: Some(input_summary),
                    verification_summary: None,
                    decision_context: serialize_mcp_audit_decision_context(&decision_context),
                    result: if inserted {
                        "success".to_string()
                    } else {
                        "noop".to_string()
                    },
                };
                // ft-xbnl0.2.3 tick 258: cx-first MCP audit write.
                let audit_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let _ = storage
                    .record_audit_action_redacted_with_cx(&audit_cx, audit)
                    .await;

                Some(inserted)
            } else if let Some(label) = params.remove.clone() {
                let removed = storage
                    .remove_event_label(params.event_id, label.clone())
                    .await?;
                let input_summary = format!(
                    "wa.events_label event_id={} remove={label}",
                    params.event_id
                );

                let mut decision_context = mcp_event_mutation_decision_context(
                    "wa.events_label",
                    "event.label.remove",
                    params.event_id,
                    "remove_label",
                    params.by.as_deref(),
                    &input_summary,
                    ts,
                );
                decision_context.add_evidence("label", &label);
                decision_context.add_evidence("changed", removed.to_string());
                let audit = crate::storage::AuditActionRecord {
                    id: 0,
                    ts,
                    actor_kind: "mcp".to_string(),
                    actor_id: params.by.clone(),
                    correlation_id: None,
                    pane_id: None,
                    domain: None,
                    action_kind: "event.label.remove".to_string(),
                    policy_decision: "allow".to_string(),
                    decision_reason: Some("MCP removed event label".to_string()),
                    rule_id: None,
                    input_summary: Some(input_summary),
                    verification_summary: None,
                    decision_context: serialize_mcp_audit_decision_context(&decision_context),
                    result: if removed {
                        "success".to_string()
                    } else {
                        "noop".to_string()
                    },
                };
                // ft-xbnl0.2.3 tick 258: cx-first MCP audit write.
                let audit_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let _ = storage
                    .record_audit_action_redacted_with_cx(&audit_cx, audit)
                    .await;

                Some(removed)
            } else {
                None
            };

            let annotations = storage
                .get_event_annotations(params.event_id)
                .await?
                .ok_or_else(|| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Event {} not found",
                        params.event_id
                    )))
                })?;
            Ok(McpEventMutationData {
                event_id: params.event_id,
                changed,
                annotations,
            })
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Err(err) => {
                let (code, hint) = map_mcp_error(&err);
                let envelope =
                    McpEnvelope::<()>::error(code, err.to_string(), hint, elapsed_ms(start));
                envelope_to_content(envelope)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Test-only fixture/bootstrap helpers below intentionally use unwrap/expect for
    // tempdir/runtime/serde setup invariants. Production MCP paths return typed
    // envelopes or McpError values instead of panicking on user-controlled input.
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::{Mutex, MutexGuard, OnceLock};

    #[cfg(unix)]
    use super::set_cass_test_binary_override;
    use super::{
        ActionKind, ActorKind, CompatRuntime, CompatRuntimeBuilder, Config, Content,
        MAX_MCP_WAIT_TIMEOUT_SECS, MAX_SEND_TEXT_BYTES, McpContext, PaneCapabilities,
        PaneFilterConfig, PolicySurface, StorageHandle, Tool, ToolHandler, WaAccountsRefreshTool,
        WaAccountsTool, WaCassSearchTool, WaCassStatusTool, WaCassViewTool, WaEventsAnnotateTool,
        WaEventsLabelTool, WaEventsTool, WaEventsTriageTool, WaGetTextTool, WaMissionAbortTool,
        WaMissionExplainTool, WaMissionPauseTool, WaMissionResumeTool, WaMissionStateTool,
        WaReleaseTool, WaReservationsTool, WaReserveTool, WaRulesListTool, WaRulesTestTool,
        WaSearchTool, WaSendTool, WaStateTool, WaTxPlanTool, WaTxRollbackTool, WaTxRunTool,
        WaTxShowTool, WaWaitForTool, WaWorkflowRunTool, accounts_refresh_policy_input,
        authorize_mcp_policy_call, build_mcp_shared_rate_limiter,
        build_policy_engine_with_shared_rate_limiter, mcp_event_mutation_decision_context,
        mcp_get_text_policy_input, mcp_load_mission_tx_contract_from_path,
        mcp_release_pane_policy_input, mcp_reserve_pane_policy_input,
        mcp_search_output_policy_input, mcp_send_text_policy_input, mcp_workflow_run_policy_input,
        merge_distributed_remote_mcp_states, redact_mcp_pane_state_fields,
        serialize_mcp_audit_decision_context, tx_run_test_wezterm_override_slot,
    };
    use crate::mcp::mcp_types::{McpPaneState, StateParams};
    use crate::mcp::now_ms;
    #[cfg(unix)]
    use crate::mcp_error::{
        MCP_ERR_CASS, MCP_ERR_INVALID_ARGS, MCP_ERR_POLICY, MCP_ERR_REMOTE_TEXT_UNAVAILABLE,
    };
    use crate::plan::{
        ApprovalState, Assignment, AssignmentId, CandidateAction, CandidateActionId,
        MISSION_TX_SCHEMA_VERSION, Mission, MissionActorRole, MissionId, MissionKillSwitchLevel,
        MissionLifecycleState, MissionOwnership, MissionTxContract, MissionTxState, Outcome,
        ReservationIntent, ReservationIntentId, StepAction, TxCommitStepInput, TxCompensation,
        TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxPrecondition, TxStep, TxStepId,
        execute_commit_phase,
    };
    use tempfile::TempDir;

    fn db_path() -> Arc<PathBuf> {
        Arc::new(PathBuf::from("/tmp/test-mcp.db"))
    }

    fn config() -> Arc<Config> {
        Arc::new(Config::default())
    }

    fn config_with_db_path(db_path: &Path) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.storage.db_path = db_path.to_string_lossy().to_string();
        Arc::new(cfg)
    }

    fn temp_db_path() -> (TempDir, Arc<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-tools-test.db");
        (dir, Arc::new(path))
    }

    fn deny_mcp_exec_command_config(command_pattern: &str, message: &str) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.safety.rules.enabled = true;
        cfg.safety.rules.rules.push(crate::config::PolicyRule {
            id: format!("test.deny.mcp.{command_pattern}"),
            description: Some(format!("deny {command_pattern} MCP mutations")),
            priority: 1,
            match_on: crate::config::PolicyRuleMatch {
                actions: vec!["exec_command".to_string()],
                actors: vec!["mcp".to_string()],
                surfaces: vec!["mcp".to_string()],
                command_patterns: vec![format!("^{command_pattern}$")],
                ..Default::default()
            },
            decision: crate::config::PolicyRuleDecision::Deny,
            message: Some(message.to_string()),
        });
        Arc::new(cfg)
    }

    fn deny_mcp_read_output_config(domain: &str, message: &str) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.safety.rules.enabled = true;
        cfg.safety.rules.rules.push(crate::config::PolicyRule {
            id: format!("test.deny.mcp.read_output.{domain}"),
            description: Some(format!("deny MCP read output on {domain}")),
            priority: 1,
            match_on: crate::config::PolicyRuleMatch {
                actions: vec!["read_output".to_string()],
                actors: vec!["mcp".to_string()],
                pane_domains: vec![domain.to_string()],
                ..Default::default()
            },
            decision: crate::config::PolicyRuleDecision::Deny,
            message: Some(message.to_string()),
        });
        Arc::new(cfg)
    }

    fn test_mcp_context() -> McpContext {
        McpContext::new(fastmcp::Cx::for_testing(), 1)
    }

    fn set_tx_run_test_wezterm_override(handle: Option<crate::wezterm::WeztermHandle>) {
        *tx_run_test_wezterm_override_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = handle;
    }

    struct TxRunWeztermOverrideGuard;

    impl Drop for TxRunWeztermOverrideGuard {
        fn drop(&mut self) {
            set_tx_run_test_wezterm_override(None);
        }
    }

    fn install_tx_run_mock_wezterm() -> (TxRunWeztermOverrideGuard, Arc<crate::wezterm::MockWezterm>)
    {
        let mock = Arc::new(crate::wezterm::MockWezterm::new());
        let handle: crate::wezterm::WeztermHandle = mock.clone();
        set_tx_run_test_wezterm_override(Some(handle));
        (TxRunWeztermOverrideGuard, mock)
    }

    fn seed_tx_run_real_targets(db_path: &Path, mock: &Arc<crate::wezterm::MockWezterm>) {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage should open");
            let seen_at = i64::try_from(now_ms()).unwrap_or(0);
            for pane_id in 1..=3u64 {
                storage
                    .upsert_pane(crate::storage::PaneRecord {
                        pane_id,
                        pane_uuid: None,
                        domain: "local".to_string(),
                        window_id: None,
                        tab_id: None,
                        title: Some(format!("pane-{pane_id}")),
                        cwd: Some("/tmp".to_string()),
                        tty_name: None,
                        first_seen_at: seen_at,
                        last_seen_at: seen_at,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    })
                    .await
                    .expect("pane record should seed");
                mock.add_pane(crate::wezterm::MockPane {
                    pane_id,
                    window_id: pane_id,
                    tab_id: pane_id,
                    title: format!("pane-{pane_id}"),
                    domain: "local".to_string(),
                    cwd: "/tmp".to_string(),
                    is_active: pane_id == 1,
                    is_zoomed: false,
                    cols: 120,
                    rows: 40,
                    content: String::new(),
                })
                .await;
            }
        });
    }

    fn seed_event(db_path: &Path) -> i64 {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            // Ensure pane exists to satisfy foreign key constraint.
            storage
                .upsert_pane(crate::storage::PaneRecord {
                    pane_id: 7,
                    pane_uuid: None,
                    domain: "local".to_string(),
                    window_id: None,
                    tab_id: None,
                    title: Some("test-pane".to_string()),
                    cwd: None,
                    tty_name: None,
                    first_seen_at: 1_700_000_000_000,
                    last_seen_at: 1_700_000_000_000,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                })
                .await
                .unwrap();
            storage
                .record_event(crate::storage::StoredEvent {
                    id: 0,
                    pane_id: 7,
                    rule_id: "codex.usage.reached".to_string(),
                    agent_type: "codex".to_string(),
                    event_type: "usage_limit".to_string(),
                    severity: "warning".to_string(),
                    confidence: 0.95,
                    extracted: None,
                    matched_text: Some("Usage limit reached".to_string()),
                    segment_id: None,
                    detected_at: 1_700_000_000_000,
                    dedupe_key: None,
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                })
                .await
                .unwrap()
        })
    }

    fn latest_audit_action(db_path: &Path, action_kind: &str) -> crate::storage::AuditActionRecord {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            let rows = storage
                .get_audit_actions(crate::storage::AuditQuery {
                    limit: Some(1),
                    action_kind: Some(action_kind.to_string()),
                    ..crate::storage::AuditQuery::default()
                })
                .await
                .unwrap();
            rows.into_iter()
                .next()
                .expect("missing audit row for requested action kind")
        })
    }

    fn parse_audit_decision_context(
        db_path: &Path,
        action_kind: &str,
    ) -> crate::policy::DecisionContext {
        let audit = latest_audit_action(db_path, action_kind);
        serde_json::from_str(audit.decision_context.as_deref().unwrap()).unwrap()
    }

    fn evidence<'a>(context: &'a crate::policy::DecisionContext, key: &str) -> Option<&'a str> {
        context
            .evidence
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }

    fn sample_tx_contract(state: MissionTxState) -> MissionTxContract {
        let tx_id = TxId("tx:test".to_string());
        MissionTxContract {
            tx_version: MISSION_TX_SCHEMA_VERSION,
            intent: TxIntent {
                tx_id: tx_id.clone(),
                requested_by: MissionActorRole::Dispatcher,
                summary: "tx test".to_string(),
                correlation_id: "corr:test".to_string(),
                created_at_ms: 1_700_000_000_000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("plan:test".to_string()),
                tx_id,
                steps: vec![
                    TxStep {
                        step_id: TxStepId("tx-step:1".to_string()),
                        ordinal: 1,
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "step-1".to_string(),
                            paste_mode: Some(false),
                        },
                        description: "step 1".to_string(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:2".to_string()),
                        ordinal: 2,
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "step-2".to_string(),
                            paste_mode: Some(false),
                        },
                        description: "step 2".to_string(),
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:3".to_string()),
                        ordinal: 3,
                        action: StepAction::SendText {
                            pane_id: 3,
                            text: "step-3".to_string(),
                            paste_mode: Some(true),
                        },
                        description: "step 3".to_string(),
                    },
                ],
                preconditions: vec![TxPrecondition::PromptActive { pane_id: 1 }],
                compensations: vec![
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:1".to_string()),
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "undo-1".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:2".to_string()),
                        action: StepAction::SendText {
                            pane_id: 2,
                            text: "undo-2".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                    TxCompensation {
                        for_step_id: TxStepId("tx-step:3".to_string()),
                        action: StepAction::SendText {
                            pane_id: 3,
                            text: "undo-3".to_string(),
                            paste_mode: Some(true),
                        },
                    },
                ],
            },
            lifecycle_state: state,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    fn write_tx_contract(dir: &TempDir, state: MissionTxState) -> std::path::PathBuf {
        let path = dir.path().join("tx-contract.json");
        let contract = sample_tx_contract(state);
        std::fs::write(&path, serde_json::to_vec_pretty(&contract).unwrap()).unwrap();
        path
    }

    fn write_tx_contract_with_partial_commit_receipts(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("tx-contract-with-receipts.json");
        let mut contract = sample_tx_contract(MissionTxState::Failed);
        let commit_report = execute_commit_phase(
            &sample_tx_contract(MissionTxState::Prepared),
            &[
                TxCommitStepInput {
                    step_id: TxStepId("tx-step:1".to_string()),
                    success: true,
                    reason_code: "commit_step_succeeded".to_string(),
                    error_code: None,
                    completed_at_ms: 10_001,
                },
                TxCommitStepInput {
                    step_id: TxStepId("tx-step:2".to_string()),
                    success: false,
                    reason_code: "commit_step_failed_injected".to_string(),
                    error_code: Some("FTX3999".to_string()),
                    completed_at_ms: 10_002,
                },
                TxCommitStepInput {
                    step_id: TxStepId("tx-step:3".to_string()),
                    success: true,
                    reason_code: "commit_step_succeeded".to_string(),
                    error_code: None,
                    completed_at_ms: 10_003,
                },
            ],
            MissionKillSwitchLevel::Off,
            false,
            10_500,
        )
        .expect("commit report");
        contract.receipts = commit_report.receipts;
        std::fs::write(&path, serde_json::to_vec_pretty(&contract).unwrap()).unwrap();
        path
    }

    fn sample_mission(state: MissionLifecycleState) -> Mission {
        let mut mission = Mission::new(
            MissionId("mission:test".to_string()),
            "Mission state MCP test",
            "ws-test",
            MissionOwnership {
                planner: "planner-agent".to_string(),
                dispatcher: "dispatcher-agent".to_string(),
                operator: "operator-human".to_string(),
            },
            1_704_000_000_000,
        );
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:a".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 1,
                text: "/retry".to_string(),
                paste_mode: Some(false),
            },
            rationale: "retry after mismatch".to_string(),
            score: Some(0.9),
            created_at_ms: 1_704_000_000_100,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:a".to_string()),
            candidate_id: CandidateActionId("candidate:a".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "executor-agent-1".to_string(),
            reservation_intent: Some(ReservationIntent {
                reservation_id: ReservationIntentId("reservation:a".to_string()),
                requested_by: MissionActorRole::Dispatcher,
                paths: vec!["crates/frankenterm-core/src/mcp_tools.rs".to_string()],
                exclusive: true,
                reason: Some("mission state test".to_string()),
                requested_at_ms: 1_704_000_000_200,
                expires_at_ms: Some(1_704_000_360_200),
            }),
            approval_state: ApprovalState::Approved {
                approved_by: "operator-human".to_string(),
                approved_at_ms: 1_704_000_000_220,
                approval_code_hash: "sha256:abcd".to_string(),
            },
            outcome: Some(Outcome::Success {
                reason_code: "retry_applied".to_string(),
                completed_at_ms: 1_704_000_000_700,
            }),
            escalation: None,
            created_at_ms: 1_704_000_000_210,
            updated_at_ms: Some(1_704_000_000_705),
        });
        mission.lifecycle_state = state;
        mission
    }

    fn write_mission_file(dir: &TempDir, state: MissionLifecycleState) -> std::path::PathBuf {
        let path = dir.path().join("mission.json");
        let mission = sample_mission(state);
        std::fs::write(&path, serde_json::to_vec_pretty(&mission).unwrap()).unwrap();
        path
    }

    fn parse_json_content(contents: Vec<Content>) -> serde_json::Value {
        assert_eq!(contents.len(), 1, "expected single MCP content item");
        assert!(
            matches!(contents.first(), Some(Content::Text { .. })),
            "expected text content"
        );
        let Some(Content::Text { text }) = contents.first() else {
            return serde_json::Value::Null;
        };
        serde_json::from_str(text).expect("valid MCP envelope json")
    }

    #[cfg(unix)]
    fn cass_tool_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[cfg(unix)]
    fn sh_single_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    struct CassToolTestEnv {
        _serial: MutexGuard<'static, ()>,
        _dir: TempDir,
        args_path: PathBuf,
    }

    #[cfg(unix)]
    impl CassToolTestEnv {
        fn install(script_body: &str) -> Self {
            let serial = cass_tool_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("cass tool tempdir");
            let args_path = dir.path().join("cass.args");
            let binary_path = dir.path().join("cass-fake");
            let script = format!(
                "#!/bin/sh\nset -eu\nargs_file={}\n: > \"$args_file\"\nfor arg in \"$@\"; do\n  printf '%s\\n' \"$arg\" >> \"$args_file\"\ndone\n{}\n",
                sh_single_quote(args_path.to_string_lossy().as_ref()),
                script_body,
            );
            std::fs::write(&binary_path, script).expect("write fake cass");
            let mut permissions = std::fs::metadata(&binary_path)
                .expect("fake cass metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&binary_path, permissions).expect("chmod fake cass");
            set_cass_test_binary_override(Some(binary_path.to_string_lossy().into_owned()));
            Self {
                _serial: serial,
                _dir: dir,
                args_path,
            }
        }

        fn args(&self) -> Vec<String> {
            std::fs::read_to_string(&self.args_path)
                .expect("read cass args")
                .lines()
                .map(ToOwned::to_owned)
                .collect()
        }
    }

    #[cfg(unix)]
    impl Drop for CassToolTestEnv {
        fn drop(&mut self) {
            set_cass_test_binary_override(None);
        }
    }

    /// Collect definitions for all 29 tools. Guarantees no panics during construction.
    fn all_definitions() -> Vec<Tool> {
        let db = db_path();
        let cfg = config();
        vec![
            WaRulesListTool.definition(),
            WaRulesTestTool.definition(),
            WaCassSearchTool.definition(),
            WaCassViewTool.definition(),
            WaCassStatusTool.definition(),
            WaStateTool::new(PaneFilterConfig::default(), None).definition(),
            WaGetTextTool::new(Arc::clone(&cfg), Some(Arc::clone(&db))).definition(),
            WaWaitForTool::new(Arc::clone(&cfg), Some(Arc::clone(&db))).definition(),
            WaSearchTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaEventsTool::new(Arc::clone(&db)).definition(),
            WaSendTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaWorkflowRunTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaTxPlanTool::new(Arc::clone(&cfg)).definition(),
            WaTxShowTool::new(Arc::clone(&cfg)).definition(),
            WaTxRunTool::new(Arc::clone(&cfg)).definition(),
            WaTxRollbackTool::new(Arc::clone(&cfg)).definition(),
            WaReservationsTool::new(Arc::clone(&db)).definition(),
            WaReserveTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaReleaseTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaAccountsTool::new(Arc::clone(&db)).definition(),
            WaAccountsRefreshTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaMissionStateTool::new(Arc::clone(&cfg)).definition(),
            WaMissionExplainTool::new(Arc::clone(&cfg)).definition(),
            WaMissionPauseTool::new(Arc::clone(&cfg)).definition(),
            WaMissionResumeTool::new(Arc::clone(&cfg)).definition(),
            WaMissionAbortTool::new(Arc::clone(&cfg)).definition(),
            WaEventsAnnotateTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaEventsTriageTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaEventsLabelTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
        ]
    }

    // ========================================================================
    // Tool Count Invariant
    // ========================================================================

    #[test]
    fn tool_count_is_29() {
        assert_eq!(all_definitions().len(), 29);
    }

    // ========================================================================
    // All Tool Names Are Unique
    // ========================================================================

    #[test]
    fn all_tool_names_are_unique() {
        let defs = all_definitions();
        let mut seen = std::collections::HashSet::new();
        for def in &defs {
            assert!(seen.insert(&def.name), "Duplicate tool name: {}", def.name);
        }
    }

    #[test]
    fn accounts_refresh_policy_input_uses_mcp_surface() {
        let summary = "caut refresh openai";
        let input = accounts_refresh_policy_input(summary);

        assert_eq!(input.action, ActionKind::ExecCommand);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Mcp);
        assert_eq!(input.text_summary.as_deref(), Some(summary));
        assert_eq!(input.command_text.as_deref(), Some(summary));
    }

    #[test]
    fn mcp_tool_policy_input_helpers_use_expected_action_actor_and_surface() {
        let summary = "helper summary";

        let get_text = mcp_get_text_policy_input(7, "local", PaneCapabilities::unknown(), summary);
        assert_eq!(get_text.action, ActionKind::ReadOutput);
        assert_eq!(get_text.actor, ActorKind::Mcp);
        assert_eq!(get_text.surface, PolicySurface::Mux);
        assert_eq!(get_text.pane_id, Some(7));
        assert_eq!(get_text.text_summary.as_deref(), Some(summary));

        let search = mcp_search_output_policy_input(summary);
        assert_eq!(search.action, ActionKind::SearchOutput);
        assert_eq!(search.actor, ActorKind::Mcp);
        assert_eq!(search.surface, PolicySurface::Mux);
        assert_eq!(search.text_summary.as_deref(), Some(summary));

        let send = mcp_send_text_policy_input(
            11,
            "local",
            PaneCapabilities::unknown(),
            summary,
            "echo hi",
        );
        assert_eq!(send.action, ActionKind::SendText);
        assert_eq!(send.actor, ActorKind::Mcp);
        assert_eq!(send.surface, PolicySurface::Mux);
        assert_eq!(send.pane_id, Some(11));
        assert_eq!(send.command_text.as_deref(), Some("echo hi"));

        let workflow =
            mcp_workflow_run_policy_input(13, "local", PaneCapabilities::unknown(), summary);
        assert_eq!(workflow.action, ActionKind::WorkflowRun);
        assert_eq!(workflow.actor, ActorKind::Mcp);
        assert_eq!(workflow.surface, PolicySurface::Workflow);
        assert_eq!(workflow.pane_id, Some(13));

        let reserve = mcp_reserve_pane_policy_input(17, summary);
        assert_eq!(reserve.action, ActionKind::ReservePane);
        assert_eq!(reserve.actor, ActorKind::Mcp);
        assert_eq!(reserve.surface, PolicySurface::Swarm);
        assert_eq!(reserve.pane_id, Some(17));
        assert_eq!(reserve.command_text.as_deref(), Some("reserve_pane"));

        let release_with_pane = mcp_release_pane_policy_input(summary, Some(19));
        assert_eq!(release_with_pane.action, ActionKind::ReleasePane);
        assert_eq!(release_with_pane.actor, ActorKind::Mcp);
        assert_eq!(release_with_pane.surface, PolicySurface::Swarm);
        assert_eq!(release_with_pane.pane_id, Some(19));
        assert_eq!(
            release_with_pane.command_text.as_deref(),
            Some("release_reservation")
        );

        let release_without_pane = mcp_release_pane_policy_input(summary, None);
        assert_eq!(release_without_pane.action, ActionKind::ReleasePane);
        assert_eq!(release_without_pane.actor, ActorKind::Mcp);
        assert_eq!(release_without_pane.surface, PolicySurface::Swarm);
        assert_eq!(release_without_pane.pane_id, None);
    }

    #[test]
    fn events_annotate_audit_records_mcp_decision_context() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsAnnotateTool::new(config(), Arc::clone(&db_path));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "note": "Investigating",
                "by": "mcp-client"
            }),
        )
        .unwrap();

        let audit = latest_audit_action(db_path.as_ref().as_path(), "event.annotate");
        assert_eq!(audit.actor_kind, "mcp");
        let context = parse_audit_decision_context(db_path.as_ref().as_path(), "event.annotate");
        let expected_event_id = event_id.to_string();
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.event.annotate")
        );
        assert_eq!(evidence(&context, "tool"), Some("wa.events_annotate"));
        assert_eq!(
            evidence(&context, "event_action_kind"),
            Some("event.annotate")
        );
        assert_eq!(
            evidence(&context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&context, "operation"), Some("set_note"));
        assert_eq!(evidence(&context, "actor_id"), Some("mcp-client"));
    }

    #[test]
    fn events_annotate_reports_changed_for_real_write_and_noop_rewrite() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsAnnotateTool::new(config(), Arc::clone(&db_path));

        let first = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "event_id": event_id,
                    "note": "Investigating",
                    "by": "mcp-client"
                }),
            )
            .unwrap(),
        );
        assert_eq!(first["ok"], serde_json::json!(true));
        assert_eq!(first["data"]["changed"], serde_json::json!(true));
        assert_eq!(
            first["data"]["annotations"]["note"],
            serde_json::json!("Investigating")
        );

        let second = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "event_id": event_id,
                    "note": "Investigating",
                    "by": "mcp-client"
                }),
            )
            .unwrap(),
        );
        assert_eq!(second["ok"], serde_json::json!(true));
        assert_eq!(second["data"]["changed"], serde_json::json!(false));
        assert_eq!(
            second["data"]["annotations"]["note"],
            serde_json::json!("Investigating")
        );
    }

    #[test]
    fn events_annotate_reports_changed_false_when_clearing_absent_note() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsAnnotateTool::new(config(), Arc::clone(&db_path));

        let response = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "event_id": event_id,
                    "clear": true
                }),
            )
            .unwrap(),
        );
        assert_eq!(response["ok"], serde_json::json!(true));
        assert_eq!(response["data"]["changed"], serde_json::json!(false));
        assert!(response["data"]["annotations"]["note"].is_null());
    }

    #[test]
    fn events_triage_audit_records_operation_state_and_change() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsTriageTool::new(config(), Arc::clone(&db_path));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "state": "investigating",
                "by": "mcp-client"
            }),
        )
        .unwrap();

        let audit = latest_audit_action(db_path.as_ref().as_path(), "event.triage");
        assert_eq!(audit.actor_kind, "mcp");
        let context = parse_audit_decision_context(db_path.as_ref().as_path(), "event.triage");
        let expected_event_id = event_id.to_string();
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("audit.event.triage")
        );
        assert_eq!(evidence(&context, "tool"), Some("wa.events_triage"));
        assert_eq!(
            evidence(&context, "event_action_kind"),
            Some("event.triage")
        );
        assert_eq!(
            evidence(&context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&context, "operation"), Some("set_triage_state"));
        assert_eq!(evidence(&context, "state"), Some("investigating"));
        assert_eq!(evidence(&context, "changed"), Some("true"));
    }

    #[test]
    fn events_label_audit_records_add_and_remove_context() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsLabelTool::new(config(), Arc::clone(&db_path));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "add": "urgent",
                "by": "mcp-client"
            }),
        )
        .unwrap();
        let add_audit = latest_audit_action(db_path.as_ref().as_path(), "event.label.add");
        assert_eq!(add_audit.actor_kind, "mcp");
        let add_context =
            parse_audit_decision_context(db_path.as_ref().as_path(), "event.label.add");
        let expected_event_id = event_id.to_string();
        assert_eq!(add_context.action, ActionKind::ExecCommand);
        assert_eq!(add_context.actor, ActorKind::Mcp);
        assert_eq!(add_context.surface, PolicySurface::Mcp);
        assert_eq!(
            add_context.determining_rule.as_deref(),
            Some("audit.event.label.add")
        );
        assert_eq!(evidence(&add_context, "tool"), Some("wa.events_label"));
        assert_eq!(
            evidence(&add_context, "event_action_kind"),
            Some("event.label.add")
        );
        assert_eq!(
            evidence(&add_context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&add_context, "operation"), Some("add_label"));
        assert_eq!(evidence(&add_context, "label"), Some("urgent"));
        assert_eq!(evidence(&add_context, "changed"), Some("true"));
        assert_eq!(evidence(&add_context, "actor_id"), Some("mcp-client"));

        tool.call(
            &test_mcp_context(),
            serde_json::json!({
                "event_id": event_id,
                "remove": "urgent"
            }),
        )
        .unwrap();
        let remove_audit = latest_audit_action(db_path.as_ref().as_path(), "event.label.remove");
        assert_eq!(remove_audit.actor_kind, "mcp");
        let remove_context =
            parse_audit_decision_context(db_path.as_ref().as_path(), "event.label.remove");
        let expected_event_id = event_id.to_string();
        assert_eq!(remove_context.action, ActionKind::ExecCommand);
        assert_eq!(remove_context.actor, ActorKind::Mcp);
        assert_eq!(remove_context.surface, PolicySurface::Mcp);
        assert_eq!(
            remove_context.determining_rule.as_deref(),
            Some("audit.event.label.remove")
        );
        assert_eq!(evidence(&remove_context, "tool"), Some("wa.events_label"));
        assert_eq!(
            evidence(&remove_context, "event_action_kind"),
            Some("event.label.remove")
        );
        assert_eq!(
            evidence(&remove_context, "event_id"),
            Some(expected_event_id.as_str())
        );
        assert_eq!(evidence(&remove_context, "operation"), Some("remove_label"));
        assert_eq!(evidence(&remove_context, "label"), Some("urgent"));
        assert_eq!(evidence(&remove_context, "changed"), Some("true"));
        assert!(evidence(&remove_context, "actor_id").is_none());
    }

    #[test]
    fn events_annotate_tool_applies_mcp_mutation_policy_gate() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsAnnotateTool::new(
            deny_mcp_exec_command_config("event\\.annotate", "event note mutations are blocked"),
            Arc::clone(&db_path),
        );

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "event_id": event_id,
                    "note": "Investigating",
                    "by": "mcp-client"
                }),
            )
            .expect("wa.events_annotate policy call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
        assert_eq!(envelope["error"], "event note mutations are blocked");
    }

    #[test]
    fn events_triage_tool_applies_mcp_mutation_policy_gate() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsTriageTool::new(
            deny_mcp_exec_command_config("event\\.triage", "event triage mutations are blocked"),
            Arc::clone(&db_path),
        );

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "event_id": event_id,
                    "state": "investigating",
                    "by": "mcp-client"
                }),
            )
            .expect("wa.events_triage policy call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
        assert_eq!(envelope["error"], "event triage mutations are blocked");
    }

    #[test]
    fn events_label_tool_applies_mcp_mutation_policy_gate() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaEventsLabelTool::new(
            deny_mcp_exec_command_config("event\\.label", "event label mutations are blocked"),
            Arc::clone(&db_path),
        );

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "event_id": event_id,
                    "add": "urgent",
                    "by": "mcp-client"
                }),
            )
            .expect("wa.events_label policy call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
        assert_eq!(envelope["error"], "event label mutations are blocked");
    }

    #[test]
    fn mcp_mutation_rate_limit_is_shared_across_requests() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let mut cfg = Config::default();
        cfg.safety.require_prompt_active = false;
        cfg.safety.rate_limit_global = 100;
        cfg.safety.rate_limit_per_pane = 100;
        let cfg = Arc::new(cfg);
        let shared_rate_limiter = build_mcp_shared_rate_limiter(cfg.as_ref());
        let tool = WaEventsAnnotateTool::new_with_shared_rate_limiter(
            Arc::clone(&cfg),
            Arc::clone(&db_path),
            shared_rate_limiter,
        );

        for attempt in 0..100 {
            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "event_id": event_id,
                        "note": format!("note-{attempt}"),
                        "by": "mcp-client"
                    }),
                )
                .expect("wa.events_annotate allowed call"),
            );

            assert_eq!(envelope["ok"], true, "attempt {attempt} should be allowed");
        }

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "event_id": event_id,
                    "note": "note-100",
                    "by": "mcp-client"
                }),
            )
            .expect("wa.events_annotate rate-limited call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains("rate limit")),
            "expected rate-limit policy error, got {envelope:?}"
        );
    }

    #[test]
    fn wa_send_rate_limit_is_shared_across_tool_instances() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db) = temp_db_path();
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 2;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);
            let shared_rate_limiter = build_mcp_shared_rate_limiter(cfg.as_ref());

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(42).await;
            let handle = mock as crate::wezterm::WeztermHandle;
            let tool_a = WaSendTool::with_wezterm_handle_and_shared_rate_limiter(
                Arc::clone(&cfg),
                Arc::clone(&db),
                Arc::clone(&handle),
                Arc::clone(&shared_rate_limiter),
            );
            let tool_b = WaSendTool::with_wezterm_handle_and_shared_rate_limiter(
                Arc::clone(&cfg),
                Arc::clone(&db),
                handle,
                shared_rate_limiter,
            );

            for attempt in 0..2 {
                let envelope = parse_json_content(
                    tool_a
                        .call(
                            &test_mcp_context(),
                            serde_json::json!({
                                "pane_id": 42,
                                "text": format!("echo attempt-{attempt}")
                            }),
                        )
                        .expect("wa.send allowed call"),
                );

                assert_eq!(envelope["ok"], true, "attempt {attempt} should be allowed");
                assert_eq!(envelope["data"]["injection"]["status"], "allowed");
            }

            let envelope = parse_json_content(
                tool_b
                    .call(
                        &test_mcp_context(),
                        serde_json::json!({
                            "pane_id": 42,
                            "text": "echo over-limit"
                        }),
                    )
                    .expect("wa.send rate-limited call"),
            );

            assert_eq!(envelope["ok"], true);
            assert_eq!(envelope["data"]["injection"]["status"], "requires_approval");
            assert!(
                envelope["data"]["injection"]["decision"]["reason"]
                    .as_str()
                    .is_some_and(|error| error.to_ascii_lowercase().contains("rate limit")),
                "expected rate-limit policy decision, got {envelope:?}"
            );
        });
    }

    #[test]
    fn wa_send_dry_run_does_not_consume_rate_limit_budget() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db) = temp_db_path();
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 1;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(42).await;
            let tool = WaSendTool::with_wezterm_handle(
                Arc::clone(&cfg),
                Arc::clone(&db),
                mock as crate::wezterm::WeztermHandle,
            );

            for attempt in 0..3 {
                let envelope = parse_json_content(
                    tool.call(
                        &test_mcp_context(),
                        serde_json::json!({
                            "pane_id": 42,
                            "text": format!("echo preview-{attempt}"),
                            "dry_run": true
                        }),
                    )
                    .expect("wa.send dry-run preview call"),
                );
                assert_eq!(envelope["ok"], true);
                assert_eq!(envelope["data"]["injection"]["status"], "allowed");
            }

            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": 42,
                        "text": "echo actual"
                    }),
                )
                .expect("first actual wa.send should still be allowed"),
            );

            assert_eq!(envelope["ok"], true);
            assert_eq!(envelope["data"]["injection"]["status"], "allowed");
        });
    }

    // ========================================================================
    // All Tool Names Use wa. Prefix
    // ========================================================================

    #[test]
    fn all_tool_names_use_wa_prefix() {
        for def in all_definitions() {
            assert!(
                def.name.starts_with("wa."),
                "Tool {} missing wa. prefix",
                def.name
            );
        }
    }

    // ========================================================================
    // All Tools Have Descriptions
    // ========================================================================

    #[test]
    fn all_tools_have_descriptions() {
        for def in all_definitions() {
            assert!(
                def.description.is_some(),
                "Tool {} missing description",
                def.name
            );
            assert!(
                !def.description.as_ref().unwrap().is_empty(),
                "Tool {} has empty description",
                def.name
            );
        }
    }

    // ========================================================================
    // All Input Schemas Are Objects
    // ========================================================================

    #[test]
    fn all_input_schemas_are_objects() {
        for def in all_definitions() {
            let schema_type = def.input_schema.get("type").and_then(|v| v.as_str());
            assert_eq!(
                schema_type,
                Some("object"),
                "Tool {} input_schema type is {:?}, expected 'object'",
                def.name,
                schema_type
            );
        }
    }

    // ========================================================================
    // All Tools Have Version
    // ========================================================================

    #[test]
    fn all_tools_have_version() {
        for def in all_definitions() {
            assert!(def.version.is_some(), "Tool {} missing version", def.name);
        }
    }

    // ========================================================================
    // All Tools Have Tags
    // ========================================================================

    #[test]
    fn all_tools_have_wa_tag() {
        for def in all_definitions() {
            assert!(
                def.tags.contains(&"wa".to_string()),
                "Tool {} missing 'wa' tag",
                def.name
            );
        }
    }

    // ========================================================================
    // Specific Tool Name Stability
    // ========================================================================

    #[test]
    fn core_tool_names_stable() {
        let expected = [
            "wa.state",
            "wa.get_text",
            "wa.send",
            "wa.wait_for",
            "wa.search",
            "wa.events",
            "wa.rules_list",
            "wa.rules_test",
            "wa.reserve",
            "wa.release",
            "wa.reservations",
            "wa.workflow_run",
            "wa.accounts",
        ];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Core tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[test]
    fn mission_tool_names_stable() {
        let expected = [
            "wa.mission_state",
            "wa.mission_explain",
            "wa.mission_pause",
            "wa.mission_resume",
            "wa.mission_abort",
        ];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Mission tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[test]
    fn mission_state_tool_rejects_mission_state_filter_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mission_path = write_mission_file(&dir, MissionLifecycleState::Completed);
        let tool = WaMissionStateTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "mission_file": mission_path.display().to_string(),
                    "mission_state": "running"
                }),
            )
            .expect("mission_state call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error_code"],
            crate::mcp_error::MCP_ERR_INVALID_ARGS
        );
        assert_eq!(
            envelope["error"],
            "mission_state filter 'running' did not match active mission lifecycle_state 'completed'"
        );
        assert_eq!(
            envelope["hint"],
            "Use wa.mission_state without mission_state to inspect the active mission, or request the current lifecycle state."
        );
        assert!(envelope.get("data").is_none());
    }

    #[test]
    fn tx_tool_names_stable() {
        let expected = ["wa.tx_plan", "wa.tx_show", "wa.tx_run", "wa.tx_rollback"];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Tx tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[test]
    fn tx_show_tool_include_contract_returns_embedded_contract() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxShowTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "include_contract": true
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["tx_id"], "tx:test");
        assert_eq!(envelope["data"]["plan_id"], "plan:test");
        assert_eq!(envelope["data"]["lifecycle_state"], "planned");
        assert_eq!(envelope["data"]["receipt_count"], 0);
        assert_eq!(
            envelope["data"]["contract"]["plan"]["steps"]
                .as_array()
                .expect("steps array")
                .len(),
            3
        );
        assert!(
            !envelope["data"]["legal_transitions"]
                .as_array()
                .expect("transitions array")
                .is_empty()
        );
    }

    #[test]
    fn tx_run_tool_rejects_unknown_fail_step_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_step": "tx-step:missing"
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(envelope["error"], "Unknown fail_step: tx-step:missing");
        assert_eq!(
            envelope["hint"],
            "Use step IDs from wa.tx_show(include_contract=true)."
        );
    }

    #[test]
    fn tx_contract_lock_rejects_parallel_same_contract_execution() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let first = super::acquire_mcp_tx_contract_lock(&contract_path)
            .expect("first lock acquisition should succeed");
        assert!(
            super::tx_contract_lock_path(&contract_path).exists(),
            "tx contract lock file should be created next to the contract"
        );

        let second = super::acquire_mcp_tx_contract_lock(&contract_path);
        assert!(
            second.is_err(),
            "second lock acquisition should fail while first is held"
        );
        let Err(err) = second else {
            return;
        };
        assert_eq!(err.code, "robot.tx_in_progress");

        drop(first);
        super::acquire_mcp_tx_contract_lock(&contract_path)
            .expect("lock should be released when guard drops");
    }

    #[test]
    fn tx_run_tool_denies_when_real_prepare_targets_are_missing() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config_with_db_path(&db_path));

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string()
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["prepare_report"]["outcome"], "denied");
        assert!(envelope["data"]["commit_report"].is_null());
        assert!(envelope["data"]["compensation_report"].is_null());
        assert_eq!(envelope["data"]["final_state"], "failed");

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::Failed);
        assert_eq!(persisted.outcome, TxOutcome::Failed);
        assert!(persisted.receipts.is_empty());
    }

    #[test]
    fn tx_run_tool_partial_failure_triggers_compensation_and_compensated_state() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config_with_db_path(&db_path));
        let (_guard, mock) = install_tx_run_mock_wezterm();
        seed_tx_run_real_targets(&db_path, &mock);

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_step": "tx-step:2"
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["prepare_report"]["outcome"], "all_ready");
        assert_eq!(
            envelope["data"]["commit_report"]["outcome"],
            "partial_failure"
        );
        assert_eq!(
            envelope["data"]["commit_report"]["failure_boundary"],
            "tx-step:2"
        );
        assert_eq!(envelope["data"]["commit_report"]["committed_count"], 1);
        assert_eq!(envelope["data"]["commit_report"]["failed_count"], 1);
        assert_eq!(
            envelope["data"]["compensation_report"]["outcome"],
            "fully_rolled_back"
        );
        assert_eq!(envelope["data"]["final_state"], "rolled_back");

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::RolledBack);
        assert_eq!(persisted.outcome, TxOutcome::Compensated);
        assert!(!persisted.receipts.is_empty());
    }

    #[test]
    fn tx_run_tool_first_step_failure_persists_compensated_state() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config_with_db_path(&db_path));
        let (_guard, mock) = install_tx_run_mock_wezterm();
        seed_tx_run_real_targets(&db_path, &mock);

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_step": "tx-step:1"
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["compensation_report"]["outcome"],
            "nothing_to_compensate"
        );
        assert_eq!(envelope["data"]["final_state"], "compensated");

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::Compensated);
        assert_eq!(persisted.outcome, TxOutcome::Compensated);
        assert!(!persisted.receipts.is_empty());
    }

    #[test]
    fn tx_rollback_tool_returns_compensated_state_for_synthetic_commit_report() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Committed);
        let tool = WaTxRollbackTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string()
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["tx_id"], "tx:test");
        assert_eq!(envelope["data"]["final_state"], "rolled_back");
        assert_eq!(
            envelope["data"]["compensation_report"]["outcome"],
            "fully_rolled_back"
        );
        assert_eq!(
            envelope["data"]["compensation_report"]["compensated_count"],
            3
        );

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::RolledBack);
        assert_eq!(persisted.outcome, TxOutcome::Compensated);
        assert!(!persisted.receipts.is_empty());
    }

    #[test]
    fn tx_rollback_tool_rejects_unknown_compensation_step_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract(&dir, MissionTxState::Committed);
        let tool = WaTxRollbackTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_compensation_for_step": "tx-step:missing"
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["error"],
            "Unknown fail_compensation_for_step: tx-step:missing"
        );
        assert_eq!(
            envelope["hint"],
            "Use a committed step ID from wa.tx_show(include_contract=true)."
        );
    }

    #[test]
    fn tx_rollback_tool_uses_receipts_to_compensate_only_committed_steps() {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = write_tx_contract_with_partial_commit_receipts(&dir);
        let tool = WaTxRollbackTool::new(config());

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string()
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["compensation_report"]["compensated_count"],
            1
        );
        assert_eq!(envelope["data"]["compensation_report"]["failed_count"], 0);
        assert_eq!(envelope["data"]["compensation_report"]["skipped_count"], 0);
        assert_eq!(envelope["data"]["final_state"], "rolled_back");
    }

    #[test]
    fn cass_tool_names_stable() {
        let expected = ["wa.cass_search", "wa.cass_view", "wa.cass_status"];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Cass tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn cass_search_tool_executes_cass_with_expected_args() {
        let env = CassToolTestEnv::install(
            r#"printf '%s' '{"query":"agent context","count":1,"hits":[{"source_path":"/tmp/session.md","line_number":42,"agent":"codex","content":"needle hit"}]}'"#,
        );
        let tool = WaCassSearchTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "query": "agent context",
                    "limit": 5,
                    "offset": 2,
                    "agent": "codex",
                    "workspace": "/tmp/ws",
                    "days": 7,
                    "fields": "minimal",
                    "max_tokens": 128,
                    "timeout_secs": 9
                }),
            )
            .expect("cass search call"),
        );

        assert_eq!(
            env.args(),
            vec![
                "search",
                "agent context",
                "--robot",
                "--limit",
                "5",
                "--offset",
                "2",
                "--agent",
                "codex",
                "--workspace",
                "/tmp/ws",
                "--days",
                "7",
                "--fields",
                "minimal",
                "--max-tokens",
                "128",
            ]
        );
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["count"], 1);
        assert_eq!(
            envelope["data"]["hits"][0]["source_path"],
            "/tmp/session.md"
        );
        assert_eq!(envelope["data"]["hits"][0]["line_number"], 42);
    }

    #[cfg(unix)]
    #[test]
    fn cass_view_tool_executes_cass_with_expected_args() {
        let env = CassToolTestEnv::install(
            r#"printf '%s' '{"source_path":"/tmp/session.md","line_number":42,"match_line":{"line_number":42,"content":"needle hit","role":"assistant"},"context_before":[{"line_number":41,"content":"before","role":"user"}],"context_after":[{"line_number":43,"content":"after","role":"assistant"}]}'"#,
        );
        let tool = WaCassViewTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "source_path": "/tmp/session.md",
                    "line_number": 42,
                    "context_lines": 3,
                    "timeout_secs": 11
                }),
            )
            .expect("cass view call"),
        );

        assert_eq!(
            env.args(),
            vec!["view", "/tmp/session.md", "-n", "42", "--json", "-C", "3"]
        );
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["source_path"], "/tmp/session.md");
        assert_eq!(envelope["data"]["match_line"]["content"], "needle hit");
        assert_eq!(envelope["data"]["context_before"][0]["line_number"], 41);
    }

    #[cfg(unix)]
    #[test]
    fn cass_status_tool_executes_cass_with_expected_args() {
        let env = CassToolTestEnv::install(
            r#"printf '%s' '{"healthy":true,"index_path":"/tmp/.cass/index","total_sessions":150,"stale":false}'"#,
        );
        let tool = WaCassStatusTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "timeout_secs": 4
                }),
            )
            .expect("cass status call"),
        );

        assert_eq!(env.args(), vec!["status", "--json"]);
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["healthy"], true);
        assert_eq!(envelope["data"]["total_sessions"], 150);
        assert_eq!(envelope["data"]["stale"], false);
    }

    #[cfg(unix)]
    #[test]
    fn cass_status_tool_maps_nonzero_exit_to_mcp_error() {
        let _env = CassToolTestEnv::install(
            r"printf '%s\n' 'cass exploded' >&2
exit 17",
        );
        let tool = WaCassStatusTool;

        let envelope = parse_json_content(
            tool.call(&test_mcp_context(), serde_json::json!({}))
                .expect("cass status call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_CASS);
        assert_eq!(
            envelope["hint"],
            "cass exited with an error. Check cass logs or rerun with verbose output."
        );
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("cass status failed: cass failed with exit code 17")
        );
    }

    #[test]
    fn annotation_tool_names_stable() {
        let expected = ["wa.events_annotate", "wa.events_triage", "wa.events_label"];
        let names: Vec<String> = all_definitions().iter().map(|d| d.name.clone()).collect();
        for expected_name in &expected {
            assert!(
                names.contains(&expected_name.to_string()),
                "Annotation tool '{}' not found in definitions",
                expected_name
            );
        }
    }

    // -- ft-tzwuw: cass tools must reject timeout_secs=0 before --
    //              dispatching a zero-duration timeout to the binary --

    /// ca.search with timeout_secs=0 must fail-fast with INVALID_ARGS,
    /// not reach cass_client_with_timeout(0) → Duration::from_secs(0)
    /// → instant timeout. The guard runs before any cass binary
    /// dispatch, so this test does not need #[cfg(unix)] or a fake
    /// binary stand-in.
    ///
    /// [ft-szuzd] Error phrasing is now the range form
    /// "timeout_secs must be in 1..=600" since the same guard also
    /// rejects upper-bound violations — that's a strict improvement
    /// on the original "must be >= 1" wording, carrying both endpoints
    /// in one error.
    #[test]
    fn ft_tzwuw_ca_search_rejects_zero_timeout_secs() {
        let envelope = parse_json_content(
            WaCassSearchTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": "anything",
                        "timeout_secs": 0
                    }),
                )
                .expect("ca.search call must produce an envelope"),
        );
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err_str = envelope["error"].as_str().expect("error string");
        assert!(
            err_str.contains("timeout_secs must be in 1..=600"),
            "expected range form 'timeout_secs must be in 1..=600' in error, got: {err_str}"
        );
        assert!(
            err_str.contains("(got 0)"),
            "error must cite the rejected value, got: {err_str}"
        );
    }

    /// [ft-szuzd] ca.search with timeout_secs above the schema
    /// maximum must also fail-fast. serde_json doesn't honour the
    /// schema's "maximum": 600, so without the runtime guard a
    /// client sending timeout_secs: 3600 would block the mcp
    /// server on cass for up to an hour.
    #[test]
    fn ft_szuzd_ca_search_rejects_above_max_timeout_secs() {
        let envelope = parse_json_content(
            WaCassSearchTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": "anything",
                        "timeout_secs": 3600
                    }),
                )
                .expect("ca.search call must produce an envelope"),
        );
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err_str = envelope["error"].as_str().expect("error string");
        assert!(
            err_str.contains("timeout_secs must be in 1..=600"),
            "expected range error, got: {err_str}"
        );
        assert!(
            err_str.contains("(got 3600)"),
            "error must cite the rejected value, got: {err_str}"
        );
    }

    /// [ft-szuzd] ca.search with limit above the schema maximum
    /// must fail-fast before staging a potentially-unbounded query
    /// into CassSearchOptions. limit=0 is a valid 'cass default'
    /// sentinel and must still be accepted.
    #[test]
    fn ft_szuzd_ca_search_rejects_above_max_limit() {
        let envelope = parse_json_content(
            WaCassSearchTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": "anything",
                        "limit": 10_000
                    }),
                )
                .expect("ca.search call must produce an envelope"),
        );
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err_str = envelope["error"].as_str().expect("error string");
        assert!(
            err_str.contains("limit must be in 0..=1000"),
            "expected limit range error, got: {err_str}"
        );
        assert!(
            err_str.contains("(got 10000)"),
            "error must cite the rejected value, got: {err_str}"
        );
    }

    /// ca.view symmetric: timeout_secs=0 → INVALID_ARGS before dispatch.
    #[test]
    fn ft_tzwuw_ca_view_rejects_zero_timeout_secs() {
        let envelope = parse_json_content(
            WaCassViewTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "source_path": "/tmp/session.md",
                        "line_number": 1,
                        "timeout_secs": 0
                    }),
                )
                .expect("ca.view call must produce an envelope"),
        );
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("timeout_secs must be >= 1")
        );
    }

    /// ca.status symmetric: timeout_secs=0 → INVALID_ARGS before dispatch.
    /// Also verifies that the explicit-zero path is reached even though
    /// CassStatusParams::default() returns the schema default (15) for
    /// the null-args path.
    #[test]
    fn ft_tzwuw_ca_status_rejects_zero_timeout_secs() {
        let envelope = parse_json_content(
            WaCassStatusTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "timeout_secs": 0
                    }),
                )
                .expect("ca.status call must produce an envelope"),
        );
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("timeout_secs must be >= 1")
        );
    }

    // ========================================================================
    // Key Parameter Schema Checks
    // ========================================================================

    #[test]
    fn state_tool_schema_has_domain_and_pane_id() {
        let def = WaStateTool::new(PaneFilterConfig::default(), None).definition();
        let props = def.input_schema.get("properties").unwrap();
        assert!(
            props.get("domain").is_some(),
            "wa.state missing 'domain' param"
        );
        assert!(
            props.get("pane_id").is_some(),
            "wa.state missing 'pane_id' param"
        );
    }

    #[test]
    fn wa_state_redaction_helper_scrubs_title_cwd_and_ignore_reason() {
        let marker_prefix = ["sk", "ant", "api03"].join("-");
        let redaction_fixture = format!(
            "{marker_prefix}-{}{}",
            "abcdefghijklmnopqrstuvwxyz", "12345678901234567890"
        );
        let mut states = vec![McpPaneState {
            pane_id: 42,
            pane_uuid: None,
            tab_id: 7,
            window_id: 3,
            domain: "local".to_string(),
            title: Some(format!("codex {redaction_fixture}")),
            cwd: Some(format!("file:///tmp/{redaction_fixture}")),
            observed: true,
            ignore_reason: Some(format!("exclude-{redaction_fixture}")),
        }];

        redact_mcp_pane_state_fields(&mut states);

        let json = serde_json::to_string(&states).expect("serialize states");
        assert!(
            !json.contains(&redaction_fixture),
            "raw secret leaked in wa.state JSON"
        );
        assert!(
            json.contains("[REDACTED]"),
            "expected redaction marker in wa.state JSON"
        );
    }

    #[test]
    fn mcp_pane_state_from_pane_record_preserves_remote_metadata() {
        let record = crate::storage::PaneRecord {
            pane_id: 42,
            pane_uuid: Some("remote-uuid".to_string()),
            domain: "distributed:agent-a:prod".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("remote-pane".to_string()),
            cwd: Some("/srv/agent".to_string()),
            tty_name: None,
            first_seen_at: 1,
            last_seen_at: 2,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let state = McpPaneState::from_pane_record(&record);
        assert_eq!(state.pane_id, 42);
        assert_eq!(state.pane_uuid.as_deref(), Some("remote-uuid"));
        assert_eq!(state.tab_id, 0);
        assert_eq!(state.window_id, 0);
        assert_eq!(state.domain, "distributed:agent-a:prod");
        assert_eq!(state.title.as_deref(), Some("remote-pane"));
    }

    #[test]
    fn merge_distributed_remote_mcp_states_filters_and_dedupes() {
        let mut states = vec![McpPaneState {
            pane_id: 1,
            pane_uuid: None,
            tab_id: 10,
            window_id: 20,
            domain: "local".to_string(),
            title: Some("local-pane".to_string()),
            cwd: None,
            observed: true,
            ignore_reason: None,
        }];
        let remote_a = crate::storage::PaneRecord {
            pane_id: 2,
            pane_uuid: Some("uuid-2".to_string()),
            domain: "distributed:agent-a:prod".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("OpenAI Codex".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1,
            last_seen_at: 2,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        let remote_a_duplicate = crate::storage::PaneRecord {
            title: Some("duplicate".to_string()),
            ..remote_a.clone()
        };
        let remote_b = crate::storage::PaneRecord {
            pane_id: 3,
            pane_uuid: Some("uuid-3".to_string()),
            domain: "distributed:agent-b:prod".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("Gemini CLI".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1,
            last_seen_at: 2,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        let params = StateParams {
            domain: Some("distributed:agent-a:prod".to_string()),
            agent: Some("codex".to_string()),
            pane_id: None,
        };

        merge_distributed_remote_mcp_states(
            &mut states,
            vec![remote_a, remote_a_duplicate, remote_b],
            &params,
        );

        assert_eq!(states.len(), 2);
        assert!(states.iter().any(|state| state.pane_id == 1));
        assert!(states.iter().any(|state| state.pane_id == 2));
        assert!(!states.iter().any(|state| state.pane_id == 3));
    }

    fn seed_distributed_remote_pane(db_path: &Path, pane_id: u64, domain: &str) {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage should open");
            storage
                .upsert_pane(crate::storage::PaneRecord {
                    pane_id,
                    pane_uuid: Some(format!("distributed-{pane_id}")),
                    domain: domain.to_string(),
                    window_id: None,
                    tab_id: None,
                    title: Some("remote-pane".to_string()),
                    cwd: Some("/srv/agent".to_string()),
                    tty_name: None,
                    first_seen_at: 1_700_000_000_000,
                    last_seen_at: 1_700_000_000_000,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                })
                .await
                .expect("distributed pane should seed");
        });
    }

    #[test]
    fn get_text_tool_requires_pane_id() {
        let def = WaGetTextTool::new(config(), Some(db_path())).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.get_text should have required fields");
        let has_pane_id = required.iter().any(|v| v.as_str() == Some("pane_id"));
        assert!(has_pane_id, "wa.get_text should require pane_id");
    }

    /// ft-ii8ss: server-side bound on the `tail` field rejects oversized
    /// requests with MCP_ERR_INVALID_ARGS independent of whether the MCP
    /// client validated the schema's `maximum: 10000` constraint. The
    /// rejection must include both the violated bound (`1..=10000`) and
    /// the offending value so on-call can correlate against logs.
    #[test]
    fn get_text_tool_rejects_tail_over_max_bound() {
        let (_dir, db) = temp_db_path();
        let tool = WaGetTextTool::new(config(), Some(Arc::clone(&db)));

        // tail = 99_999 violates the server-side TAIL_MAX = 10_000.
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 1,
                    "tail": 99_999
                }),
            )
            .expect("wa.get_text over-max call must return an envelope, not panic"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err = envelope["error"]
            .as_str()
            .expect("error message must be a string");
        assert!(
            err.contains("tail must be in"),
            "error message must name the bound: {err}"
        );
        assert!(
            err.contains("99999") || err.contains("99_999"),
            "error message must echo the offending value: {err}"
        );
        assert!(
            err.contains("10000") || err.contains("10_000"),
            "error message must name the upper bound: {err}"
        );
    }

    /// ft-ii8ss: server-side bound also rejects `tail: 0`. The schema
    /// declares `minimum: 1`; without server-side enforcement a buggy
    /// caller could send 0 and silently get a no-op response.
    #[test]
    fn get_text_tool_rejects_tail_below_min_bound() {
        let (_dir, db) = temp_db_path();
        let tool = WaGetTextTool::new(config(), Some(Arc::clone(&db)));

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 1,
                    "tail": 0
                }),
            )
            .expect("wa.get_text tail=0 call must return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err = envelope["error"]
            .as_str()
            .expect("error message must be a string");
        assert!(err.contains("tail must be in"), "error message: {err}");
    }

    #[test]
    fn get_text_tool_returns_remote_text_unavailable_for_distributed_panes() {
        let (_dir, db) = temp_db_path();
        seed_distributed_remote_pane(&db, 4_242, "distributed:agent-a:prod");
        let tool = WaGetTextTool::new(config(), Some(Arc::clone(&db)));

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 4242,
                    "tail": 20
                }),
            )
            .expect("wa.get_text remote pane call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_REMOTE_TEXT_UNAVAILABLE);
        assert_eq!(
            envelope["error"],
            "Live get-text is unavailable for distributed panes"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("wa.search")),
            "remote-pane guidance should point callers at persisted-output search"
        );
    }

    #[test]
    fn get_text_tool_applies_policy_rules_to_distributed_panes() {
        let (_dir, db) = temp_db_path();
        seed_distributed_remote_pane(&db, 7_777, "distributed:agent-b:prod");
        let mut cfg = Config::default();
        cfg.safety.rules.enabled = true;
        cfg.safety.rules.rules.push(crate::config::PolicyRule {
            id: "test.deny.distributed.get_text".to_string(),
            description: Some("deny distributed pane reads".to_string()),
            priority: 1,
            match_on: crate::config::PolicyRuleMatch {
                actions: vec!["read_output".to_string()],
                actors: vec!["mcp".to_string()],
                pane_domains: vec!["distributed:agent-b:prod".to_string()],
                ..Default::default()
            },
            decision: crate::config::PolicyRuleDecision::Deny,
            message: Some("distributed pane reads are blocked".to_string()),
        });
        let tool = WaGetTextTool::new(Arc::new(cfg), Some(Arc::clone(&db)));

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 7777
                }),
            )
            .expect("wa.get_text remote pane policy call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
        assert_eq!(envelope["error"], "distributed pane reads are blocked");
    }

    #[test]
    fn wait_for_tool_applies_policy_rules_before_waiting() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(42).await;
            let tool = WaWaitForTool::with_wezterm_handle(
                deny_mcp_read_output_config("local", "wait-for reads are blocked"),
                None,
                mock as crate::wezterm::WeztermHandle,
            );

            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": 42,
                        "pattern": "ready",
                        "timeout_secs": 1
                    }),
                )
                .expect("wa.wait_for policy call"),
            );

            assert_eq!(envelope["ok"], false);
            assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
            assert_eq!(envelope["error"], "wait-for reads are blocked");
        });
    }

    #[test]
    fn wait_for_tool_persists_policy_denial_audit_when_storage_is_attached() {
        // ft-cro2u: the existing wait_for_tool_applies_policy_rules_before_waiting
        // test passes db_path=None, so the storage.is_some() guard at
        // mcp_tools.rs:1810 short-circuits and the persist_mcp_policy_denial_async
        // call path (the policy_denied_audit row write) is never exercised.
        // Re-run the same Deny scenario with a real on-disk SQLite handle in
        // scope and assert that the audit row appeared.
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db_path) = temp_db_path();

            // ft-7tq4z is fixed: fresh DBs now run migrations, so the
            // tool's StorageHandle::new opens with the v24
            // policy_denied_audit table already created. No workaround
            // needed.

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(42).await;
            let tool = WaWaitForTool::with_wezterm_handle(
                deny_mcp_read_output_config("local", "wait-for reads are blocked"),
                Some(Arc::clone(&db_path)),
                mock as crate::wezterm::WeztermHandle,
            );

            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": 42,
                        "pattern": "ready",
                        "timeout_secs": 1
                    }),
                )
                .expect("wa.wait_for policy call"),
            );

            // Same policy-deny envelope contract as the storage=None test.
            assert_eq!(envelope["ok"], false);
            assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
            assert_eq!(envelope["error"], "wait-for reads are blocked");

            // Now the audit-persist branch: a policy_denied_audit row must
            // have landed for this Deny. Query the DB directly via rusqlite
            // — the storage layer doesn't expose a list/count reader, but
            // the table is documented + indexed (storage.rs:2152-2168) so
            // direct SELECT is the appropriate fence.
            let conn = rusqlite::Connection::open(db_path.as_path())
                .expect("open db for audit verification");
            let (count, tool_name, decision, reason_code, reason): (
                i64,
                String,
                String,
                String,
                String,
            ) = conn
                .query_row(
                    "SELECT COUNT(*), MIN(tool_name), MIN(decision), \
                            MIN(reason_code), MIN(reason) \
                     FROM policy_denied_audit \
                     WHERE tool_name = 'wa.wait_for'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .expect("query policy_denied_audit");
            assert_eq!(
                count, 1,
                "exactly one policy_denied_audit row expected for wa.wait_for"
            );
            assert_eq!(tool_name, "wa.wait_for");
            assert_eq!(
                decision,
                crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED
            );
            assert_eq!(
                reason_code,
                crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED
            );
            // Reason text is the policy-engine-redacted message at
            // mcp_tools.rs:1807-1809 — verify it round-trips intact.
            assert_eq!(reason, "wait-for reads are blocked");
        });
    }

    #[test]
    fn wait_for_and_send_schemas_bound_timeout_secs() {
        for def in [
            WaWaitForTool::new(config(), None).definition(),
            WaSendTool::new(config(), db_path()).definition(),
        ] {
            let timeout_schema = def
                .input_schema
                .get("properties")
                .and_then(|v| v.as_object())
                .and_then(|properties| properties.get("timeout_secs"))
                .expect("timeout_secs property should exist");

            assert_eq!(timeout_schema["minimum"], serde_json::json!(1));
            assert_eq!(
                timeout_schema["maximum"],
                serde_json::json!(MAX_MCP_WAIT_TIMEOUT_SECS)
            );
        }
    }

    #[test]
    fn wait_for_tool_rejects_above_max_timeout_secs() {
        let tool = WaWaitForTool::new(config(), None);
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 42,
                    "pattern": "ready",
                    "timeout_secs": MAX_MCP_WAIT_TIMEOUT_SECS + 1
                }),
            )
            .expect("wa.wait_for timeout validation call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|text| text.contains("timeout_secs must be in 1..=600")),
            "expected bounded timeout error, got {envelope:?}"
        );
    }

    #[test]
    fn send_tool_rejects_above_max_timeout_secs() {
        let tool = WaSendTool::new(config(), db_path());
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 42,
                    "text": "hello",
                    "timeout_secs": MAX_MCP_WAIT_TIMEOUT_SECS + 1
                }),
            )
            .expect("wa.send timeout validation call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|text| text.contains("timeout_secs must be in 1..=600")),
            "expected bounded timeout error, got {envelope:?}"
        );
    }

    #[test]
    fn send_tool_requires_pane_id_and_text() {
        let def = WaSendTool::new(config(), db_path()).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.send should have required fields");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"pane_id"), "wa.send should require pane_id");
        assert!(names.contains(&"text"), "wa.send should require text");
    }

    // ── ft-05hfm send-payload size-cap regressions ──────────────────

    #[test]
    fn max_send_text_bytes_is_four_mib() {
        // [ft-05hfm] Pin the constant so any accidental bump surfaces
        // as an explicit test update rather than silent drift.
        // 4 MiB is ~40x the largest typical paste; rejecting above
        // it catches obvious DoS attempts without burdening bulk-
        // input workflows.
        assert_eq!(MAX_SEND_TEXT_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn wa_send_schema_declares_text_field() {
        // Regression guard: the size cap only matters if the schema
        // still actually exposes `text` as a required string. Any
        // refactor that renames or drops the field must fail this
        // test and force a deliberate cap-check update in lockstep.
        let def = WaSendTool::new(config(), db_path()).definition();
        let props = def
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("wa.send schema should have properties");
        let text_schema = props.get("text").expect("text field must exist");
        assert_eq!(text_schema["type"], "string");
    }

    #[test]
    fn max_send_text_bytes_exceeds_typical_paste() {
        // Boundary-sanity test: a 100 KiB paste (full prompt+context
        // for a Claude Code turn) must fit well under the cap so the
        // fix doesn't regress legitimate usage.
        let typical_max_paste = 100 * 1024;
        assert!(
            MAX_SEND_TEXT_BYTES > typical_max_paste,
            "MAX_SEND_TEXT_BYTES {} must leave headroom above typical paste {}",
            MAX_SEND_TEXT_BYTES,
            typical_max_paste
        );
    }

    #[test]
    fn wa_send_dry_run_reports_terminal_control_byte_risk() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db) = temp_db_path();
            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(42).await;
            let tool = WaSendTool::with_wezterm_handle(
                config(),
                Arc::clone(&db),
                mock as crate::wezterm::WeztermHandle,
            );

            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": 42,
                        "text": "npm test\u{0003}",
                        "dry_run": true
                    }),
                )
                .expect("wa.send dry-run call"),
            );

            assert_eq!(envelope["ok"], true);
            assert_eq!(envelope["data"]["dry_run"], true);
            let factor_ids: Vec<&str> =
                envelope["data"]["injection"]["decision"]["context"]["risk"]["factors"]
                    .as_array()
                    .expect("risk factors array")
                    .iter()
                    .filter_map(|factor| factor["id"].as_str())
                    .collect();
            assert!(
                factor_ids.contains(&"content.terminal_control_bytes"),
                "dry-run policy preview should surface terminal-control-byte risk, got {:?}",
                factor_ids
            );
        });
    }

    #[test]
    fn search_tool_requires_query() {
        let def = WaSearchTool::new(config(), db_path()).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.search should have required fields");
        let has_query = required.iter().any(|v| v.as_str() == Some("query"));
        assert!(has_query, "wa.search should require query");
    }

    #[test]
    fn reserve_tool_requires_pane_id() {
        let def = WaReserveTool::new(config(), db_path()).definition();
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.reserve should have required fields");
        let has_pane_id = required.iter().any(|v| v.as_str() == Some("pane_id"));
        assert!(has_pane_id, "wa.reserve should require pane_id");
    }

    // ========================================================================
    // Policy input helpers — send, workflow, reserve, release
    // ========================================================================

    #[test]
    fn mcp_send_text_policy_input_fields() {
        let caps = PaneCapabilities::unknown();
        let input = mcp_send_text_policy_input(5, "local", caps, "send summary", "echo hello");
        assert_eq!(input.action, ActionKind::SendText);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Mux);
        assert_eq!(input.pane_id, Some(5));
        assert_eq!(input.domain.as_deref(), Some("local"));
        assert_eq!(input.text_summary.as_deref(), Some("send summary"));
        assert_eq!(input.command_text.as_deref(), Some("echo hello"));
    }

    #[test]
    fn mcp_workflow_run_policy_input_fields() {
        let caps = PaneCapabilities::unknown();
        let input = mcp_workflow_run_policy_input(9, "SSH:host", caps, "run workflow");
        assert_eq!(input.action, ActionKind::WorkflowRun);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Workflow);
        assert_eq!(input.pane_id, Some(9));
        assert_eq!(input.domain.as_deref(), Some("SSH:host"));
        assert_eq!(input.text_summary.as_deref(), Some("run workflow"));
    }

    #[test]
    fn dry_run_policy_authorization_does_not_consume_workflow_rate_limit_budget() {
        let mut cfg = Config::default();
        cfg.safety.require_prompt_active = false;
        cfg.safety.rate_limit_per_pane = 1;
        cfg.safety.rate_limit_global = 100;
        let shared_rate_limiter = build_mcp_shared_rate_limiter(&cfg);
        let mut engine = build_policy_engine_with_shared_rate_limiter(
            &cfg,
            cfg.safety.require_prompt_active,
            Arc::clone(&shared_rate_limiter),
        );
        let input = mcp_workflow_run_policy_input(
            42,
            "local",
            PaneCapabilities::unknown(),
            "workflow run preview",
        );

        for attempt in 0..3 {
            let decision = authorize_mcp_policy_call(&mut engine, &input, true);
            assert!(
                decision.is_allowed(),
                "dry-run attempt {attempt} should not be rate limited"
            );
        }

        let outcome = shared_rate_limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .preview(ActionKind::WorkflowRun, Some(42));
        assert!(
            outcome.is_allowed(),
            "dry-run workflow previews must not consume WorkflowRun rate-limit budget"
        );

        let real_decision = authorize_mcp_policy_call(&mut engine, &input, false);
        assert!(real_decision.is_allowed());

        let outcome_after_real_run = shared_rate_limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .preview(ActionKind::WorkflowRun, Some(42));
        assert!(
            !outcome_after_real_run.is_allowed(),
            "a real workflow run must still consume WorkflowRun rate-limit budget"
        );
    }

    #[test]
    fn mcp_reserve_pane_policy_input_fields() {
        let input = mcp_reserve_pane_policy_input(42, "reserve pane 42");
        assert_eq!(input.action, ActionKind::ReservePane);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Swarm);
        assert_eq!(input.pane_id, Some(42));
        assert_eq!(input.command_text.as_deref(), Some("reserve_pane"));
    }

    #[test]
    fn mcp_release_pane_policy_input_with_pane_id() {
        let input = mcp_release_pane_policy_input("release pane 42", Some(42));
        assert_eq!(input.action, ActionKind::ReleasePane);
        assert_eq!(input.actor, ActorKind::Mcp);
        assert_eq!(input.surface, PolicySurface::Swarm);
        assert_eq!(input.pane_id, Some(42));
        assert_eq!(input.command_text.as_deref(), Some("release_reservation"));
    }

    #[test]
    fn mcp_release_pane_policy_input_without_pane_id() {
        let input = mcp_release_pane_policy_input("release all", None);
        assert_eq!(input.action, ActionKind::ReleasePane);
        assert_eq!(input.pane_id, None);
    }

    // ========================================================================
    // Event mutation decision context
    // ========================================================================

    #[test]
    fn mcp_event_mutation_decision_context_fields() {
        let context = mcp_event_mutation_decision_context(
            "wa.events_annotate",
            "events_annotate",
            123,
            "add_note",
            Some("agent-42"),
            "Annotate event 123",
            9999,
        );

        assert_eq!(context.timestamp_ms, 9999);
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);

        let evidence: std::collections::HashMap<String, String> = context
            .evidence
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();

        assert_eq!(
            evidence.get("tool").map(String::as_str),
            Some("wa.events_annotate")
        );
        assert_eq!(
            evidence.get("event_action_kind").map(String::as_str),
            Some("events_annotate")
        );
        assert_eq!(evidence.get("event_id").map(String::as_str), Some("123"));
        assert_eq!(
            evidence.get("operation").map(String::as_str),
            Some("add_note")
        );
        assert_eq!(
            evidence.get("actor_id").map(String::as_str),
            Some("agent-42")
        );
        assert_eq!(
            evidence.get("event_surface").map(String::as_str),
            Some("mcp")
        );
    }

    #[test]
    fn mcp_event_mutation_decision_context_without_actor_id() {
        let context = mcp_event_mutation_decision_context(
            "wa.events_triage",
            "events_triage",
            456,
            "set_state",
            None,
            "Triage event 456",
            1000,
        );

        let evidence: std::collections::HashMap<String, String> = context
            .evidence
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();

        assert!(
            !evidence.contains_key("actor_id"),
            "actor_id should be absent when None"
        );
        assert_eq!(evidence.get("event_id").map(String::as_str), Some("456"));
    }

    #[test]
    fn serialize_mcp_audit_decision_context_produces_valid_json() {
        let context = mcp_event_mutation_decision_context(
            "wa.events_label",
            "events_label",
            789,
            "add_label",
            Some("test-agent"),
            "Label event 789",
            5000,
        );
        let json = serialize_mcp_audit_decision_context(&context);
        assert!(json.is_some(), "serialization should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json.unwrap()).expect("should be valid JSON");
        assert!(parsed.is_object());
    }
}
