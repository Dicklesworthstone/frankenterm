//! Extracted MCP tool handlers (strangler-fig migration slice).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
#[cfg(all(test, unix))]
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::de::DeserializeOwned;

use super::mcp_missions::mcp_save_mission_tx_contract_to_path;
use super::mcp_types::{
    self, AccountsParams, AccountsRefreshParams, AttentionParams, AwaitEventParams,
    CassSearchParams, CassStatusParams, CassViewParams, EventsAnnotateParams, EventsLabelParams,
    EventsParams, EventsTriageParams, GetTextParams, McpAccountInfo, McpAccountsData,
    McpAccountsRefreshData, McpAwaitConditionStatus, McpAwaitEventData, McpEnvelope, McpEventItem,
    McpEventMutationData, McpEventsData, McpGetTextData, McpMissionControlData,
    McpMissionExplainData, McpMissionStateData, McpPaneState, McpReleaseData, McpReservationInfo,
    McpReservationsData, McpReserveData, McpRuleItem, McpRuleMatchItem, McpRuleTraceInfo,
    McpRulesListData, McpRulesTestData, McpSearchData, McpSearchHit, McpSendData, McpTxPlanData,
    McpTxRollbackData, McpTxRunData, McpTxShowData, McpWaitForData, McpWorkflowRunData,
    MissionAbortParams, MissionExplainParams, MissionObjectivePlanParams, MissionPauseParams,
    MissionResumeParams, MissionStateParams, OperatingEnvelopeParams, RehearsalScoreParams,
    ReleaseParams, ReservationsParams, ReserveParams, RulesListParams, RulesTestParams,
    SearchParams, SendParams, StateParams, TxPlanParams, TxRollbackParams, TxRunParams,
    TxShowParams, WaitForParams, WorkflowRunParams, WorkflowStatusParams, apply_tail_truncation,
    now_ms,
};
#[allow(unused_imports)]
use super::{
    AccountRecord, ActionKind, ActorKind, AgentProvider, AgentType, ApprovalStore, CassAgent,
    CassClient, CassError, CassSearchOptions, CassSearchResult, CassStatus, CassViewOptions,
    CassViewResult, CautClient, CautService, Config, DecisionContext, EventQuery,
    HandleAuthRequired, HandleClaudeCodeLimits, HandleCompaction, HandleGeminiQuota,
    HandleProcessTriageLifecycle, HandleSessionEnd, HandleUsageLimits, InjectionResult,
    MCP_ERR_CASS, MCP_ERR_CAUT, MCP_ERR_CONFIG, MCP_ERR_FTS_QUERY, MCP_ERR_INVALID_ARGS,
    MCP_ERR_PANE_NOT_FOUND, MCP_ERR_POLICY, MCP_ERR_STORAGE, MCP_ERR_TIMEOUT, MCP_ERR_WEZTERM,
    MCP_ERR_WORKFLOW, McpToolError, Osc133State, PaneCapabilities, PaneFilterConfig, PaneInfo,
    PaneReservation, PaneWaiter, PatternEngine, PolicyDecision, PolicyEngine, PolicyGatedInjector,
    PolicyInput, SearchQueryDefaults, SearchQueryInput, SharedRateLimiter, StorageHandle,
    UnifiedSearchMode, WaitOptions, WaitResult, WeztermError, WeztermHandleSource, Workflow,
    WorkflowExecutionResult, approval_command, build_mcp_shared_rate_limiter,
    build_mcp_workflow_assembly, build_policy_engine_with_shared_rate_limiter,
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
use crate::attention_router::{
    AttentionRouterSourceAdapterInput, AttentionRouterSurface,
    build_attention_router_surface_payload,
};
use crate::demo_scenarios::DemoScenarioManifest;
use crate::mcp_error::MCP_ERR_REMOTE_TEXT_UNAVAILABLE;
#[allow(unused_imports)]
use crate::mcp_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkTool as Tool,
    FrameworkResponseDeliveryCoordinator, FrameworkResponseDeliveryOutcome,
    FrameworkToolAnnotations as ToolAnnotations, FrameworkToolHandler as ToolHandler,
};
use crate::mission_objective_plan::{
    MissionObjectiveCandidateReadiness, MissionObjectiveCandidateWork,
    MissionObjectiveCapacityPosture, MissionObjectiveDirtyPath, MissionObjectiveEvidenceCategory,
    MissionObjectiveEvidenceItem, MissionObjectivePlannerInput, MissionObjectiveProofAvailability,
    MissionObjectiveSourceKind, MissionObjectiveSourceSnapshot, MissionObjectiveStrictness,
    build_mission_objective_plan_surface_data, plan_mission_objective,
};
use crate::operating_envelope::{
    OperatingEnvelopeScenario, OperatingEnvelopeSurface, build_operating_envelope_surface_data,
    operating_envelope_input_for_scenario, plan_operating_envelope,
};
use crate::policy::PolicySurface;
use crate::rehearsal_score::{
    REHEARSAL_SCORE_SURFACE_CONTRACT_ID, RehearsalScoreSurface, RehearsalScoreSurfaceReport,
};
use crate::robot_types::{
    SubmitGuaranteeLevel, WorkflowActionPlan, WorkflowStatusData, WorkflowStatusDetailData,
    WorkflowStatusListData, WorkflowStepLog,
};
use crate::runtime_async::{CompatRuntime, RuntimeBuilder as CompatRuntimeBuilder};
use crate::storage::{EventDeliveryLease, EventDeliveryReservation, EventStreamQuery};
use crate::workflows::ManualWorkflowRunOutcome;

/// br-ft-pgjat: route silent `record_audit_action_redacted_with_cx`
/// failures through the same observability counter as ft-luav8's
/// `record_mcp_audit*` helpers in mcp_helpers.rs.
///
/// 4 callers in this file
/// (wa.events_annotate / wa.events_triage / wa.events_label×2)
/// previously did `let _ = storage.record_audit_action_redacted_with_cx(...).await`
/// — silently swallowing both the audit row id (Ok) and the
/// failure (Err). The audit row was missing AND operators had no
/// signal — no tracing::warn, no counter bump.
///
/// This helper preserves the fire-and-forget contract (audit
/// failures must NOT propagate to the MCP client per the ft-luav8
/// design — the client succeeded; the audit fidelity gap is
/// surfaced via the counter + log) while making the failure
/// observable.
async fn record_event_mutation_audit_or_log(
    storage: &crate::storage::StorageHandle,
    audit_cx: &crate::cx::Cx,
    audit: crate::storage::AuditActionRecord,
    tool_name: &'static str,
) {
    if let Err(err) = storage
        .record_audit_action_redacted_with_cx(audit_cx, audit)
        .await
    {
        crate::mcp::record_mcp_audit_failure();
        tracing::warn!(
            target: "ft.security.audit",
            tool = tool_name,
            error = %err,
            "br-ft-pgjat: silent record_audit_action_redacted_with_cx failure; audit row \
             missing for this MCP event-mutation call (client still got success per the \
             ft-luav8 fire-and-forget contract). MCP_AUDIT_FAILURE_COUNT bumped."
        );
    }
}

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
pub const MAX_SEND_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Hard cap for MCP wait pattern strings before substring matching or regex
/// compilation.
///
/// `wa.wait_for.pattern` and `wa.send.wait_for` are control-plane selectors,
/// not bulk payload channels. Keeping them bounded prevents malformed clients
/// from forcing very large matcher allocation or expensive regex compilation
/// before the normal timeout/tail guards can help.
pub const MAX_MCP_WAIT_PATTERN_BYTES: usize = 64 * 1024;

/// Hard cap for caller-supplied submit idempotency nonces.
///
/// The durable store only sees the canonical hashed key derived from
/// `(pane_id, text, caller_nonce)`, but the raw caller nonce still crosses the
/// MCP boundary and should stay small.
pub const MAX_MCP_SUBMIT_IDEMPOTENCY_KEY_BYTES: usize = 256;

/// Hard cap for MCP rule-test text before feeding it into the pattern engine.
///
/// `wa.rules_test` is a control-plane conformance/debug surface, not a bulk
/// pane-output ingestion path. Keep it bounded so malformed MCP clients cannot
/// force unbounded anchor scans and regex matching work inside a synchronous
/// tool handler.
pub const MAX_MCP_RULES_TEST_TEXT_BYTES: usize = 256 * 1024;

/// Hard cap for MCP rule-list agent type selectors before normalization.
///
/// `agent_type` is an enum-like control field (`codex`, `claude_code`,
/// `gemini`, `wezterm`). Bound it before any case normalization so malformed
/// clients cannot force large string allocations through a tiny selector.
pub const MAX_MCP_RULES_AGENT_TYPE_BYTES: usize = 64;

/// Hard cap for MCP state agent filters before case-insensitive matching.
///
/// `wa.state.agent` is a selector over pane titles and known agent families.
/// Bound it before normalization/matching so malformed clients cannot force
/// large allocations or repeated scans through a tiny filter field.
pub const MAX_MCP_STATE_AGENT_FILTER_BYTES: usize = 256;

/// Hard cap for MCP CASS agent selectors before CASS/agent-provider parsing.
///
/// `wa.cass_search.agent` is an enum-like selector. Keep it bounded before
/// normalization in downstream parser code so malformed clients cannot use a
/// tiny filter field as an unbounded allocation path.
pub const MAX_MCP_CASS_AGENT_FILTER_BYTES: usize = 64;

/// Hard cap for MCP CASS query strings before dispatching to the CASS subprocess.
///
/// `wa.cass_search.query` is a search expression, not a bulk payload channel.
/// Bound it before trim checks and process dispatch so malformed clients cannot
/// force very large argv/payload handling through the MCP server.
pub const MAX_MCP_CASS_QUERY_BYTES: usize = 64 * 1024;

/// Hard cap for account service selectors accepted through MCP tools.
///
/// Service names are small enumerated identifiers. Keep malformed account tool
/// arguments out of storage/Caut dispatch and out of error/audit text.
pub const MAX_MCP_ACCOUNT_SERVICE_BYTES: usize = 64;

/// Hard cap for MCP search queries before parsing, embedding, or storage search.
///
/// `wa.search.query` is a search expression, not a bulk text transport. Bound it
/// before unified query parsing so malformed clients cannot drive oversized FTS
/// or semantic-search work through the MCP surface.
pub const MAX_MCP_SEARCH_QUERY_BYTES: usize = 64 * 1024;

// br-ft-rnpuc: clock-anomaly observability for MCP tool audit
// timestamps. mcp_tools.rs has 11 sites with the pattern
// `i64::try_from(now_ms()).unwrap_or(0)` — when the u64 → i64
// conversion fails (now_ms() corrupted to a value > i64::MAX),
// the audit row records ts_ms=0 silently. In practice this never
// fails (now_ms() returns ~1.7T ms; i64::MAX is ~9.2 quintillion),
// but the silent fallback to 0 is a code smell and the
// observability gap is real — same shape as the
// POLICY_CLOCK_ANOMALY_COUNT helper in policy.rs:1878. This counter
// surfaces every collapse so operators can cross-reference against
// audit-row ts=0 entries when investigating clock anomalies.
static MCP_CLOCK_ANOMALY_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of u64→i64 timestamp-cast failures observed in
/// MCP tool audit row construction since process load. Each
/// increment represents one audit row that recorded `ts_ms=0`
/// because `now_ms()` returned a value above `i64::MAX`. > 0
/// means cross-reference against host-side clock telemetry.
#[must_use]
pub fn mcp_clock_anomaly_count() -> u64 {
    MCP_CLOCK_ANOMALY_COUNT.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests that simulate the
/// collapse path can assert post-increment values without state
/// leakage between tests.
#[cfg(test)]
fn reset_mcp_clock_anomaly_count_for_test() {
    MCP_CLOCK_ANOMALY_COUNT.store(0, Ordering::Relaxed);
}

/// br-ft-rnpuc: u64 → i64 timestamp cast with anomaly counter
/// bump on collapse. Replaces the inline pattern
/// `i64::try_from(now_ms()).unwrap_or(0)` at every audit-row
/// construction site so a hypothetical clock corruption gets
/// observable, not silent. Same shape as policy.rs's
/// `checked_now_ms_i64`.
pub fn mcp_audit_ts_ms_from_u64(ts_ms: u64) -> i64 {
    match i64::try_from(ts_ms) {
        Ok(v) => v,
        Err(_) => {
            MCP_CLOCK_ANOMALY_COUNT.fetch_add(1, Ordering::Relaxed);
            0
        }
    }
}

/// Current MCP audit timestamp as signed milliseconds since the
/// Unix epoch, with the same anomaly observability as
/// [`mcp_audit_ts_ms_from_u64`].
pub fn mcp_now_ms_i64() -> i64 {
    mcp_audit_ts_ms_from_u64(now_ms())
}

// br-ft-ncijf: workflow-status plan_json deserialize observability.
// `workflow_status_data` calls `serde_json::from_str::<ActionPlan>(&plan_record.plan_json).ok()`
// — when a persisted plan_json fails to parse (schema bump, hand-edit,
// truncation, encoding skew), the operator-facing WorkflowStatusDetailData
// silently returns `plan_step_name = None` and `total_steps = None`.
// The operator running `mcp__frankenterm__workflow_status` sees the
// workflow as "running step ?" with no signal that the plan record is
// corrupt. This counter bumps on every malformed parse so operators
// can cross-reference against the workflow_action_plans table when
// investigating "missing plan metadata" reports.
//
// Same observability defect family as ft-iwg7x
// (robot_profile_bootstrap_serde_drop_count), ft-zkthg
// (workflows_serde_drop_count), ft-jyywz (audit_chain_export_dropped_count),
// ft-yygus (policy_decision_context_serde_drop_count), ft-rnpuc
// (mcp_clock_anomaly_count), and ft-bn6qi (epoch_clock_anomaly_count).
static MCP_WORKFLOW_PLAN_SERDE_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// br-ft-ncijf: cumulative count of workflow `plan_json` deserialize
/// failures observed in `workflow_status_data`. Each increment
/// represents one workflow_status response where the operator's
/// stored plan record was schema-skewed and the response silently
/// substituted `plan_step_name = None` + `total_steps = None`.
/// A non-zero value means investigate the `workflow_action_plans`
/// table for schema-bump or hand-edit corruption.
#[must_use]
pub fn mcp_workflow_plan_serde_drop_count() -> u64 {
    MCP_WORKFLOW_PLAN_SERDE_DROP_COUNT.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so regression tests can assert
/// post-bump values without state leakage between tests.
#[cfg(test)]
fn reset_mcp_workflow_plan_serde_drop_count_for_test() {
    MCP_WORKFLOW_PLAN_SERDE_DROP_COUNT.store(0, Ordering::Relaxed);
}

#[inline]
fn record_mcp_workflow_plan_serde_drop() {
    MCP_WORKFLOW_PLAN_SERDE_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// br-ft-ncijf: parse workflow plan_json with audit-fidelity counter
/// bump + structured warn on serde failure. Replaces the silent
/// `serde_json::from_str::<ActionPlan>(&plan_json).ok()` pattern at
/// `workflow_status_data` so plan-record corruption surfaces via
/// metrics scrape AND log search.
pub fn parse_workflow_plan_json(plan_json: &str, plan_id: &str) -> Option<crate::plan::ActionPlan> {
    match serde_json::from_str::<crate::plan::ActionPlan>(plan_json) {
        Ok(plan) => Some(plan),
        Err(err) => {
            record_mcp_workflow_plan_serde_drop();
            tracing::warn!(
                target: "frankenterm::mcp_tools",
                event = "br-ft-ncijf",
                error = %err,
                plan_id = %plan_id,
                plan_json_len = plan_json.len(),
                "workflow plan_json failed to deserialize as ActionPlan; \
                 status response will report plan_step_name=None and total_steps=None"
            );
            None
        }
    }
}

/// Hard cap for MCP pane-output waits.
///
/// `wa.wait_for` and `wa.send --wait_for` run through synchronous MCP tool
/// handlers backed by a current-thread runtime. Keep their operator-tunable
/// wait window bounded so a malformed MCP client cannot pin a handler for
/// hours or days.
pub const MAX_MCP_WAIT_TIMEOUT_SECS: u64 = 600;

/// ft-<ux-audit>: shared hint string for every `MCP_ERR_POLICY` hard-deny
/// response. Previously every deny site passed `None` as the hint,
/// leaving the MCP client with "Read denied by policy" and nowhere to go.
/// The hint names two actionable things: where the operator looks to
/// understand the deny (`config.safety.rules`) and where the decision
/// context (rule_id + reason) is queryable (`policy_denied_audit`).
///
/// Kept as a single const so all 7 deny sites and the gate helper can
/// diverge-by-accident-proof: one edit here, every hint updates.
pub const POLICY_DENY_HINT: &str = "Hard policy deny: review `config.safety.rules` for the active deny list, \
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

fn validate_mcp_wait_pattern_bytes(
    tool_name: &str,
    field_name: &str,
    pattern: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if pattern.len() <= MAX_MCP_WAIT_PATTERN_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "{field_name} is {} bytes; max allowed is {MAX_MCP_WAIT_PATTERN_BYTES} bytes",
            pattern.len()
        ),
        Some(format!(
            "{tool_name} accepts wait patterns up to {MAX_MCP_WAIT_PATTERN_BYTES} bytes; \
             use wa.search for large pane-output scans."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_submit_idempotency_key_bytes(
    tool_name: &str,
    idempotency_key: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if idempotency_key.len() <= MAX_MCP_SUBMIT_IDEMPOTENCY_KEY_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "idempotency_key is {} bytes; max allowed is {MAX_MCP_SUBMIT_IDEMPOTENCY_KEY_BYTES} bytes",
            idempotency_key.len()
        ),
        Some(format!(
            "{tool_name} accepts short caller replay keys; use a compact retry/session nonce."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_rules_test_text_bytes(
    tool_name: &str,
    text: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if text.len() <= MAX_MCP_RULES_TEST_TEXT_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "text is {} bytes; max allowed is {MAX_MCP_RULES_TEST_TEXT_BYTES} bytes",
            text.len()
        ),
        Some(format!(
            "{tool_name} accepts bounded sample text only; use pane capture/search surfaces for \
             large outputs."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_rules_agent_type_bytes(
    tool_name: &str,
    agent_type: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if agent_type.len() <= MAX_MCP_RULES_AGENT_TYPE_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "agent_type is {} bytes; max allowed is {MAX_MCP_RULES_AGENT_TYPE_BYTES} bytes",
            agent_type.len()
        ),
        Some(format!(
            "{tool_name} accepts only short agent type selectors: codex, claude_code, gemini, \
             wezterm."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_state_agent_filter_bytes(
    tool_name: &str,
    agent: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if agent.len() <= MAX_MCP_STATE_AGENT_FILTER_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "agent is {} bytes; max allowed is {MAX_MCP_STATE_AGENT_FILTER_BYTES} bytes",
            agent.len()
        ),
        Some(format!(
            "{tool_name} accepts bounded agent filters only; use known selectors such as codex, \
             claude_code, or gemini."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_cass_agent_filter_bytes(
    tool_name: &str,
    agent: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if agent.len() <= MAX_MCP_CASS_AGENT_FILTER_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "agent is {} bytes; max allowed is {MAX_MCP_CASS_AGENT_FILTER_BYTES} bytes",
            agent.len()
        ),
        Some(format!(
            "{tool_name} accepts only short agent selectors: codex, claude_code, gemini, cursor, \
             aider, chatgpt."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_cass_query_bytes(
    tool_name: &str,
    query: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if query.len() <= MAX_MCP_CASS_QUERY_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "query is {} bytes; max allowed is {MAX_MCP_CASS_QUERY_BYTES} bytes",
            query.len()
        ),
        Some(format!(
            "{tool_name} accepts bounded search expressions only; narrow the query before calling \
             cass."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_account_service_bytes(
    tool_name: &str,
    service: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if service.len() <= MAX_MCP_ACCOUNT_SERVICE_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "service is {} bytes; max allowed is {MAX_MCP_ACCOUNT_SERVICE_BYTES} bytes",
            service.len()
        ),
        Some(format!(
            "{tool_name} accepts short service identifiers only; use a supported account service \
             alias."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn validate_mcp_search_query_bytes(
    tool_name: &str,
    query: &str,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if query.len() <= MAX_MCP_SEARCH_QUERY_BYTES {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "query is {} bytes; max allowed is {MAX_MCP_SEARCH_QUERY_BYTES} bytes",
            query.len()
        ),
        Some(format!(
            "{tool_name} accepts bounded search expressions only; narrow the query before \
             searching."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn parse_mcp_rules_agent_type(raw: &str) -> Option<AgentType> {
    let normalized = raw.trim();
    if normalized.eq_ignore_ascii_case("codex") {
        Some(AgentType::Codex)
    } else if normalized.eq_ignore_ascii_case("claude_code") {
        Some(AgentType::ClaudeCode)
    } else if normalized.eq_ignore_ascii_case("gemini") {
        Some(AgentType::Gemini)
    } else if normalized.eq_ignore_ascii_case("wezterm") {
        Some(AgentType::Wezterm)
    } else {
        None
    }
}

fn parse_mcp_tool_params<T: DeserializeOwned>(
    tool_name: &str,
    arguments: serde_json::Value,
    expected: &'static str,
    start: Instant,
) -> Result<T, McpResult<Vec<Content>>> {
    serde_json::from_value(arguments).map_err(|err| {
        let envelope = McpEnvelope::<()>::error(
            MCP_ERR_INVALID_ARGS,
            redact_mcp_output_secrets(&format!("Invalid params for {tool_name}: {err}")),
            Some(expected.to_string()),
            elapsed_ms(start),
        );
        envelope_to_content(envelope)
    })
}

/// CASS timeout_secs schema bounds — single source of truth for
/// `wa.cass_search`, `wa.cass_view`, and `wa.cass_status`.
pub const CASS_TIMEOUT_SECS_MIN: u64 = 1;
pub const CASS_TIMEOUT_SECS_MAX: u64 = 600;

/// [ft-aylbh] Shared CASS timeout_secs guard used by `wa.cass_search`,
/// `wa.cass_view`, and `wa.cass_status`. All three tool schemas declare
/// `timeout_secs ∈ [1, 600]`, but serde_json doesn't honour JSON-Schema
/// bounds — only `cass_search` enforced the upper bound at runtime
/// (ft-szuzd); the view and status handlers only rejected zero, leaving
/// the upper bound bypassable by hostile/buggy clients.
///
/// Returns `Some(error_envelope)` when the timeout is out of range, or
/// `None` when validation passes. Same shape as
/// [`validate_mcp_wait_timeout_secs`].
fn validate_cass_timeout_secs(
    tool_name: &str,
    timeout_secs: u64,
    start: Instant,
) -> Option<McpResult<Vec<Content>>> {
    if (CASS_TIMEOUT_SECS_MIN..=CASS_TIMEOUT_SECS_MAX).contains(&timeout_secs) {
        return None;
    }

    let envelope = McpEnvelope::<()>::error(
        MCP_ERR_INVALID_ARGS,
        format!(
            "timeout_secs must be in {CASS_TIMEOUT_SECS_MIN}..={CASS_TIMEOUT_SECS_MAX} (got {timeout_secs})"
        ),
        Some(format!(
            "The {tool_name} tool schema declares timeout_secs ∈ \
             [{CASS_TIMEOUT_SECS_MIN}, {CASS_TIMEOUT_SECS_MAX}]; clamp \
             your request or omit the field to use the default (15)."
        )),
        elapsed_ms(start),
    );
    Some(envelope_to_content(envelope))
}

fn acquire_mcp_tx_contract_lock(
    workspace_root: &Path,
    path: &Path,
) -> std::result::Result<crate::tx_execution::TxContractLockGuard, McpToolError> {
    match path.try_exists() {
        Ok(true) => {}
        Ok(false) => {
            return Err(McpToolError::new(
                MCP_ERR_WORKFLOW,
                format!("Tx contract file not found: {}", path.display()),
                Some("Pass contract_file or create .ft/mission/tx-active.json.".to_string()),
            ));
        }
        Err(err) => {
            return Err(McpToolError::new(
                MCP_ERR_STORAGE,
                format!(
                    "Failed to inspect tx contract before locking {}: {err}",
                    path.display()
                ),
                Some("Fix access to the transaction contract path, then retry.".to_string()),
            ));
        }
    }
    crate::tx_execution::acquire_tx_contract_lock(workspace_root, path).map_err(|err| {
        let in_progress = err.kind() == crate::tx_execution::TxContractStoreErrorKind::InProgress;
        McpToolError::new(
            if in_progress {
                MCP_ERR_WORKFLOW
            } else {
                MCP_ERR_STORAGE
            },
            err.to_string(),
            Some(if in_progress {
                "Wait for the in-flight transaction mutation to finish, then retry.".to_string()
            } else {
                "Fix access to the transaction contract lock file, then retry.".to_string()
            }),
        )
    })
}

fn load_mcp_tx_contract_from_guard(
    guard: &crate::tx_execution::TxContractLockGuard,
) -> std::result::Result<crate::plan::MissionTxContract, McpToolError> {
    let path = guard.authoritative_path();
    let raw = guard.read_authoritative_contract_bytes().map_err(|err| {
        let oversize = err.kind() == crate::tx_execution::TxContractStoreErrorKind::TooLarge;
        McpToolError::new(
            if oversize {
                MCP_ERR_INVALID_ARGS
            } else {
                MCP_ERR_STORAGE
            },
            err.to_string(),
            Some(if oversize {
                "Reduce the transaction contract below the supported byte limit before retrying."
                    .to_string()
            } else {
                "The pinned transaction object or control-plane anchor is no longer safe; resolve the namespace change and retry."
                    .to_string()
            }),
        )
    })?;
    let contract: crate::plan::MissionTxContract = serde_json::from_slice(&raw).map_err(|err| {
        McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("Invalid tx contract JSON in {}: {err}", path.display()),
            Some("Ensure the file matches the MissionTxContract schema.".to_string()),
        )
    })?;
    contract.validate().map_err(|err| {
        McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("Tx contract validation failed: {err}"),
            Some("Inspect contract via wa.tx_show include_contract=true.".to_string()),
        )
    })?;
    Ok(contract)
}

fn authorize_mcp_tx_contract_for_effects(
    guard: &crate::tx_execution::TxContractLockGuard,
    authoritative_path: &Path,
) -> std::result::Result<(), McpToolError> {
    guard.authorizes(authoritative_path).map_err(|err| {
        McpToolError::new(
            MCP_ERR_STORAGE,
            format!(
                "Transaction contract authorization changed before external effect dispatch: {err}"
            ),
            Some(
                "No transaction effect was dispatched; resolve and lock the authoritative contract again before retrying."
                    .to_string(),
            ),
        )
    })
}

#[cfg(test)]
fn tx_run_test_wezterm_override_slot()
-> &'static std::sync::Mutex<Option<crate::wezterm::WeztermHandle>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<crate::wezterm::WeztermHandle>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
std::thread_local! {
    static TX_CONTRACT_POST_LOCK_TEST_HOOK: RefCell<Option<Box<dyn FnOnce()>>> =
        const { RefCell::new(None) };
    static TX_CONTRACT_POST_AUTH_TEST_HOOK: RefCell<Option<Box<dyn FnOnce()>>> =
        const { RefCell::new(None) };
    static TX_CONTRACT_WORKSPACE_TEST_ROOT: RefCell<Option<PathBuf>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
fn set_tx_contract_post_lock_test_hook(hook: Option<Box<dyn FnOnce()>>) {
    TX_CONTRACT_POST_LOCK_TEST_HOOK.with(|slot| {
        *slot.borrow_mut() = hook;
    });
}

#[cfg(test)]
fn run_tx_contract_post_lock_test_hook() {
    let hook = TX_CONTRACT_POST_LOCK_TEST_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
fn set_tx_contract_post_auth_test_hook(hook: Option<Box<dyn FnOnce()>>) {
    TX_CONTRACT_POST_AUTH_TEST_HOOK.with(|slot| {
        *slot.borrow_mut() = hook;
    });
}

#[cfg(test)]
fn run_tx_contract_post_auth_test_hook() {
    let hook = TX_CONTRACT_POST_AUTH_TEST_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

fn tx_mutation_workspace_layout(config: &Config) -> crate::Result<crate::config::WorkspaceLayout> {
    #[cfg(test)]
    if let Some(root) = TX_CONTRACT_WORKSPACE_TEST_ROOT.with(|slot| slot.borrow().as_ref().cloned())
    {
        return Ok(crate::config::WorkspaceLayout::new(
            root,
            &config.storage,
            &config.ipc,
        ));
    }
    config.workspace_layout(None)
}

#[cfg(test)]
fn set_tx_contract_workspace_test_root(root: Option<PathBuf>) {
    TX_CONTRACT_WORKSPACE_TEST_ROOT.with(|slot| {
        *slot.borrow_mut() = root;
    });
}

/// Pre-resolve pane capabilities for every pane a tx contract references.
///
/// The MCP tx surface has no live ingest registry, so prepare gates would
/// otherwise evaluate against `PaneCapabilities::unknown()` — which fails
/// `PromptActive` preconditions closed and forces the untrusted MCP actor
/// into approval on every `SendText` step even when prompt and alt-screen
/// evidence exists. This resolves the same evidence chain used by `wa.send`
/// and `wa.get_text`: OSC-133 prompt state from stored segments, alt-screen
/// and gap state from the watcher IPC, reservations from storage.
async fn resolve_tx_prepare_capabilities(
    config: &Config,
    storage: &StorageHandle,
    contract: &crate::plan::MissionTxContract,
) -> std::collections::HashMap<u64, PaneCapabilities> {
    let mut capabilities = std::collections::HashMap::new();
    for pane_id in contract.referenced_pane_ids() {
        let resolution = resolve_pane_capabilities(config, Some(storage), pane_id).await;
        capabilities.insert(pane_id, resolution.capabilities);
    }
    capabilities
}

fn tx_run_wezterm_handle(config: &Config) -> crate::wezterm::WeztermHandle {
    #[cfg(test)]
    if let Some(handle) = tx_run_test_wezterm_override_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return handle;
    }

    crate::wezterm::wezterm_handle_from_config(config)
}

fn mcp_tx_contract_save_failure_after_effects(
    save_err: McpToolError,
    completion_context: &str,
    retry_tool: &str,
    effect_label: &str,
) -> McpToolError {
    let mut hint = format!(
        "Do not retry {retry_tool} until inspecting the durable tx idempotency ledger and any transaction recovery artifact; retrying now is unsafe because {effect_label} may already exist."
    );
    if let Some(recovery_hint) = save_err.hint {
        hint.push(' ');
        hint.push_str(&recovery_hint);
    }

    McpToolError::new(
        save_err.code,
        format!(
            "{completion_context}, but updated transaction evidence could not be confirmed durably persisted: {}. {effect_label} may already exist.",
            save_err.message
        ),
        Some(hint),
    )
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
/// one is a larger refactor.
///
/// br-ft-6h1rv: RequireApproval is now an EXPLICIT FAIL-CLOSED dead end
/// for this surface. Pre-fix the hint advised "obtain an allow-once
/// approval token and retry via the approving client" — but no token
/// was issued from this path and no surface accepts one for these
/// tools, so the operator was sent on an impossible errand. The
/// audit row used `REASON_CODE_REQUIRE_APPROVAL`, which conflated
/// these dead-end rows with the approval-supported flow. Now: the
/// audit row uses `REASON_CODE_REQUIRE_APPROVAL_UNSUPPORTED` and
/// `DECISION_REQUIRE_APPROVAL_UNSUPPORTED` so operators querying
/// the audit table can distinguish the dead-end class, and the
/// hint names the limitation explicitly + lists the alternatives
/// (change the policy rule to Allow/Deny, or use an approval-aware
/// surface like `wa.workflow_run`). Wiring `attach_to_decision` so
/// these tools support the full flow is still tracked as a future
/// follow-up; this fix removes the misleading dead end.
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
        // br-ft-6h1rv: this surface does not wire ApprovalStore;
        // RequireApproval is a fail-closed dead end here. Surface
        // the limitation explicitly in BOTH the audit row (via the
        // _UNSUPPORTED decision/reason codes) AND the client hint
        // (which now names the alternatives instead of dispatching
        // the client to an impossible token-fetch).
        let policy_reason_text = policy_reason(&decision)
            .unwrap_or("Policy requires approval for this MCP mutation")
            .to_string();
        let reason = format!(
            "{policy_reason_text} — but the {summary} MCP surface does not support \
             allow-once approval (br-ft-6h1rv); fail-closed."
        );
        tracing::warn!(
            target: "ft::security::policy",
            tool = %summary,
            command = %command_text,
            decision = "require_approval_unsupported",
            rule_id = ?decision.rule_id(),
            reason = %reason,
            "MCP mutation policy returned RequireApproval but tool surface does not \
             support approval flow; fail-closed (br-ft-6h1rv)"
        );
        persist_mcp_policy_denial(
            config,
            summary,
            command_text,
            &reason,
            decision.rule_id(),
            crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL_UNSUPPORTED,
            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL_UNSUPPORTED,
        );
        let hint = Some(format!(
            "br-ft-6h1rv: the {summary} MCP tool does not support the allow-once \
             approval flow (no ApprovalStore::attach_to_decision plumbing on this \
             surface). Either change the policy rule for this tool to Allow or Deny \
             outright, OR drive the operation through an approval-aware surface \
             (e.g., wa.workflow_run with attached approval_token). Pre-fix this \
             path returned a misleading 'obtain an allow-once token and retry' hint."
        ));
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
        ts_ms: mcp_now_ms_i64(),
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
        ts_ms: mcp_now_ms_i64(),
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

/// ft-p8git: audit a denied MCP action with degraded-mode fidelity.
///
/// ALWAYS emits the primary `tracing::warn!` denial signal — including when
/// `storage` is `None` (degraded mode: `db_path` unset, no `StorageHandle`) —
/// then best-effort persists the `policy_denied_audit` row when storage is
/// available. The Option-storage deny sites previously persisted only inside
/// `if let Some(storage)`, with no tracing of their own, so a denial in
/// degraded mode produced NO record at all: no audit row AND no log line. The
/// log line is the floor of audit fidelity that survives a missing database.
async fn audit_mcp_policy_denial_async(
    storage: Option<&StorageHandle>,
    tool_name: &str,
    command_text: &str,
    reason: &str,
    rule_id: Option<&str>,
    decision: &str,
    reason_code: &str,
) {
    tracing::warn!(
        target: "ft::security::policy",
        tool = %tool_name,
        command = %command_text,
        decision = %decision,
        rule_id = ?rule_id,
        reason = %reason,
        reason_code = %reason_code,
        storage_attached = storage.is_some(),
        "MCP action denied by policy"
    );
    if let Some(storage) = storage {
        persist_mcp_policy_denial_async(
            storage,
            tool_name,
            command_text,
            reason,
            rule_id,
            decision,
            reason_code,
        )
        .await;
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
        .inspect_err(|e| {
            // br-ft-yygus: route through the cross-module counter
            // so MCP-built audit rows that lose decision_context
            // are visible alongside the policy.rs sites in the
            // same metric. Don't replace the tracing::warn; bump
            // is additive observability.
            crate::policy::record_policy_decision_context_serde_drop();
            tracing::warn!(error = %e, "mcp audit decision_context serialization failed");
        })
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

fn parse_mcp_attention_surface(surface: Option<&str>) -> Option<AttentionRouterSurface> {
    match surface
        .unwrap_or("status")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "status" => Some(AttentionRouterSurface::Status),
        "next" => Some(AttentionRouterSurface::Next),
        "explain" => Some(AttentionRouterSurface::Explain),
        _ => None,
    }
}

fn mcp_attention_source(surface: AttentionRouterSurface) -> &'static str {
    match surface {
        AttentionRouterSurface::Status => "mcp.wa_attention.status",
        AttentionRouterSurface::Next => "mcp.wa_attention.next",
        AttentionRouterSurface::Explain => "mcp.wa_attention.explain",
    }
}

fn parse_mcp_rehearsal_score_surface(surface: Option<&str>) -> Option<RehearsalScoreSurface> {
    match surface
        .unwrap_or("score")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "score" => Some(RehearsalScoreSurface::Score),
        "explain" => Some(RehearsalScoreSurface::Explain),
        _ => None,
    }
}

fn resolve_mcp_rehearsal_manifest_path(
    config: &Config,
    manifest_path: &str,
) -> std::result::Result<PathBuf, String> {
    let trimmed = manifest_path.trim();
    if trimmed.is_empty() {
        return Err("manifest_path must not be empty".to_string());
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err("manifest_path must be workspace-relative for MCP calls".to_string());
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        return Err("manifest_path must not traverse outside the workspace".to_string());
    }

    let layout = config
        .workspace_layout(None)
        .map_err(|error| format!("resolve workspace layout: {error}"))?;
    Ok(layout.root.join(path))
}

fn load_mcp_rehearsal_manifest(
    config: &Config,
    params: &RehearsalScoreParams,
) -> std::result::Result<(DemoScenarioManifest, String), String> {
    if let Some(manifest) = params.manifest.clone() {
        manifest
            .validate()
            .map_err(|error| format!("validate inline manifest: {error}"))?;
        return Ok((manifest, "inline_manifest".to_string()));
    }

    let path = resolve_mcp_rehearsal_manifest_path(config, &params.manifest_path)?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("read manifest {}: {error}", path.display()))?;
    let manifest = DemoScenarioManifest::from_json(&raw)
        .map_err(|error| format!("parse manifest {}: {error}", path.display()))?;
    Ok((manifest, params.manifest_path.clone()))
}

// wa.attention tool
pub(super) struct WaAttentionTool;

impl ToolHandler for WaAttentionTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.attention".to_string(),
            description: Some(
                "Read attention-router status, next action, or item explanation from caller-supplied evidence (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "surface": { "type": "string", "enum": ["status", "next", "explain"], "default": "status" },
                    "item_id": { "type": "string", "description": "Attention item id to explain when surface=explain" },
                    "input": { "type": "object", "description": "AttentionRouterSourceAdapterInput object; omitted input yields an explicit degraded no-input snapshot" },
                    "generated_at_ms": { "type": "integer", "minimum": 0 },
                    "workspace": { "type": "string", "description": "Workspace label used only when input is omitted or workspace override is desired" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "attention".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: AttentionParams = if arguments.is_null() {
            AttentionParams::default()
        } else {
            match parse_mcp_tool_params(
                "wa.attention",
                arguments,
                "Expected object with optional surface, item_id, input, generated_at_ms, workspace",
                start,
            ) {
                Ok(params) => params,
                Err(response) => return response,
            }
        };

        let Some(surface) = parse_mcp_attention_surface(params.surface.as_deref()) else {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Unsupported attention surface",
                Some("Use surface=status, surface=next, or surface=explain.".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        };

        let mut input = params.input.unwrap_or_else(|| {
            AttentionRouterSourceAdapterInput::new(
                params.generated_at_ms.unwrap_or_else(now_ms),
                params.workspace.clone().unwrap_or_else(|| ".".to_string()),
            )
        });
        if let Some(generated_at_ms) = params.generated_at_ms {
            input.generated_at_ms = generated_at_ms;
        }
        if let Some(workspace) = params
            .workspace
            .filter(|workspace| !workspace.trim().is_empty())
        {
            input.workspace = workspace;
        }

        let payload = build_attention_router_surface_payload(
            &input,
            surface,
            mcp_attention_source(surface),
            params.item_id.as_deref(),
        );
        envelope_to_content(McpEnvelope::success(payload, elapsed_ms(start)))
    }
}

// wa.rehearsal_score tool
pub(super) struct WaRehearsalScoreTool {
    config: Arc<Config>,
}

impl WaRehearsalScoreTool {
    #[must_use]
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaRehearsalScoreTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.rehearsal_score".to_string(),
            description: Some(
                "Score or explain a demo rehearsal manifest without mutating panes, services, Beads, or storage (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "surface": { "type": "string", "enum": ["score", "explain"], "default": "score" },
                    "manifest_path": { "type": "string", "default": "fixtures/demo-lab/manifest.v1.json", "description": "Workspace-relative demo scenario manifest path" },
                    "manifest": { "type": "object", "description": "Inline DemoScenarioManifest object; overrides manifest_path when supplied" },
                    "rehearsal_id": { "type": "string", "default": "rehearsal-demo-manifest" },
                    "scenario_id": { "type": "string", "default": "demo_lab.manifest" }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "rehearsal".to_string(),
                "demo".to_string(),
            ],
            annotations: Some(
                ToolAnnotations::new()
                    .read_only(true)
                    .destructive(false)
                    .idempotent(true),
            ),
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: RehearsalScoreParams = if arguments.is_null() {
            RehearsalScoreParams::default()
        } else {
            match parse_mcp_tool_params(
                "wa.rehearsal_score",
                arguments,
                "Expected object with optional surface, manifest_path, manifest, rehearsal_id, scenario_id",
                start,
            ) {
                Ok(params) => params,
                Err(response) => return response,
            }
        };

        let Some(surface) = parse_mcp_rehearsal_score_surface(params.surface.as_deref()) else {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Unsupported rehearsal score surface",
                Some("Use surface=score or surface=explain.".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        };

        let (manifest, source_ref) = match load_mcp_rehearsal_manifest(&self.config, &params) {
            Ok(loaded) => loaded,
            Err(error) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    error,
                    Some(
                        "Pass a valid inline manifest object or a workspace-relative manifest_path."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let rehearsal_id = params
            .rehearsal_id
            .unwrap_or_else(|| "rehearsal-demo-manifest".to_string());
        let scenario_id = params
            .scenario_id
            .unwrap_or_else(|| "demo_lab.manifest".to_string());
        let report = RehearsalScoreSurfaceReport::from_demo_scenario_manifest(
            &manifest,
            source_ref,
            rehearsal_id,
            scenario_id,
            surface,
        );
        debug_assert_eq!(report.contract_id, REHEARSAL_SCORE_SURFACE_CONTRACT_ID);

        envelope_to_content(McpEnvelope::success(report, elapsed_ms(start)))
    }
}

/// Params for `wa.steer_plan` (ft-7h5da.6.5). `scenario` is validated against
/// the steer-plan scenario set.
#[derive(Debug, serde::Deserialize)]
struct SteerPlanParams {
    scenario: String,
    objective: String,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    ttl_ms: Option<i64>,
}

/// `wa.steer_plan` — MCP mirror of `ft steer plan` (ft-7h5da.6.5). Read-only,
/// deterministic: returns the same `SteeringReceipt` the CLI emits for a
/// scenario (byte-equal by sharing `frankenterm_core::steer_plan::steer_plan`).
/// A base tool — no pane content, no policy gate, no DB (the receipt is pure;
/// the CLI's audit row is a CLI-side concern).
///
/// Carries `Arc<Config>` so the default `workspace_id` binds the SAME
/// resolved workspace root the CLI binds (`config.workspace_layout`):
/// `receipt_id` is content-addressed over `workspace_id`, so a
/// surface-specific default (the original hardcoded `"mcp"`) made the CLI
/// and MCP mirrors emit DIFFERENT receipt ids for the same logical call,
/// contradicting the byte-equal parity contract above.
pub(super) struct WaSteerPlanTool {
    config: Arc<Config>,
}

impl WaSteerPlanTool {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ToolHandler for WaSteerPlanTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.steer_plan".to_string(),
            description: Some(
                "Emit a deterministic steering receipt for a standard scenario \
                 (robot parity, read-only)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scenario": {
                        "type": "string",
                        "enum": ["clean-ready", "dirty-overlap", "rch-blocked", "approval-required", "capacity-red"],
                        "description": "Standard steer-plan scenario"
                    },
                    "objective": { "type": "string", "description": "Operator objective description" },
                    "workspace_id": { "type": "string", "description": "Workspace id to bind (default: resolved workspace root, matching the CLI)" },
                    "ttl_ms": { "type": "integer", "description": "Receipt TTL in milliseconds (default: none)" }
                },
                "required": ["scenario", "objective"],
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "robot".to_string(), "steer".to_string()],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: SteerPlanParams = match parse_mcp_tool_params(
            "wa.steer_plan",
            arguments,
            "Expected object with scenario + objective.",
            start,
        ) {
            Ok(p) => p,
            Err(response) => return response,
        };
        let scenario = match crate::steer_plan::SteerPlanScenario::parse(&params.scenario) {
            Ok(s) => s,
            Err(e) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    e,
                    Some(
                        "scenario must be one of: clean-ready, dirty-overlap, \
                         rch-blocked, approval-required, capacity-red"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        // Receipt parity with the CLI (`ft steer plan`): receipt_id is
        // content-addressed over workspace_id, so the default must be the
        // same resolved workspace root the CLI binds — never a
        // surface-specific constant.
        let workspace_id = match params.workspace_id {
            Some(id) => id,
            None => match self.config.workspace_layout(None) {
                Ok(layout) => layout.root.to_string_lossy().to_string(),
                Err(e) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_CONFIG,
                        format!("Failed to resolve workspace layout: {e}"),
                        Some("Pass an explicit workspace_id, or set FT_WORKSPACE.".to_string()),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            },
        };
        let now = now_ms();
        let result = crate::steer_plan::steer_plan(
            scenario,
            &params.objective,
            &workspace_id,
            now,
            i64::try_from(now).unwrap_or(i64::MAX),
            params.ttl_ms,
        );
        let envelope = McpEnvelope::success(result.receipt, elapsed_ms(start));
        envelope_to_content(envelope)
    }
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
                    "agent_type": { "type": "string", "maxLength": MAX_MCP_RULES_AGENT_TYPE_BYTES, "description": "Filter by agent type (codex, claude_code, gemini, wezterm)" },
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
            match parse_mcp_tool_params(
                "wa.rules_list",
                arguments,
                "Expected object with optional agent_type, verbose",
                start,
            ) {
                Ok(p) => p,
                Err(response) => return response,
            }
        };

        let agent_filter: Option<AgentType> = match params.agent_type.as_ref() {
            Some(s) => {
                if let Some(error) = validate_mcp_rules_agent_type_bytes("wa.rules_list", s, start)
                {
                    return error;
                }
                match parse_mcp_rules_agent_type(s) {
                    Some(agent_type) => Some(agent_type),
                    None => {
                        let envelope = McpEnvelope::<()>::error(
                            MCP_ERR_INVALID_ARGS,
                            format!("Unknown agent_type: {}", redact_mcp_output_secrets(s)),
                            Some("Valid types: codex, claude_code, gemini, wezterm".to_string()),
                            elapsed_ms(start),
                        );
                        return envelope_to_content(envelope);
                    }
                }
            }
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
                    "text": { "type": "string", "maxLength": MAX_MCP_RULES_TEST_TEXT_BYTES, "description": "Text to test pattern detection against" },
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

        let params: RulesTestParams = match parse_mcp_tool_params(
            "wa.rules_test",
            arguments,
            "Expected object with text (required), trace",
            start,
        ) {
            Ok(params) => params,
            Err(response) => return response,
        };

        if let Some(error) =
            validate_mcp_rules_test_text_bytes("wa.rules_test", &params.text, start)
        {
            return error;
        }

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
                    "query": { "type": "string", "maxLength": MAX_MCP_CASS_QUERY_BYTES, "description": "Search query string" },
                    "limit": { "type": "integer", "minimum": 0, "maximum": 1000, "default": 10, "description": "Maximum results (0 = cass default)" },
                    "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Offset into results" },
                    "agent": { "type": "string", "maxLength": MAX_MCP_CASS_AGENT_FILTER_BYTES, "description": "Agent filter: codex|claude_code|gemini|cursor|aider|chatgpt" },
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

        let params: CassSearchParams = match parse_mcp_tool_params(
            "wa.cass_search",
            arguments,
            "Expected object with query (required) and optional limit/offset/agent/workspace/days/fields/max_tokens/timeout_secs",
            start,
        ) {
            Ok(params) => params,
            Err(response) => return response,
        };

        if let Some(error) = validate_mcp_cass_query_bytes("wa.cass_search", &params.query, start) {
            return error;
        }

        if params.query.trim().is_empty() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "query cannot be empty".to_string(),
                Some("Provide a non-empty search query string".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        // [ft-tzwuw + ft-szuzd] Enforce schema's "timeout_secs":
        // { "minimum": 1, "maximum": 600 } bound. serde_json doesn't
        // honour JSON-Schema bounds, so without this guard a client
        // sending timeout_secs=0 would surface a confusing "cass timeout
        // (0 secs)" error on every call, and a client sending
        // timeout_secs: 3600 would block the mcp server on cass for up
        // to an hour. Routes through the shared validate_cass_timeout_secs
        // helper (ft-aylbh) shared with cass_view + cass_status so all
        // three CASS surfaces converge on the same bound.
        if let Some(error) =
            validate_cass_timeout_secs("wa.cass_search", params.timeout_secs, start)
        {
            return error;
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
            if let Some(error) =
                validate_mcp_cass_agent_filter_bytes("wa.cass_search", agent_str, start)
            {
                return error;
            }
            match parse_cass_agent(agent_str) {
                Some(agent) => Some(agent),
                None => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid agent: {}", redact_mcp_output_secrets(agent_str)),
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
            Ok(mut result) => {
                // Redact secrets in indexed-session content before returning,
                // mirroring every other MCP read tool (see
                // `redact_cass_search_result`).
                redact_cass_search_result(&mut result);
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

        // [ft-tzwuw + ft-aylbh] Enforce schema's "timeout_secs":
        // { "minimum": 1, "maximum": 600 } bound. The previous handler
        // only rejected zero (ft-tzwuw), leaving the upper bound
        // bypassable — a hostile/buggy client sending timeout_secs:
        // 3600 would block the mcp server on cass for up to an hour
        // despite the schema's 10-minute cap. Routes through the shared
        // validate_cass_timeout_secs helper so cass_view, cass_search,
        // and cass_status converge on the same range and error shape.
        if let Some(error) = validate_cass_timeout_secs("wa.cass_view", params.timeout_secs, start)
        {
            return error;
        }

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        // [ft-0uzlr] Arbitrary-file-read gate. `cass view` is a general file
        // reader, and wa.cass_view is a BASE tool with no policy gate, so a
        // prompt-injected agent could otherwise exfiltrate any path the
        // watcher UID can read (/etc/passwd, ~/.ssh/id_rsa, ~/.aws/credentials).
        // Constrain the readable set to cass's OWN index via `cass context`,
        // which resolves only indexed session paths. `Ok(None)` => the path is
        // not an indexed session (refuse, never read it); `Err` => the probe
        // itself failed (fail closed, surface the cass error). Only `Ok(Some)`
        // — a confirmed indexed session — proceeds to read.
        let result: std::result::Result<Option<CassViewResult>, CassError> =
            runtime.block_on(async {
                let client = cass_client_with_timeout(params.timeout_secs);
                let path = std::path::Path::new(&params.source_path);
                if !client.is_session_indexed(path).await? {
                    return Ok(None);
                }
                let options = CassViewOptions {
                    context_lines: Some(params.context_lines),
                };
                client
                    .query(path, params.line_number, &options)
                    .await
                    .map(Some)
            });

        match result {
            Ok(Some(mut result)) => {
                // Redact secrets in the match line + context before returning,
                // mirroring every other MCP read tool (see
                // `redact_cass_view_result`).
                redact_cass_view_result(&mut result);
                let envelope = McpEnvelope::success(result, elapsed_ms(start));
                envelope_to_content(envelope)
            }
            Ok(None) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    "source_path is not an indexed cass session".to_string(),
                    Some(
                        "wa.cass_view reads only files cass has indexed. Use wa.cass_search and \
                         pass back a source_path from its results."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
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

        // [ft-tzwuw + ft-aylbh] Enforce schema's "timeout_secs":
        // { "minimum": 1, "maximum": 600 } bound. The previous handler
        // only rejected zero (ft-tzwuw), leaving the upper bound
        // bypassable — a hostile/buggy client sending timeout_secs:
        // 3600 would block the mcp server on cass for up to an hour
        // despite the schema's 10-minute cap. Routes through the shared
        // validate_cass_timeout_secs helper so cass_status, cass_search,
        // and cass_view converge on the same range and error shape.
        if let Some(error) =
            validate_cass_timeout_secs("wa.cass_status", params.timeout_secs, start)
        {
            return error;
        }

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
    // GH#72: the parsed config is carried so pane listing goes through the
    // config-aware unified handle (vendored mux socket when configured)
    // instead of the CLI-only default handle.
    config: Arc<Config>,
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

/// Redact secret material in cass search hits before they leave the MCP
/// surface. Indexed cass sessions routinely contain pasted credentials and
/// `.env` dumps; every other read tool (`wa.get_text`/`wa.search`/`wa.state`)
/// redacts its content, so these must too — otherwise a prompt-injected caller
/// exfiltrates secrets via e.g. `wa.cass_search "sk- OR api_key OR AKIA"`. The
/// cass-index gate (ft-0uzlr) bounds *which* sessions are readable but says
/// nothing about *what their content contains*.
fn redact_cass_search_result(result: &mut CassSearchResult) {
    for hit in &mut result.hits {
        if let Some(content) = hit.content.as_mut() {
            *content = redact_mcp_output_secrets(content);
        }
        // ft-7lh4k: the typed `content` walk above misses the `#[serde(flatten)]
        // extra` passthrough map, which round-trips verbatim into the envelope.
        redact_cass_extra_map(&mut hit.extra);
    }
    redact_cass_extra_map(&mut result.extra);
}

/// Redact secret material in a cass view (match line + surrounding context)
/// before it leaves the MCP surface. See [`redact_cass_search_result`].
fn redact_cass_view_result(result: &mut CassViewResult) {
    if let Some(line) = result.match_line.as_mut() {
        if let Some(content) = line.content.as_mut() {
            *content = redact_mcp_output_secrets(content);
        }
        // ft-7lh4k: scrub the line's `#[serde(flatten)] extra` passthrough too.
        redact_cass_extra_map(&mut line.extra);
    }
    for lines in [
        result.context_before.as_mut(),
        result.context_after.as_mut(),
    ] {
        let Some(lines) = lines else { continue };
        for line in lines {
            if let Some(content) = line.content.as_mut() {
                *content = redact_mcp_output_secrets(content);
            }
            redact_cass_extra_map(&mut line.extra);
        }
    }
    redact_cass_extra_map(&mut result.extra);
}

/// Redact secret-shaped tokens from any text that will appear in an MCP
/// tool response — data fields *or* error messages. The MCP error path can
/// echo caller-supplied input: the `wait_for` pattern in particular is
/// reflected verbatim by `fancy_regex`/`regex` into compile-error strings
/// (e.g. `regex parse error:\n    sk-ant-…\n    ^`), so a secret embedded in
/// an invalid pattern would otherwise leak through the error envelope. See
/// ft-qde8p for the remote-error precedent.
///
/// This is the shared redaction chokepoint for MCP output: coverage is only as
/// complete as the call sites that route through it. Each tool must funnel its
/// operator-visible strings here before they leave the process — directly for
/// typed string fields, or via [`redact_json_value_in_place`] for structured
/// `#[serde(flatten)]` passthrough maps (ft-7lh4k). It is NOT an automatic
/// whole-response filter.
fn redact_mcp_output_secrets(text: &str) -> String {
    static REDACTOR: LazyLock<crate::redactor::Redactor> =
        LazyLock::new(crate::redactor::Redactor::new);
    REDACTOR.redact(text)
}

/// Recursively redact every JSON string in `value` in place, routing each
/// through [`redact_mcp_output_secrets`]. This scrubs the
/// `#[serde(flatten)] extra` passthrough maps on the cass DTOs (ft-7lh4k): the
/// cass redactors walk only the typed `content` fields, but `flatten`
/// round-trips any unmodeled key verbatim, so a secret-shaped string arriving in
/// (or nested under) an `extra` entry — e.g. a future `text`/`snippet`/`raw`
/// field, or a value nested in an object/array — would otherwise reach the
/// caller unredacted.
fn redact_json_value_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            let redacted = redact_mcp_output_secrets(text);
            if redacted != *text {
                *text = redacted;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                redact_json_value_in_place(item);
            }
        }
        serde_json::Value::Object(map) => {
            for nested in map.values_mut() {
                redact_json_value_in_place(nested);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Redact every string in a `#[serde(flatten)] extra` passthrough map.
fn redact_cass_extra_map(extra: &mut std::collections::HashMap<String, serde_json::Value>) {
    for value in extra.values_mut() {
        redact_json_value_in_place(value);
    }
}

fn redact_mcp_wait_pattern_for_output(pattern: &str) -> String {
    redact_mcp_output_secrets(pattern)
}

impl WaStateTool {
    pub(super) fn new(
        config: Arc<Config>,
        filter: PaneFilterConfig,
        db_path: Option<Arc<PathBuf>>,
    ) -> Self {
        Self {
            config,
            filter,
            db_path,
        }
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
                    "agent": { "type": "string", "maxLength": MAX_MCP_STATE_AGENT_FILTER_BYTES },
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
            match parse_mcp_tool_params(
                "wa.state",
                arguments,
                "Expected object with optional domain/agent/pane_id",
                start,
            ) {
                Ok(params) => params,
                Err(response) => return response,
            }
        };

        if let Some(agent) = params.agent.as_deref() {
            if let Some(error) = validate_mcp_state_agent_filter_bytes("wa.state", agent, start) {
                return error;
            }
        }

        let db_path = self.db_path.as_ref().map(Arc::clone);
        let config = Arc::clone(&self.config);

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result = runtime.block_on(async {
            // GH#72: honor any configured vendored mux socket.
            let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
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
    #[cfg(test)]
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

        let params: GetTextParams = match parse_mcp_tool_params(
            "wa.get_text",
            arguments,
            "Expected object matching wa.get_text input schema.",
            start,
        ) {
            Ok(params) => params,
            Err(response) => return response,
        };

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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
                // GH#72: honor any configured vendored mux socket.
                let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
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
                    audit_mcp_policy_denial_async(
                        storage.as_ref(),
                        "wa.get_text",
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

/// Params for `wa.dom` — the semantic pane API MCP mirror (ft-7h5da.2.6).
/// `query` deserializes directly into `DomQueryKind` (snake_case).
#[derive(Debug, serde::Deserialize)]
struct DomParams {
    pane_id: u64,
    query: crate::robot_types::DomQueryKind,
    #[serde(default)]
    command_index: Option<i64>,
}

/// `wa.dom` — MCP mirror of `ft robot dom` (the semantic pane API). Returns a
/// flat list of OSC 133 zones, NOT a DOM tree. Policy-gated read, byte-equal to
/// the robot CLI envelope by sharing `robot_dom::build_dom_data`. See
/// docs/robot-contracts/semantic-pane-api.md.
pub(super) struct WaDomTool {
    config: Arc<Config>,
    db_path: Option<Arc<PathBuf>>,
    policy_rate_limiter: SharedRateLimiter,
}

impl WaDomTool {
    #[cfg(test)]
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

impl ToolHandler for WaDomTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.dom".to_string(),
            description: Some(
                "Semantic pane API: flat OSC 133 zones for a pane (robot parity). \
                 Returns a flat list, NOT a DOM tree."
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": { "type": "integer", "minimum": 0, "description": "The pane ID to query" },
                    "query": {
                        "type": "string",
                        "enum": ["zones", "last_command", "output_of", "exit_code"],
                        "description": "Semantic-pane verb: zones (flat list), last_command, output_of, or exit_code"
                    },
                    "command_index": {
                        "type": "integer",
                        "description": "Command index for output_of/exit_code; -1 (default) selects the most recent"
                    }
                },
                "required": ["pane_id", "query"],
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

        let params: DomParams = match parse_mcp_tool_params(
            "wa.dom",
            arguments,
            "Expected object matching wa.dom input schema.",
            start,
        ) {
            Ok(params) => params,
            Err(response) => return response,
        };

        let config = Arc::clone(&self.config);
        let db_path = self.db_path.as_ref().map(Arc::clone);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result: std::result::Result<crate::robot_types::DomData, McpToolError> = runtime
            .block_on(async move {
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

                let redactor = crate::redactor::Redactor::new();
                // GH#72: honor any configured vendored mux socket.
                let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
                let wezterm_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);

                // Semantic zones require a live local pane; no remote-pane path.
                // A missing pane is an honest "unavailable" observation, not an
                // error (there is no content to gate).
                let pane_info = match wezterm.get_pane_with_cx(&wezterm_cx, params.pane_id).await {
                    Ok(pane_info) => pane_info,
                    Err(_) => {
                        return Ok(crate::robot_dom::dom_unavailable(
                            params.pane_id,
                            params.query,
                            params.command_index,
                            Vec::new(),
                            "semantic data unavailable: pane not found or not a live local pane",
                            &redactor,
                        ));
                    }
                };
                let domain = pane_info.inferred_domain();

                let resolution =
                    resolve_pane_capabilities(&config, storage.as_ref(), params.pane_id).await;
                let capabilities = resolution.capabilities;

                let mut engine = build_policy_engine_with_shared_rate_limiter(
                    &config,
                    false,
                    Arc::clone(&policy_rate_limiter),
                );
                let summary = format!("wa.dom pane_id={}", params.pane_id);
                let mut input = mcp_get_text_policy_input(
                    params.pane_id,
                    domain.clone(),
                    capabilities,
                    &summary,
                );
                if let Some(title) = &pane_info.title {
                    input = input.with_pane_title(title.clone());
                }
                if let Some(cwd) = &pane_info.cwd {
                    input = input.with_pane_cwd(cwd.clone());
                }

                let decision = engine.authorize(&input);
                if decision.is_denied() {
                    let reason = policy_reason(&decision)
                        .unwrap_or("Read denied by policy")
                        .to_string();
                    audit_mcp_policy_denial_async(
                        storage.as_ref(),
                        "wa.dom",
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
                        persist_mcp_policy_denial_async(
                            storage,
                            "wa.dom",
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

                // Fetch live OSC 133 zones and build the byte-equal envelope via
                // the same core builder the robot CLI uses.
                let snapshot = match wezterm
                    .get_semantic_zones_with_cx(&wezterm_cx, params.pane_id)
                    .await
                {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        return Ok(crate::robot_dom::dom_unavailable(
                            params.pane_id,
                            params.query,
                            params.command_index,
                            Vec::new(),
                            format!("semantic data unavailable: {err}"),
                            &redactor,
                        ));
                    }
                };
                Ok(crate::robot_dom::build_dom_data(
                    params.pane_id,
                    params.query,
                    params.command_index,
                    &snapshot,
                    &redactor,
                ))
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
    #[cfg(test)]
    pub(super) fn new(config: Arc<Config>, db_path: Option<Arc<PathBuf>>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        Self::new_with_shared_rate_limiter(config, db_path, policy_rate_limiter)
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Option<Arc<PathBuf>>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        // GH#72: honor any configured vendored mux socket.
        let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            wezterm,
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
                    "pattern": { "type": "string", "maxLength": MAX_MCP_WAIT_PATTERN_BYTES, "description": "Pattern to match (substring or regex)" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600, "default": 30, "description": "Timeout in seconds" },
                    "tail": { "type": "integer", "minimum": 1, "maximum": 10000, "default": 200, "description": "Tail lines to search. Server enforces 1..=10000 (ft-ymo2i); for full-buffer scans use wa.search instead." },
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

        let params: WaitForParams = match parse_mcp_tool_params(
            "wa.wait_for",
            arguments,
            "Expected object matching wa.wait_for input schema.",
            start,
        ) {
            Ok(params) => params,
            Err(response) => return response,
        };

        // Enforce the advertised timeout range server-side. serde accepts any
        // u64, and some MCP clients do not validate against the tool schema.
        if let Some(error) =
            validate_mcp_wait_timeout_secs("wa.wait_for", params.timeout_secs, start)
        {
            return error;
        }
        if let Some(error) =
            validate_mcp_wait_pattern_bytes("wa.wait_for", "pattern", &params.pattern, start)
        {
            return error;
        }

        // ft-ymo2i: enforce a server-side bound on `tail`. Round-2 security
        // audit (docs/review/round-2-security-audit.md) found wa.wait_for's
        // tail field accepted any usize and the previous schema declared
        // `minimum: 0` with the explicit semantics "0 = full buffer" — a
        // memory-pressure vector if the buffer is large. Same class as
        // ft-ii8ss (wa.get_text). Same LIMIT_MIN/LIMIT_MAX pattern.
        // Callers wanting full-buffer scans should use wa.search.
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
                    "The wa.wait_for tool schema declares tail ∈ [{TAIL_MIN}, {TAIL_MAX}]; \
                     clamp your request, omit the field to use the default (200), or use \
                     wa.search for full-buffer scans."
                )),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let matcher = match crate::wezterm::compile_wait_matcher(&params.pattern, params.regex) {
            Ok(matcher) => matcher,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    redact_mcp_output_secrets(&format!("Invalid regex pattern: {err}")),
                    Some("Check the regex syntax".to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
                audit_mcp_policy_denial_async(
                    storage.as_ref(),
                    "wa.wait_for",
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

        let redacted_pattern = redact_mcp_wait_pattern_for_output(&pattern);
        match result {
            Ok(WaitResult::Matched {
                elapsed_ms: wait_elapsed_ms,
                polls,
            }) => {
                let data = McpWaitForData {
                    pane_id,
                    pattern: redacted_pattern.clone(),
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
                        "Timeout waiting for pattern '{redacted_pattern}' after {wait_elapsed_ms}ms ({polls} polls)"
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
    #[cfg(test)]
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
                    "query": { "type": "string", "maxLength": MAX_MCP_SEARCH_QUERY_BYTES, "description": "FTS5 search query" },
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

        let params: SearchParams = match parse_mcp_tool_params(
            "wa.search",
            arguments,
            "Expected object matching wa.search input schema.",
            start,
        ) {
            Ok(params) => params,
            Err(response) => return response,
        };

        if let Some(error) = validate_mcp_search_query_bytes("wa.search", &params.query, start) {
            return error;
        }

        let parsed = match parse_unified_search_query(
            SearchQueryInput {
                query: params.query,
                limit: params.limit,
                pane: params.pane,
                zone: None,
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
                    // br-ft-9ia4p: mirror WaGetTextTool's distributed-pane
                    // fallback. Distributed remote panes are persisted-only
                    // by design — a live-mux `get_pane(pane_id)` lookup
                    // fails for them with no live WezTerm context. The
                    // user-facing recovery hint from wa.get_text points at
                    // wa.search/ft search, so this path must succeed
                    // against the storage record when live lookup fails.
                    let remote_pane = load_distributed_remote_pane(Some(&storage), pane_id)
                        .await
                        .map_err(McpToolError::from_error)?;
                    // GH#72: honor any configured vendored mux socket.
                    let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
                    let pane_info = match wezterm.get_pane(pane_id).await {
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
                        .map(|p| p.inferred_domain())
                        .or_else(|| remote_pane.as_ref().map(|r| r.domain.clone()))
                        .ok_or_else(|| {
                            McpToolError::new(
                                MCP_ERR_PANE_NOT_FOUND,
                                format!("Pane {pane_id} not found"),
                                Some("Use wa.state to list available panes.".to_string()),
                            )
                        })?;
                    let resolution =
                        resolve_pane_capabilities(&config, Some(&storage), pane_id).await;
                    input = input
                        .with_pane(pane_id)
                        .with_domain(domain)
                        .with_capabilities(resolution.capabilities);
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

const MCP_AWAIT_EVENT_TIMEOUT_SECS_MIN: u64 = 1;
pub(crate) const MCP_AWAIT_EVENT_TIMEOUT_SECS_MAX: u64 = 300;
const MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MIN: u64 = 10;
const MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MAX: u64 = 30_000;
const MCP_AWAIT_EVENT_CONDITION_SET_MAX: usize = 16;
const MCP_AWAIT_EVENT_BATCH_LIMIT: usize = 500;
const MCP_AWAIT_EVENT_BLOCKED_EVENT_MAX: usize = MCP_AWAIT_EVENT_BATCH_LIMIT;
pub(crate) const MCP_AWAIT_EVENT_DELIVERY_FINALIZE_GRACE_SECS: u64 = 30;
const MCP_AWAIT_EVENT_CANCEL_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(50);
const MCP_AWAIT_EVENT_CLAIM_WORKFLOW_ID: &str = "mcp.wa.await_event";
const MCP_AWAIT_EVENT_CLAIM_STATUS: &str = "claimed";

fn mcp_request_checkpoint(cx: &crate::cx::Cx, operation: &str) -> crate::Result<()> {
    cx.checkpoint().map_err(|error| {
        crate::Error::Cancelled(format!(
            "{operation} request cancelled or budget exhausted: {error}"
        ))
    })?;
    Ok(())
}

fn mcp_await_event_checkpoint(cx: &crate::cx::Cx) -> crate::Result<()> {
    mcp_request_checkpoint(cx, "wa.await_event")
}

async fn mcp_await_event_wait_with_cx(
    cx: &crate::cx::Cx,
    wait: std::time::Duration,
) -> crate::Result<()> {
    let started = Instant::now();
    loop {
        mcp_await_event_checkpoint(cx)?;
        let remaining = wait.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        let step = remaining.min(MCP_AWAIT_EVENT_CANCEL_POLL_INTERVAL);
        crate::runtime_async::sleep_with_cx(cx, step)
            .await
            .map_err(|error| {
                crate::Error::Cancelled(format!(
                    "wa.await_event poll/retry delay cancelled or budget exhausted: {error}"
                ))
            })?;
    }
}

fn complete_mcp_await_event_deliveries(
    db_path: Arc<PathBuf>,
    leases: Vec<EventDeliveryLease>,
    outcome: FrameworkResponseDeliveryOutcome,
) {
    if leases.is_empty() {
        return;
    }

    let runtime = match CompatRuntimeBuilder::current_thread().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(
                error = %err,
                delivery_count = leases.len(),
                ?outcome,
                "Unable to build runtime for MCP event-delivery completion; leases will recover by expiry"
            );
            return;
        }
    };

    let delivery_count = leases.len();
    let completion = runtime.block_on(async move {
        let cx = crate::cx::for_request();
        let operation_cx = cx.clone();
        crate::runtime_async::timeout_with_cx(
            &cx,
            std::time::Duration::from_secs(MCP_AWAIT_EVENT_DELIVERY_FINALIZE_GRACE_SECS),
            async move {
                let storage = match StorageHandle::new_with_cx(
                    &operation_cx,
                    &db_path.to_string_lossy(),
                )
                .await
                {
                    Ok(storage) => storage,
                    Err(err) => {
                        tracing::error!(
                            error = %err,
                            delivery_count = leases.len(),
                            ?outcome,
                            "Unable to open storage for MCP event-delivery completion; leases will recover by expiry"
                        );
                        return;
                    }
                };

                for lease in &leases {
                    let completion = match outcome {
                        FrameworkResponseDeliveryOutcome::DeliveryAcknowledged => {
                            storage
                                .finalize_event_delivery_with_cx(
                                    &operation_cx,
                                    lease,
                                    Some(MCP_AWAIT_EVENT_CLAIM_WORKFLOW_ID.to_string()),
                                    MCP_AWAIT_EVENT_CLAIM_STATUS,
                                )
                                .await
                        }
                        FrameworkResponseDeliveryOutcome::Failed => {
                            storage
                                .release_event_delivery_with_cx(&operation_cx, lease)
                                .await
                        }
                    };

                    match completion {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::warn!(
                                event_id = lease.event_id(),
                                ?outcome,
                                "MCP event-delivery completion lost lease ownership; no handled-state mutation was made"
                            );
                        }
                        Err(err) => {
                            tracing::error!(
                                error = %err,
                                event_id = lease.event_id(),
                                ?outcome,
                                "MCP event-delivery completion failed; lease expiry remains the recovery authority"
                            );
                        }
                    }
                }

                if let Err(err) = storage.shutdown().await {
                    tracing::warn!(
                        error = %err,
                        ?outcome,
                        "MCP event-delivery completion storage shutdown failed"
                    );
                }
            },
        )
        .await
    });

    if let Err(err) = completion {
        tracing::error!(
            error = %err,
            delivery_count,
            finalize_grace_secs = MCP_AWAIT_EVENT_DELIVERY_FINALIZE_GRACE_SECS,
            ?outcome,
            "MCP event-delivery completion exceeded its hard deadline; lease expiry remains the recovery authority"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpAwaitEventCondition {
    Rule(String),
}

fn parse_mcp_await_event_condition(
    spec: &str,
) -> std::result::Result<McpAwaitEventCondition, String> {
    let spec = spec.trim();
    if let Some(glob) = spec.strip_prefix("rule:") {
        if glob.is_empty() {
            return Err(format!("empty rule glob in condition `{spec}`"));
        }
        Ok(McpAwaitEventCondition::Rule(glob.to_string()))
    } else if spec.starts_with("state:") || spec.starts_with("quiescence:") {
        Err(format!(
            "condition `{spec}` requires live watcher state; wa.await_event currently supports \
             DB-backed event conditions only (`rule:<glob>`)"
        ))
    } else {
        Err(format!(
            "unrecognized condition `{spec}`; expected `rule:<glob>`"
        ))
    }
}

fn mcp_await_event_condition_matches(
    condition: &McpAwaitEventCondition,
    event: &crate::storage::StoredEvent,
) -> bool {
    match condition {
        McpAwaitEventCondition::Rule(glob) => {
            crate::events::rule_glob_matches(glob, &event.rule_id)
        }
    }
}

fn mcp_await_event_unmet_match_mask(
    conditions: &[McpAwaitEventCondition],
    met: &[bool],
    event: &crate::storage::StoredEvent,
) -> u16 {
    debug_assert_eq!(conditions.len(), met.len());
    debug_assert!(conditions.len() <= MCP_AWAIT_EVENT_CONDITION_SET_MAX);
    conditions
        .iter()
        .zip(met)
        .enumerate()
        .fold(0_u16, |mask, (index, (condition, met))| {
            if !*met && mcp_await_event_condition_matches(condition, event) {
                mask | (1_u16 << index)
            } else {
                mask
            }
        })
}

fn mcp_await_event_apply_match_mask(met: &mut [bool], mask: u16) {
    for (index, value) in met.iter_mut().enumerate() {
        if mask & (1_u16 << index) != 0 {
            *value = true;
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct McpAwaitBlockedEvent {
    event_id: i64,
    expires_at_ms: i64,
    matching_any: u16,
    matching_all: u16,
}

fn mcp_await_event_safe_cursor(
    scan_after_id: Option<i64>,
    blocked_events: &[McpAwaitBlockedEvent],
) -> Option<i64> {
    blocked_events
        .iter()
        .map(|blocked| blocked.event_id)
        .min()
        .map_or(scan_after_id, |first_blocked_id| {
            Some(first_blocked_id.saturating_sub(1))
        })
}

async fn mcp_await_event_refetch_exact_unhandled_event(
    storage: &StorageHandle,
    cx: &crate::cx::Cx,
    event_id: i64,
    pane_id: Option<u64>,
) -> crate::Result<Option<crate::storage::StoredEvent>> {
    let events = storage
        .get_events_stream_with_cx(
            cx,
            EventStreamQuery {
                after_id: Some(event_id.saturating_sub(1)),
                limit: Some(1),
                pane_id,
                rule_id: None,
                event_type: None,
                triage_state: None,
                label: None,
                unhandled_only: true,
                since: None,
                until: None,
            },
        )
        .await?;
    Ok(events.into_iter().next().filter(|event| event.id == event_id))
}

fn mcp_await_event_blocked_capacity_error() -> McpToolError {
    McpToolError::new(
        MCP_ERR_STORAGE,
        format!(
            "wa.await_event cannot safely track more than {MCP_AWAIT_EVENT_BLOCKED_EVENT_MAX} concurrently leased matching events"
        ),
        Some(
            "Retry from the same cursor after competing event-delivery claims complete."
                .to_string(),
        ),
    )
}

fn mcp_await_event_is_satisfied(any_met: &[bool], all_met: &[bool]) -> bool {
    let all_ok = all_met.iter().all(|met| *met);
    let any_ok = any_met.is_empty() || any_met.iter().any(|met| *met);
    all_ok && any_ok
}

fn mcp_await_condition_status(specs: &[String], met: &[bool]) -> Vec<McpAwaitConditionStatus> {
    specs
        .iter()
        .cloned()
        .zip(met.iter().copied())
        .map(|(condition, met)| McpAwaitConditionStatus { condition, met })
        .collect()
}

async fn mcp_event_item_from_stored_event(
    storage: &StorageHandle,
    cx: &crate::cx::Cx,
    event: crate::storage::StoredEvent,
    redactor: &crate::redactor::Redactor,
) -> McpEventItem {
    let event = crate::export::redact_event(event, redactor);
    let pack_id = event.rule_id.split('.').next().map_or_else(
        || "builtin:unknown".to_string(),
        |agent| format!("builtin:{agent}"),
    );

    let annotations = match storage.get_event_annotations_with_cx(cx, event.id).await {
        Ok(Some(a)) => Some(a),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                error = %err,
                event_id = event.id,
                "Failed to load event annotations"
            );
            None
        }
    };

    McpEventItem {
        id: event.id,
        pane_id: event.pane_id,
        rule_id: event.rule_id,
        pack_id,
        event_type: event.event_type,
        severity: event.severity,
        confidence: event.confidence,
        extracted: event.extracted,
        annotations,
        captured_at: event.detected_at,
        handled_at: event.handled_at,
        workflow_id: event.handled_by_workflow_id,
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

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();

        let params: EventsParams = if arguments.is_null() {
            EventsParams::default()
        } else {
            match parse_mcp_tool_params(
                "wa.events",
                arguments,
                "Expected object with optional limit, pane, rule_id, event_type, triage_state, label, unhandled, since",
                start,
            ) {
                Ok(p) => p,
                Err(response) => return response,
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
        let request_cx = ctx.cx().clone();
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result: crate::Result<McpEventsData> = runtime.block_on(async {
            mcp_request_checkpoint(&request_cx, "wa.events")?;
            let storage_result =
                StorageHandle::new_with_cx(&request_cx, &db_path.to_string_lossy()).await;
            if let Err(cancellation) = mcp_request_checkpoint(&request_cx, "wa.events") {
                if let Ok(storage) = storage_result {
                    if let Err(shutdown_error) = storage.shutdown().await {
                        tracing::warn!(
                            error = %shutdown_error,
                            "wa.events cancelled after storage open; shutdown failed"
                        );
                    }
                }
                return Err(cancellation);
            }
            let storage = storage_result?;

            let operation: crate::Result<McpEventsData> = async {
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

                let events_result = storage.get_events_with_cx(&request_cx, query).await;
                mcp_request_checkpoint(&request_cx, "wa.events")?;
                let events = events_result?;
                let total_count = events.len();

                let redactor = crate::redactor::Redactor::new();
                let mut items: Vec<McpEventItem> = Vec::with_capacity(events.len());
                for event in events {
                    let item = mcp_event_item_from_stored_event(
                        &storage,
                        &request_cx,
                        event,
                        &redactor,
                    )
                    .await;
                    mcp_request_checkpoint(&request_cx, "wa.events")?;
                    items.push(item);
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
            }
            .await;

            if let Err(error) = storage.shutdown().await {
                tracing::warn!(error = %error, "wa.events storage shutdown failed");
            }
            operation
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

pub(super) struct WaAwaitEventTool {
    db_path: Arc<PathBuf>,
    response_delivery: Option<Arc<FrameworkResponseDeliveryCoordinator>>,
}

impl WaAwaitEventTool {
    #[cfg(test)]
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self {
            db_path,
            response_delivery: None,
        }
    }

    pub(super) fn new_with_response_delivery(
        db_path: Arc<PathBuf>,
        response_delivery: Arc<FrameworkResponseDeliveryCoordinator>,
    ) -> Self {
        Self {
            db_path,
            response_delivery: Some(response_delivery),
        }
    }
}

impl ToolHandler for WaAwaitEventTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.await_event".to_string(),
            description: Some(
                "Long-poll for pattern events with ft robot await envelope parity".to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "any": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1, "maxLength": 256 },
                        "default": [],
                        "maxItems": MCP_AWAIT_EVENT_CONDITION_SET_MAX,
                        "description": "At least one condition in this set must match; supported condition: rule:<glob>"
                    },
                    "all": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1, "maxLength": 256 },
                        "default": [],
                        "maxItems": MCP_AWAIT_EVENT_CONDITION_SET_MAX,
                        "description": "Every condition in this set must match; supported condition: rule:<glob>"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": MCP_AWAIT_EVENT_TIMEOUT_SECS_MIN,
                        "maximum": MCP_AWAIT_EVENT_TIMEOUT_SECS_MAX,
                        "default": 30,
                        "description": "Maximum long-poll duration in seconds"
                    },
                    "poll_interval_ms": {
                        "type": "integer",
                        "minimum": MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MIN,
                        "maximum": MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MAX,
                        "default": 250,
                        "description": "DB cursor poll interval while awaiting events"
                    },
                    "cursor": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Resume cursor; only events with id greater than this are considered"
                    },
                    "pane": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Filter by pane ID"
                    },
                    "unhandled": {
                        "type": "boolean",
                        "default": false,
                        "description": "Only consider unhandled events"
                    },
                    "claim": {
                        "type": "boolean",
                        "default": false,
                        "description": "Atomically lease matched emitted events; finalize handled only after successful requested-format response delivery, and release leases on known delivery failure"
                    }
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

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        // Capture the no-cursor lower bound at request entry. Storage opening
        // may block on SQLite initialization or another writer; taking this
        // timestamp after open would permanently exclude events detected in
        // that interval.
        let request_boundary_ms = mcp_now_ms_i64();

        let params: AwaitEventParams = if arguments.is_null() {
            AwaitEventParams::default()
        } else {
            match parse_mcp_tool_params(
                "wa.await_event",
                arguments,
                "Expected object with any/all rule:<glob> conditions plus optional timeout_secs, poll_interval_ms, cursor, pane, unhandled, claim",
                start,
            ) {
                Ok(p) => p,
                Err(response) => return response,
            }
        };

        if params.any.is_empty() && params.all.is_empty() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "wa.await_event requires at least one any/all condition".to_string(),
                Some("Example: {\"all\":[\"rule:codex.*\"],\"timeout_secs\":30}".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }
        if params.any.len() > MCP_AWAIT_EVENT_CONDITION_SET_MAX
            || params.all.len() > MCP_AWAIT_EVENT_CONDITION_SET_MAX
        {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!(
                    "any and all may each contain at most {MCP_AWAIT_EVENT_CONDITION_SET_MAX} conditions (got any={}, all={})",
                    params.any.len(),
                    params.all.len()
                ),
                Some(
                    "Split larger condition sets across multiple wa.await_event requests."
                        .to_string(),
                ),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }
        if params.timeout_secs < MCP_AWAIT_EVENT_TIMEOUT_SECS_MIN
            || params.timeout_secs > MCP_AWAIT_EVENT_TIMEOUT_SECS_MAX
        {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!(
                    "timeout_secs must be in {MCP_AWAIT_EVENT_TIMEOUT_SECS_MIN}..={MCP_AWAIT_EVENT_TIMEOUT_SECS_MAX} (got {})",
                    params.timeout_secs
                ),
                None,
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }
        if params.poll_interval_ms < MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MIN
            || params.poll_interval_ms > MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MAX
        {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                format!(
                    "poll_interval_ms must be in {MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MIN}..={MCP_AWAIT_EVENT_POLL_INTERVAL_MS_MAX} (got {})",
                    params.poll_interval_ms
                ),
                None,
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }
        if params.cursor.is_some_and(|cursor| cursor < 0) {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "cursor must be non-negative".to_string(),
                None,
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }
        if params.claim && self.response_delivery.is_none() {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_CONFIG,
                "wa.await_event claim requires an acknowledgment-aware MCP response transport"
                    .to_string(),
                Some(
                    "Run wa.await_event through the FrankenTerm MCP server transport; direct handler dispatch cannot safely claim events"
                        .to_string(),
                ),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let any_conditions: Vec<McpAwaitEventCondition> = match params
            .any
            .iter()
            .map(|spec| parse_mcp_await_event_condition(spec))
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(conditions) => conditions,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(MCP_ERR_INVALID_ARGS, err, None, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let all_conditions: Vec<McpAwaitEventCondition> = match params
            .all
            .iter()
            .map(|spec| parse_mcp_await_event_condition(spec))
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            Ok(conditions) => conditions,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(MCP_ERR_INVALID_ARGS, err, None, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let db_path = Arc::clone(&self.db_path);
        let request_cx = ctx.cx().clone();
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let (result, delivery_leases): (
            std::result::Result<McpAwaitEventData, McpToolError>,
            Vec<EventDeliveryLease>,
        ) = runtime.block_on(async {
            let mut delivery_leases = Vec::new();
            let cx = request_cx;
            if let Err(error) = mcp_await_event_checkpoint(&cx) {
                return (Err(McpToolError::from_error(error)), delivery_leases);
            }
            let storage_result =
                StorageHandle::new_with_cx(&cx, &db_path.to_string_lossy()).await;
            if let Err(error) = mcp_await_event_checkpoint(&cx) {
                if let Ok(storage) = storage_result {
                    if let Err(shutdown_error) = storage.shutdown().await {
                        tracing::warn!(
                            error = %shutdown_error,
                            "wa.await_event cancelled after storage open; shutdown failed"
                        );
                    }
                }
                return (Err(McpToolError::from_error(error)), delivery_leases);
            }
            let storage = match storage_result {
                Ok(storage) => storage,
                Err(err) => return (Err(McpToolError::from_error(err)), delivery_leases),
            };
            let redactor = crate::redactor::Redactor::new();
            let started = Instant::now();
            let timeout = std::time::Duration::from_secs(params.timeout_secs);
            let poll = std::time::Duration::from_millis(params.poll_interval_ms);
            // `scan_after_id` is a monotonic query high-watermark. Temporarily
            // leased matching rows are retained separately and retried by ID,
            // so one live hole neither head-of-line blocks later claimable
            // matches nor forces the full scanned tail through SQLite again on
            // every poll.
            let mut scan_after_id = params.cursor;
            let mut final_cursor = params.cursor;
            let mut any_met = vec![false; any_conditions.len()];
            let mut all_met = vec![false; all_conditions.len()];
            let mut matched_events = Vec::new();
            let mut blocked_events =
                Vec::<McpAwaitBlockedEvent>::with_capacity(MCP_AWAIT_EVENT_BLOCKED_EVENT_MAX);
            let mut blocked_retry_scratch =
                Vec::<McpAwaitBlockedEvent>::with_capacity(MCP_AWAIT_EVENT_BLOCKED_EVENT_MAX);
            let mut retry_blocked_events = true;

            let operation: std::result::Result<McpAwaitEventData, McpToolError> = async {
                let (satisfied, timed_out) = loop {
                    mcp_await_event_checkpoint(&cx).map_err(McpToolError::from_error)?;
                    if started.elapsed() >= timeout {
                        break (false, true);
                    }

                    if params.claim && retry_blocked_events {
                        retry_blocked_events = false;
                        debug_assert!(blocked_retry_scratch.is_empty());
                        std::mem::swap(&mut blocked_events, &mut blocked_retry_scratch);
                        for blocked in blocked_retry_scratch.drain(..) {
                            // A cursor hole is independent of whether another
                            // event has since satisfied the same condition. An
                            // exact durable refetch proves handled/deleted state
                            // without accidentally substituting the next row.
                            let event_result = mcp_await_event_refetch_exact_unhandled_event(
                                &storage,
                                &cx,
                                blocked.event_id,
                                params.pane,
                            )
                            .await;
                            mcp_await_event_checkpoint(&cx)
                                .map_err(McpToolError::from_error)?;
                            let Some(event) = event_result.map_err(McpToolError::from_error)? else {
                                continue;
                            };

                            let remaining = timeout.saturating_sub(started.elapsed());
                            let lease_ttl = remaining.saturating_add(
                                std::time::Duration::from_secs(
                                    MCP_AWAIT_EVENT_DELIVERY_FINALIZE_GRACE_SECS,
                                ),
                            );
                            let reservation_result = storage
                                .reserve_event_delivery_with_cx(&cx, blocked.event_id, lease_ttl)
                                .await;
                            let acquired_event = match reservation_result {
                                Ok(EventDeliveryReservation::Acquired(lease)) => {
                                    // Record ownership before the checkpoint so
                                    // cancellation after the writer CAS still
                                    // reaches the common release path.
                                    delivery_leases.push(lease);
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                    Some(event)
                                }
                                Ok(EventDeliveryReservation::LeasedUntil { expires_at_ms }) => {
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                    blocked_events.push(McpAwaitBlockedEvent {
                                        event_id: blocked.event_id,
                                        expires_at_ms,
                                        matching_any: blocked.matching_any,
                                        matching_all: blocked.matching_all,
                                    });
                                    None
                                }
                                Ok(EventDeliveryReservation::AlreadyHandledOrMissing) => {
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                    None
                                }
                                Err(error) => {
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                    return Err(McpToolError::from_error(error));
                                }
                            };
                            let Some(acquired_event) = acquired_event else {
                                continue;
                            };

                            mcp_await_event_apply_match_mask(
                                &mut any_met,
                                blocked.matching_any,
                            );
                            mcp_await_event_apply_match_mask(
                                &mut all_met,
                                blocked.matching_all,
                            );
                            let item = mcp_event_item_from_stored_event(
                                &storage,
                                &cx,
                                acquired_event,
                                &redactor,
                            )
                            .await;
                            mcp_await_event_checkpoint(&cx)
                                .map_err(McpToolError::from_error)?;
                            matched_events.push(item);
                        }
                        final_cursor =
                            mcp_await_event_safe_cursor(scan_after_id, &blocked_events);
                        if mcp_await_event_is_satisfied(&any_met, &all_met) {
                            break (true, false);
                        }
                    }

                    let query = EventStreamQuery {
                        after_id: scan_after_id,
                        limit: Some(MCP_AWAIT_EVENT_BATCH_LIMIT),
                        pane_id: params.pane,
                        rule_id: None,
                        event_type: None,
                        triage_state: None,
                        label: None,
                        unhandled_only: params.unhandled || params.claim,
                        since: if scan_after_id.is_some() {
                            None
                        } else {
                            Some(request_boundary_ms)
                        },
                        until: None,
                    };
                    let events_result = storage.get_events_stream_with_cx(&cx, query).await;
                    mcp_await_event_checkpoint(&cx).map_err(McpToolError::from_error)?;
                    let events = events_result.map_err(McpToolError::from_error)?;
                    let batch_len = events.len();

                    for event in events {
                        let event_id = event.id;
                        let matching_any = mcp_await_event_unmet_match_mask(
                            &any_conditions,
                            &any_met,
                            &event,
                        );
                        let matching_all = mcp_await_event_unmet_match_mask(
                            &all_conditions,
                            &all_met,
                            &event,
                        );
                        let event_matched = matching_any != 0 || matching_all != 0;

                        if !event_matched {
                            scan_after_id = Some(event_id);
                            continue;
                        }

                        if params.claim {
                            let remaining = timeout.saturating_sub(started.elapsed());
                            let lease_ttl = remaining.saturating_add(
                                std::time::Duration::from_secs(
                                    MCP_AWAIT_EVENT_DELIVERY_FINALIZE_GRACE_SECS,
                                ),
                            );
                            let reservation_result = storage
                                .reserve_event_delivery_with_cx(&cx, event_id, lease_ttl)
                                .await;
                            match reservation_result {
                                Ok(EventDeliveryReservation::Acquired(lease)) => {
                                    delivery_leases.push(lease);
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                }
                                Ok(EventDeliveryReservation::LeasedUntil { expires_at_ms }) => {
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                    if blocked_events.len()
                                        >= MCP_AWAIT_EVENT_BLOCKED_EVENT_MAX
                                    {
                                        // Do not advance `scan_after_id` over a
                                        // hole we cannot retain. The public
                                        // error is deliberately path/detail
                                        // free; the event ID stays in logs.
                                        tracing::warn!(
                                            event_id,
                                            blocked_event_cap =
                                                MCP_AWAIT_EVENT_BLOCKED_EVENT_MAX,
                                            "wa.await_event blocked-event tracking saturated"
                                        );
                                        return Err(mcp_await_event_blocked_capacity_error());
                                    }
                                    blocked_events.push(McpAwaitBlockedEvent {
                                        event_id,
                                        expires_at_ms,
                                        matching_any,
                                        matching_all,
                                    });
                                    scan_after_id = Some(event_id);
                                    continue;
                                }
                                Ok(EventDeliveryReservation::AlreadyHandledOrMissing) => {
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                    // A concurrent handler completed it after our read. It is no
                                    // longer a delivery candidate, so moving past it is safe.
                                    scan_after_id = Some(event_id);
                                    continue;
                                }
                                Err(error) => {
                                    mcp_await_event_checkpoint(&cx)
                                        .map_err(McpToolError::from_error)?;
                                    return Err(McpToolError::from_error(error));
                                }
                            }
                        }

                        scan_after_id = Some(event_id);
                        mcp_await_event_apply_match_mask(&mut any_met, matching_any);
                        mcp_await_event_apply_match_mask(&mut all_met, matching_all);
                        let item =
                            mcp_event_item_from_stored_event(&storage, &cx, event, &redactor).await;
                        mcp_await_event_checkpoint(&cx).map_err(McpToolError::from_error)?;
                        matched_events.push(item);
                        if mcp_await_event_is_satisfied(&any_met, &all_met) {
                            break;
                        }
                    }

                    final_cursor = mcp_await_event_safe_cursor(scan_after_id, &blocked_events);
                    mcp_await_event_checkpoint(&cx).map_err(McpToolError::from_error)?;
                    if mcp_await_event_is_satisfied(&any_met, &all_met) {
                        break (true, false);
                    }
                    if started.elapsed() >= timeout {
                        break (false, true);
                    }

                    if batch_len >= MCP_AWAIT_EVENT_BATCH_LIMIT {
                        // A leased hole blocks only the durable/exposed cursor,
                        // not this transient scan cursor. Keep paging forward
                        // before polling the hole again.
                        continue;
                    }

                    let remaining = timeout.saturating_sub(started.elapsed());
                    let lease_retry_at_ms = blocked_events
                        .iter()
                        .map(|blocked| blocked.expires_at_ms)
                        .min();
                    let wait = lease_retry_at_ms.map_or(poll, |expires_at_ms| {
                        let until_expiry_ms = expires_at_ms.saturating_sub(mcp_now_ms_i64());
                        let until_expiry_ms = u64::try_from(until_expiry_ms).unwrap_or(0);
                        poll.min(std::time::Duration::from_millis(until_expiry_ms))
                    });
                    let wait = wait.min(remaining);
                    mcp_await_event_wait_with_cx(&cx, wait)
                        .await
                        .map_err(McpToolError::from_error)?;
                    // Retry retained holes directly after the bounded wait;
                    // `scan_after_id` stays monotonic so the already-inspected
                    // tail is never replayed merely because one lease is live.
                    retry_blocked_events = true;
                };

                matched_events.sort_by_key(|event| event.id);

                Ok(McpAwaitEventData {
                    record_type: "await_result",
                    satisfied,
                    timed_out,
                    elapsed_ms: elapsed_ms(start),
                    final_cursor,
                    any: mcp_await_condition_status(&params.any, &any_met),
                    all: mcp_await_condition_status(&params.all, &all_met),
                    events: matched_events,
                    unhandled_only: params.unhandled || params.claim,
                    claim: params.claim,
                    claim_delivery: params.claim.then_some("finalize_after_delivery_ack"),
                })
            }
            .await;

            if let Err(err) = storage.shutdown().await {
                tracing::warn!(
                    error = %err,
                    "wa.await_event storage shutdown failed"
                );
            }
            (operation, delivery_leases)
        });

        match result {
            Ok(data) => {
                let envelope = McpEnvelope::success(data, elapsed_ms(start));
                let content = match envelope_to_content(envelope) {
                    Ok(content) => content,
                    Err(err) => {
                        complete_mcp_await_event_deliveries(
                            Arc::clone(&self.db_path),
                            delivery_leases,
                            FrameworkResponseDeliveryOutcome::Failed,
                        );
                        return Err(err);
                    }
                };

                if delivery_leases.is_empty() {
                    return Ok(content);
                }

                let Some(response_delivery) = self.response_delivery.as_ref() else {
                    complete_mcp_await_event_deliveries(
                        Arc::clone(&self.db_path),
                        delivery_leases,
                        FrameworkResponseDeliveryOutcome::Failed,
                    );
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_CONFIG,
                        "wa.await_event claim response has no acknowledgment-aware transport"
                            .to_string(),
                        None,
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                };

                let completion_db_path = Arc::clone(&self.db_path);
                let action = Box::new(move |outcome| {
                    complete_mcp_await_event_deliveries(
                        completion_db_path,
                        delivery_leases,
                        outcome,
                    );
                });
                if let Err(action) = response_delivery.try_prepare(action) {
                    action(FrameworkResponseDeliveryOutcome::Failed);
                    response_delivery.fail_all();
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_CONFIG,
                        "wa.await_event claim response collided with an in-flight delivery"
                            .to_string(),
                        Some(
                            "The MCP response transport must remain sequential and single-flight"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }

                Ok(content)
            }
            Err(err) => {
                complete_mcp_await_event_deliveries(
                    Arc::clone(&self.db_path),
                    delivery_leases,
                    FrameworkResponseDeliveryOutcome::Failed,
                );
                let envelope = McpEnvelope::<()>::error(
                    err.code,
                    err.message,
                    err.hint,
                    elapsed_ms(start),
                );
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

fn mcp_submit_guarantee_level(params: &SendParams) -> Option<SubmitGuaranteeLevel> {
    params.submit_level.or_else(|| {
        params
            .verify_submit
            .then_some(SubmitGuaranteeLevel::Submitted)
            .or_else(|| {
                params
                    .idempotency_key
                    .as_ref()
                    .map(|_| SubmitGuaranteeLevel::Write)
            })
    })
}

fn mcp_submit_idempotency_key(params: &SendParams) -> Option<String> {
    params.idempotency_key.as_deref().map(|caller_key| {
        crate::verified_submit::idempotency_key(params.pane_id, &params.text, Some(caller_key))
    })
}

fn mcp_submit_idempotency_storage_error(
    operation: &str,
    key: &str,
    error: impl std::fmt::Display,
) -> crate::Error {
    crate::Error::Storage(crate::StorageError::Database(format!(
        "submit idempotency {operation} failed for key {key}: {error}"
    )))
}

fn mcp_submit_receipt_from_verified_report(
    report: &crate::verified_submit::VerifiedSubmitReport,
    idempotency_key: String,
    elapsed_ms: u64,
    guarantee_level: SubmitGuaranteeLevel,
) -> crate::robot_types::SubmitReceipt {
    crate::robot_types::SubmitReceipt {
        state: report.state,
        guarantee_level,
        guarantee_met: guarantee_level.is_met_by(report.state, &report.evidence_rule_ids),
        agent_type: report.agent_type.clone(),
        profile_id: report.profile_id.clone(),
        profile_version: report.profile_version.clone(),
        attempts: report.attempts,
        evidence_rule_ids: report.evidence_rule_ids.clone(),
        elapsed_ms,
        polls: report.polls,
        cursor_before: report.cursor_before.clone(),
        cursor_after: report.cursor_after.clone(),
        idempotency_key,
    }
}

fn mcp_verified_report_from_submit_receipt(
    receipt: &crate::robot_types::SubmitReceipt,
) -> crate::verified_submit::VerifiedSubmitReport {
    crate::verified_submit::VerifiedSubmitReport {
        state: receipt.state,
        agent_type: receipt.agent_type.clone(),
        profile_id: receipt.profile_id.clone(),
        profile_version: receipt.profile_version.clone(),
        attempts: receipt.attempts,
        evidence_rule_ids: receipt.evidence_rule_ids.clone(),
        polls: receipt.polls,
        cursor_before: receipt.cursor_before.clone(),
        cursor_after: receipt.cursor_after.clone(),
    }
}

fn mcp_infer_submit_agent_type(pane_info: &PaneInfo) -> AgentType {
    let mut correlator = crate::agent_correlator::AgentCorrelator::new();
    correlator.update_from_pane_info(pane_info);
    if let Some(entry) = correlator.inventory().running.get(&pane_info.pane_id) {
        let provider = AgentProvider::from_slug(&entry.slug);
        let agent_type = provider.to_agent_type();
        if agent_type != AgentType::Unknown {
            return agent_type;
        }
    }
    crate::sharding::infer_agent_type(pane_info)
}

fn mcp_load_submit_profile(
    config: &Config,
    agent_type: AgentType,
) -> Option<crate::patterns::SubmitProfile> {
    if matches!(agent_type, AgentType::Unknown | AgentType::Wezterm) {
        return None;
    }

    match PatternEngine::from_config(&config.patterns) {
        Ok(engine) => engine.submit_profile_for_agent(agent_type).cloned(),
        Err(error) => {
            tracing::warn!(
                %error,
                %agent_type,
                "Failed to load submit profile pattern engine for wa.send; verified-submit will fail open"
            );
            None
        }
    }
}

async fn mcp_capture_submit_text(
    wezterm: &crate::wezterm::WeztermHandle,
    cx: &crate::cx::Cx,
    pane_id: u64,
) -> Option<String> {
    match wezterm.get_text_with_cx(cx, pane_id, false).await {
        Ok(text) => Some(text),
        Err(error) => {
            tracing::debug!(
                pane_id,
                %error,
                "wa.send verified-submit text capture unavailable"
            );
            None
        }
    }
}

async fn mcp_capture_submit_semantic_snapshot(
    wezterm: &crate::wezterm::WeztermHandle,
    cx: &crate::cx::Cx,
    pane_id: u64,
) -> Option<crate::wezterm::MuxSemanticSnapshot> {
    match wezterm.get_semantic_zones_with_cx(cx, pane_id).await {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            tracing::debug!(
                pane_id,
                %error,
                "wa.send verified-submit semantic capture unavailable"
            );
            None
        }
    }
}

async fn mcp_classify_submit_after_send(
    wezterm: &crate::wezterm::WeztermHandle,
    cx: &crate::cx::Cx,
    pane_id: u64,
    text: &str,
    agent_type: AgentType,
    submit_profile: Option<&crate::patterns::SubmitProfile>,
    before_text: Option<&str>,
    attempts: u32,
    polls: usize,
) -> crate::verified_submit::VerifiedSubmitReport {
    let (after_text, after_semantic_snapshot) = if submit_profile.is_some() {
        let _ =
            crate::runtime_async::sleep_with_cx(cx, std::time::Duration::from_millis(120)).await;
        let after_text = mcp_capture_submit_text(wezterm, cx, pane_id).await;
        let after_semantic_snapshot =
            mcp_capture_submit_semantic_snapshot(wezterm, cx, pane_id).await;
        (after_text, after_semantic_snapshot)
    } else {
        (None, None)
    };

    crate::verified_submit::classify_verified_submit(crate::verified_submit::VerifiedSubmitInput {
        pane_id,
        command_text: text,
        agent_type,
        profile: submit_profile,
        before_text,
        after_text: after_text.as_deref(),
        after_semantic_snapshot: after_semantic_snapshot.as_ref(),
        attempts,
        polls,
    })
}

async fn attach_mcp_submit_receipt_to_audit(
    storage: &StorageHandle,
    cx: &crate::cx::Cx,
    injection: &InjectionResult,
    receipt: &crate::robot_types::SubmitReceipt,
) {
    let Some(audit_action_id) = injection.audit_action_id() else {
        tracing::debug!(
            idempotency_key = %receipt.idempotency_key,
            "wa.send submit receipt has no audit action id to attach"
        );
        return;
    };

    let verification_summary = match serde_json::to_string(receipt) {
        Ok(summary) => summary,
        Err(error) => {
            tracing::warn!(%error, "Failed to serialize wa.send submit receipt");
            return;
        }
    };

    match storage
        .update_audit_action_submit_receipt_with_cx(
            cx,
            audit_action_id,
            receipt.idempotency_key.clone(),
            verification_summary,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                audit_action_id,
                "wa.send audit row not found for submit receipt attach"
            );
        }
        Err(error) => {
            tracing::warn!(
                audit_action_id,
                %error,
                "Failed to attach wa.send submit receipt to audit"
            );
        }
    }
}

impl WaSendTool {
    #[cfg(test)]
    pub(super) fn new(config: Arc<Config>, db_path: Arc<PathBuf>) -> Self {
        let policy_rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        // GH#72: honor any configured vendored mux socket.
        let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            wezterm,
            policy_rate_limiter,
        )
    }

    pub(super) fn new_with_shared_rate_limiter(
        config: Arc<Config>,
        db_path: Arc<PathBuf>,
        policy_rate_limiter: SharedRateLimiter,
    ) -> Self {
        // GH#72: honor any configured vendored mux socket.
        let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
        Self::with_wezterm_handle_and_shared_rate_limiter(
            config,
            db_path,
            wezterm,
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
                    "verify_submit": { "type": "boolean", "default": false, "description": "Return a SubmitReceipt using the submitted guarantee level unless submit_level is set" },
                    "submit_level": { "type": "string", "enum": ["write", "composer", "submitted", "working"], "description": "Optional SubmitReceipt guarantee level; setting this enables verified-submit receipts" },
                    "idempotency_key": { "type": "string", "maxLength": MAX_MCP_SUBMIT_IDEMPOTENCY_KEY_BYTES, "description": "Caller replay key; repeated non-dry-run sends with the same pane/text/key return the stored submitted or queued receipt without re-sending" },
                    "wait_for": { "type": "string", "maxLength": MAX_MCP_WAIT_PATTERN_BYTES, "description": "Wait for a pattern after sending" },
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

        let params: SendParams = match parse_mcp_tool_params(
            "wa.send",
            arguments,
            "Expected object matching wa.send input schema.",
            start,
        ) {
            Ok(params) => params,
            Err(response) => return response,
        };

        // Enforce the advertised timeout range server-side. `wa.send` uses
        // this bound for its optional wait_for phase.
        if let Some(error) = validate_mcp_wait_timeout_secs("wa.send", params.timeout_secs, start) {
            return error;
        }
        if let Some(wait_for) = params.wait_for.as_deref() {
            if let Some(error) =
                validate_mcp_wait_pattern_bytes("wa.send", "wait_for", wait_for, start)
            {
                return error;
            }
        }
        if let Some(idempotency_key) = params.idempotency_key.as_deref() {
            if let Some(error) =
                validate_mcp_submit_idempotency_key_bytes("wa.send", idempotency_key, start)
            {
                return error;
            }
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
        let submit_guarantee_level = mcp_submit_guarantee_level(&params);
        let submit_idempotency_key = mcp_submit_idempotency_key(&params);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result = runtime.block_on(async move {
            // ft-xbnl0.2.3 tick 303: cx-first MCP pane-state storage open (reuse wezterm_cx).
            let wezterm_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let storage =
                StorageHandle::new_with_cx(&wezterm_cx, &db_path.to_string_lossy()).await?;
            let ft_dir = db_path.parent().ok_or_else(|| {
                crate::Error::Config(crate::error::ConfigError::ValidationError(format!(
                    "database path has no parent directory: {}",
                    db_path.display()
                )))
            })?;
            if !params.dry_run {
                if let Some(key) = submit_idempotency_key.as_deref() {
                    let key_path =
                        crate::submit_idempotency_store::key_path(ft_dir, key).map_err(|error| {
                            mcp_submit_idempotency_storage_error("key validation", key, error)
                        })?;
                    if let Some(store_dir) = key_path.parent() {
                        std::fs::create_dir_all(store_dir).map_err(|error| {
                            mcp_submit_idempotency_storage_error("preflight", key, error)
                        })?;
                    }
                    let (outcome, prior) = crate::submit_idempotency_store::decide(ft_dir, key)
                        .map_err(|error| {
                            mcp_submit_idempotency_storage_error("lookup", key, error)
                        })?;
                    if matches!(
                        outcome,
                        crate::verified_submit::IdempotencyOutcome::DuplicateNoop
                    ) {
                        let guarantee_level =
                            submit_guarantee_level.unwrap_or(SubmitGuaranteeLevel::Write);
                        let prior = prior.ok_or_else(|| {
                            crate::Error::Storage(crate::StorageError::Corruption {
                                details: format!(
                                    "submit idempotency key {key} had duplicate outcome without a stored receipt"
                                ),
                            })
                        })?;
                        let submit = mcp_submit_receipt_from_verified_report(
                            &prior,
                            key.to_string(),
                            elapsed_ms(start),
                            guarantee_level,
                        );
                        let verification_error =
                            crate::verified_submit::submit_guarantee_failure_message(&submit);
                        let injection = InjectionResult::Allowed {
                            decision: PolicyDecision::allow_with_rule(
                                "submit_idempotency.duplicate_noop",
                            ),
                            summary: "duplicate submit replay suppressed".to_string(),
                            pane_id: params.pane_id,
                            action: ActionKind::SendText,
                            audit_action_id: None,
                        };
                        return Ok(McpSendData {
                            pane_id: params.pane_id,
                            injection,
                            wait_for: None,
                            verification_error,
                            submit: Some(submit),
                            dry_run: false,
                        });
                    }
                }
            }
            let wezterm = Arc::clone(&self.wezterm);
            let pane_info = wezterm
                .get_pane_with_cx(&wezterm_cx, params.pane_id)
                .await?;
            let domain = pane_info.inferred_domain();
            let submit_agent_type = mcp_infer_submit_agent_type(&pane_info);
            let submit_profile = submit_guarantee_level
                .filter(|level| level.requires_submit_profile())
                .and_then(|_| mcp_load_submit_profile(&config, submit_agent_type));
            let verified_submit_text = (submit_profile.is_some() && !params.text.trim().is_empty())
                .then(|| {
                    crate::verified_submit::append_verification_canary(params.pane_id, &params.text)
                });
            let outbound_submit_text = verified_submit_text.as_deref().unwrap_or(&params.text);

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

            let redacted_wait_for = params
                .wait_for
                .as_ref()
                .map(|pattern| redact_mcp_wait_pattern_for_output(pattern));
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
                    submit: None,
                    dry_run: true,
                });
            }

            let mut injector =
                PolicyGatedInjector::with_storage(engine, Arc::clone(&wezterm), storage.clone());
            let submit_before_text = if submit_profile.is_some() {
                mcp_capture_submit_text(&wezterm, &wezterm_cx, params.pane_id).await
            } else {
                None
            };
            let mut injection = injector
                .send_text(
                    params.pane_id,
                    outbound_submit_text,
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

            let mut submit = None;
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
                            let pattern_out =
                                redacted_wait_for.as_deref().unwrap_or(pattern).to_string();
                            wait_for_data = Some(McpWaitForData {
                                pane_id: params.pane_id,
                                pattern: pattern_out,
                                matched: true,
                                elapsed_ms,
                                polls,
                                is_regex: params.wait_for_regex,
                            });
                        }
                        Ok(WaitResult::TimedOut {
                            elapsed_ms, polls, ..
                        }) => {
                            let pattern_out =
                                redacted_wait_for.as_deref().unwrap_or(pattern).to_string();
                            wait_for_data = Some(McpWaitForData {
                                pane_id: params.pane_id,
                                pattern: pattern_out.clone(),
                                matched: false,
                                elapsed_ms,
                                polls,
                                is_regex: params.wait_for_regex,
                            });
                            verification_error =
                                Some(format!("Timeout waiting for pattern '{pattern_out}'"));
                        }
                        Ok(WaitResult::Cancelled { reason, polls }) => {
                            let pattern_out =
                                redacted_wait_for.as_deref().unwrap_or(pattern).to_string();
                            wait_for_data = Some(McpWaitForData {
                                pane_id: params.pane_id,
                                pattern: pattern_out,
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

            if let Some(guarantee_level) = submit_guarantee_level {
                let submit_verification =
                    if injection.is_allowed() && guarantee_level.requires_submit_profile() {
                        let polls = wait_for_data
                            .as_ref()
                            .map_or(0, |data| data.polls)
                            .saturating_add(usize::from(submit_profile.is_some()));
                        Some(
                            mcp_classify_submit_after_send(
                                &wezterm,
                                &wezterm_cx,
                                params.pane_id,
                                outbound_submit_text,
                                submit_agent_type,
                                submit_profile.as_ref(),
                                submit_before_text.as_deref(),
                                1,
                                polls,
                            )
                            .await,
                        )
                    } else {
                        None
                    };
                let mut receipt = crate::verified_submit::build_submit_receipt_with_guarantee(
                    params.pane_id,
                    &params.text,
                    &injection,
                    submit_verification.as_ref(),
                    elapsed_ms(start),
                    guarantee_level,
                );
                if let Some(key) = submit_idempotency_key.as_deref() {
                    receipt.idempotency_key = key.to_string();
                }
                if let Some(error) =
                    crate::verified_submit::submit_guarantee_failure_message(&receipt)
                {
                    verification_error = Some(match verification_error.take() {
                        Some(existing) => format!("{existing}; {error}"),
                        None => error,
                    });
                }
                attach_mcp_submit_receipt_to_audit(&storage, &wezterm_cx, &injection, &receipt)
                    .await;
                if let Some(key) = submit_idempotency_key.as_deref() {
                    let report = mcp_verified_report_from_submit_receipt(&receipt);
                    if let Err(error) = crate::submit_idempotency_store::record(ft_dir, key, &report)
                    {
                        verification_error = Some(match verification_error.take() {
                            Some(existing) => {
                                format!("{existing}; submit idempotency record failed: {error}")
                            }
                            None => format!("submit idempotency record failed: {error}"),
                        });
                    }
                }
                submit = Some(receipt);
            }

            Ok(McpSendData {
                pane_id: params.pane_id,
                injection,
                wait_for: wait_for_data,
                verification_error,
                submit,
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
                // The wait_for compile error (and any other error from this
                // block) is reflected verbatim into the message; redact
                // secret-shaped tokens before they reach the MCP client.
                let message = redact_mcp_output_secrets(&err.to_string());
                let envelope = McpEnvelope::<()>::error(code, message, hint, elapsed_ms(start));
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
    #[cfg(test)]
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
                    Some("Expected object with name, pane_id, dry_run".to_string()),
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result: std::result::Result<McpWorkflowRunData, McpToolError> =
            runtime.block_on(async move {
                // ft-xbnl0.2.3 tick 303: cx-first MCP workflow run storage open.
                let wf_open_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let storage =
                    StorageHandle::new_with_cx(&wf_open_cx, &db_path.to_string_lossy())
                        .await
                        .map_err(McpToolError::from_error)?;
                let storage = Arc::new(storage);

                // GH#72: honor any configured vendored mux socket.
                let wezterm = crate::wezterm::wezterm_handle_from_config(config.as_ref());
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
                // ft-cli44: manual runs must use the same lock +
                // execution-record protocol as the detection path. Calling
                // `run_workflow` directly left no execution record, so every
                // record-requiring persistence helper failed with
                // "Workflow not found: <execution_id>" and the run was
                // invisible to status/abort surfaces.
                let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let outcome = runner
                    .run_workflow_manual_with_cx(
                        &cx,
                        params.pane_id,
                        workflow,
                        &execution_id,
                        Some(serde_json::json!({
                            "trigger": "mcp",
                            "tool": "wa.workflow_run",
                        })),
                    )
                    .await;
                let result = match outcome {
                    ManualWorkflowRunOutcome::Ran(result) => result,
                    ManualWorkflowRunOutcome::PaneLocked {
                        held_by_workflow,
                        held_by_execution,
                        ..
                    } => {
                        return Err(McpToolError::new(
                            MCP_ERR_WORKFLOW,
                            format!(
                                "Pane {} is locked by workflow '{held_by_workflow}' (execution {held_by_execution})",
                                params.pane_id
                            ),
                            Some(
                                "Wait for the running workflow to finish, or check wa.workflow_status."
                                    .to_string(),
                            ),
                        ));
                    }
                    ManualWorkflowRunOutcome::ConcurrencyLimitReached { active, limit } => {
                        return Err(McpToolError::new(
                            MCP_ERR_WORKFLOW,
                            format!(
                                "Workflow concurrency limit reached ({active} active / limit {limit})"
                            ),
                            Some("Retry after a running workflow completes.".to_string()),
                        ));
                    }
                    ManualWorkflowRunOutcome::StartError { error } => {
                        return Err(McpToolError::new(
                            MCP_ERR_WORKFLOW,
                            format!("Failed to start workflow: {error}"),
                            None,
                        ));
                    }
                };

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

pub(super) struct WaWorkflowStatusTool {
    db_path: Arc<PathBuf>,
}

impl WaWorkflowStatusTool {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

fn workflow_status_missing_filter(params: &WorkflowStatusParams) -> bool {
    params.execution_id.is_none() && params.pane_id.is_none() && !params.active
}

fn workflow_record_elapsed_ms(record: &crate::storage::WorkflowRecord) -> Option<u64> {
    let end = record
        .completed_at
        .unwrap_or_else(|| i64::try_from(now_ms()).unwrap_or(i64::MAX));
    end.checked_sub(record.started_at)
        .map(|elapsed| u64::try_from(elapsed.max(0)).unwrap_or(u64::MAX))
}

fn parse_optional_json(raw: Option<String>) -> Option<serde_json::Value> {
    raw.and_then(|value| serde_json::from_str(&value).ok())
}

fn workflow_step_log_to_robot(log: crate::storage::WorkflowStepLogRecord) -> WorkflowStepLog {
    WorkflowStepLog {
        step_index: log.step_index,
        step_name: log.step_name,
        result_type: log.result_type,
        step_id: log.step_id,
        step_kind: log.step_kind,
        result_data: parse_optional_json(log.result_data),
        policy_summary: parse_optional_json(log.policy_summary),
        verification_refs: parse_optional_json(log.verification_refs),
        error_code: log.error_code,
        started_at: log.started_at,
        completed_at: Some(log.completed_at),
        duration_ms: Some(log.duration_ms),
    }
}

fn workflow_status_data(
    record: crate::storage::WorkflowRecord,
    latest_log: Option<crate::storage::WorkflowStepLogRecord>,
    step_logs: Option<Vec<WorkflowStepLog>>,
    action_plan_record: Option<crate::storage::WorkflowActionPlanRecord>,
    verbose: bool,
) -> WorkflowStatusDetailData {
    let (action_plan, plan_step_name, total_steps) = if let Some(plan_record) = action_plan_record {
        // br-ft-ncijf: route through parse_workflow_plan_json so
        // a malformed plan_json bumps MCP_WORKFLOW_PLAN_SERDE_DROP_COUNT
        // and emits a structured warn instead of returning None silently.
        let parsed_plan = parse_workflow_plan_json(&plan_record.plan_json, &plan_record.plan_id);
        let step_name = parsed_plan
            .as_ref()
            .and_then(|plan| plan.steps.get(record.current_step))
            .map(|step| step.description.clone());
        let total_steps = parsed_plan.as_ref().and_then(|plan| {
            let count = plan.steps.len();
            (count > 0).then_some(count)
        });
        let action_plan = if verbose {
            let plan_value = parsed_plan
                .as_ref()
                .and_then(|plan| serde_json::to_value(plan).ok())
                .or_else(|| serde_json::from_str(&plan_record.plan_json).ok());
            Some(WorkflowActionPlan {
                plan_id: plan_record.plan_id,
                plan_hash: plan_record.plan_hash,
                plan: plan_value,
                created_at: Some(plan_record.created_at),
            })
        } else {
            None
        };
        (action_plan, step_name, total_steps)
    } else {
        (None, None, None)
    };

    let step_name = plan_step_name.or_else(|| latest_log.as_ref().map(|log| log.step_name.clone()));
    let last_step_result = latest_log.map(|log| log.result_type);
    let elapsed_ms = workflow_record_elapsed_ms(&record);

    WorkflowStatusDetailData {
        execution_id: record.id,
        workflow_name: record.workflow_name,
        pane_id: Some(record.pane_id),
        trigger_event_id: record.trigger_event_id,
        status: record.status,
        step_name,
        elapsed_ms,
        last_step_result,
        current_step: Some(record.current_step),
        total_steps,
        wait_condition: record.wait_condition,
        context: record.context,
        result: record.result,
        error: record.error,
        started_at: u64::try_from(record.started_at).ok(),
        updated_at: u64::try_from(record.updated_at).ok(),
        completed_at: record.completed_at.and_then(|ts| u64::try_from(ts).ok()),
        step_logs,
        action_plan,
    }
}

fn workflow_status_list_item(record: crate::storage::WorkflowRecord) -> WorkflowStatusData {
    WorkflowStatusData {
        execution_id: record.id,
        workflow_name: record.workflow_name,
        pane_id: Some(record.pane_id),
        trigger_event_id: record.trigger_event_id,
        status: record.status,
        message: record.error,
        started_at: Some(record.started_at),
        completed_at: record.completed_at,
        current_step: Some(record.current_step),
        total_steps: None,
        plan: None,
        created_at: Some(record.started_at),
    }
}

impl ToolHandler for WaWorkflowStatusTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.workflow_status".to_string(),
            description: Some("Query workflow execution status (robot parity)".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "execution_id": { "type": "string", "description": "Workflow execution ID to inspect" },
                    "pane_id": { "type": "integer", "minimum": 0, "description": "List workflows for a pane" },
                    "active": { "type": "boolean", "default": false, "description": "List active running or waiting workflows" },
                    "verbose": { "type": "boolean", "default": false, "description": "Include step logs and action plan details for execution_id queries" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50, "description": "Maximum workflows returned for list queries" }
                },
                "anyOf": [
                    { "required": ["execution_id"] },
                    { "required": ["pane_id"] },
                    { "required": ["active"], "properties": { "active": { "const": true } } }
                ],
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

        let params: WorkflowStatusParams = match serde_json::from_value(arguments) {
            Ok(params) => params,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with execution_id, pane_id, active, verbose, and limit"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };

        if workflow_status_missing_filter(&params) {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "Must provide execution_id, pane_id, or active=true".to_string(),
                Some(
                    "Specify an execution_id, pane_id, or active=true to bound the workflow status query."
                        .to_string(),
                ),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let limit = params.limit.unwrap_or(50);
        if !(1..=500).contains(&limit) {
            let envelope = McpEnvelope::<()>::error(
                MCP_ERR_INVALID_ARGS,
                "limit must be between 1 and 500".to_string(),
                Some("Use a positive limit no larger than 500.".to_string()),
                elapsed_ms(start),
            );
            return envelope_to_content(envelope);
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("Runtime init failed: {e}")))?;

        let result = runtime.block_on(async move {
            let storage_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let storage = StorageHandle::new_with_cx(&storage_cx, &db_path.to_string_lossy())
                .await
                .map_err(McpToolError::from_error)?;

            if let Some(exec_id) = params.execution_id.as_deref() {
                let record = storage
                    .get_workflow_with_cx(&storage_cx, exec_id)
                    .await
                    .map_err(McpToolError::from_error)?
                    .ok_or_else(|| {
                        McpToolError::new(
                            MCP_ERR_WORKFLOW,
                            format!("No workflow execution found with ID: {exec_id}"),
                            Some(
                                "Check the execution ID or query active=true for running workflows."
                                    .to_string(),
                            ),
                        )
                    })?;
                let (step_logs, latest_log) = if params.verbose {
                    let logs = storage
                        .get_step_logs_with_cx(&storage_cx, exec_id)
                        .await
                        .map_err(McpToolError::from_error)?;
                    let latest = logs.last().cloned();
                    let mapped = logs.into_iter().map(workflow_step_log_to_robot).collect();
                    (Some(mapped), latest)
                } else {
                    let latest = storage
                        .get_latest_step_log_with_cx(&storage_cx, exec_id)
                        .await
                        .map_err(McpToolError::from_error)?;
                    (None, latest)
                };
                let action_plan = storage
                    .get_action_plan_with_cx(&storage_cx, exec_id)
                    .await
                    .map_err(McpToolError::from_error)?;
                let data = workflow_status_data(
                    record,
                    latest_log,
                    step_logs,
                    action_plan,
                    params.verbose,
                );
                Ok(serde_json::to_value(data).unwrap_or(serde_json::Value::Null))
            } else {
                if let Some(pane_id) = params.pane_id {
                    let pane = storage
                        .get_pane_with_cx(&storage_cx, pane_id)
                        .await
                        .map_err(McpToolError::from_error)?;
                    if pane.is_none() {
                        return Err(McpToolError::new(
                            MCP_ERR_PANE_NOT_FOUND,
                            format!("No pane found with ID: {pane_id}"),
                            Some("Check the pane ID or call wa.state to list panes.".to_string()),
                        ));
                    }
                }

                let mut records = if params.active {
                    storage
                        .find_incomplete_workflows_with_cx(&storage_cx)
                        .await
                        .map_err(McpToolError::from_error)?
                } else if let Some(pane_id) = params.pane_id {
                    storage
                        .export_workflows_with_cx(
                            &storage_cx,
                            crate::storage::ExportQuery {
                                pane_id: Some(pane_id),
                                limit: Some(limit),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(McpToolError::from_error)?
                } else {
                    Vec::new()
                };
                if let Some(pane_id) = params.pane_id {
                    records.retain(|record| record.pane_id == pane_id);
                }
                if records.len() > limit {
                    records.truncate(limit);
                }

                let executions: Vec<WorkflowStatusData> =
                    records.into_iter().map(workflow_status_list_item).collect();
                let data = WorkflowStatusListData {
                    count: executions.len(),
                    executions,
                    pane_filter: params.pane_id,
                    active_only: params.active.then_some(true),
                };
                Ok(serde_json::to_value(data).unwrap_or(serde_json::Value::Null))
            }
        });

        match result {
            Ok(data) => envelope_to_content(McpEnvelope::success(data, elapsed_ms(start))),
            Err(err) => envelope_to_content(McpEnvelope::<()>::error(
                err.code,
                err.message,
                err.hint,
                elapsed_ms(start),
            )),
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
    #[cfg(test)]
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

        let layout = match tx_mutation_workspace_layout(self.config.as_ref()) {
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
        let contract_lock = match acquire_mcp_tx_contract_lock(&layout.root, &contract_path) {
            Ok(lock) => lock,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let authoritative_contract_path = contract_lock.authoritative_path().to_path_buf();
        #[cfg(test)]
        run_tx_contract_post_lock_test_hook();
        let contract = match load_mcp_tx_contract_from_guard(&contract_lock) {
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

        let now_ms = mcp_now_ms_i64();
        let runtime = match CompatRuntimeBuilder::current_thread().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_STORAGE,
                    format!("Runtime init failed: {err}"),
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
            .with_surface(PolicySurface::Mcp)
            .with_actor(crate::policy::ActorKind::Mcp);
        let approvals = crate::plan::StorageBackedPrepareApprovalChecker::new(Some(&storage));
        let resolved_capabilities = runtime.block_on(resolve_tx_prepare_capabilities(
            self.config.as_ref(),
            &storage,
            &contract,
        ));
        let targets = crate::plan::StorageBackedPrepareTargetLookup::new(None, Some(&storage))
            .with_resolved_capabilities(resolved_capabilities);
        let executor = crate::tx_execution::PaneStepExecutor::new(
            tx_run_wezterm_handle(self.config.as_ref()),
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

        if let Err(err) =
            authorize_mcp_tx_contract_for_effects(&contract_lock, &authoritative_contract_path)
        {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }
        #[cfg(test)]
        run_tx_contract_post_auth_test_hook();
        let mut idem_store = match contract_lock
            .open_idempotency_store(crate::tx_idempotency::IdempotencyPolicy::default())
        {
            Ok(store) => store,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_STORAGE,
                    format!("failed to open durable tx idempotency store: {err}"),
                    Some(
                        "Fix access to the tx_ledgers directory; no transaction step was dispatched."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let mut persisted_contract = contract.clone();
        let execution_result =
            execution_engine.execute_with_store(&mut persisted_contract, &mut idem_store, now_ms);
        let save_result = mcp_save_mission_tx_contract_to_path(
            &contract_lock,
            &authoritative_contract_path,
            &persisted_contract,
        );
        let execution = match (execution_result, save_result) {
            (Ok(execution), Ok(())) => execution,
            (Ok(_), Err(save_err)) => {
                let err = mcp_tx_contract_save_failure_after_effects(
                    save_err,
                    "Transaction execution completed",
                    "wa.tx_run",
                    "external transaction effects",
                );
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
            (Err(execution_err), Ok(())) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_WORKFLOW,
                    format!(
                        "tx execution failed after persisting available transaction evidence: {execution_err}"
                    ),
                    Some(
                        "Inspect wa.tx_show(include_contract=true) before retrying; external effects may have occurred."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
            (Err(execution_err), Err(save_err)) => {
                let err = mcp_tx_contract_save_failure_after_effects(
                    save_err,
                    &format!("Transaction execution failed ({execution_err})"),
                    "wa.tx_run",
                    "external transaction effects",
                );
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let data = McpTxRunData {
            contract_file: authoritative_contract_path.display().to_string(),
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

fn mcp_tx_rollback_proof_error(
    kind: crate::tx_execution::RollbackProofKind,
) -> (&'static str, &'static str) {
    match kind {
        crate::tx_execution::RollbackProofKind::Missing => (
            MCP_ERR_WORKFLOW,
            "Do not rerun the commit or rollback. Inspect and reconcile the contract receipts, external effects, and workspace .ft/tx_ledgers records first: missing durable proof does not establish that an external effect was absent. For future new transactions, execute commits through MCP wa.tx_run, `ft tx run`, or `ft robot tx run` so receipts and authoritative proofs are persisted together; do not fabricate receipts.",
        ),
        crate::tx_execution::RollbackProofKind::Conflict => (
            MCP_ERR_WORKFLOW,
            "Do not blindly rerun the commit or rollback. Inspect and reconcile the contract receipts with the workspace .ft/tx_ledgers records first; ambiguous or contradictory durable state may represent an external effect that was already dispatched.",
        ),
    }
}

impl WaTxRollbackTool {
    #[cfg(test)]
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

        let layout = match tx_mutation_workspace_layout(self.config.as_ref()) {
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
        let contract_lock = match acquire_mcp_tx_contract_lock(&layout.root, &contract_path) {
            Ok(lock) => lock,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let authoritative_contract_path = contract_lock.authoritative_path().to_path_buf();
        #[cfg(test)]
        run_tx_contract_post_lock_test_hook();
        let contract = match load_mcp_tx_contract_from_guard(&contract_lock) {
            Ok(contract) => contract,
            Err(err) => {
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };

        let now_ms = mcp_now_ms_i64();
        let commit_report = match crate::plan::mission_tx_rollback_commit_report(&contract, now_ms)
        {
            Ok(report) => report,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    err,
                    Some(
                        "Use wa.tx_show(include_contract=true) to inspect persisted commit receipts. Do not rerun the commit or rollback solely to repair missing receipts: durable state may represent an external effect that was already dispatched. Reconcile the contract with the workspace .ft/tx_ledgers records; do not fabricate receipts."
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
        let runtime = match CompatRuntimeBuilder::current_thread().build() {
            Ok(runtime) => runtime,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_STORAGE,
                    format!("Runtime init failed: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let storage = match runtime.block_on(async {
            let rollback_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            StorageHandle::new_with_cx(&rollback_cx, &layout.db_path.to_string_lossy()).await
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
            .with_surface(PolicySurface::Mcp)
            .with_actor(crate::policy::ActorKind::Mcp);
        let approvals = crate::plan::StorageBackedPrepareApprovalChecker::new(Some(&storage));
        let resolved_capabilities = runtime.block_on(resolve_tx_prepare_capabilities(
            self.config.as_ref(),
            &storage,
            &contract,
        ));
        let targets = crate::plan::StorageBackedPrepareTargetLookup::new(None, Some(&storage))
            .with_resolved_capabilities(resolved_capabilities);
        let executor = crate::tx_execution::PaneStepExecutor::new(
            tx_run_wezterm_handle(self.config.as_ref()),
            RefCell::new(policy_engine),
            approvals,
            targets,
            prepare_context,
        );
        let execution_engine = crate::tx_execution::TxExecutionEngine::new(
            executor,
            crate::tx_execution::TxExecutionConfig {
                fail_compensation_for_step: params.fail_compensation_for_step.clone(),
                ..crate::tx_execution::TxExecutionConfig::default()
            },
        );
        if let Err(err) =
            authorize_mcp_tx_contract_for_effects(&contract_lock, &authoritative_contract_path)
        {
            let envelope =
                McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
            return envelope_to_content(envelope);
        }
        #[cfg(test)]
        run_tx_contract_post_auth_test_hook();
        let mut idem_store = match contract_lock
            .open_idempotency_store(crate::tx_idempotency::IdempotencyPolicy::default())
        {
            Ok(store) => store,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_STORAGE,
                    format!("failed to open durable tx idempotency store: {err}"),
                    Some(
                        "Fix access to the tx_ledgers directory; no compensation was dispatched."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let mut persisted_contract = contract.clone();
        let rollback_result = match execution_engine.rollback_with_store(
            &mut persisted_contract,
            &mut idem_store,
            now_ms,
        ) {
            Err(
                rollback_err @ crate::tx_execution::TxExecutionError::RollbackProof { kind, .. },
            ) => {
                let (error_code, hint) = mcp_tx_rollback_proof_error(kind);
                let envelope = McpEnvelope::<()>::error(
                    error_code,
                    format!("rollback rejected before compensation dispatch: {rollback_err}"),
                    Some(hint.to_string()),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
            Err(rollback_err @ crate::tx_execution::TxExecutionError::InProgress(_)) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_WORKFLOW,
                    format!("rollback deferred before compensation dispatch: {rollback_err}"),
                    Some(
                        "Wait for the in-flight transaction mutation to finish, then retry."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
            result => result,
        };
        let save_result = mcp_save_mission_tx_contract_to_path(
            &contract_lock,
            &authoritative_contract_path,
            &persisted_contract,
        );
        let compensation_report = match (rollback_result, save_result) {
            (Ok(result), Ok(())) => result.compensation_report,
            (Ok(_), Err(save_err)) => {
                let err = mcp_tx_contract_save_failure_after_effects(
                    save_err,
                    "Transaction compensation completed",
                    "wa.tx_rollback",
                    "compensation effects",
                );
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
            (Err(rollback_err), Ok(())) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_WORKFLOW,
                    format!(
                        "rollback failed after persisting available transaction evidence: {rollback_err}"
                    ),
                    Some(
                        "Inspect wa.tx_show(include_contract=true) before retrying; compensation effects may have occurred."
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
            (Err(rollback_err), Err(save_err)) => {
                let err = mcp_tx_contract_save_failure_after_effects(
                    save_err,
                    &format!("Transaction compensation failed ({rollback_err})"),
                    "wa.tx_rollback",
                    "compensation effects",
                );
                let envelope =
                    McpEnvelope::<()>::error(err.code, err.message, err.hint, elapsed_ms(start));
                return envelope_to_content(envelope);
            }
        };
        let final_state = persisted_contract.lifecycle_state;

        let data = McpTxRollbackData {
            contract_file: authoritative_contract_path.display().to_string(),
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
    #[cfg(test)]
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
    #[cfg(test)]
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
                    "service": { "type": "string", "maxLength": MAX_MCP_ACCOUNT_SERVICE_BYTES, "description": "Service name (openai, anthropic, google)" }
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

        if let Some(error) =
            validate_mcp_account_service_bytes("wa.accounts", params.service.as_str(), start)
        {
            return error;
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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
    #[cfg(test)]
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
                    "service": { "type": "string", "maxLength": MAX_MCP_ACCOUNT_SERVICE_BYTES, "description": "Service name (openai)" }
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

        if let Some(service) = params.service.as_deref()
            && let Some(error) =
                validate_mcp_account_service_bytes("wa.accounts_refresh", service, start)
        {
            return error;
        }

        let config = Arc::clone(&self.config);
        let db_path = Arc::clone(&self.db_path);
        let policy_rate_limiter = Arc::clone(&self.policy_rate_limiter);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result: std::result::Result<McpAccountsRefreshData, McpToolError> =
            runtime.block_on(async move {
                let service = params.service.unwrap_or_else(|| "openai".to_string());
                let caut_service = parse_caut_service(&service).ok_or_else(|| {
                    McpToolError::new(
                        MCP_ERR_INVALID_ARGS,
                        "Unknown service".to_string(),
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

fn parse_mission_objective_strictness(
    raw: Option<&str>,
) -> std::result::Result<MissionObjectiveStrictness, McpToolError> {
    match raw.unwrap_or("normal").trim().to_ascii_lowercase().as_str() {
        "advisory" => Ok(MissionObjectiveStrictness::Advisory),
        "normal" => Ok(MissionObjectiveStrictness::Normal),
        "strict" => Ok(MissionObjectiveStrictness::Strict),
        other => Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("Invalid strictness '{other}'"),
            Some("Use advisory, normal, or strict.".to_string()),
        )),
    }
}

fn parse_mission_objective_source(
    raw: &str,
) -> std::result::Result<MissionObjectiveSourceKind, McpToolError> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "beads" => Ok(MissionObjectiveSourceKind::Beads),
        "agent_mail" => Ok(MissionObjectiveSourceKind::AgentMail),
        "rch" => Ok(MissionObjectiveSourceKind::Rch),
        "git" => Ok(MissionObjectiveSourceKind::Git),
        "robot" => Ok(MissionObjectiveSourceKind::Robot),
        "blocker_radar" => Ok(MissionObjectiveSourceKind::BlockerRadar),
        "resource_cockpit" => Ok(MissionObjectiveSourceKind::ResourceCockpit),
        "manual" => Ok(MissionObjectiveSourceKind::Manual),
        other => Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("Invalid source domain '{other}'"),
            Some("Use beads, agent-mail, rch, git, robot, blocker-radar, resource-cockpit, or manual.".to_string()),
        )),
    }
}

fn parse_mission_objective_proof_availability(
    raw: Option<&str>,
) -> std::result::Result<MissionObjectiveProofAvailability, McpToolError> {
    match raw
        .unwrap_or("available")
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_")
        .as_str()
    {
        "not_required" => Ok(MissionObjectiveProofAvailability::NotRequired),
        "available" => Ok(MissionObjectiveProofAvailability::Available),
        "blocked" => Ok(MissionObjectiveProofAvailability::Blocked),
        "unavailable" => Ok(MissionObjectiveProofAvailability::Unavailable),
        other => Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("Invalid proof_availability '{other}'"),
            Some("Use not-required, available, blocked, or unavailable.".to_string()),
        )),
    }
}

fn parse_mission_objective_capacity_posture(
    raw: Option<&str>,
) -> std::result::Result<MissionObjectiveCapacityPosture, McpToolError> {
    match raw.unwrap_or("admit").trim().to_ascii_lowercase().as_str() {
        "admit" => Ok(MissionObjectiveCapacityPosture::Admit),
        "defer" => Ok(MissionObjectiveCapacityPosture::Defer),
        "pause" => Ok(MissionObjectiveCapacityPosture::Pause),
        "unknown" => Ok(MissionObjectiveCapacityPosture::Unknown),
        other => Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("Invalid capacity_posture '{other}'"),
            Some("Use admit, defer, pause, or unknown.".to_string()),
        )),
    }
}

fn mission_objective_source_slug(kind: MissionObjectiveSourceKind) -> &'static str {
    match kind {
        MissionObjectiveSourceKind::Beads => "beads",
        MissionObjectiveSourceKind::AgentMail => "agent_mail",
        MissionObjectiveSourceKind::Rch => "rch",
        MissionObjectiveSourceKind::Git => "git",
        MissionObjectiveSourceKind::Robot => "robot",
        MissionObjectiveSourceKind::BlockerRadar => "blocker_radar",
        MissionObjectiveSourceKind::ResourceCockpit => "resource_cockpit",
        MissionObjectiveSourceKind::Manual => "manual",
        MissionObjectiveSourceKind::Fixture => "fixture",
    }
}

fn mission_objective_evidence_category(
    kind: MissionObjectiveSourceKind,
) -> MissionObjectiveEvidenceCategory {
    match kind {
        MissionObjectiveSourceKind::Beads => MissionObjectiveEvidenceCategory::BeadsReadyQueue,
        MissionObjectiveSourceKind::AgentMail => {
            MissionObjectiveEvidenceCategory::AgentMailAvailability
        }
        MissionObjectiveSourceKind::Rch => MissionObjectiveEvidenceCategory::RchWorkerSelection,
        MissionObjectiveSourceKind::Git => MissionObjectiveEvidenceCategory::GitDirtyTree,
        MissionObjectiveSourceKind::Robot => MissionObjectiveEvidenceCategory::RobotInventory,
        MissionObjectiveSourceKind::BlockerRadar | MissionObjectiveSourceKind::ResourceCockpit => {
            MissionObjectiveEvidenceCategory::CapacityPressure
        }
        MissionObjectiveSourceKind::Manual | MissionObjectiveSourceKind::Fixture => {
            MissionObjectiveEvidenceCategory::Manual
        }
    }
}

fn mission_objective_source_snapshot(
    raw_source: &str,
    state: &'static str,
) -> std::result::Result<MissionObjectiveSourceSnapshot, McpToolError> {
    let kind = parse_mission_objective_source(raw_source)?;
    let slug = mission_objective_source_slug(kind);
    let reason_code = format!("source.{slug}.{state}");
    let snapshot = MissionObjectiveSourceSnapshot::new(format!("{slug}.{state}"), kind)
        .with_evidence(
            MissionObjectiveEvidenceItem::new(
                mission_objective_evidence_category(kind),
                format!("{slug} source reported {state} by MCP caller"),
            )
            .with_reason_code(reason_code.clone()),
        );
    Ok(match state {
        "unavailable" => snapshot.unavailable(reason_code),
        "degraded" => snapshot.degraded(reason_code),
        "stale" => snapshot.stale(reason_code),
        _ => snapshot.with_reason_code(reason_code),
    })
}

fn mcp_non_empty_option(
    raw: Option<String>,
    field_name: &'static str,
) -> std::result::Result<Option<String>, McpToolError> {
    match raw {
        Some(value) if value.trim().is_empty() => Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            format!("{field_name} cannot be empty"),
            None,
        )),
        Some(value) => Ok(Some(value.trim().to_string())),
        None => Ok(None),
    }
}

fn build_mcp_mission_objective_plan_input(
    params: MissionObjectivePlanParams,
) -> std::result::Result<MissionObjectivePlannerInput, McpToolError> {
    if params.execute {
        return Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            "wa.mission_objective_plan is dry-run only and cannot execute actions".to_string(),
            Some(
                "Remove execute=true; use the plan as input to a separate reviewed workflow."
                    .to_string(),
            ),
        ));
    }
    if params.objective.trim().is_empty() {
        return Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            "objective cannot be empty".to_string(),
            Some("Provide an objective string.".to_string()),
        ));
    }
    if params.explain_step.is_some() && params.explain_reason.is_some() {
        return Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            "use either explain_step or explain_reason, not both".to_string(),
            None,
        ));
    }

    let target_bead = mcp_non_empty_option(params.target_bead, "target_bead")?;
    let candidate_id = mcp_non_empty_option(params.candidate_id, "candidate_id")?;
    let candidate_title = mcp_non_empty_option(params.candidate_title, "candidate_title")?;
    let active_assignee = mcp_non_empty_option(params.active_assignee, "active_assignee")?;
    let proof_availability =
        parse_mission_objective_proof_availability(params.proof_availability.as_deref())?;
    let capacity_posture =
        parse_mission_objective_capacity_posture(params.capacity_posture.as_deref())?;

    let mut input = MissionObjectivePlannerInput::new(
        params.generated_at_ms.unwrap_or_else(now_ms),
        "wa.mission_objective_plan",
        params.objective.trim(),
    )
    .strictness(parse_mission_objective_strictness(
        params.strictness.as_deref(),
    )?)
    .with_source_snapshot(
        MissionObjectiveSourceSnapshot::new(
            "wa.mission_objective_plan.manual",
            MissionObjectiveSourceKind::Manual,
        )
        .with_evidence(
            MissionObjectiveEvidenceItem::new(
                MissionObjectiveEvidenceCategory::Manual,
                "objective supplied as redacted MCP argument",
            )
            .with_reason_code("objective.input.redacted_summary"),
        ),
    );

    for source in &params.source_unavailable {
        input =
            input.with_source_snapshot(mission_objective_source_snapshot(source, "unavailable")?);
    }
    for source in &params.source_degraded {
        input = input.with_source_snapshot(mission_objective_source_snapshot(source, "degraded")?);
    }
    for source in &params.source_stale {
        input = input.with_source_snapshot(mission_objective_source_snapshot(source, "stale")?);
    }
    for dirty_path in &params.dirty_paths {
        if !dirty_path.trim().is_empty() {
            input = input.with_dirty_path(
                MissionObjectiveDirtyPath::new(dirty_path.trim(), "modified")
                    .category("mcp_dirty_tree"),
            );
        }
    }

    let should_create_candidate = target_bead.is_some()
        || candidate_id.is_some()
        || params.testing_skill_lane
        || params.dependency_blocked
        || active_assignee.is_some();

    if should_create_candidate {
        let readiness = if params.dependency_blocked {
            MissionObjectiveCandidateReadiness::BlockedDependency
        } else if active_assignee.is_some() {
            MissionObjectiveCandidateReadiness::ActiveSameDomain
        } else if params.testing_skill_lane {
            MissionObjectiveCandidateReadiness::TestingSkillLane
        } else if target_bead.is_some() {
            MissionObjectiveCandidateReadiness::ReadyBead
        } else {
            MissionObjectiveCandidateReadiness::PlanningOnly
        };
        let resolved_candidate_id = candidate_id
            .clone()
            .or_else(|| target_bead.clone())
            .unwrap_or_else(|| "objective.manual_candidate".to_string());
        let mut candidate = MissionObjectiveCandidateWork::new(resolved_candidate_id, readiness)
            .title(
                candidate_title
                    .clone()
                    .or_else(|| target_bead.clone())
                    .unwrap_or_else(|| params.objective.trim().to_string()),
            )
            .dependency_ready(!params.dependency_blocked)
            .proof_availability(proof_availability)
            .capacity_posture(capacity_posture);
        if let Some(target_bead) = target_bead {
            candidate = candidate.target_bead_id(target_bead);
        }
        if let Some(active_assignee) = active_assignee {
            candidate =
                candidate.active_owner(active_assignee, params.active_age_seconds.unwrap_or(0));
        }
        if let Some(stale_after_seconds) = params.stale_after_seconds {
            candidate = candidate.stale_after_seconds(stale_after_seconds);
        }
        for owned_path in &params.owned_paths {
            if !owned_path.trim().is_empty() {
                candidate = candidate.with_owned_path(owned_path.trim());
            }
        }
        input = input.with_candidate(candidate);
    }

    Ok(input)
}

// wa.mission_objective_plan tool
pub(super) struct WaMissionObjectivePlanTool;

impl ToolHandler for WaMissionObjectivePlanTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.mission_objective_plan".to_string(),
            description: Some(
                "Compile an operator objective into a dry-run mission plan without mutating panes, services, or Beads (robot parity)"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "objective": { "type": "string", "description": "Operator objective to plan" },
                    "strictness": { "type": "string", "enum": ["advisory", "normal", "strict"], "default": "normal" },
                    "target_bead": { "type": "string", "description": "Ready Beads id to rank" },
                    "candidate_id": { "type": "string", "description": "Planner candidate id when no Beads target exists" },
                    "candidate_title": { "type": "string", "description": "Human label for the candidate" },
                    "owned_paths": { "type": "array", "items": { "type": "string" } },
                    "dirty_paths": { "type": "array", "items": { "type": "string" } },
                    "source_unavailable": { "type": "array", "items": { "type": "string" } },
                    "source_degraded": { "type": "array", "items": { "type": "string" } },
                    "source_stale": { "type": "array", "items": { "type": "string" } },
                    "proof_availability": { "type": "string", "enum": ["not-required", "available", "blocked", "unavailable"], "default": "available" },
                    "capacity_posture": { "type": "string", "enum": ["admit", "defer", "pause", "unknown"], "default": "admit" },
                    "dependency_blocked": { "type": "boolean", "default": false },
                    "active_assignee": { "type": "string" },
                    "active_age_seconds": { "type": "integer", "minimum": 0 },
                    "stale_after_seconds": { "type": "integer", "minimum": 60 },
                    "testing_skill_lane": { "type": "boolean", "default": false },
                    "generated_at_ms": { "type": "integer", "minimum": 0 },
                    "explain_step": { "type": "string" },
                    "explain_reason": { "type": "string" },
                    "execute": { "type": "boolean", "default": false, "description": "Always rejected; objective plans are dry-run only" }
                },
                "required": ["objective"],
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
        let params: MissionObjectivePlanParams = match serde_json::from_value(arguments) {
            Ok(parsed) => parsed,
            Err(err) => {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    Some(
                        "Expected object with objective and optional objective-plan hints"
                            .to_string(),
                    ),
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        };
        let explain_step = params.explain_step.clone();
        let explain_reason = params.explain_reason.clone();

        let result = build_mcp_mission_objective_plan_input(params).map(|input| {
            let plan = plan_mission_objective(&input);
            build_mission_objective_plan_surface_data(
                plan,
                explain_step.as_deref(),
                explain_reason.as_deref(),
            )
        });

        match result {
            Ok(data) => envelope_to_content(McpEnvelope::success(data, elapsed_ms(start))),
            Err(err) => envelope_to_content(McpEnvelope::<()>::error(
                err.code,
                err.message,
                err.hint,
                elapsed_ms(start),
            )),
        }
    }
}

fn parse_operating_envelope_scenario(
    value: Option<&str>,
) -> std::result::Result<OperatingEnvelopeScenario, McpToolError> {
    value.map_or(Ok(OperatingEnvelopeScenario::Current), |raw| {
        OperatingEnvelopeScenario::from_token(raw).ok_or_else(|| {
            McpToolError::new(
                MCP_ERR_INVALID_ARGS,
                format!("unknown operating-envelope scenario '{raw}'"),
                Some("Use one of: current, healthy, degraded, blocked, emergency.".to_string()),
            )
        })
    })
}

fn parse_operating_envelope_surface(
    value: Option<&str>,
    explain_reason: Option<&str>,
) -> std::result::Result<OperatingEnvelopeSurface, McpToolError> {
    value.map_or_else(
        || {
            Ok(if explain_reason.is_some() {
                OperatingEnvelopeSurface::Explain
            } else {
                OperatingEnvelopeSurface::Status
            })
        },
        |raw| {
            OperatingEnvelopeSurface::from_token(raw).ok_or_else(|| {
                McpToolError::new(
                    MCP_ERR_INVALID_ARGS,
                    format!("unknown operating-envelope surface '{raw}'"),
                    Some("Use status or explain.".to_string()),
                )
            })
        },
    )
}

fn build_mcp_operating_envelope_surface(
    params: OperatingEnvelopeParams,
) -> std::result::Result<serde_json::Value, McpToolError> {
    if params.execute {
        return Err(McpToolError::new(
            MCP_ERR_INVALID_ARGS,
            "wa.operating_envelope is dry-run only and cannot execute actions".to_string(),
            Some(
                "Remove execute=true; this tool only returns read-only envelope status."
                    .to_string(),
            ),
        ));
    }

    let explain_reason = mcp_non_empty_option(params.explain_reason, "explain_reason")?;
    let scenario = parse_operating_envelope_scenario(params.scenario.as_deref())?;
    let surface =
        parse_operating_envelope_surface(params.surface.as_deref(), explain_reason.as_deref())?;
    let generated_at_ms = params.generated_at_ms.unwrap_or_else(now_ms);
    let envelope_id = mcp_non_empty_option(params.envelope_id, "envelope_id")?
        .unwrap_or_else(|| format!("wa-operating-envelope-{}", scenario.as_str()));
    let objective_id = mcp_non_empty_option(params.objective_id, "objective_id")?
        .unwrap_or_else(|| "operator.current_safety".to_string());
    let input =
        operating_envelope_input_for_scenario(generated_at_ms, envelope_id, objective_id, scenario);
    let plan = plan_operating_envelope(input);
    Ok(build_operating_envelope_surface_data(
        &plan,
        surface,
        "mcp.operating_envelope",
        explain_reason.as_deref(),
    ))
}

// wa.operating_envelope tool
pub(super) struct WaOperatingEnvelopeTool;

impl ToolHandler for WaOperatingEnvelopeTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "wa.operating_envelope".to_string(),
            description: Some(
                "Read the dry-run swarm operating envelope and optional reason-code explanation"
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scenario": {
                        "type": "string",
                        "enum": ["current", "healthy", "degraded", "blocked", "emergency"],
                        "default": "current",
                        "description": "Deterministic redacted source scenario; current fails closed when live collectors are unavailable"
                    },
                    "surface": {
                        "type": "string",
                        "enum": ["status", "explain"],
                        "default": "status"
                    },
                    "explain_reason": {
                        "type": "string",
                        "description": "Reason code to drill into when surface=explain"
                    },
                    "envelope_id": { "type": "string" },
                    "objective_id": { "type": "string" },
                    "generated_at_ms": { "type": "integer", "minimum": 0 },
                    "execute": {
                        "type": "boolean",
                        "default": false,
                        "description": "Always rejected; operating-envelope surfaces are read-only"
                    }
                },
                "additionalProperties": false
            }),
            output_schema: None,
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "robot".to_string(),
                "swarm".to_string(),
                "operating-envelope".to_string(),
            ],
            annotations: None,
        }
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let params: OperatingEnvelopeParams = if arguments.is_null() {
            OperatingEnvelopeParams::default()
        } else {
            match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        Some(
                            "Expected object with optional scenario, surface, and explain_reason"
                                .to_string(),
                        ),
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        };

        match build_mcp_operating_envelope_surface(params) {
            Ok(data) => envelope_to_content(McpEnvelope::success(data, elapsed_ms(start))),
            Err(err) => envelope_to_content(McpEnvelope::<()>::error(
                err.code,
                err.message,
                err.hint,
                elapsed_ms(start),
            )),
        }
    }
}

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
    #[cfg(test)]
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

        let requested_at_ms = mcp_now_ms_i64();
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
    #[cfg(test)]
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

        let requested_at_ms = mcp_now_ms_i64();
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
    #[cfg(test)]
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

        let requested_at_ms = mcp_now_ms_i64();
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
    #[cfg(test)]
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

        // br-ft-wztvw: emptiness + size bounds for free-text fields.
        if let Some(note) = params.note.as_deref() {
            if let Err(err) = mcp_types::validate_event_mutation_string(
                note,
                "note",
                mcp_types::MAX_EVENT_NOTE_BYTES,
            ) {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        }
        if let Some(by) = params.by.as_deref() {
            if let Err(err) = mcp_types::validate_event_mutation_string(
                by,
                "by",
                mcp_types::MAX_EVENT_ACTOR_BYTES,
            ) {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

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

            let ts = mcp_now_ms_i64();
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
            // br-ft-pgjat: route silent failure through observability counter.
            record_event_mutation_audit_or_log(&storage, &audit_cx, audit, "wa.events_annotate")
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
    #[cfg(test)]
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

        // br-ft-wztvw: emptiness + size bounds for free-text fields.
        if let Some(state) = params.state.as_deref() {
            if let Err(err) = mcp_types::validate_event_mutation_string(
                state,
                "state",
                mcp_types::MAX_EVENT_TRIAGE_STATE_BYTES,
            ) {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        }
        if let Some(by) = params.by.as_deref() {
            if let Err(err) = mcp_types::validate_event_mutation_string(
                by,
                "by",
                mcp_types::MAX_EVENT_ACTOR_BYTES,
            ) {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
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
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;

            let changed = storage
                .set_event_triage_state(params.event_id, params.state.clone(), params.by.clone())
                .await?;

            let ts = mcp_now_ms_i64();
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
            // br-ft-pgjat: route silent failure through observability counter.
            record_event_mutation_audit_or_log(&storage, &audit_cx, audit, "wa.events_triage")
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
    #[cfg(test)]
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

        // br-ft-wztvw: emptiness + size bounds for label and actor.
        for (value, field) in [
            (params.add.as_deref(), "add"),
            (params.remove.as_deref(), "remove"),
        ] {
            if let Some(v) = value {
                if let Err(err) = mcp_types::validate_event_mutation_string(
                    v,
                    field,
                    mcp_types::MAX_EVENT_LABEL_BYTES,
                ) {
                    let envelope = McpEnvelope::<()>::error(
                        MCP_ERR_INVALID_ARGS,
                        format!("Invalid params: {err}"),
                        None,
                        elapsed_ms(start),
                    );
                    return envelope_to_content(envelope);
                }
            }
        }
        if let Some(by) = params.by.as_deref() {
            if let Err(err) = mcp_types::validate_event_mutation_string(
                by,
                "by",
                mcp_types::MAX_EVENT_ACTOR_BYTES,
            ) {
                let envelope = McpEnvelope::<()>::error(
                    MCP_ERR_INVALID_ARGS,
                    format!("Invalid params: {err}"),
                    None,
                    elapsed_ms(start),
                );
                return envelope_to_content(envelope);
            }
        }

        // br-ft-wdb0q: `wa.events_label` is a three-way API
        // (add | remove | list). Only add/remove mutate state;
        // list reads annotations. Apply the mutation policy gate
        // only on the mutating branches so a read-only listing
        // request stays available even when the operator has
        // tightened mutation policy.
        let is_mutation = params.add.is_some() || params.remove.is_some();
        if is_mutation {
            if let Some(deny) = mcp_authorize_mcp_mutation(
                self.config.as_ref(),
                &self.policy_rate_limiter,
                "wa.events_label",
                "event.label",
                start,
            ) {
                return deny;
            }
        }

        let db_path = Arc::clone(&self.db_path);
        let runtime = CompatRuntimeBuilder::current_thread()
            .build()
            .map_err(|e| McpError::internal_error(format!("MCP runtime init failed: {e}")))?;

        let result: crate::Result<McpEventMutationData> = runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await?;
            let ts = mcp_now_ms_i64();

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
                // br-ft-pgjat: route silent failure through observability counter.
                record_event_mutation_audit_or_log(&storage, &audit_cx, audit, "wa.events_label")
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
                // br-ft-pgjat: route silent failure through observability counter.
                record_event_mutation_audit_or_log(&storage, &audit_cx, audit, "wa.events_label")
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
        ActionKind, ActorKind, CASS_TIMEOUT_SECS_MAX, CASS_TIMEOUT_SECS_MIN, CompatRuntime,
        CompatRuntimeBuilder, Config, Content, MAX_MCP_ACCOUNT_SERVICE_BYTES,
        MAX_MCP_CASS_AGENT_FILTER_BYTES, MAX_MCP_CASS_QUERY_BYTES, MAX_MCP_RULES_AGENT_TYPE_BYTES,
        MAX_MCP_RULES_TEST_TEXT_BYTES, MAX_MCP_SEARCH_QUERY_BYTES,
        MAX_MCP_STATE_AGENT_FILTER_BYTES, MAX_MCP_SUBMIT_IDEMPOTENCY_KEY_BYTES,
        MAX_MCP_WAIT_PATTERN_BYTES, MAX_MCP_WAIT_TIMEOUT_SECS, MAX_SEND_TEXT_BYTES, McpContext,
        PaneCapabilities, PaneFilterConfig, PolicySurface, StorageHandle, Tool, ToolHandler,
        WaAccountsRefreshTool, WaAccountsTool, WaAttentionTool, WaAwaitEventTool, WaCassSearchTool,
        WaCassStatusTool, WaCassViewTool, WaDomTool, WaEventsAnnotateTool, WaEventsLabelTool,
        WaEventsTool, WaEventsTriageTool, WaGetTextTool, WaMissionAbortTool, WaMissionExplainTool,
        WaMissionObjectivePlanTool, WaMissionPauseTool, WaMissionResumeTool, WaMissionStateTool,
        WaOperatingEnvelopeTool, WaRehearsalScoreTool, WaReleaseTool, WaReservationsTool,
        WaReserveTool, WaRulesListTool, WaRulesTestTool, WaSearchTool, WaSendTool, WaStateTool,
        WaSteerPlanTool, WaTxPlanTool, WaTxRollbackTool, WaTxRunTool, WaTxShowTool, WaWaitForTool,
        WaWorkflowRunTool, WaWorkflowStatusTool, accounts_refresh_policy_input,
        audit_mcp_policy_denial_async, authorize_mcp_policy_call, build_mcp_shared_rate_limiter,
        build_policy_engine_with_shared_rate_limiter, mcp_event_mutation_decision_context,
        mcp_get_text_policy_input, mcp_load_mission_tx_contract_from_path, mcp_now_ms_i64,
        mcp_release_pane_policy_input, mcp_reserve_pane_policy_input,
        mcp_search_output_policy_input, mcp_send_text_policy_input, mcp_workflow_run_policy_input,
        merge_distributed_remote_mcp_states, redact_mcp_output_secrets,
        redact_mcp_pane_state_fields, redact_mcp_wait_pattern_for_output,
        serialize_mcp_audit_decision_context, tx_run_test_wezterm_override_slot,
        tx_run_wezterm_handle, validate_cass_timeout_secs,
    };
    use crate::mcp::mcp_types::{IpcPaneState, McpPaneState, StateParams};
    use crate::mcp::set_mcp_test_pane_state_override;
    use crate::mcp_error::{
        MCP_ERR_CASS, MCP_ERR_CONFIG, MCP_ERR_INVALID_ARGS, MCP_ERR_POLICY,
        MCP_ERR_REMOTE_TEXT_UNAVAILABLE, MCP_ERR_STORAGE, MCP_ERR_TIMEOUT, MCP_ERR_WORKFLOW,
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

    fn redaction_test_prefix() -> String {
        ["s", "k", "-ant", "-api03", "-"].concat()
    }

    fn redaction_test_token() -> String {
        [
            redaction_test_prefix().as_str(),
            "abcdefghijklmnopqrstuvwxyz",
            "12345678901234567890",
        ]
        .concat()
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

    fn workspace_tempdir() -> TempDir {
        tempfile::tempdir_in(std::env::current_dir().expect("current workspace directory"))
            .expect("workspace-scoped temporary directory")
    }

    fn safe_test_ipc_pane_state(pane_id: u64) -> IpcPaneState {
        IpcPaneState {
            pane_id,
            known: true,
            observed: Some(true),
            alt_screen: Some(false),
            last_status_at: Some(1_700_000_000_000),
            in_gap: Some(false),
            cursor_alt_screen: Some(false),
            reason: None,
        }
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

    fn tx_run_test_wezterm_override_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct TxRunWeztermOverrideGuard {
        _serialization_guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for TxRunWeztermOverrideGuard {
        fn drop(&mut self) {
            super::set_tx_contract_post_lock_test_hook(None);
            super::set_tx_contract_post_auth_test_hook(None);
            super::set_tx_contract_workspace_test_root(None);
            set_tx_run_test_wezterm_override(None);
        }
    }

    fn install_tx_contract_post_lock_test_hook(hook: impl FnOnce() + 'static) {
        super::set_tx_contract_post_lock_test_hook(Some(Box::new(hook)));
    }

    fn install_tx_contract_post_auth_test_hook(hook: impl FnOnce() + 'static) {
        super::set_tx_contract_post_auth_test_hook(Some(Box::new(hook)));
    }

    fn install_tx_run_mock_wezterm() -> (TxRunWeztermOverrideGuard, Arc<crate::wezterm::MockWezterm>)
    {
        let serialization_guard = tx_run_test_wezterm_override_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mock = Arc::new(crate::wezterm::MockWezterm::new());
        let handle: crate::wezterm::WeztermHandle = mock.clone();
        set_tx_run_test_wezterm_override(Some(handle));
        (
            TxRunWeztermOverrideGuard {
                _serialization_guard: serialization_guard,
            },
            mock,
        )
    }

    fn lock_tx_run_test_wezterm_override() -> TxRunWeztermOverrideGuard {
        let serialization_guard = tx_run_test_wezterm_override_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_tx_run_test_wezterm_override(None);
        TxRunWeztermOverrideGuard {
            _serialization_guard: serialization_guard,
        }
    }

    /// Seed panes 1..=3 as real, prompt-active prepare targets.
    ///
    /// "Real" means every evidence source the prepare gates consult is
    /// populated: a fresh observed `panes` row (liveness), an OSC-133 prompt
    /// marker segment (the `PromptActive` precondition), a watcher pane-state
    /// override reporting normal screen with no capture gap (the untrusted
    /// MCP actor's alt-screen policy gate), and a live mock mux pane (step
    /// dispatch). Callers must hold the returned override guards for the
    /// duration of the tool call, binding them AFTER the wezterm override
    /// guard so they drop while the tx serialization mutex is still held.
    #[must_use]
    fn seed_tx_run_real_targets(
        db_path: &Path,
        mock: &Arc<crate::wezterm::MockWezterm>,
    ) -> Vec<crate::mcp::McpTestPaneStateOverrideGuard> {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage should open");
            let seen_at = mcp_now_ms_i64();
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
                storage
                    .append_segment(pane_id, "\u{1b}]133;A\u{7}$ ", None)
                    .await
                    .expect("prompt evidence segment should seed");
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
        (1..=3u64)
            .map(|pane_id| set_mcp_test_pane_state_override(safe_test_ipc_pane_state(pane_id)))
            .collect()
    }

    fn tx_run_mock_pane_content(mock: &Arc<crate::wezterm::MockWezterm>, pane_id: u64) -> String {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            mock.pane_state(pane_id)
                .await
                .unwrap_or_else(|| panic!("mock pane {pane_id} should exist"))
                .content
        })
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
        let outcome = match state {
            MissionTxState::Committed => TxOutcome::Committed,
            MissionTxState::Failed => TxOutcome::Failed,
            MissionTxState::Compensated | MissionTxState::RolledBack => TxOutcome::Compensated,
            MissionTxState::Draft
            | MissionTxState::Planned
            | MissionTxState::Prepared
            | MissionTxState::Committing
            | MissionTxState::Compensating => TxOutcome::Pending,
        };
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
            outcome,
            receipts: Vec::new(),
        }
    }

    fn write_tx_contract(dir: &TempDir, state: MissionTxState) -> std::path::PathBuf {
        super::set_tx_contract_workspace_test_root(Some(dir.path().to_path_buf()));
        let path = dir.path().join("tx-contract.json");
        let contract = sample_tx_contract(state);
        std::fs::write(&path, serde_json::to_vec_pretty(&contract).unwrap()).unwrap();
        path
    }

    fn write_tx_contract_with_proven_commit_receipts(
        dir: &TempDir,
        db_path: &Path,
        fail_step: Option<&str>,
    ) -> std::path::PathBuf {
        super::set_tx_contract_workspace_test_root(Some(dir.path().to_path_buf()));
        let path = dir.path().join("tx-contract-with-proven-receipts.json");
        let config = config_with_db_path(db_path);
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        let storage = runtime
            .block_on(async { StorageHandle::new(&db_path.to_string_lossy()).await })
            .expect("tx fixture storage should open");
        let rate_limiter = build_mcp_shared_rate_limiter(config.as_ref());
        let policy_engine =
            build_policy_engine_with_shared_rate_limiter(config.as_ref(), false, rate_limiter);
        let workspace_id =
            super::resolve_workspace_id(config.as_ref()).expect("fixture workspace should resolve");
        let prepare_context = crate::plan::TxPrepareEvaluationContext::new(workspace_id)
            .with_surface(PolicySurface::Mcp)
            .with_actor(crate::policy::ActorKind::Mcp);
        let approvals = crate::plan::StorageBackedPrepareApprovalChecker::new(Some(&storage));
        let mut contract = sample_tx_contract(MissionTxState::Planned);
        let resolved_capabilities = runtime.block_on(super::resolve_tx_prepare_capabilities(
            config.as_ref(),
            &storage,
            &contract,
        ));
        let targets = crate::plan::StorageBackedPrepareTargetLookup::new(None, Some(&storage))
            .with_resolved_capabilities(resolved_capabilities);
        let executor = crate::tx_execution::PaneStepExecutor::new(
            tx_run_wezterm_handle(config.as_ref()),
            std::cell::RefCell::new(policy_engine),
            approvals,
            targets,
            prepare_context,
        );
        let engine = crate::tx_execution::TxExecutionEngine::new(
            executor,
            crate::tx_execution::TxExecutionConfig {
                auto_compensate: false,
                fail_step: fail_step.map(str::to_string),
                ..crate::tx_execution::TxExecutionConfig::default()
            },
        );
        let mut store = crate::tx_idempotency::IdempotencyStore::open(
            &dir.path().join(".ft"),
            crate::tx_idempotency::IdempotencyPolicy::default(),
        )
        .expect("durable tx fixture store should open");
        let execution = engine
            .execute_with_store(&mut contract, &mut store, mcp_now_ms_i64())
            .expect("proof-linked fixture execution should complete");

        assert!(execution.compensation_report.is_none());
        if fail_step.is_some() {
            assert_eq!(execution.final_state, MissionTxState::Failed);
            assert_eq!(contract.lifecycle_state, MissionTxState::Failed);
            assert_eq!(contract.outcome, TxOutcome::Failed);
            assert_eq!(
                execution
                    .commit_report
                    .as_ref()
                    .expect("partial commit report")
                    .committed_count,
                1
            );
        } else {
            assert_eq!(execution.final_state, MissionTxState::Committed);
            assert_eq!(contract.lifecycle_state, MissionTxState::Committed);
            assert_eq!(contract.outcome, TxOutcome::Committed);
            assert_eq!(
                execution
                    .commit_report
                    .as_ref()
                    .expect("full commit report")
                    .committed_count,
                3
            );
        }
        assert_eq!(contract.receipts.len(), 3);
        std::fs::write(&path, serde_json::to_vec_pretty(&contract).unwrap()).unwrap();
        path
    }

    /// Builds receipt-only commit claims for argument-preflight tests.
    ///
    /// This fixture deliberately has no matching durable execution proofs, so
    /// callers must reject the request before rollback execution can begin.
    fn write_tx_contract_with_receipt_only_commit_claims(dir: &TempDir) -> std::path::PathBuf {
        super::set_tx_contract_workspace_test_root(Some(dir.path().to_path_buf()));
        let path = dir
            .path()
            .join("tx-contract-with-receipt-only-commit-claims.json");
        let mut contract = sample_tx_contract(MissionTxState::Committed);
        let commit_report = execute_commit_phase(
            &sample_tx_contract(MissionTxState::Committing),
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
                    success: true,
                    reason_code: "commit_step_succeeded".to_string(),
                    error_code: None,
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
        .expect("full commit report");
        assert_eq!(commit_report.committed_count, 3);
        assert_eq!(commit_report.receipts.len(), 3);
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

    /// Collect definitions for all 35 tools. Guarantees no panics during construction.
    fn all_definitions() -> Vec<Tool> {
        let db = db_path();
        let cfg = config();
        vec![
            WaRulesListTool.definition(),
            WaRulesTestTool.definition(),
            WaCassSearchTool.definition(),
            WaCassViewTool.definition(),
            WaCassStatusTool.definition(),
            WaStateTool::new(Arc::new(Config::default()), PaneFilterConfig::default(), None).definition(),
            WaGetTextTool::new(Arc::clone(&cfg), Some(Arc::clone(&db))).definition(),
            WaWaitForTool::new(Arc::clone(&cfg), Some(Arc::clone(&db))).definition(),
            WaSearchTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaEventsTool::new(Arc::clone(&db)).definition(),
            WaAwaitEventTool::new(Arc::clone(&db)).definition(),
            WaSendTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaWorkflowRunTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaWorkflowStatusTool::new(Arc::clone(&db)).definition(),
            WaTxPlanTool::new(Arc::clone(&cfg)).definition(),
            WaTxShowTool::new(Arc::clone(&cfg)).definition(),
            WaTxRunTool::new(Arc::clone(&cfg)).definition(),
            WaTxRollbackTool::new(Arc::clone(&cfg)).definition(),
            WaReservationsTool::new(Arc::clone(&db)).definition(),
            WaReserveTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaReleaseTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaAccountsTool::new(Arc::clone(&db)).definition(),
            WaAccountsRefreshTool::new(Arc::clone(&cfg), Arc::clone(&db)).definition(),
            WaMissionObjectivePlanTool.definition(),
            WaOperatingEnvelopeTool.definition(),
            WaAttentionTool.definition(),
            WaRehearsalScoreTool::new(Arc::clone(&cfg)).definition(),
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
    fn tool_count_is_35() {
        assert_eq!(all_definitions().len(), 35);
    }

    #[test]
    fn await_event_enforces_condition_set_bounds_without_relying_on_client_schema_validation() {
        let (_dir, db_path) = temp_db_path();
        let tool = WaAwaitEventTool::new(Arc::clone(&db_path));
        let too_many = (0..=super::MCP_AWAIT_EVENT_CONDITION_SET_MAX)
            .map(|index| format!("rule:rule.{index}"))
            .collect::<Vec<_>>();

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "any": too_many,
                    "timeout_secs": 1,
                    "poll_interval_ms": 10
                }),
            )
            .expect("oversized condition set should return an invalid-args envelope"),
        );
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|message| message.contains("at most 16")),
            "server-side bound must remain explicit: {envelope}"
        );
    }

    #[test]
    fn await_event_claim_direct_handler_dispatch_fails_closed_without_a_lease() {
        let (_dir, db_path) = temp_db_path();
        let event_id = seed_event(db_path.as_ref().as_path());
        let tool = WaAwaitEventTool::new(Arc::clone(&db_path));

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "any": ["rule:codex.*"],
                    "cursor": 0,
                    "timeout_secs": 1,
                    "poll_interval_ms": 10,
                    "claim": true
                }),
            )
            .expect("direct handler dispatch should return a fail-closed envelope"),
        );
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_CONFIG);
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|message| message.contains("acknowledgment-aware")),
            "direct claim rejection must explain the missing delivery boundary: {envelope}"
        );

        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db_path.to_string_lossy()).await.unwrap();
            let lease = match storage
                .reserve_event_delivery(event_id, std::time::Duration::from_secs(1))
                .await
                .expect("probe event reservation after rejected direct dispatch")
            {
                crate::storage::EventDeliveryReservation::Acquired(lease) => lease,
                other => panic!(
                    "rejected direct dispatch must leave the event unhandled and unleased; got {other:?}"
                ),
            };
            assert!(
                storage
                    .release_event_delivery(&lease)
                    .await
                    .expect("release probe lease")
            );
            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn await_event_threads_pre_cancelled_request_context_into_storage() {
        let (_dir, db_path) = temp_db_path();
        let tool = WaAwaitEventTool::new(Arc::clone(&db_path));
        let cx = crate::cx::Cx::for_testing();
        cx.set_cancel_requested(true);
        let context = McpContext::new(cx, 2);
        let started = Instant::now();

        let envelope = parse_json_content(
            tool.call(
                &context,
                serde_json::json!({
                    "any": ["rule:never.matches"],
                    "timeout_secs": 300,
                    "poll_interval_ms": 30_000
                }),
            )
            .expect("pre-cancelled await should return a fail-closed envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_TIMEOUT);
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("Retry")),
            "pre-cancellation must remain explicitly retryable: {envelope}"
        );
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|message| message.contains("cancelled")),
            "cancellation must remain visible in the MCP error: {envelope}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "a pre-cancelled request must not enter the 300-second poll"
        );
    }

    #[test]
    fn await_event_observes_mid_poll_request_cancellation_promptly() {
        let (_dir, db_path) = temp_db_path();
        let tool = WaAwaitEventTool::new(Arc::clone(&db_path));
        let cx = crate::cx::Cx::for_testing();
        let cancel_cx = cx.clone();
        let context = McpContext::new(cx, 3);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            cancel_cx.set_cancel_requested(true);
        });
        let started = Instant::now();

        let envelope = parse_json_content(
            tool.call(
                &context,
                serde_json::json!({
                    "any": ["rule:never.matches"],
                    "timeout_secs": 300,
                    "poll_interval_ms": 30_000
                }),
            )
            .expect("cancelled await should return a fail-closed envelope"),
        );
        canceller.join().expect("join request canceller");

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_TIMEOUT);
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("Retry")),
            "mid-poll cancellation must remain explicitly retryable: {envelope}"
        );
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|message| message.contains("cancel")),
            "mid-poll cancellation must remain visible: {envelope}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "cancellation polling must not wait for the 30-second DB poll interval"
        );
    }

    #[test]
    fn wa_attention_tool_is_read_only_and_explains_inline_input() {
        let tool = WaAttentionTool;
        let status = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "surface": "status",
                    "generated_at_ms": 1_770_000_300_000u64,
                    "workspace": "/repo"
                }),
            )
            .expect("wa.attention status should respond"),
        );
        assert_eq!(status["ok"], true);
        assert_eq!(status["data"]["surface"], "status");
        assert_eq!(status["data"]["dry_run"], true);
        assert_eq!(status["data"]["live_mutation_allowed"], false);
        assert_eq!(status["data"]["side_effects_executed"], false);
        assert_eq!(status["data"]["degraded_mode"]["active"], true);
        assert_eq!(
            status["data"]["mcp_resources"][0]["uri"],
            crate::attention_router::ATTENTION_ROUTER_MCP_CURRENT_URI
        );

        let input = crate::attention_router::AttentionRouterSourceAdapterInput::new(
            1_770_000_300_001,
            "/repo",
        )
        .with_observation(
            crate::attention_router::AttentionRouterSourceObservation::new(
                "beads.ready",
                crate::attention_router::AttentionRouterSourceKind::Beads,
                crate::attention_router::AttentionRouterSourceHealth::Available,
                "br ready --json",
                "ready work",
            )
            .with_fact(
                crate::attention_router::AttentionRouterSourceFact::new(
                    crate::attention_router::AttentionRouterSourceFactKind::BeadsReady,
                    "docs-only ready static slice",
                )
                .with_bead_id("ft-docs")
                .with_reason_code("beads.ready_available"),
            ),
        );
        let explain = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "surface": "explain",
                    "item_id": "attention:ready_now:beads_ready:ft-docs",
                    "input": input
                }),
            )
            .expect("wa.attention explain should respond"),
        );
        assert_eq!(explain["ok"], true);
        assert_eq!(explain["data"]["surface"], "explain");
        assert_eq!(explain["data"]["explanation"]["matched"], true);
        assert_eq!(
            explain["data"]["selected_item"]["item_id"],
            "attention:ready_now:beads_ready:ft-docs"
        );
        assert_eq!(
            explain["data"]["selected_item"]["recommended_action"]["mutates"],
            false
        );
    }

    #[test]
    fn wa_rehearsal_score_tool_scores_inline_manifest_and_explains_log() {
        let tool = WaRehearsalScoreTool::new(config());
        let manifest: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/demo-lab/manifest.v1.json"
        )))
        .expect("demo manifest fixture should parse as JSON");
        let response = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "surface": "explain",
                    "manifest": manifest,
                    "rehearsal_id": "mcp-rehearsal-test",
                    "scenario_id": "demo_lab.manifest"
                }),
            )
            .expect("wa.rehearsal_score explain should respond"),
        );

        assert_eq!(response["ok"], true);
        assert_eq!(
            response["data"]["contract_id"],
            crate::rehearsal_score::REHEARSAL_SCORE_SURFACE_CONTRACT_ID
        );
        assert_eq!(response["data"]["surface"], "explain");
        assert_eq!(response["data"]["raw_pane_content_stored"], false);
        assert_eq!(response["data"]["live_mutation_allowed"], false);
        assert_eq!(response["data"]["side_effects_executed"], false);
        assert_eq!(
            response["data"]["receipt"]["aggregate_verdict"],
            "missing_evidence"
        );
        assert!(
            response["data"]["evaluation_log"]
                .as_array()
                .is_some_and(|log| !log.is_empty())
        );
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
            // ft-kccj8: a distinct pane id (42xx convention) + the IPC
            // pane-state override, like every sibling that asserts
            // "allowed" — without it, alt_screen resolves to None (no
            // watcher socket) and the fail-closed policy.alt_screen_unknown
            // gate fires before the rate limiter under test. Distinct ids
            // matter: the override map is process-global and its guard
            // removes by pane_id.
            let pane_id = 4_205;
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 2;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);
            let shared_rate_limiter = build_mcp_shared_rate_limiter(cfg.as_ref());

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(pane_id).await;
            let _pane_state = set_mcp_test_pane_state_override(safe_test_ipc_pane_state(pane_id));
            // ft-kccj8: the over-limit call attaches an approval token, and
            // approval_tokens.pane_id REFERENCES panes(pane_id) — the pane
            // row must exist in storage or the insert dies on the FK.
            {
                let storage = StorageHandle::new(&db.to_string_lossy())
                    .await
                    .expect("storage should open");
                storage
                    .upsert_pane(crate::storage::PaneRecord {
                        pane_id,
                        pane_uuid: None,
                        domain: "local".to_string(),
                        window_id: None,
                        tab_id: None,
                        title: Some("wa-send-rate-limit".to_string()),
                        cwd: None,
                        tty_name: None,
                        first_seen_at: 1_700_000_000_000,
                        last_seen_at: 1_700_000_000_000,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    })
                    .await
                    .expect("pane row should seed");
                let _ = storage.shutdown().await;
            }
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
                                "pane_id": pane_id,
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
                            "pane_id": pane_id,
                            "text": "echo over-limit"
                        }),
                    )
                    .expect("wa.send rate-limited call"),
            );

            assert_eq!(
                envelope["ok"], true,
                "wa.send fast-path envelope: {envelope:?}"
            );
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
            // ft-kccj8: distinct pane id + IPC pane-state override — see
            // wa_send_rate_limit_is_shared_across_tool_instances.
            let pane_id = 4_206;
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 1;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(pane_id).await;
            let _pane_state = set_mcp_test_pane_state_override(safe_test_ipc_pane_state(pane_id));
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
                            "pane_id": pane_id,
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
                        "pane_id": pane_id,
                        "text": "echo actual"
                    }),
                )
                .expect("first actual wa.send should still be allowed"),
            );

            assert_eq!(
                envelope["ok"], true,
                "wa.send write-level envelope: {envelope:?}"
            );
            assert_eq!(envelope["data"]["injection"]["status"], "allowed");
        });
    }

    #[test]
    fn wa_send_default_fast_path_omits_submit_receipt() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db) = temp_db_path();
            let pane_id = 4_201;
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 100;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(pane_id).await;
            let _pane_state = set_mcp_test_pane_state_override(safe_test_ipc_pane_state(pane_id));
            let tool = WaSendTool::with_wezterm_handle(
                Arc::clone(&cfg),
                Arc::clone(&db),
                mock as crate::wezterm::WeztermHandle,
            );

            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": pane_id,
                        "text": "echo fast"
                    }),
                )
                .expect("wa.send fast-path call"),
            );

            assert_eq!(
                envelope["ok"], true,
                "wa.send fast-path envelope: {envelope:?}"
            );
            assert_eq!(envelope["data"]["injection"]["status"], "allowed");
            assert!(
                envelope["data"].get("submit").is_none(),
                "default wa.send should not emit a submit receipt: {envelope:?}"
            );
        });
    }

    #[test]
    fn wa_send_submit_level_write_returns_submit_receipt() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db) = temp_db_path();
            let pane_id = 4_202;
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 100;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(pane_id).await;
            let _pane_state = set_mcp_test_pane_state_override(safe_test_ipc_pane_state(pane_id));
            let tool = WaSendTool::with_wezterm_handle(
                Arc::clone(&cfg),
                Arc::clone(&db),
                mock as crate::wezterm::WeztermHandle,
            );

            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": pane_id,
                        "text": "echo receipt",
                        "submit_level": "write"
                    }),
                )
                .expect("wa.send write-level submit call"),
            );

            assert_eq!(envelope["ok"], true);
            let submit = envelope["data"]["submit"]
                .as_object()
                .expect("submit receipt should be present");
            assert_eq!(submit["state"], serde_json::json!("submitted"));
            assert_eq!(submit["guarantee_level"], serde_json::json!("write"));
            assert_eq!(submit["guarantee_met"], serde_json::json!(true));
            assert_eq!(
                submit["idempotency_key"],
                serde_json::json!(
                    crate::robot_idempotency::send_text_key(pane_id, "echo receipt").to_string()
                )
            );
        });
    }

    #[test]
    fn wa_send_idempotency_key_replays_prior_receipt_without_resending() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db) = temp_db_path();
            let pane_id = 4_204;
            let text = "echo idempotent";
            let caller_key = "retry-step-1";
            let expected_key =
                crate::verified_submit::idempotency_key(pane_id, text, Some(caller_key));
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 100;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(pane_id).await;
            let _pane_state = set_mcp_test_pane_state_override(safe_test_ipc_pane_state(pane_id));
            let tool = WaSendTool::with_wezterm_handle(
                Arc::clone(&cfg),
                Arc::clone(&db),
                Arc::clone(&mock) as crate::wezterm::WeztermHandle,
            );

            let first = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": pane_id,
                        "text": text,
                        "idempotency_key": caller_key
                    }),
                )
                .expect("first idempotent wa.send call"),
            );

            assert_eq!(first["ok"], true, "first envelope: {first:?}");
            let first_submit = first["data"]["submit"]
                .as_object()
                .expect("first send should emit a submit receipt");
            assert_eq!(first_submit["state"], serde_json::json!("submitted"));
            assert_eq!(first_submit["guarantee_level"], serde_json::json!("write"));
            assert_eq!(
                first_submit["idempotency_key"],
                serde_json::json!(expected_key)
            );
            let content_after_first = mock
                .pane_state(pane_id)
                .await
                .expect("mock pane should still exist")
                .content;
            assert_eq!(
                content_after_first.matches(text).count(),
                1,
                "first send should inject exactly once"
            );

            let second = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": pane_id,
                        "text": text,
                        "idempotency_key": caller_key
                    }),
                )
                .expect("duplicate idempotent wa.send call"),
            );

            assert_eq!(second["ok"], true, "second envelope: {second:?}");
            assert_eq!(second["data"]["injection"]["status"], "allowed");
            assert_eq!(
                second["data"]["injection"]["decision"]["rule_id"],
                serde_json::json!("submit_idempotency.duplicate_noop")
            );
            let second_submit = second["data"]["submit"]
                .as_object()
                .expect("duplicate should replay the prior submit receipt");
            assert_eq!(second_submit["state"], serde_json::json!("submitted"));
            assert_eq!(second_submit["guarantee_level"], serde_json::json!("write"));
            assert_eq!(
                second_submit["idempotency_key"],
                serde_json::json!(expected_key)
            );
            let content_after_second = mock
                .pane_state(pane_id)
                .await
                .expect("mock pane should still exist")
                .content;
            assert_eq!(
                content_after_second, content_after_first,
                "duplicate replay must not send text to the pane again"
            );
        });
    }

    #[test]
    fn wa_send_verify_submit_defaults_to_submitted_level() {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (_dir, db) = temp_db_path();
            let pane_id = 4_203;
            let mut cfg = Config::default();
            cfg.safety.require_prompt_active = false;
            cfg.safety.rate_limit_per_pane = 100;
            cfg.safety.rate_limit_global = 100;
            let cfg = Arc::new(cfg);

            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(pane_id).await;
            let _pane_state = set_mcp_test_pane_state_override(safe_test_ipc_pane_state(pane_id));
            let tool = WaSendTool::with_wezterm_handle(
                Arc::clone(&cfg),
                Arc::clone(&db),
                mock as crate::wezterm::WeztermHandle,
            );

            let envelope = parse_json_content(
                tool.call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "pane_id": pane_id,
                        "text": "echo verified",
                        "verify_submit": true
                    }),
                )
                .expect("wa.send verified submit call"),
            );

            assert_eq!(envelope["ok"], true);
            let submit = envelope["data"]["submit"]
                .as_object()
                .expect("submit receipt should be present");
            assert_eq!(
                submit["state"],
                serde_json::json!("verification_unavailable")
            );
            assert_eq!(submit["guarantee_level"], serde_json::json!("submitted"));
            assert_eq!(submit["guarantee_met"], serde_json::json!(false));
            assert!(
                envelope["data"]["verification_error"]
                    .as_str()
                    .is_some_and(|error| error.contains("submit guarantee 'submitted' not met")),
                "expected submitted guarantee error, got {envelope:?}"
            );
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
            "wa.workflow_status",
            "wa.accounts",
            "wa.attention",
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
    fn workflow_status_definition_declares_filter_requirement() {
        let def = WaWorkflowStatusTool::new(db_path()).definition();

        assert_eq!(def.name, "wa.workflow_status");
        assert!(def.input_schema.get("anyOf").is_some());
        assert!(def.input_schema["properties"].get("execution_id").is_some());
        assert!(def.input_schema["properties"].get("pane_id").is_some());
        assert!(def.input_schema["properties"].get("active").is_some());
    }

    #[test]
    fn workflow_run_definition_does_not_advertise_ignored_force() {
        let def = WaWorkflowRunTool::new(config(), db_path()).definition();

        assert_eq!(def.name, "wa.workflow_run");
        assert_eq!(def.input_schema["additionalProperties"], false);
        assert!(def.input_schema["properties"].get("name").is_some());
        assert!(def.input_schema["properties"].get("pane_id").is_some());
        assert!(def.input_schema["properties"].get("dry_run").is_some());
        assert!(def.input_schema["properties"].get("force").is_none());
    }

    #[test]
    fn accounts_service_args_do_not_echo_malformed_values() {
        let redaction_sample = format!("{}account-service-fixture", redaction_test_prefix());
        let oversized = format!(
            "{redaction_sample}{}",
            "x".repeat(MAX_MCP_ACCOUNT_SERVICE_BYTES + 1)
        );

        let accounts_envelope = parse_json_content(
            WaAccountsTool::new(db_path())
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "service": oversized,
                    }),
                )
                .expect("accounts oversized service returns envelope"),
        );
        assert_eq!(accounts_envelope["ok"], false);
        assert_eq!(accounts_envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            accounts_envelope["error"]
                .as_str()
                .expect("accounts error string")
                .contains("max allowed")
        );
        assert!(
            !accounts_envelope
                .to_string()
                .contains(&redaction_test_prefix()),
            "wa.accounts oversized service leaked caller-supplied value"
        );

        let refresh_envelope = parse_json_content(
            WaAccountsRefreshTool::new(config(), db_path())
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "service": redaction_sample,
                    }),
                )
                .expect("accounts_refresh unknown service returns envelope"),
        );
        assert_eq!(refresh_envelope["ok"], false);
        assert_eq!(refresh_envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(refresh_envelope["error"].as_str(), Some("Unknown service"));
        assert!(
            !refresh_envelope
                .to_string()
                .contains(&redaction_test_prefix()),
            "wa.accounts_refresh unknown service leaked caller-supplied value"
        );
    }

    #[test]
    fn accounts_schema_declares_service_max_length() {
        let accounts = WaAccountsTool::new(db_path()).definition();
        assert_eq!(
            accounts.input_schema["properties"]["service"]["maxLength"].as_u64(),
            Some(MAX_MCP_ACCOUNT_SERVICE_BYTES as u64)
        );

        let refresh = WaAccountsRefreshTool::new(config(), db_path()).definition();
        assert_eq!(
            refresh.input_schema["properties"]["service"]["maxLength"].as_u64(),
            Some(MAX_MCP_ACCOUNT_SERVICE_BYTES as u64)
        );
    }

    #[test]
    fn workflow_status_requires_filter_param() {
        let tool = WaWorkflowStatusTool::new(db_path());

        let envelope = parse_json_content(
            tool.call(&test_mcp_context(), serde_json::json!({}))
                .expect("workflow_status missing-filter call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error_code"],
            crate::mcp_error::MCP_ERR_INVALID_ARGS
        );
        assert_eq!(
            envelope["error"],
            "Must provide execution_id, pane_id, or active=true"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .unwrap()
                .contains("execution_id, pane_id, or active=true")
        );
    }

    #[test]
    fn rules_list_malformed_args_redacts_serde_error_value() {
        let tool = WaRulesListTool;
        let redaction_sample = redaction_test_token();

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "agent_type": "codex",
                    "verbose": redaction_sample,
                }),
            )
            .expect("rules_list bad-arg call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["hint"],
            "Expected object with optional agent_type, verbose"
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "malformed wa.rules_list args leaked the caller-supplied secret"
        );
    }

    #[test]
    fn rules_list_unknown_agent_type_redacts_argument_value() {
        let tool = WaRulesListTool;
        let redaction_sample = redaction_test_token();

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "agent_type": redaction_sample,
                    "verbose": false,
                }),
            )
            .expect("rules_list unknown-agent call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "unknown wa.rules_list agent_type leaked the caller-supplied secret"
        );
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("[REDACTED]")
        );
    }

    #[test]
    fn rules_list_rejects_oversized_agent_type_without_echoing_value() {
        let tool = WaRulesListTool;
        let redaction_sample = redaction_test_token();
        let agent_type = format!(
            "{redaction_sample}{}",
            "x".repeat(MAX_MCP_RULES_AGENT_TYPE_BYTES + 1)
        );

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "agent_type": agent_type,
                    "verbose": false,
                }),
            )
            .expect("rules_list oversized-agent call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("max allowed")
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "oversized wa.rules_list agent_type leaked the caller-supplied value"
        );
    }

    #[test]
    fn rules_list_schema_declares_agent_type_max_length() {
        let def = WaRulesListTool.definition();
        assert_eq!(
            def.input_schema["properties"]["agent_type"]["maxLength"].as_u64(),
            Some(MAX_MCP_RULES_AGENT_TYPE_BYTES as u64)
        );
    }

    #[test]
    fn rules_test_malformed_args_redacts_serde_error_value() {
        let tool = WaRulesTestTool;
        let redaction_sample = redaction_test_token();

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "text": "plain pane output",
                    "trace": redaction_sample,
                }),
            )
            .expect("rules_test bad-arg call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["hint"],
            "Expected object with text (required), trace"
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "malformed-argument envelope leaked the caller-supplied secret"
        );
    }

    #[test]
    fn rules_test_rejects_oversized_text_without_echoing_text() {
        let tool = WaRulesTestTool;
        let redaction_sample = redaction_test_token();
        let text = format!(
            "{redaction_sample}{}",
            "x".repeat(MAX_MCP_RULES_TEST_TEXT_BYTES + 1)
        );

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "text": text,
                    "trace": true,
                }),
            )
            .expect("rules_test oversized-text call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("max allowed")
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "oversized wa.rules_test envelope leaked caller-supplied text"
        );
    }

    #[test]
    fn rules_test_schema_declares_text_max_length() {
        let def = WaRulesTestTool.definition();
        assert_eq!(
            def.input_schema["properties"]["text"]["maxLength"].as_u64(),
            Some(MAX_MCP_RULES_TEST_TEXT_BYTES as u64)
        );
    }

    #[test]
    fn mission_tool_names_stable() {
        let expected = [
            "wa.mission_objective_plan",
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
    fn mission_objective_plan_tool_returns_dry_run_surface() {
        let tool = WaMissionObjectivePlanTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "objective": "ship next safe slice",
                    "target_bead": "ft-auy2g.4",
                    "owned_paths": ["crates/frankenterm/src/main.rs"],
                    "source_unavailable": ["agent-mail"],
                    "generated_at_ms": 123,
                    "explain_step": "ft-auy2g.4"
                }),
            )
            .expect("mission objective plan call"),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["dry_run"], true);
        assert_eq!(envelope["data"]["side_effects_executed"], false);
        assert_eq!(
            envelope["data"]["contract_id"],
            crate::mission_objective_plan::MISSION_OBJECTIVE_PLAN_CONTRACT_ID
        );
        assert_eq!(envelope["data"]["explain"]["matched"], true);
        assert_eq!(envelope["data"]["plan"]["steps"][0]["target"], "ft-auy2g.4");
    }

    #[test]
    fn mission_objective_plan_tool_rejects_execute_attempt() {
        let tool = WaMissionObjectivePlanTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "objective": "ship next safe slice",
                    "execute": true
                }),
            )
            .expect("mission objective plan call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error_code"],
            crate::mcp_error::MCP_ERR_INVALID_ARGS
        );
        assert!(envelope["error"].as_str().unwrap().contains("dry-run only"));
    }

    #[test]
    fn operating_envelope_tool_returns_dry_run_status() {
        let tool = WaOperatingEnvelopeTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "scenario": "degraded",
                    "generated_at_ms": 123,
                }),
            )
            .expect("operating envelope call"),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["dry_run"], true);
        assert_eq!(envelope["data"]["side_effects_executed"], false);
        assert_eq!(envelope["data"]["live_mutation_allowed"], false);
        assert_eq!(envelope["data"]["raw_pane_content_stored"], false);
        assert_eq!(
            envelope["data"]["contract_id"],
            crate::operating_envelope::OPERATING_ENVELOPE_CONTRACT_ID
        );
        assert_eq!(envelope["data"]["surface"], "status");
        assert_eq!(
            envelope["data"]["summary"]["envelope_tier"].as_str(),
            Some("yellow")
        );
        assert!(
            envelope["data"]["safety_notice"]
                .as_str()
                .expect("safety notice")
                .contains("not permission")
        );
    }

    #[test]
    fn operating_envelope_tool_explains_reason_code() {
        let tool = WaOperatingEnvelopeTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "scenario": "blocked",
                    "surface": "explain",
                    "explain_reason": "rch.topology_preflight_failed",
                    "generated_at_ms": 123,
                }),
            )
            .expect("operating envelope explain call"),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["surface"], "explain");
        assert_eq!(envelope["data"]["explain"]["matched"], true);
        assert!(
            envelope["data"]["explain"]["entries"]
                .as_array()
                .expect("explain entries")
                .iter()
                .any(|entry| entry["scope"] == "source" || entry["scope"] == "evidence")
        );
    }

    #[test]
    fn operating_envelope_tool_rejects_execute_attempt() {
        let tool = WaOperatingEnvelopeTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "scenario": "healthy",
                    "execute": true
                }),
            )
            .expect("operating envelope call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error_code"],
            crate::mcp_error::MCP_ERR_INVALID_ARGS
        );
        assert!(envelope["error"].as_str().unwrap().contains("dry-run only"));
    }

    #[test]
    fn mission_state_tool_rejects_mission_state_filter_miss() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
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
    fn tx_run_wezterm_handle_preserves_test_override_with_config() {
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let expected: crate::wezterm::WeztermHandle = mock;

        let actual = tx_run_wezterm_handle(config().as_ref());

        assert!(
            Arc::ptr_eq(&actual, &expected),
            "the test override must take precedence over config-backed mux construction"
        );
    }

    #[test]
    fn tx_contract_save_failure_after_effects_marks_retry_unsafe() {
        for (completion_context, retry_tool, effect_label) in [
            (
                "Transaction execution completed",
                "wa.tx_run",
                "external transaction effects",
            ),
            (
                "Transaction compensation completed",
                "wa.tx_rollback",
                "compensation effects",
            ),
        ] {
            let err = super::mcp_tx_contract_save_failure_after_effects(
                super::McpToolError::new(
                    MCP_ERR_STORAGE,
                    "disk full".to_string(),
                    Some("Recovery artifact: /tmp/tx.recovery.tmp".to_string()),
                ),
                completion_context,
                retry_tool,
                effect_label,
            );

            assert_eq!(err.code, MCP_ERR_STORAGE);
            assert!(err.message.contains(completion_context));
            assert!(err.message.contains("disk full"));
            assert!(
                err.message
                    .contains(&format!("{effect_label} may already exist"))
            );
            let hint = err.hint.expect("unsafe-retry guidance");
            assert!(hint.contains(&format!("Do not retry {retry_tool}")));
            assert!(hint.contains("durable tx idempotency ledger"));
            assert!(hint.contains("transaction recovery artifact"));
            assert!(hint.contains("retrying now is unsafe"));
            assert!(hint.contains("/tmp/tx.recovery.tmp"));
        }
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
        let dir = workspace_tempdir();
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
        let dir = workspace_tempdir();
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
        let dir = workspace_tempdir();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let first = super::acquire_mcp_tx_contract_lock(dir.path(), &contract_path)
            .expect("first lock acquisition should succeed");
        let lock_dir = dir.path().join(".ft").join("tx_contract_locks");
        assert!(
            lock_dir.is_dir()
                && std::fs::read_dir(&lock_dir)
                    .expect("list workspace-global contract locks")
                    .count()
                    == 1,
            "one workspace-global tx contract sidecar should be retained"
        );

        let second = super::acquire_mcp_tx_contract_lock(dir.path(), &contract_path);
        assert!(
            second.is_err(),
            "second lock acquisition should fail while first is held"
        );
        let Err(err) = second else {
            return;
        };
        assert_eq!(err.code, MCP_ERR_WORKFLOW);

        drop(first);
        super::acquire_mcp_tx_contract_lock(dir.path(), &contract_path)
            .expect("lock should be released when guard drops");
    }

    #[cfg(unix)]
    #[test]
    fn tx_run_uses_locked_canonical_contract_after_intermediate_symlink_retarget() {
        use std::os::unix::fs::symlink;

        let (_db_dir, db_path) = temp_db_path();
        let original_dir = workspace_tempdir();
        let foreign_dir = workspace_tempdir();
        let alias_dir = workspace_tempdir();
        let original_path = write_tx_contract(&original_dir, MissionTxState::Planned);
        let contract_name = original_path.file_name().unwrap().to_owned();

        let mut foreign_contract = sample_tx_contract(MissionTxState::Planned);
        for (index, step) in foreign_contract.plan.steps.iter_mut().enumerate() {
            if let StepAction::SendText { text, .. } = &mut step.action {
                *text = format!("foreign-step-{}", index + 1);
            }
        }
        let foreign_path = foreign_dir.path().join(&contract_name);
        let foreign_before = serde_json::to_vec_pretty(&foreign_contract).unwrap();
        std::fs::write(&foreign_path, &foreign_before).unwrap();

        let active_alias = alias_dir.path().join("active");
        let replacement_alias = alias_dir.path().join("active-next");
        symlink(original_dir.path(), &active_alias).unwrap();
        symlink(foreign_dir.path(), &replacement_alias).unwrap();

        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let active_alias_for_hook = active_alias.clone();
        install_tx_contract_post_lock_test_hook(move || {
            std::fs::rename(&replacement_alias, &active_alias_for_hook)
                .expect("retarget contract parent alias after lock");
        });

        let envelope = parse_json_content(
            WaTxRunTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": active_alias.join(&contract_name).display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["contract_file"],
            original_path.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3");
        for pane_id in 1..=3 {
            assert!(
                !tx_run_mock_pane_content(&mock, pane_id).contains("foreign-step"),
                "retargeted contract must not dispatch a foreign pane effect"
            );
        }
        assert!(original_dir.path().join(".ft").join("tx_ledgers").is_dir());
        assert!(
            !foreign_dir.path().join(".ft").join("tx_ledgers").exists(),
            "retargeted directory must not receive the durable ledger"
        );
        assert_eq!(std::fs::read(&foreign_path).unwrap(), foreign_before);
        assert_eq!(
            mcp_load_mission_tx_contract_from_path(&original_path)
                .unwrap()
                .lifecycle_state,
            MissionTxState::Committed
        );
        assert_eq!(
            active_alias.join(&contract_name).canonicalize().unwrap(),
            foreign_path.canonicalize().unwrap(),
            "the attacker-controlled alias should actually have been retargeted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tx_run_post_lock_parent_detach_fails_before_effects() {
        let (_db_dir, db_path) = temp_db_path();
        let workspace = workspace_tempdir();
        super::set_tx_contract_workspace_test_root(Some(workspace.path().to_path_buf()));
        let active_dir = workspace.path().join("active");
        let foreign_dir = workspace.path().join("foreign");
        let detached_dir = workspace.path().join("active-detached");
        std::fs::create_dir(&active_dir).unwrap();
        std::fs::create_dir(&foreign_dir).unwrap();
        let contract_path = active_dir.join("tx-contract.json");
        let baseline =
            serde_json::to_vec_pretty(&sample_tx_contract(MissionTxState::Planned)).unwrap();
        std::fs::write(&contract_path, &baseline).unwrap();
        let foreign_path = foreign_dir.join("tx-contract.json");
        let foreign_sentinel = b"foreign contract sentinel".to_vec();
        std::fs::write(&foreign_path, &foreign_sentinel).unwrap();

        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let active_for_hook = active_dir.clone();
        install_tx_contract_post_lock_test_hook(move || {
            std::fs::rename(&active_for_hook, &detached_dir)
                .expect("detach contract parent after lock");
            std::fs::rename(&foreign_dir, &active_for_hook)
                .expect("install foreign parent after lock");
        });

        let envelope = parse_json_content(
            WaTxRunTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_STORAGE);
        for pane_id in 1..=3 {
            assert_eq!(tx_run_mock_pane_content(&mock, pane_id), "");
        }
        assert_eq!(
            std::fs::read(active_dir.join("tx-contract.json")).unwrap(),
            foreign_sentinel
        );
        assert_eq!(
            std::fs::read(
                workspace
                    .path()
                    .join("active-detached")
                    .join("tx-contract.json")
            )
            .unwrap(),
            baseline
        );
        assert!(!workspace.path().join(".ft").join("tx_ledgers").exists());
    }

    #[cfg(unix)]
    #[test]
    fn tx_run_post_lock_transient_parent_detach_restored_before_auth_succeeds() {
        let (_db_dir, db_path) = temp_db_path();
        let workspace = workspace_tempdir();
        super::set_tx_contract_workspace_test_root(Some(workspace.path().to_path_buf()));
        let active_dir = workspace.path().join("active");
        let foreign_dir = workspace.path().join("foreign");
        let detached_dir = workspace.path().join("active-detached");
        let displaced_foreign_dir = workspace.path().join("foreign-displaced");
        std::fs::create_dir(&active_dir).unwrap();
        std::fs::create_dir(&foreign_dir).unwrap();
        let contract_path = active_dir.join("tx-contract.json");
        std::fs::write(
            &contract_path,
            serde_json::to_vec_pretty(&sample_tx_contract(MissionTxState::Planned)).unwrap(),
        )
        .unwrap();
        let foreign_path = foreign_dir.join("tx-contract.json");
        let foreign_sentinel = b"transient foreign contract sentinel".to_vec();
        std::fs::write(&foreign_path, &foreign_sentinel).unwrap();

        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let active_for_hook = active_dir.clone();
        install_tx_contract_post_lock_test_hook(move || {
            std::fs::rename(&active_for_hook, &detached_dir)
                .expect("transiently detach contract parent");
            std::fs::rename(&foreign_dir, &active_for_hook)
                .expect("transiently install foreign parent");
            std::fs::rename(&active_for_hook, &displaced_foreign_dir)
                .expect("move transient foreign parent aside");
            std::fs::rename(&detached_dir, &active_for_hook)
                .expect("restore original parent before authorization");
        });

        let envelope = parse_json_content(
            WaTxRunTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3");
        assert_eq!(
            std::fs::read(
                workspace
                    .path()
                    .join("foreign-displaced")
                    .join("tx-contract.json")
            )
            .unwrap(),
            foreign_sentinel
        );
        assert!(workspace.path().join(".ft").join("tx_ledgers").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn tx_run_post_auth_parent_detach_returns_error_without_foreign_save() {
        let (_db_dir, db_path) = temp_db_path();
        let workspace = workspace_tempdir();
        super::set_tx_contract_workspace_test_root(Some(workspace.path().to_path_buf()));
        let active_dir = workspace.path().join("active");
        let foreign_dir = workspace.path().join("foreign");
        let detached_dir = workspace.path().join("active-detached");
        std::fs::create_dir(&active_dir).unwrap();
        std::fs::create_dir(&foreign_dir).unwrap();
        let contract_path = active_dir.join("tx-contract.json");
        std::fs::write(
            &contract_path,
            serde_json::to_vec_pretty(&sample_tx_contract(MissionTxState::Planned)).unwrap(),
        )
        .unwrap();

        let foreign_contract = sample_tx_contract(MissionTxState::Planned);
        let foreign_path = foreign_dir.join("tx-contract.json");
        let foreign_before = serde_json::to_vec_pretty(&foreign_contract).unwrap();
        std::fs::write(&foreign_path, &foreign_before).unwrap();

        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let active_for_hook = active_dir.clone();
        install_tx_contract_post_auth_test_hook(move || {
            std::fs::rename(&active_for_hook, &detached_dir)
                .expect("detach authorized contract parent after authorization");
            std::fs::rename(&foreign_dir, &active_for_hook)
                .expect("install foreign parent after authorization");
        });

        let envelope = parse_json_content(
            WaTxRunTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_STORAGE);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error text")
                .contains("namespace-detached")
        );
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3");
        assert_eq!(
            std::fs::read(active_dir.join("tx-contract.json")).unwrap(),
            foreign_before
        );
        assert_eq!(
            mcp_load_mission_tx_contract_from_path(
                &workspace
                    .path()
                    .join("active-detached")
                    .join("tx-contract.json")
            )
            .unwrap()
            .lifecycle_state,
            MissionTxState::Committed
        );
        assert!(workspace.path().join(".ft").join("tx_ledgers").is_dir());

        let retry_envelope = parse_json_content(
            WaTxRunTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": active_dir
                            .join("tx-contract.json")
                            .display()
                            .to_string()
                    }),
                )
                .unwrap(),
        );
        assert_eq!(retry_envelope["ok"], true);
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3");
    }

    #[cfg(unix)]
    #[test]
    fn tx_run_post_auth_basename_substitution_preserves_foreign_sentinel_and_recovery() {
        let (_db_dir, db_path) = temp_db_path();
        let workspace = workspace_tempdir();
        let contract_path = write_tx_contract(&workspace, MissionTxState::Planned);
        let detached_contract = workspace.path().join("tx-contract-original-detached.json");
        let baseline = std::fs::read(&contract_path).unwrap();
        let foreign_sentinel = b"post-auth foreign basename sentinel".to_vec();

        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let contract_for_hook = contract_path.clone();
        let sentinel_for_hook = foreign_sentinel.clone();
        install_tx_contract_post_auth_test_hook(move || {
            std::fs::rename(&contract_for_hook, &detached_contract)
                .expect("detach authorized contract basename after authorization");
            std::fs::write(&contract_for_hook, &sentinel_for_hook)
                .expect("install foreign basename sentinel after authorization");
        });

        let envelope = parse_json_content(
            WaTxRunTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_STORAGE);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error text")
                .contains("basename no longer names the pre-effect authorized inode")
        );
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3");
        assert_eq!(std::fs::read(&contract_path).unwrap(), foreign_sentinel);
        assert_eq!(
            std::fs::read(workspace.path().join("tx-contract-original-detached.json")).unwrap(),
            baseline
        );
        let recovery_paths = std::fs::read_dir(workspace.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".recovery.tmp"))
            })
            .collect::<Vec<_>>();
        assert_eq!(recovery_paths.len(), 1);
        let recovered = mcp_load_mission_tx_contract_from_path(&recovery_paths[0]).unwrap();
        assert_eq!(recovered.lifecycle_state, MissionTxState::Committed);
    }

    #[cfg(unix)]
    #[test]
    fn tx_rollback_uses_locked_canonical_contract_after_intermediate_symlink_retarget() {
        use std::os::unix::fs::symlink;

        let (_db_dir, db_path) = temp_db_path();
        let original_dir = workspace_tempdir();
        let foreign_dir = workspace_tempdir();
        let alias_dir = workspace_tempdir();
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let original_path =
            write_tx_contract_with_proven_commit_receipts(&original_dir, &db_path, None);
        let contract_name = original_path.file_name().unwrap().to_owned();

        let mut foreign_contract = mcp_load_mission_tx_contract_from_path(&original_path).unwrap();
        for (index, compensation) in foreign_contract.plan.compensations.iter_mut().enumerate() {
            if let StepAction::SendText { text, .. } = &mut compensation.action {
                *text = format!("foreign-undo-{}", index + 1);
            }
        }
        let foreign_path = foreign_dir.path().join(&contract_name);
        let foreign_before = serde_json::to_vec_pretty(&foreign_contract).unwrap();
        std::fs::write(&foreign_path, &foreign_before).unwrap();

        let active_alias = alias_dir.path().join("active");
        let replacement_alias = alias_dir.path().join("active-next");
        symlink(original_dir.path(), &active_alias).unwrap();
        symlink(foreign_dir.path(), &replacement_alias).unwrap();
        let active_alias_for_hook = active_alias.clone();
        install_tx_contract_post_lock_test_hook(move || {
            std::fs::rename(&replacement_alias, &active_alias_for_hook)
                .expect("retarget contract parent alias after lock");
        });

        let envelope = parse_json_content(
            WaTxRollbackTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": active_alias.join(&contract_name).display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], true);
        assert_eq!(
            envelope["data"]["contract_file"],
            original_path.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1undo-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2undo-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3undo-3");
        for pane_id in 1..=3 {
            assert!(
                !tx_run_mock_pane_content(&mock, pane_id).contains("foreign-undo"),
                "retargeted contract must not dispatch a foreign compensation effect"
            );
        }
        assert!(original_dir.path().join(".ft").join("tx_ledgers").is_dir());
        assert!(
            !foreign_dir.path().join(".ft").join("tx_ledgers").exists(),
            "retargeted directory must not receive the durable rollback ledger"
        );
        assert_eq!(std::fs::read(&foreign_path).unwrap(), foreign_before);
        assert_eq!(
            mcp_load_mission_tx_contract_from_path(&original_path)
                .unwrap()
                .lifecycle_state,
            MissionTxState::RolledBack
        );
        assert_eq!(
            active_alias.join(&contract_name).canonicalize().unwrap(),
            foreign_path.canonicalize().unwrap(),
            "the attacker-controlled alias should actually have been retargeted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn tx_rollback_post_auth_parent_detach_returns_error_without_foreign_save() {
        let (_db_dir, db_path) = temp_db_path();
        let workspace = workspace_tempdir();
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let root_contract =
            write_tx_contract_with_proven_commit_receipts(&workspace, &db_path, None);
        let active_dir = workspace.path().join("active");
        let foreign_dir = workspace.path().join("foreign");
        let detached_dir = workspace.path().join("active-detached");
        std::fs::create_dir(&active_dir).unwrap();
        std::fs::create_dir(&foreign_dir).unwrap();
        let contract_path = active_dir.join("tx-contract.json");
        std::fs::rename(&root_contract, &contract_path).unwrap();

        let mut foreign_contract = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        for (index, compensation) in foreign_contract.plan.compensations.iter_mut().enumerate() {
            if let StepAction::SendText { text, .. } = &mut compensation.action {
                *text = format!("foreign-undo-{}", index + 1);
            }
        }
        let foreign_path = foreign_dir.join("tx-contract.json");
        let foreign_before = serde_json::to_vec_pretty(&foreign_contract).unwrap();
        std::fs::write(&foreign_path, &foreign_before).unwrap();

        let active_for_hook = active_dir.clone();
        install_tx_contract_post_auth_test_hook(move || {
            std::fs::rename(&active_for_hook, &detached_dir)
                .expect("detach authorized rollback parent after authorization");
            std::fs::rename(&foreign_dir, &active_for_hook)
                .expect("install foreign rollback parent after authorization");
        });

        let envelope = parse_json_content(
            WaTxRollbackTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_STORAGE);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error text")
                .contains("namespace-detached")
        );
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1undo-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2undo-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3undo-3");
        assert_eq!(
            std::fs::read(active_dir.join("tx-contract.json")).unwrap(),
            foreign_before
        );
        assert_eq!(
            mcp_load_mission_tx_contract_from_path(
                &workspace
                    .path()
                    .join("active-detached")
                    .join("tx-contract.json")
            )
            .unwrap()
            .lifecycle_state,
            MissionTxState::RolledBack
        );
        assert!(workspace.path().join(".ft").join("tx_ledgers").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn tx_rollback_post_auth_basename_substitution_preserves_foreign_sentinel_and_recovery() {
        let (_db_dir, db_path) = temp_db_path();
        let workspace = workspace_tempdir();
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let contract_path =
            write_tx_contract_with_proven_commit_receipts(&workspace, &db_path, None);
        let detached_contract = workspace
            .path()
            .join("tx-rollback-contract-original-detached.json");
        let baseline = std::fs::read(&contract_path).unwrap();
        let foreign_sentinel = b"post-auth foreign rollback basename sentinel".to_vec();

        let contract_for_hook = contract_path.clone();
        let detached_for_hook = detached_contract.clone();
        let sentinel_for_hook = foreign_sentinel.clone();
        install_tx_contract_post_auth_test_hook(move || {
            std::fs::rename(&contract_for_hook, &detached_for_hook)
                .expect("detach authorized rollback contract basename after authorization");
            std::fs::write(&contract_for_hook, &sentinel_for_hook)
                .expect("install foreign rollback basename sentinel after authorization");
        });

        let envelope = parse_json_content(
            WaTxRollbackTool::new(config_with_db_path(&db_path))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_STORAGE);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error text")
                .contains("basename no longer names the pre-effect authorized inode")
        );
        assert!(
            envelope["hint"]
                .as_str()
                .expect("unsafe-retry hint")
                .contains("Do not retry wa.tx_rollback")
        );
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1undo-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2undo-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3undo-3");
        assert_eq!(std::fs::read(&contract_path).unwrap(), foreign_sentinel);
        assert_eq!(std::fs::read(&detached_contract).unwrap(), baseline);
        let recovery_paths = std::fs::read_dir(workspace.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".recovery.tmp"))
            })
            .collect::<Vec<_>>();
        assert_eq!(recovery_paths.len(), 1);
        let recovered = mcp_load_mission_tx_contract_from_path(&recovery_paths[0]).unwrap();
        assert_eq!(recovered.lifecycle_state, MissionTxState::RolledBack);
        assert_eq!(recovered.outcome, TxOutcome::Compensated);
        assert!(workspace.path().join(".ft").join("tx_ledgers").is_dir());
    }

    #[test]
    fn tx_run_tool_denies_when_real_prepare_targets_are_missing() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = workspace_tempdir();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config_with_db_path(&db_path));
        let _guard = lock_tx_run_test_wezterm_override();

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
        let dir = workspace_tempdir();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config_with_db_path(&db_path));
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);

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
        let dir = workspace_tempdir();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let tool = WaTxRunTool::new(config_with_db_path(&db_path));
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);

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
    fn tx_tools_full_success_run_show_rollback_converges_persisted_receipts_and_effects() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = workspace_tempdir();
        let contract_path = write_tx_contract(&dir, MissionTxState::Planned);
        let config = config_with_db_path(&db_path);
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);

        let run = parse_json_content(
            WaTxRunTool::new(Arc::clone(&config))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(run["ok"], true);
        assert_eq!(run["data"]["prepare_report"]["outcome"], "all_ready");
        assert_eq!(run["data"]["commit_report"]["outcome"], "fully_committed");
        assert_eq!(run["data"]["commit_report"]["committed_count"], 3);
        assert!(run["data"]["compensation_report"].is_null());
        assert_eq!(run["data"]["final_state"], "committed");
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3");

        let committed_receipts = run["data"]["commit_report"]["receipts"]
            .as_array()
            .expect("run commit receipts")
            .clone();
        assert_eq!(committed_receipts.len(), 3);
        for (index, (receipt, step_id)) in committed_receipts
            .iter()
            .zip(["tx-step:1", "tx-step:2", "tx-step:3"])
            .enumerate()
        {
            assert_eq!(receipt["seq"].as_u64(), Some(index as u64 + 1));
            assert_eq!(receipt["phase"], "commit");
            assert_eq!(receipt["step_id"], step_id);
            assert_eq!(receipt["outcome"], "committed");
        }

        let persisted_after_run: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&contract_path).expect("persisted run contract should be readable"),
        )
        .expect("persisted run contract should be valid JSON");
        assert_eq!(persisted_after_run["lifecycle_state"], "committed");
        assert_eq!(persisted_after_run["outcome"], "committed");
        assert_eq!(
            persisted_after_run["receipts"]
                .as_array()
                .expect("persisted receipt array"),
            &committed_receipts
        );

        let show = parse_json_content(
            WaTxShowTool::new(Arc::clone(&config))
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string(),
                        "include_contract": true
                    }),
                )
                .unwrap(),
        );

        assert_eq!(show["ok"], true);
        assert_eq!(show["data"]["lifecycle_state"], "committed");
        assert_eq!(show["data"]["receipt_count"], 3);
        assert_eq!(show["data"]["contract"], persisted_after_run);

        let rollback = parse_json_content(
            WaTxRollbackTool::new(config)
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "contract_file": contract_path.display().to_string()
                    }),
                )
                .unwrap(),
        );

        assert_eq!(rollback["ok"], true);
        assert_eq!(rollback["data"]["final_state"], "rolled_back");
        assert_eq!(
            rollback["data"]["compensation_report"]["outcome"],
            "fully_rolled_back"
        );
        assert_eq!(
            rollback["data"]["compensation_report"]["compensated_count"],
            3
        );
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1undo-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2undo-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3undo-3");

        let compensation_receipts = rollback["data"]["compensation_report"]["receipts"]
            .as_array()
            .expect("rollback compensation receipts")
            .clone();
        assert_eq!(compensation_receipts.len(), 3);
        for (index, (receipt, step_id)) in compensation_receipts
            .iter()
            .zip(["tx-step:3", "tx-step:2", "tx-step:1"])
            .enumerate()
        {
            assert_eq!(receipt["seq"].as_u64(), Some(index as u64 + 4));
            assert_eq!(receipt["phase"], "compensate");
            assert_eq!(receipt["step_id"], step_id);
            assert_eq!(receipt["outcome"], "compensated");
        }

        let persisted_after_rollback =
            mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(
            persisted_after_rollback.lifecycle_state,
            MissionTxState::RolledBack
        );
        assert_eq!(persisted_after_rollback.outcome, TxOutcome::Compensated);
        assert_eq!(persisted_after_rollback.receipts.len(), 6);
        assert_eq!(
            &persisted_after_rollback.receipts[..3],
            committed_receipts.as_slice()
        );
        assert_eq!(
            &persisted_after_rollback.receipts[3..],
            compensation_receipts.as_slice()
        );
    }

    #[test]
    fn tx_rollback_tool_rejects_committed_contract_without_commit_receipts() {
        let dir = workspace_tempdir();
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

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["error"],
            "rollback requires commit receipts, got none for tx state committed"
        );
        assert_eq!(
            envelope["hint"],
            "Use wa.tx_show(include_contract=true) to inspect persisted commit receipts. Do not rerun the commit or rollback solely to repair missing receipts: durable state may represent an external effect that was already dispatched. Reconcile the contract with the workspace .ft/tx_ledgers records; do not fabricate receipts."
        );

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::Committed);
        assert!(persisted.receipts.is_empty());
    }

    #[test]
    fn tx_rollback_tool_executes_real_compensations_for_full_commit_receipts() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = workspace_tempdir();
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let contract_path = write_tx_contract_with_proven_commit_receipts(&dir, &db_path, None);
        let tool = WaTxRollbackTool::new(config_with_db_path(&db_path));

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
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1undo-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2undo-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3undo-3");

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::RolledBack);
        assert_eq!(persisted.outcome, TxOutcome::Compensated);
        assert_eq!(persisted.receipts.len(), 6);
    }

    #[test]
    fn tx_rollback_tool_persists_failure_then_retries_only_failed_compensation() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = workspace_tempdir();
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let contract_path = write_tx_contract_with_proven_commit_receipts(&dir, &db_path, None);
        let tool = WaTxRollbackTool::new(config_with_db_path(&db_path));

        let first = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string(),
                    "fail_compensation_for_step": "tx-step:1"
                }),
            )
            .unwrap(),
        );

        assert_eq!(first["ok"], true);
        assert_eq!(first["data"]["final_state"], "failed");
        assert_eq!(
            first["data"]["compensation_report"]["outcome"],
            "compensation_failed"
        );
        assert_eq!(first["data"]["compensation_report"]["compensated_count"], 2);
        assert_eq!(first["data"]["compensation_report"]["failed_count"], 1);
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2undo-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3undo-3");

        let failed_contract = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(failed_contract.lifecycle_state, MissionTxState::Failed);
        assert_eq!(failed_contract.outcome, TxOutcome::Failed);
        assert_eq!(failed_contract.receipts.len(), 6);

        // The process/time nonce in rollback execution IDs guarantees a
        // distinct durable retry ledger even within the same millisecond.
        let retry = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string()
                }),
            )
            .unwrap(),
        );

        assert_eq!(retry["ok"], true);
        assert_eq!(retry["data"]["final_state"], "rolled_back");
        assert_eq!(
            retry["data"]["compensation_report"]["outcome"],
            "fully_rolled_back"
        );
        assert_eq!(retry["data"]["compensation_report"]["compensated_count"], 1);
        assert_eq!(retry["data"]["compensation_report"]["failed_count"], 0);
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1undo-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "step-2undo-2");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "step-3undo-3");

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::RolledBack);
        assert_eq!(persisted.outcome, TxOutcome::Compensated);
        assert_eq!(persisted.receipts.len(), 7);
    }

    #[test]
    fn tx_rollback_tool_rejects_unknown_compensation_step_with_guidance() {
        let dir = workspace_tempdir();
        let contract_path = write_tx_contract_with_receipt_only_commit_claims(&dir);
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
    fn tx_rollback_tool_rejects_receipt_only_commit_claims_before_dispatch() {
        let dir = workspace_tempdir();
        let contract_path = write_tx_contract_with_receipt_only_commit_claims(&dir);
        let original_contract = std::fs::read(&contract_path).expect("read forged contract");
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

        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error_code"], MCP_ERR_WORKFLOW,
            "MCP envelopes must use the published FT-MCP taxonomy: {envelope}"
        );
        assert!(
            envelope["error"]
                .as_str()
                .unwrap_or_default()
                .contains("rejected before compensation dispatch")
        );
        assert_eq!(
            envelope["hint"],
            "Do not rerun the commit or rollback. Inspect and reconcile the contract receipts, external effects, and workspace .ft/tx_ledgers records first: missing durable proof does not establish that an external effect was absent. For future new transactions, execute commits through MCP wa.tx_run, `ft tx run`, or `ft robot tx run` so receipts and authoritative proofs are persisted together; do not fabricate receipts."
        );
        assert_eq!(
            std::fs::read(&contract_path).expect("reread forged contract"),
            original_contract,
            "proof rejection must leave the contract byte-for-byte unchanged"
        );
    }

    #[test]
    fn tx_rollback_tool_maps_real_durable_conflict_without_dispatch() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = workspace_tempdir();
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let contract_path = write_tx_contract_with_proven_commit_receipts(&dir, &db_path, None);
        let mut contract = mcp_load_mission_tx_contract_from_path(&contract_path)
            .expect("load proven transaction fixture");
        let receipt = contract
            .receipts
            .iter_mut()
            .find(|receipt| {
                receipt.get("phase").and_then(serde_json::Value::as_str) == Some("commit")
                    && receipt.get("step_id").and_then(serde_json::Value::as_str)
                        == Some("tx-step:1")
            })
            .expect("tx-step:1 commit receipt");
        receipt["outcome"] = serde_json::json!("failed");
        receipt["reason_code"] = serde_json::json!("forged_commit_failure");
        receipt["error_code"] = serde_json::json!("FTX3999");
        receipt["decision_path"] = serde_json::json!("forged_receipt_history");
        std::fs::write(
            &contract_path,
            serde_json::to_vec_pretty(&contract).expect("serialize contradictory receipt fixture"),
        )
        .expect("persist contradictory receipt fixture");
        let original_contract =
            std::fs::read(&contract_path).expect("read contradictory receipt fixture");
        let original_panes = (1..=3)
            .map(|pane_id| tx_run_mock_pane_content(&mock, pane_id))
            .collect::<Vec<_>>();
        let tool = WaTxRollbackTool::new(config_with_db_path(&db_path));

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "contract_file": contract_path.display().to_string()
                }),
            )
            .unwrap(),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(
            envelope["error_code"], MCP_ERR_WORKFLOW,
            "MCP envelopes must use the published FT-MCP taxonomy: {envelope}"
        );
        assert!(
            envelope["error"]
                .as_str()
                .unwrap_or_default()
                .contains("Rollback proof conflict")
        );
        let hint = envelope["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("Do not blindly rerun"));
        assert!(hint.contains("already dispatched"));
        assert_eq!(
            std::fs::read(&contract_path).expect("reread rejected conflict fixture"),
            original_contract
        );
        assert_eq!(
            (1..=3)
                .map(|pane_id| tx_run_mock_pane_content(&mock, pane_id))
                .collect::<Vec<_>>(),
            original_panes,
            "proof conflict must not dispatch compensation"
        );
    }

    #[test]
    fn tx_rollback_tool_compensates_only_durably_committed_steps() {
        let (_db_dir, db_path) = temp_db_path();
        let dir = workspace_tempdir();
        let (_guard, mock) = install_tx_run_mock_wezterm();
        let _pane_state_overrides = seed_tx_run_real_targets(&db_path, &mock);
        let contract_path =
            write_tx_contract_with_proven_commit_receipts(&dir, &db_path, Some("tx-step:2"));
        let tool = WaTxRollbackTool::new(config_with_db_path(&db_path));

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
        assert_eq!(tx_run_mock_pane_content(&mock, 1), "step-1undo-1");
        assert_eq!(tx_run_mock_pane_content(&mock, 2), "");
        assert_eq!(tx_run_mock_pane_content(&mock, 3), "");

        let persisted = mcp_load_mission_tx_contract_from_path(&contract_path).unwrap();
        assert_eq!(persisted.lifecycle_state, MissionTxState::RolledBack);
        assert_eq!(persisted.outcome, TxOutcome::Compensated);
        assert_eq!(persisted.receipts.len(), 4);
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
                "--",
                "agent context",
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
        // [ft-0uzlr] wa.cass_view now probes `cass context` for index
        // membership BEFORE `cass view`. The fake cass branches on the
        // subcommand: `context` returns a `source` object (path is indexed
        // -> gate passes); `view` returns the actual view result. The args
        // file is truncated per-invocation, so env.args() reflects the LAST
        // (view) call.
        let env = CassToolTestEnv::install(
            r#"if [ "$1" = context ]; then printf '%s' '{"source":{"path":"/tmp/session.md"}}'; else printf '%s' '{"source_path":"/tmp/session.md","line_number":42,"match_line":{"line_number":42,"content":"needle hit","role":"assistant"},"context_before":[{"line_number":41,"content":"before","role":"user"}],"context_after":[{"line_number":43,"content":"after","role":"assistant"}]}'; fi"#,
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
            vec![
                "view",
                "-n",
                "42",
                "--json",
                "-C",
                "3",
                "--",
                "/tmp/session.md"
            ]
        );
        assert_eq!(envelope["ok"], true);
        assert_eq!(envelope["data"]["source_path"], "/tmp/session.md");
        assert_eq!(envelope["data"]["match_line"]["content"], "needle hit");
        assert_eq!(envelope["data"]["context_before"][0]["line_number"], 41);
    }

    /// [ft-0uzlr] SECURITY REGRESSION: wa.cass_view must REFUSE a path that
    /// cass has not indexed, and must NOT invoke `cass view` on it — closing
    /// the prompt-injection arbitrary-file-read exfil. The fake cass returns
    /// the `not-found` error object for `cass context`, mirroring cass's real
    /// behavior on a non-session path like /etc/passwd.
    #[cfg(unix)]
    #[test]
    fn ft_0uzlr_cass_view_refuses_non_indexed_path() {
        let env = CassToolTestEnv::install(
            r#"if [ "$1" = context ]; then printf '%s' '{"error":{"code":4,"kind":"not-found","message":"No session found at path"}}'; else printf '%s' 'SECRET-FILE-CONTENTS-LEAKED'; fi"#,
        );
        let tool = WaCassViewTool;

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "source_path": "/etc/passwd",
                    "line_number": 1,
                    "timeout_secs": 11
                }),
            )
            .expect("cass view call must produce an envelope"),
        );

        // Refused with a typed error, no file content returned.
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not an indexed cass session"),
            "expected index-membership refusal, got: {}",
            envelope["error"]
        );
        // The gate must short-circuit BEFORE `cass view`: the last (and only)
        // invocation is the `context` probe, never `view`.
        let args = env.args();
        assert_eq!(args.first().map(String::as_str), Some("context"));
        assert!(
            !args.iter().any(|a| a == "view"),
            "cass view must NOT run for a non-indexed path; args were {args:?}"
        );
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

    #[test]
    fn cass_search_malformed_args_redacts_serde_error_value() {
        let redaction_sample = redaction_test_token();

        let envelope = parse_json_content(
            WaCassSearchTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": "agent history",
                        "limit": redaction_sample,
                    }),
                )
                .expect("cass_search bad-arg call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["hint"],
            "Expected object with query (required) and optional limit/offset/agent/workspace/days/fields/max_tokens/timeout_secs"
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "malformed wa.cass_search args leaked the caller-supplied secret"
        );
    }

    #[test]
    fn cass_search_unknown_agent_redacts_argument_value() {
        let redaction_sample = redaction_test_token();

        let envelope = parse_json_content(
            WaCassSearchTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": "agent history",
                        "agent": redaction_sample,
                    }),
                )
                .expect("cass_search unknown-agent call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "unknown wa.cass_search agent leaked the caller-supplied secret"
        );
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("[REDACTED]")
        );
    }

    /// ft-7lh4k: a secret that arrives in an UNMODELED cass field (captured by
    /// `#[serde(flatten)] extra`) must be redacted before the result leaves the
    /// MCP surface. Models the real exfil path: cass renames/adds a content field,
    /// or nests content, so it bypasses the typed `content` walk. Deserialize →
    /// redact → re-serialize and assert the secret does not survive any extra slot
    /// (hit-level, result-level, or nested in an object/array).
    #[test]
    fn redact_cass_search_result_scrubs_secrets_in_flattened_extra() {
        use super::{CassSearchResult, redact_cass_search_result};

        let secret = redaction_test_token();
        let prefix = redaction_test_prefix();
        let mut result: CassSearchResult = serde_json::from_value(serde_json::json!({
            "hits": [{
                "content": format!("typed content with {secret}"),
                "renamed_text": secret,
                "nested": { "deep": [secret] },
            }],
            "result_level_leak": secret,
        }))
        .expect("cass search fixture deserializes");

        redact_cass_search_result(&mut result);

        let serialized = serde_json::to_string(&result).expect("serialize redacted result");
        assert!(
            !serialized.contains(prefix.as_str()),
            "secret survived redaction via a flattened extra field: {serialized}"
        );
        assert!(
            serialized.contains("[REDACTED"),
            "expected a redaction marker in the scrubbed output: {serialized}"
        );
    }

    /// ft-7lh4k: same forward-compat invariant for `wa.cass_view` — match line,
    /// surrounding context lines, and the view-level result all scrub their
    /// `#[serde(flatten)] extra` passthrough.
    #[test]
    fn redact_cass_view_result_scrubs_secrets_in_flattened_extra() {
        use super::{CassViewResult, redact_cass_view_result};

        let secret = redaction_test_token();
        let prefix = redaction_test_prefix();
        let mut result: CassViewResult = serde_json::from_value(serde_json::json!({
            "match_line": { "content": "match", "raw_line": secret },
            "context_before": [{ "content": "before", "snippet": secret }],
            "context_after": [{ "content": "after", "meta": { "blob": secret } }],
            "view_level_leak": secret,
        }))
        .expect("cass view fixture deserializes");

        redact_cass_view_result(&mut result);

        let serialized = serde_json::to_string(&result).expect("serialize redacted view");
        assert!(
            !serialized.contains(prefix.as_str()),
            "secret survived redaction via a flattened extra field: {serialized}"
        );
        assert!(
            serialized.contains("[REDACTED"),
            "expected a redaction marker in the scrubbed output: {serialized}"
        );
    }

    #[test]
    fn cass_search_rejects_oversized_query_without_echoing_value() {
        let redaction_sample = redaction_test_token();
        let query = format!(
            "{redaction_sample}{}",
            "x".repeat(MAX_MCP_CASS_QUERY_BYTES + 1)
        );

        let envelope = parse_json_content(
            WaCassSearchTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": query,
                    }),
                )
                .expect("cass_search oversized-query call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("max allowed")
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "oversized wa.cass_search query leaked the caller-supplied value"
        );
    }

    #[test]
    fn cass_search_schema_declares_query_max_length() {
        let def = WaCassSearchTool.definition();
        assert_eq!(
            def.input_schema["properties"]["query"]["maxLength"].as_u64(),
            Some(MAX_MCP_CASS_QUERY_BYTES as u64)
        );
    }

    #[test]
    fn cass_search_rejects_oversized_agent_without_echoing_value() {
        let redaction_sample = redaction_test_token();
        let agent = format!(
            "{redaction_sample}{}",
            "x".repeat(MAX_MCP_CASS_AGENT_FILTER_BYTES + 1)
        );

        let envelope = parse_json_content(
            WaCassSearchTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": "agent history",
                        "agent": agent,
                    }),
                )
                .expect("cass_search oversized-agent call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("max allowed")
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "oversized wa.cass_search agent leaked the caller-supplied value"
        );
    }

    #[test]
    fn cass_search_schema_declares_agent_max_length() {
        let def = WaCassSearchTool.definition();
        assert_eq!(
            def.input_schema["properties"]["agent"]["maxLength"].as_u64(),
            Some(MAX_MCP_CASS_AGENT_FILTER_BYTES as u64)
        );
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
    /// ft-aylbh: error text now uses the unified range form
    /// "timeout_secs must be in 1..=600" because cass_view, cass_search,
    /// and cass_status all route through validate_cass_timeout_secs.
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
        let err_str = envelope["error"].as_str().expect("error string");
        assert!(
            err_str.contains("timeout_secs must be in 1..=600"),
            "expected unified range form 'timeout_secs must be in 1..=600' (ft-aylbh), got: {err_str}"
        );
        assert!(
            err_str.contains("(got 0)"),
            "error must cite the rejected value, got: {err_str}"
        );
    }

    /// [ft-aylbh] ca.view with timeout_secs above the schema maximum
    /// must also fail-fast. Pre-fix the handler only rejected zero;
    /// a hostile/buggy client sending timeout_secs: 3600 would pin the
    /// mcp server on cass for up to an hour despite the schema's
    /// 10-minute cap. Mirror of ft_szuzd_ca_search_rejects_above_max_timeout_secs.
    #[test]
    fn ft_aylbh_ca_view_rejects_above_max_timeout_secs() {
        let envelope = parse_json_content(
            WaCassViewTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "source_path": "/tmp/session.md",
                        "line_number": 1,
                        "timeout_secs": 3600
                    }),
                )
                .expect("ca.view call must produce an envelope"),
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

    /// ca.status symmetric: timeout_secs=0 → INVALID_ARGS before dispatch.
    /// Also verifies that the explicit-zero path is reached even though
    /// CassStatusParams::default() returns the schema default (15) for
    /// the null-args path. ft-aylbh: error text now uses the unified
    /// range form via validate_cass_timeout_secs.
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
        let err_str = envelope["error"].as_str().expect("error string");
        assert!(
            err_str.contains("timeout_secs must be in 1..=600"),
            "expected unified range form 'timeout_secs must be in 1..=600' (ft-aylbh), got: {err_str}"
        );
        assert!(
            err_str.contains("(got 0)"),
            "error must cite the rejected value, got: {err_str}"
        );
    }

    /// [ft-aylbh] ca.status with timeout_secs above the schema maximum
    /// must also fail-fast. Pre-fix the handler only rejected zero;
    /// a hostile/buggy client sending timeout_secs: 3600 would pin the
    /// mcp server on cass for up to an hour despite the schema's
    /// 10-minute cap. Mirror of ft_szuzd_ca_search_rejects_above_max_timeout_secs.
    #[test]
    fn ft_aylbh_ca_status_rejects_above_max_timeout_secs() {
        let envelope = parse_json_content(
            WaCassStatusTool
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "timeout_secs": 3600
                    }),
                )
                .expect("ca.status call must produce an envelope"),
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

    /// [ft-aylbh] Property test for the shared CASS timeout validator.
    /// Asserts the contract holds across the entire u64 input space:
    ///
    ///   1. timeout_secs ∈ [CASS_TIMEOUT_SECS_MIN, CASS_TIMEOUT_SECS_MAX]
    ///      → returns None (validation passes, caller proceeds)
    ///   2. timeout_secs outside the range → returns Some(envelope)
    ///      with MCP_ERR_INVALID_ARGS, the configured tool_name in the
    ///      hint, and the rejected value in the error message
    ///
    /// Uses a representative-value sweep (boundary + chaos values)
    /// rather than full proptest because (a) the file has no proptest
    /// imports today, (b) the validator is a total function over u64
    /// and the property is convex over the partition (in-range vs
    /// out-of-range), so finite representative coverage of each
    /// equivalence class suffices, and (c) keeps the test body
    /// hermetic and compile-light.
    #[test]
    fn ft_aylbh_validate_cass_timeout_secs_property_holds() {
        // In-range values must pass: boundary lo, mid, boundary hi
        for &v in &[
            CASS_TIMEOUT_SECS_MIN,
            CASS_TIMEOUT_SECS_MIN + 1,
            CASS_TIMEOUT_SECS_MIN.midpoint(CASS_TIMEOUT_SECS_MAX),
            CASS_TIMEOUT_SECS_MAX - 1,
            CASS_TIMEOUT_SECS_MAX,
        ] {
            let outcome = validate_cass_timeout_secs("wa.test_tool", v, std::time::Instant::now());
            assert!(
                outcome.is_none(),
                "ft-aylbh: in-range timeout_secs={v} must pass validation"
            );
        }

        // Out-of-range values must produce a structured error.
        // Includes both below-min (0) and above-max (boundary+1, chaos
        // values up to u64::MAX) to cover both partitions of the
        // out-of-range equivalence class.
        for &v in &[
            0u64,
            CASS_TIMEOUT_SECS_MAX + 1,
            CASS_TIMEOUT_SECS_MAX * 2,
            3600,
            86400,
            u64::MAX,
        ] {
            let outcome = validate_cass_timeout_secs("wa.test_tool", v, std::time::Instant::now());
            assert!(
                outcome.is_some(),
                "ft-aylbh: out-of-range timeout_secs={v} must produce an error envelope"
            );
            let envelope_content = outcome
                .expect("ft-aylbh: asserted out-of-range timeout produced envelope")
                .expect("envelope_to_content infallible for valid envelope");
            let envelope = parse_json_content(envelope_content);
            assert_eq!(
                envelope["ok"], false,
                "ft-aylbh: out-of-range envelope must have ok=false (timeout_secs={v})"
            );
            assert_eq!(
                envelope["error_code"], MCP_ERR_INVALID_ARGS,
                "ft-aylbh: out-of-range envelope must use MCP_ERR_INVALID_ARGS (timeout_secs={v})"
            );
            let err_str = envelope["error"].as_str().expect("error string");
            assert!(
                err_str.contains(&format!(
                    "timeout_secs must be in {CASS_TIMEOUT_SECS_MIN}..={CASS_TIMEOUT_SECS_MAX}"
                )),
                "ft-aylbh: error must cite the bounds (timeout_secs={v}), got: {err_str}"
            );
            assert!(
                err_str.contains(&format!("(got {v})")),
                "ft-aylbh: error must cite the rejected value (timeout_secs={v}), got: {err_str}"
            );
            // Hint must name the configured tool_name so operators can
            // identify the offending tool surface.
            let hint = envelope["hint"].as_str().expect("hint string");
            assert!(
                hint.contains("wa.test_tool"),
                "ft-aylbh: hint must name the tool (timeout_secs={v}), got: {hint}"
            );
        }
    }

    // ========================================================================
    // Key Parameter Schema Checks
    // ========================================================================

    #[test]
    fn state_tool_schema_has_domain_and_pane_id() {
        let def = WaStateTool::new(Arc::new(Config::default()), PaneFilterConfig::default(), None).definition();
        let props = def.input_schema.get("properties").unwrap();
        assert!(
            props.get("domain").is_some(),
            "wa.state missing 'domain' param"
        );
        assert!(
            props.get("pane_id").is_some(),
            "wa.state missing 'pane_id' param"
        );
        assert_eq!(
            props["agent"]["maxLength"].as_u64(),
            Some(MAX_MCP_STATE_AGENT_FILTER_BYTES as u64)
        );
    }

    /// GH#72 regression guard: the production MCP constructors must build
    /// their pane-control handle from the parsed `Config` (a config-aware
    /// `UnifiedClient`), never from `default_wezterm_handle()`, which has no
    /// mux pool and unconditionally falls through to the external
    /// `wezterm cli` subprocess even when `vendored.mux_socket_path` is
    /// explicitly configured. A config-aware handle reports
    /// `Some(backend_selection)`; the CLI-only default handle reports `None`.
    #[test]
    fn wa_send_production_constructor_uses_config_aware_handle() {
        use crate::wezterm::MuxInterface;
        let config = Arc::new(Config::default());
        let limiter = build_mcp_shared_rate_limiter(config.as_ref());
        let tool = WaSendTool::new_with_shared_rate_limiter(
            Arc::clone(&config),
            Arc::new(PathBuf::from("/tmp/wa-send-handle-test.db")),
            limiter,
        );
        assert!(
            tool.wezterm.backend_selection().is_some(),
            "wa.send production constructor must use the config-aware unified \
             handle so a configured vendored mux socket is honored (GH#72)"
        );
    }

    /// GH#72 regression guard: see
    /// [`wa_send_production_constructor_uses_config_aware_handle`].
    #[test]
    fn wa_wait_for_production_constructor_uses_config_aware_handle() {
        use crate::wezterm::MuxInterface;
        let config = Arc::new(Config::default());
        let limiter = build_mcp_shared_rate_limiter(config.as_ref());
        let tool = WaWaitForTool::new_with_shared_rate_limiter(Arc::clone(&config), None, limiter);
        assert!(
            tool.wezterm.backend_selection().is_some(),
            "wa.wait_for production constructor must use the config-aware \
             unified handle so a configured vendored mux socket is honored (GH#72)"
        );
    }

    /// GH#72 regression guard: `default_wezterm_handle()` must keep
    /// reporting `None` so the two tests above cannot silently pass against
    /// a config-blind handle.
    #[test]
    fn default_wezterm_handle_reports_no_backend_selection() {
        use crate::wezterm::MuxInterface;
        let handle = crate::wezterm::default_wezterm_handle();
        assert!(handle.backend_selection().is_none());
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
    fn wa_state_malformed_args_redacts_serde_error_value() {
        let tool = WaStateTool::new(Arc::new(Config::default()), PaneFilterConfig::default(), None);
        let redaction_sample = redaction_test_token();

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "domain": "local",
                    "pane_id": redaction_sample,
                }),
            )
            .expect("wa.state bad-arg call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["hint"],
            "Expected object with optional domain/agent/pane_id"
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "malformed wa.state args leaked the caller-supplied secret"
        );
    }

    #[test]
    fn wa_state_rejects_oversized_agent_filter_without_echoing_value() {
        let tool = WaStateTool::new(Arc::new(Config::default()), PaneFilterConfig::default(), None);
        let redaction_sample = redaction_test_token();
        let agent = format!(
            "{redaction_sample}{}",
            "x".repeat(MAX_MCP_STATE_AGENT_FILTER_BYTES + 1)
        );

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "agent": agent,
                }),
            )
            .expect("wa.state oversized-agent call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("max allowed")
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "oversized wa.state agent filter leaked the caller-supplied value"
        );
    }

    #[test]
    fn wa_events_malformed_args_redacts_serde_error_value() {
        let tool = WaEventsTool::new(db_path());
        let redaction_sample = redaction_test_token();

        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "limit": redaction_sample,
                    "event_type": "state_change",
                }),
            )
            .expect("wa.events bad-arg call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert_eq!(
            envelope["hint"],
            "Expected object with optional limit, pane, rule_id, event_type, triage_state, label, unhandled, since"
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "malformed wa.events args leaked the caller-supplied secret"
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

    /// ft-7h5da.2.6: the wa.dom MCP mirror's definition must match the
    /// semantic-pane contract — name, required fields, and the four-verb query
    /// enum (kept in lockstep with DomQueryKind). The envelope itself is
    /// byte-equal to the robot CLI by construction (both call
    /// frankenterm_core::robot_dom::build_dom_data), covered by robot_dom tests
    /// and the mcp_manifest golden.
    #[test]
    fn dom_tool_definition_matches_semantic_pane_contract() {
        let def = WaDomTool::new(config(), Some(db_path())).definition();
        assert_eq!(def.name, "wa.dom");
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.dom should have required fields");
        assert!(required.iter().any(|v| v.as_str() == Some("pane_id")));
        assert!(required.iter().any(|v| v.as_str() == Some("query")));
        let query_enum: Vec<String> = def
            .input_schema
            .get("properties")
            .and_then(|p| p.get("query"))
            .and_then(|q| q.get("enum"))
            .and_then(|e| e.as_array())
            .expect("wa.dom query must declare an enum")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            query_enum,
            vec!["zones", "last_command", "output_of", "exit_code"],
            "wa.dom query enum must match the DomQueryKind verbs"
        );
    }

    /// ft-7h5da.6.5: the wa.steer_plan MCP mirror's definition must list the
    /// five standard scenarios + require scenario/objective. The envelope is the
    /// same SteeringReceipt the CLI emits (both call
    /// frankenterm_core::steer_plan::steer_plan), pinned by the steer-plan
    /// golden + the mcp_manifest golden.
    #[test]
    fn steer_plan_tool_definition_lists_all_scenarios() {
        let def = WaSteerPlanTool::new(config()).definition();
        assert_eq!(def.name, "wa.steer_plan");
        let required = def
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("wa.steer_plan should have required fields");
        assert!(required.iter().any(|v| v.as_str() == Some("scenario")));
        assert!(required.iter().any(|v| v.as_str() == Some("objective")));
        let scenarios: Vec<String> = def
            .input_schema
            .get("properties")
            .and_then(|p| p.get("scenario"))
            .and_then(|s| s.get("enum"))
            .and_then(|e| e.as_array())
            .expect("wa.steer_plan scenario must declare an enum")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            scenarios,
            vec![
                "clean-ready",
                "dirty-overlap",
                "rch-blocked",
                "approval-required",
                "capacity-red"
            ],
            "wa.steer_plan scenario enum must match SteerPlanScenario"
        );
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
    fn wait_for_pattern_output_redaction_masks_secret_tokens() {
        let redaction_sample = redaction_test_token();

        let redacted = redact_mcp_wait_pattern_for_output(&format!("ready {redaction_sample}"));

        assert!(
            !redacted.contains(&redaction_sample),
            "raw secret leaked in MCP wait-for pattern output"
        );
        assert!(
            redacted.contains("[REDACTED]"),
            "expected redaction marker in MCP wait-for pattern output"
        );
    }

    #[test]
    fn invalid_regex_compile_error_does_not_leak_secret_in_pattern() {
        // A wait_for pattern that is an invalid regex AND carries a secret can
        // leak: the `regex` crate (reached via fancy_regex's delegated
        // `CompileError::InnerError`) reflects the offending pattern source
        // verbatim into a code-frame compile error. Both WaWaitForTool (the
        // `Invalid regex pattern: {err}` envelope) and WaSendTool (the
        // `err.to_string()` catch-all) now route that string through
        // `redact_mcp_output_secrets`, so the secret must not survive.
        let redaction_sample = redaction_test_token();

        // Faithfully model the `regex`-crate compile-error shape that echoes
        // the pattern source verbatim (this is the exact text that would reach
        // the MCP client through `Error::CompileError` -> "Regex error: ...").
        let raw_message = format!(
            "Invalid regex pattern: Error compiling regex: Regex error: \
             regex parse error:\n    {redaction_sample}{{\n    ^\nerror: repetition \
             quantifier expects a valid decimal"
        );
        // Non-vacuous: the unredacted message really does echo the secret.
        assert!(
            raw_message.contains(&redaction_sample),
            "test precondition: regex compile error echoes the pattern source"
        );

        let redacted = redact_mcp_output_secrets(&raw_message);
        assert!(
            !redacted.contains(&redaction_sample),
            "secret leaked through invalid-regex MCP error message"
        );
        assert!(
            redacted.contains("[REDACTED]"),
            "expected redaction marker in invalid-regex MCP error message"
        );

        // Exercise a real compile failure end-to-end: whatever fancy_regex
        // emits for a malformed pattern, the operator-visible string is
        // redaction-safe and the call never panics.
        let live_err =
            crate::wezterm::compile_wait_matcher(&format!("(?P<g>{redaction_sample}"), true)
                .expect_err("unclosed group must fail to compile");
        let live_redacted =
            redact_mcp_output_secrets(&format!("Invalid regex pattern: {live_err}"));
        assert!(
            !live_redacted.contains(&redaction_sample),
            "secret leaked through live fancy_regex compile error"
        );
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

    /// ft-p8git: in degraded mode (storage = None / db_path unset) a denied
    /// MCP action must still leave an audit trail via the primary tracing
    /// signal — no policy_denied_audit row can be written without a DB, so the
    /// log line is the floor of audit fidelity. Before the fix the
    /// Option-storage deny sites persisted only inside `if let Some(storage)`
    /// and emitted no tracing of their own, so the denial was completely
    /// silent (no row AND no log).
    #[test]
    fn audit_mcp_policy_denial_emits_tracing_signal_in_degraded_mode() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;

        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for CaptureWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        #[derive(Clone)]
        struct CaptureMaker(Arc<Mutex<Vec<u8>>>);
        impl<'a> MakeWriter<'a> for CaptureMaker {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                CaptureWriter(Arc::clone(&self.0))
            }
        }

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CaptureMaker(Arc::clone(&buf)))
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();

        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(audit_mcp_policy_denial_async(
                None, // degraded mode: no StorageHandle / db_path
                "wa.get_text",
                "get-text pane_id=7",
                "reads are blocked",
                Some("policy.read_block"),
                crate::storage::PolicyDeniedAuditRecord::DECISION_DENIED,
                crate::storage::PolicyDeniedAuditRecord::REASON_CODE_DENIED,
            ));
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("MCP action denied by policy"),
            "degraded-mode denial must emit the primary tracing signal; got: {captured:?}"
        );
        assert!(
            captured.contains("wa.get_text"),
            "tracing must name the denied tool; got: {captured:?}"
        );
        assert!(
            captured.contains("storage_attached=false"),
            "tracing must flag the degraded (no-storage) state; got: {captured:?}"
        );
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
    fn wait_for_and_send_schemas_bound_wait_pattern_length() {
        let wait_for_def = WaWaitForTool::new(config(), None).definition();
        let wait_for_props = wait_for_def
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("wa.wait_for schema properties");
        assert_eq!(
            wait_for_props["pattern"]["maxLength"],
            serde_json::json!(MAX_MCP_WAIT_PATTERN_BYTES)
        );

        let send_def = WaSendTool::new(config(), db_path()).definition();
        let send_props = send_def
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("wa.send schema properties");
        assert_eq!(
            send_props["wait_for"]["maxLength"],
            serde_json::json!(MAX_MCP_WAIT_PATTERN_BYTES)
        );
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
    fn wait_for_tool_rejects_oversized_pattern_before_regex_compile() {
        let tool = WaWaitForTool::new(config(), None);
        let pattern = "a".repeat(MAX_MCP_WAIT_PATTERN_BYTES + 1);
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 42,
                    "pattern": pattern,
                    "regex": true,
                    "timeout_secs": 1
                }),
            )
            .expect("wa.wait_for oversized pattern validation call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err = envelope["error"].as_str().expect("error string");
        assert!(
            err.contains("pattern is"),
            "error should identify pattern field: {err}"
        );
        assert!(
            err.contains(&MAX_MCP_WAIT_PATTERN_BYTES.to_string()),
            "error should cite max pattern bytes: {err}"
        );
    }

    /// ft-ymo2i: server-side bound on the `tail` field rejects oversized
    /// requests with MCP_ERR_INVALID_ARGS independent of whether the MCP
    /// client validated the schema's `maximum: 10000` constraint. Mirrors
    /// the ft-ii8ss test for wa.get_text — same fix template applied to
    /// wa.wait_for after the round-2 audit found the same anti-pattern.
    #[test]
    fn wait_for_tool_rejects_tail_over_max_bound() {
        let tool = WaWaitForTool::new(config(), None);
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 1,
                    "pattern": "ready",
                    "tail": 99_999
                }),
            )
            .expect("wa.wait_for over-max tail call must return an envelope, not panic"),
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

    /// ft-ymo2i: tail=0 used to mean "full buffer" per the pre-fix schema
    /// description. After the bound, 0 is rejected and callers wanting
    /// full-buffer scans must use wa.search. The hint in the rendered
    /// error must point that way.
    #[test]
    fn wait_for_tool_rejects_tail_below_min_bound() {
        let tool = WaWaitForTool::new(config(), None);
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 1,
                    "pattern": "ready",
                    "tail": 0
                }),
            )
            .expect("wa.wait_for tail=0 call must return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err = envelope["error"]
            .as_str()
            .expect("error message must be a string");
        assert!(err.contains("tail must be in"), "error message: {err}");
        let hint = envelope["hint"]
            .as_str()
            .expect("hint must be present to redirect callers");
        assert!(
            hint.contains("wa.search"),
            "hint should point at wa.search for full-buffer scans: {hint}"
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
    fn send_tool_rejects_oversized_wait_for_before_runtime_dispatch() {
        let tool = WaSendTool::new(config(), db_path());
        let wait_for = "ready".repeat((MAX_MCP_WAIT_PATTERN_BYTES / 5) + 1);
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "pane_id": 42,
                    "text": "hello",
                    "wait_for": wait_for,
                    "wait_for_regex": true,
                    "timeout_secs": 1
                }),
            )
            .expect("wa.send oversized wait_for validation call"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        let err = envelope["error"].as_str().expect("error string");
        assert!(
            err.contains("wait_for is"),
            "error should identify wait_for field: {err}"
        );
        assert!(
            err.contains(&MAX_MCP_WAIT_PATTERN_BYTES.to_string()),
            "error should cite max pattern bytes: {err}"
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
        let verify_submit_schema = props
            .get("verify_submit")
            .expect("verify_submit field must exist");
        assert_eq!(verify_submit_schema["type"], "boolean");
        assert_eq!(verify_submit_schema["default"], serde_json::json!(false));
        let submit_level_schema = props
            .get("submit_level")
            .expect("submit_level field must exist");
        assert_eq!(submit_level_schema["type"], "string");
        assert_eq!(
            submit_level_schema["enum"],
            serde_json::json!(["write", "composer", "submitted", "working"])
        );
        let idempotency_key_schema = props
            .get("idempotency_key")
            .expect("idempotency_key field must exist");
        assert_eq!(idempotency_key_schema["type"], "string");
        assert_eq!(
            idempotency_key_schema["maxLength"],
            serde_json::json!(MAX_MCP_SUBMIT_IDEMPOTENCY_KEY_BYTES)
        );
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
    fn search_rejects_oversized_query_without_echoing_value() {
        let redaction_sample = format!("{}search-query-fixture", redaction_test_prefix());
        let query = format!(
            "{redaction_sample}{}",
            "x".repeat(MAX_MCP_SEARCH_QUERY_BYTES + 1)
        );

        let envelope = parse_json_content(
            WaSearchTool::new(config(), db_path())
                .call(
                    &test_mcp_context(),
                    serde_json::json!({
                        "query": query,
                    }),
                )
                .expect("search oversized-query call should return an envelope"),
        );

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_INVALID_ARGS);
        assert!(
            envelope["error"]
                .as_str()
                .expect("error string")
                .contains("max allowed")
        );
        assert!(
            !envelope.to_string().contains(&redaction_test_prefix()),
            "oversized wa.search query leaked the caller-supplied value"
        );
    }

    #[test]
    fn search_schema_declares_query_max_length() {
        let def = WaSearchTool::new(config(), db_path()).definition();
        assert_eq!(
            def.input_schema["properties"]["query"]["maxLength"].as_u64(),
            Some(MAX_MCP_SEARCH_QUERY_BYTES as u64)
        );
    }

    #[test]
    fn robot_parity_tools_return_invalid_args_envelopes_for_bad_json_params() {
        let redaction_sample = redaction_test_token();

        let cases = [
            (
                "wa.get_text",
                WaGetTextTool::new(config(), None).call(
                    &test_mcp_context(),
                    serde_json::json!({"pane_id": redaction_sample.as_str()}),
                ),
            ),
            (
                "wa.wait_for",
                WaWaitForTool::new(config(), None).call(
                    &test_mcp_context(),
                    serde_json::json!({"pane_id": redaction_sample.as_str(), "pattern": "ready"}),
                ),
            ),
            (
                "wa.search",
                WaSearchTool::new(config(), db_path()).call(
                    &test_mcp_context(),
                    serde_json::json!({"query": "ready", "limit": redaction_sample.as_str()}),
                ),
            ),
            (
                "wa.send",
                WaSendTool::new(config(), db_path()).call(
                    &test_mcp_context(),
                    serde_json::json!({"pane_id": redaction_sample.as_str(), "text": "ready"}),
                ),
            ),
        ];

        for (tool_name, result) in cases {
            assert!(
                result.is_ok(),
                "{tool_name} must return an FT-MCP envelope, not a framework error"
            );
            let envelope = parse_json_content(
                result.expect("asserted robot parity bad params returned an envelope"),
            );
            assert_eq!(envelope["ok"], false, "{tool_name} envelope={envelope}");
            assert_eq!(
                envelope["error_code"],
                crate::mcp_error::MCP_ERR_INVALID_ARGS,
                "{tool_name} envelope={envelope}"
            );
            assert_eq!(envelope["data"], serde_json::Value::Null);
            let rendered = envelope.to_string();
            assert!(
                !rendered.contains(&redaction_sample),
                "{tool_name} leaked user-controlled bad input through envelope={envelope}"
            );
        }
    }

    /// br-ft-9ia4p: wa.search with pane_id pointing at a distributed
    /// remote pane must fall back to the storage record when the live
    /// mux `get_pane(pane_id)` lookup fails. Without
    /// this, the recovery hint emitted by `wa.get_text` ("use
    /// wa.search ...") is broken — pane-scoped search aborts before
    /// querying storage.
    #[test]
    fn search_tool_falls_back_to_storage_for_distributed_panes_ft_9ia4p() {
        let (_dir, db) = temp_db_path();
        let pane_id = 9_241u64;
        seed_distributed_remote_pane(&db, pane_id, "distributed:agent-c:prod");
        // Seed a single output segment so search can return a hit;
        // the fix is only meaningful if the search reaches storage,
        // and storage hits prove the live-WezTerm precondition was
        // bypassed correctly.
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = StorageHandle::new(&db.to_string_lossy())
                .await
                .expect("storage should open");
            storage
                .append_segment(pane_id, "distributed-pane-needle-marker-9ia4p", None)
                .await
                .expect("output segment should append");
            let _ = storage.shutdown().await;
        });

        let tool = WaSearchTool::new(config(), Arc::clone(&db));
        // ft-kccj8: three fixture defects hid the actual br-ft-9ia4p path:
        // (1) the hyphenated bareword parsed as FTS5 column-filter negation
        // ("no such column: marker") — quote it as a phrase; (2) the param
        // key is `pane`, not `pane_id` — serde silently dropped the filter
        // so the distributed fallback under test never executed; (3) the
        // response field is `total_hits`, not `total`.
        let envelope = parse_json_content(
            tool.call(
                &test_mcp_context(),
                serde_json::json!({
                    "query": "\"needle-marker-9ia4p\"",
                    "pane": pane_id,
                    "mode": "lexical",
                }),
            )
            .expect("wa.search distributed-pane call must not panic"),
        );

        // Pre-fix this returned an MCP_ERR error from the live
        // get_pane lookup. Post-fix the storage fallback resolves
        // the pane record, policy authorizes the search, and storage
        // returns the seeded segment.
        assert_eq!(
            envelope["ok"], true,
            "br-ft-9ia4p: search must succeed for distributed remote panes; got envelope={envelope}"
        );
        let total = envelope["data"]["total_hits"].as_u64().unwrap_or(0);
        assert!(
            total >= 1,
            "br-ft-9ia4p: search must return the seeded segment; got total={total} envelope={envelope}"
        );
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

    // ─── br-ft-rnpuc: mcp_now_ms_i64 + clock-anomaly counter ──────────────

    fn mcp_clock_anomaly_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn mcp_clock_anomaly_counter_starts_at_zero_after_reset_ft_rnpuc() {
        let _guard = mcp_clock_anomaly_test_lock();
        super::reset_mcp_clock_anomaly_count_for_test();
        assert_eq!(super::mcp_clock_anomaly_count(), 0);
    }

    #[test]
    fn mcp_now_ms_i64_returns_post_epoch_value_under_real_clock_ft_rnpuc() {
        // Negative test: under a real (post-epoch, in-i64-range)
        // system clock — i.e., any modern host — the helper does NOT
        // bump the counter. Pins that the counter only fires on
        // actual u64→i64 collapse.
        let _guard = mcp_clock_anomaly_test_lock();
        super::reset_mcp_clock_anomaly_count_for_test();
        let ts = super::mcp_now_ms_i64();
        assert!(
            ts > 0,
            "mcp_now_ms_i64 should return a positive epoch ms value under a real clock"
        );
        assert!(
            ts < i64::MAX,
            "mcp_now_ms_i64 should never return i64::MAX under a real clock"
        );
        assert_eq!(
            super::mcp_clock_anomaly_count(),
            0,
            "br-ft-rnpuc: real clock must not bump the anomaly counter"
        );
    }

    #[test]
    fn mcp_clock_anomaly_counter_increments_on_overflow_collapse_ft_rnpuc() {
        // The collapse path is unreachable from a real clock under
        // current u64→i64 semantics (now_ms() returns ~1.7T ms;
        // i64::MAX is ~9.2 quintillion). Exercise the conversion
        // helper directly so the test covers the same branch used by
        // live MCP audit-row timestamp construction.
        let _guard = mcp_clock_anomaly_test_lock();
        super::reset_mcp_clock_anomaly_count_for_test();
        assert_eq!(
            super::mcp_audit_ts_ms_from_u64(i64::MAX as u64),
            i64::MAX,
            "in-range i64::MAX should not collapse"
        );
        assert_eq!(super::mcp_clock_anomaly_count(), 0);
        assert_eq!(super::mcp_audit_ts_ms_from_u64(i64::MAX as u64 + 1), 0);
        assert_eq!(super::mcp_audit_ts_ms_from_u64(u64::MAX), 0);
        assert_eq!(
            super::mcp_clock_anomaly_count(),
            2,
            "br-ft-rnpuc: counter must reflect actual overflow collapses"
        );
    }

    // ── br-ft-6h1rv: RequireApproval fail-closed dead end ──────────────

    /// br-ft-6h1rv: build a Config that forces a RequireApproval
    /// policy decision for a specific MCP exec_command pattern.
    /// Mirrors the existing `deny_mcp_exec_command_config` helper
    /// used by the deny-path tests in this module.
    fn require_approval_mcp_exec_command_config(
        command_pattern: &str,
        message: &str,
    ) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.safety.rules.enabled = true;
        cfg.safety.rules.rules.push(crate::config::PolicyRule {
            id: format!("test.require_approval.mcp.{command_pattern}"),
            description: Some(format!(
                "force RequireApproval for {command_pattern} MCP mutations"
            )),
            priority: 1,
            match_on: crate::config::PolicyRuleMatch {
                actions: vec!["exec_command".to_string()],
                actors: vec!["mcp".to_string()],
                surfaces: vec!["mcp".to_string()],
                command_patterns: vec![format!("^{command_pattern}$")],
                ..Default::default()
            },
            decision: crate::config::PolicyRuleDecision::RequireApproval,
            message: Some(message.to_string()),
        });
        Arc::new(cfg)
    }

    /// br-ft-6h1rv: when the policy returns RequireApproval for a
    /// `wa.tx_run` invocation, the helper now returns a fail-closed
    /// envelope with an explicit hint that names the limitation +
    /// lists the alternatives. Pre-fix the hint advised "obtain an
    /// allow-once approval token and retry via the approving
    /// client" — but no token was issued from this path, sending the
    /// operator on an impossible errand.
    #[test]
    fn mcp_authorize_mutation_tx_run_require_approval_is_fail_closed_ft_6h1rv() {
        let cfg = require_approval_mcp_exec_command_config("tx\\.run", "test require approval");
        let rate_limiter = build_mcp_shared_rate_limiter(cfg.as_ref());
        let result = super::mcp_authorize_mcp_mutation(
            cfg.as_ref(),
            &rate_limiter,
            "wa.tx_run",
            "tx.run",
            std::time::Instant::now(),
        );
        let envelope_content = result
            .expect("RequireApproval policy must produce an error envelope")
            .expect("envelope_to_content infallible for valid envelope");
        let envelope = parse_json_content(envelope_content);

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_POLICY);

        let err_str = envelope["error"].as_str().expect("error string");
        assert!(
            err_str.contains("br-ft-6h1rv"),
            "ft-6h1rv: error must reference the bead breadcrumb; got {err_str}"
        );
        assert!(
            err_str.contains("does not support") && err_str.contains("approval"),
            "ft-6h1rv: error must explicitly say the surface does not support approval; got {err_str}"
        );
        assert!(
            err_str.contains("fail-closed"),
            "ft-6h1rv: error must surface the fail-closed posture; got {err_str}"
        );

        let hint = envelope["hint"].as_str().expect("hint string");
        assert!(
            hint.contains("br-ft-6h1rv"),
            "ft-6h1rv: hint must reference the bead breadcrumb; got {hint}"
        );
        assert!(
            hint.contains("Allow") && hint.contains("Deny"),
            "ft-6h1rv: hint must point operators at the policy-rule alternatives; got {hint}"
        );
        assert!(
            hint.contains("wa.workflow_run") || hint.contains("approval-aware"),
            "ft-6h1rv: hint must point at an approval-aware alternative surface; got {hint}"
        );
        assert!(
            !hint.contains("Obtain an allow-once approval token and retry"),
            "ft-6h1rv: pre-fix dead-end hint must not appear; got {hint}"
        );
    }

    /// br-ft-6h1rv: same fail-closed posture for `wa.mission_pause`
    /// (the bead body explicitly named tx_run AND mission_pause as
    /// the regression targets).
    #[test]
    fn mcp_authorize_mutation_mission_pause_require_approval_is_fail_closed_ft_6h1rv() {
        let cfg =
            require_approval_mcp_exec_command_config("mission\\.pause", "test require approval");
        let rate_limiter = build_mcp_shared_rate_limiter(cfg.as_ref());
        let result = super::mcp_authorize_mcp_mutation(
            cfg.as_ref(),
            &rate_limiter,
            "wa.mission_pause",
            "mission.pause",
            std::time::Instant::now(),
        );
        let envelope_content = result
            .expect("RequireApproval policy must produce an error envelope")
            .expect("envelope_to_content infallible for valid envelope");
        let envelope = parse_json_content(envelope_content);

        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error_code"], MCP_ERR_POLICY);
        let err_str = envelope["error"].as_str().expect("error string");
        assert!(
            err_str.contains("br-ft-6h1rv") && err_str.contains("wa.mission_pause"),
            "ft-6h1rv: error must reference the bead AND the tool name; got {err_str}"
        );
    }

    /// br-ft-6h1rv: vacuous-regression guard. When the policy does
    /// NOT return RequireApproval (default config has no policy
    /// rules; helper returns None → Allow), the helper returns None
    /// and the tool proceeds. This pins that the new fail-closed
    /// language doesn't false-positive on the legitimate Allow path.
    #[test]
    fn mcp_authorize_mutation_returns_none_on_allow_ft_6h1rv() {
        let cfg = config();
        let rate_limiter = build_mcp_shared_rate_limiter(cfg.as_ref());
        let result = super::mcp_authorize_mcp_mutation(
            cfg.as_ref(),
            &rate_limiter,
            "wa.tx_run",
            "tx.run",
            std::time::Instant::now(),
        );
        assert!(
            result.is_none(),
            "ft-6h1rv: default-config Allow path must return None (proceed)"
        );
    }

    /// br-ft-6h1rv: serde stability for the new audit-record
    /// constants. Pins the wire format so external operator tooling
    /// querying the audit table can branch on the new
    /// `require_approval_unsupported` decision/reason codes.
    #[test]
    fn require_approval_unsupported_audit_constants_are_stable_ft_6h1rv() {
        assert_eq!(
            crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL_UNSUPPORTED,
            "require_approval_unsupported"
        );
        assert_eq!(
            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL_UNSUPPORTED,
            "require_approval_unsupported"
        );
        // The pre-existing constants must NOT collide with the new
        // ones — operators reading the audit table need to
        // distinguish the dead-end class from the supported flow.
        assert_ne!(
            crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL,
            crate::storage::PolicyDeniedAuditRecord::DECISION_REQUIRE_APPROVAL_UNSUPPORTED
        );
        assert_ne!(
            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL,
            crate::storage::PolicyDeniedAuditRecord::REASON_CODE_REQUIRE_APPROVAL_UNSUPPORTED
        );
    }
}

// br-ft-ncijf: serialize tests that touch the process-global
// MCP_WORKFLOW_PLAN_SERDE_DROP_COUNT counter so concurrent test
// threads don't trample each other's reset/observe pairs.
#[cfg(test)]
mod workflow_plan_serde_drop_tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static PLAN_DROP_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> MutexGuard<'static, ()> {
        PLAN_DROP_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn well_formed_plan_parses_without_bump() {
        let _g = lock();
        reset_mcp_workflow_plan_serde_drop_count_for_test();
        // Use the public builder so the test follows the live
        // ActionPlan construction contract instead of depending on
        // a synthetic Default impl.
        let plan = crate::plan::ActionPlan::builder("test plan", "test-workspace").build();
        let raw = serde_json::to_string(&plan).expect("serialize default plan");
        let parsed = parse_workflow_plan_json(&raw, "plan-001");
        assert!(parsed.is_some());
        assert_eq!(mcp_workflow_plan_serde_drop_count(), 0);
    }

    #[test]
    fn malformed_json_bumps_counter() {
        let _g = lock();
        reset_mcp_workflow_plan_serde_drop_count_for_test();
        // Truncated JSON (closing brace stripped).
        let parsed = parse_workflow_plan_json("{\"steps\":", "plan-trunc");
        assert!(parsed.is_none());
        assert_eq!(mcp_workflow_plan_serde_drop_count(), 1);
    }

    #[test]
    fn wrong_shape_object_bumps_counter() {
        let _g = lock();
        reset_mcp_workflow_plan_serde_drop_count_for_test();
        // Valid JSON object but missing every ActionPlan-required field.
        let parsed = parse_workflow_plan_json("{\"unrelated\":\"value\"}", "plan-shape");
        assert!(parsed.is_none());
        assert_eq!(mcp_workflow_plan_serde_drop_count(), 1);
    }

    #[test]
    fn primitive_top_level_bumps_counter() {
        let _g = lock();
        reset_mcp_workflow_plan_serde_drop_count_for_test();
        // Primitive at root: not a serde_json::Map, fails ActionPlan deserialization.
        let parsed = parse_workflow_plan_json("42", "plan-prim");
        assert!(parsed.is_none());
        assert_eq!(mcp_workflow_plan_serde_drop_count(), 1);
    }

    #[test]
    fn repeated_failures_bump_monotonically() {
        let _g = lock();
        reset_mcp_workflow_plan_serde_drop_count_for_test();
        for i in 0..7 {
            let id = format!("plan-{i}");
            let parsed = parse_workflow_plan_json("not json at all", &id);
            assert!(parsed.is_none());
        }
        assert_eq!(mcp_workflow_plan_serde_drop_count(), 7);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(48))]

        // br-ft-ncijf: any non-ActionPlan-shaped JSON or non-JSON
        // input must bump the counter exactly once and yield None.
        #[test]
        fn arbitrary_malformed_input_always_bumps(
            shape in proptest::sample::select(vec![
                "null".to_string(),
                "true".to_string(),
                "false".to_string(),
                "0".to_string(),
                "-1".to_string(),
                "\"a string\"".to_string(),
                "[]".to_string(),
                "[1,2,3]".to_string(),
                "{}".to_string(),
                "{\"unknown\":42}".to_string(),
                "{\"steps\":\"not an array\"}".to_string(),
                "not json".to_string(),
                String::new(),
                "{".to_string(),
            ]),
        ) {
            let _g = lock();
            reset_mcp_workflow_plan_serde_drop_count_for_test();
            let parsed = parse_workflow_plan_json(&shape, "plan-prop");
            // Some inputs (e.g. "{}") might happen to deserialize
            // if all ActionPlan fields are #[serde(default)]. We
            // assert: parse-Ok ⟺ counter unchanged; parse-None ⟺
            // counter bumped exactly once.
            match parsed {
                Some(_) => proptest::prop_assert_eq!(mcp_workflow_plan_serde_drop_count(), 0),
                None => proptest::prop_assert_eq!(mcp_workflow_plan_serde_drop_count(), 1),
            }
        }
    }
}
