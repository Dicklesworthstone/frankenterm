//! MCP resource/template handlers extracted from legacy `mcp.rs`.
//!
//! This module is extraction-only and keeps resource behavior/URIs stable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::mcp_framework::{
    FrameworkContent as Content, FrameworkMcpContext as McpContext, FrameworkMcpError as McpError,
    FrameworkMcpResult as McpResult, FrameworkResource as Resource,
    FrameworkResourceContent as ResourceContent, FrameworkResourceHandler as ResourceHandler,
    FrameworkResourceTemplate as ResourceTemplate, FrameworkToolHandler as ToolHandler,
};

use crate::context_horizon::predict_context_horizon_from_sqlite;
use crate::mcp_error::{MCP_ERR_CONFIG, MCP_ERR_STORAGE};
use crate::proof_lane::{
    ProofHistoryArtifactInput, ProofHistoryIndex, ProofHistoryQuery, ProofReleaseScoreboard,
    ProofState,
};
use crate::render_quality::{
    RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI, RENDERER_SSIM_PARITY_MCP_RESOURCE_URI,
    renderer_slos_doctor_report,
};
use crate::swarm_scheduler::{
    HerdWaveEventKind, HerdWaveMcpResourceSurface, build_herd_wave_surface_report,
};

use super::mcp_tools::{
    WaAccountsTool, WaEventsTool, WaReservationsTool, WaRulesListTool, WaStateTool,
};
use super::{McpEnvelope, McpWorkflowItem, McpWorkflowsData, builtin_workflows, elapsed_ms};
use crate::config::{Config, PaneFilterConfig};

const PROOF_HISTORY_RESOURCE_URI: &str = "wa://proof-history";
const PROOF_HISTORY_RELEASE_BLOCKING_URI: &str = "wa://proof-history/release-blocking";

fn tool_output_as_resource(uri: &str, contents: Vec<Content>) -> McpResult<Vec<ResourceContent>> {
    let text = contents
        .into_iter()
        .find_map(|content| match content {
            Content::Text { text } => Some(text),
            _ => None,
        })
        .ok_or_else(|| McpError::internal_error("Tool output missing text payload"))?;

    Ok(vec![ResourceContent {
        uri: uri.to_string(),
        mime_type: Some("application/json".to_string()),
        text: Some(text),
        blob: None,
    }])
}

fn envelope_as_resource<T: Serialize>(
    uri: &str,
    envelope: McpEnvelope<T>,
) -> McpResult<Vec<ResourceContent>> {
    let text = serde_json::to_string(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialize resource payload: {e}")))?;
    Ok(vec![ResourceContent {
        uri: uri.to_string(),
        mime_type: Some("application/json".to_string()),
        text: Some(text),
        blob: None,
    }])
}

fn read_events_resource(
    ctx: &McpContext,
    db_path: &Arc<PathBuf>,
    uri: &str,
    limit: usize,
    unhandled: bool,
) -> McpResult<Vec<ResourceContent>> {
    let tool = WaEventsTool::new(Arc::clone(db_path));
    let contents = tool.call(
        ctx,
        serde_json::json!({
            "limit": limit.clamp(1, 1000),
            "unhandled": unhandled,
        }),
    )?;
    tool_output_as_resource(uri, contents)
}

fn read_accounts_resource(
    ctx: &McpContext,
    db_path: &Arc<PathBuf>,
    uri: &str,
    service: &str,
) -> McpResult<Vec<ResourceContent>> {
    let tool = WaAccountsTool::new(Arc::clone(db_path));
    let contents = tool.call(ctx, serde_json::json!({ "service": service }))?;
    tool_output_as_resource(uri, contents)
}

fn read_rules_resource(
    ctx: &McpContext,
    uri: &str,
    agent_type: Option<&str>,
) -> McpResult<Vec<ResourceContent>> {
    let args = if let Some(agent_type) = agent_type {
        serde_json::json!({ "verbose": true, "agent_type": agent_type })
    } else {
        serde_json::json!({ "verbose": true })
    };
    let tool = WaRulesListTool;
    let contents = tool.call(ctx, args)?;
    tool_output_as_resource(uri, contents)
}

fn read_reservations_resource(
    ctx: &McpContext,
    db_path: &Arc<PathBuf>,
    uri: &str,
    pane_id: Option<u64>,
) -> McpResult<Vec<ResourceContent>> {
    let tool = WaReservationsTool::new(Arc::clone(db_path));
    let args = if let Some(pane_id) = pane_id {
        serde_json::json!({ "pane_id": pane_id })
    } else {
        serde_json::Value::Null
    };
    let contents = tool.call(ctx, args)?;
    tool_output_as_resource(uri, contents)
}

fn attestation_retractions_root(config: &Config) -> Result<(PathBuf, PathBuf), String> {
    let layout = config
        .workspace_layout(None)
        .map_err(|err| format!("resolve workspace layout: {err}"))?;
    let root = std::env::var_os("FT_ATTESTATION_RETRACTIONS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            layout
                .root
                .join("docs")
                .join("attestations")
                .join("retractions")
        });
    Ok((layout.root, root))
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn collect_attestation_retraction_files(
    root: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(root).map_err(|err| format!("read {}: {err}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read entry in {}: {err}", root.display()))?;
        let path = entry.path();
        if path
            .components()
            .any(|component| component.as_os_str() == std::ffi::OsStr::new("archive"))
        {
            continue;
        }
        if path.is_dir() {
            collect_attestation_retraction_files(&path, files)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            && !name.ends_with(".canonical.json")
            && !name.ends_with(".payload.json")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn load_attestation_retractions_for_resource(
    workspace_root: &Path,
    root: &Path,
) -> Result<Vec<serde_json::Value>, String> {
    let mut files = Vec::new();
    collect_attestation_retraction_files(root, &mut files)?;
    files.sort();

    let mut rows = Vec::new();
    for path in files {
        let bytes =
            std::fs::read(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|err| format!("parse {} as JSON: {err}", path.display()))?;
        let rel = path
            .strip_prefix(workspace_root)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        if let Some(object) = value.as_object_mut() {
            object.insert("path".to_string(), serde_json::Value::String(rel));
        }
        rows.push(value);
    }
    Ok(rows)
}

fn proof_history_artifact_roots(config: &Config) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let layout = config
        .workspace_layout(None)
        .map_err(|err| format!("resolve workspace layout: {err}"))?;
    let roots = std::env::var_os("FT_PROOF_HISTORY_ROOT")
        .map(|root| vec![PathBuf::from(root)])
        .unwrap_or_else(|| vec![layout.root.join("tests").join("e2e").join("artifacts")]);
    Ok((layout.root, roots))
}

fn load_proof_history_query_for_resource(
    config: &Config,
    query: ProofHistoryQuery,
) -> Result<crate::proof_lane::ProofHistoryQueryResult, String> {
    let (workspace_root, roots) = proof_history_artifact_roots(config)?;
    load_proof_history_query_from_roots(&workspace_root, roots, query)
}

fn load_proof_history_query_from_roots(
    workspace_root: &Path,
    roots: Vec<PathBuf>,
    query: ProofHistoryQuery,
) -> Result<crate::proof_lane::ProofHistoryQueryResult, String> {
    let mut paths = Vec::new();
    for root in roots {
        collect_proof_history_jsonl_paths(&root, &mut paths)?;
    }
    paths.sort();
    paths.dedup();

    let artifacts = paths
        .into_iter()
        .map(|path| proof_history_artifact_input_from_path(workspace_root, &path))
        .collect::<Vec<_>>();
    let index = ProofHistoryIndex::from_artifacts(&artifacts);
    let scoreboard = ProofReleaseScoreboard::from_history(&index);
    Ok(scoreboard.query(&query))
}

fn collect_proof_history_jsonl_paths(path: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    if path.is_file() {
        if is_proof_history_jsonl(path) {
            paths.push(path.to_path_buf());
        }
        return Ok(());
    }
    if !path.is_dir() {
        paths.push(path.to_path_buf());
        return Ok(());
    }

    let entries =
        std::fs::read_dir(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("read entry in {}: {err}", path.display()))?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            collect_proof_history_jsonl_paths(&entry_path, paths)?;
        } else if is_proof_history_jsonl(&entry_path) {
            paths.push(entry_path);
        }
    }

    Ok(())
}

fn is_proof_history_jsonl(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        && (file_name.contains("proof-record") || file_name.contains("proof_record"))
}

fn proof_history_artifact_input_from_path(
    workspace_root: &Path,
    path: &Path,
) -> ProofHistoryArtifactInput {
    let artifact_path = path
        .strip_prefix(workspace_root)
        .unwrap_or(path)
        .display()
        .to_string();
    match std::fs::read(path) {
        Ok(bytes) => {
            let content_sha256 = proof_history_sha256_hex(&bytes);
            match String::from_utf8(bytes) {
                Ok(content) => {
                    let mut input = ProofHistoryArtifactInput::new(artifact_path, content);
                    input.content_sha256 = Some(content_sha256);
                    input
                }
                Err(error) => ProofHistoryArtifactInput::unavailable(
                    artifact_path,
                    Some(format!("artifact is not UTF-8: {error}")),
                ),
            }
        }
        Err(error) => {
            ProofHistoryArtifactInput::unavailable(artifact_path, Some(error.to_string()))
        }
    }
}

fn proof_history_sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn proof_history_state_from_resource_param(value: &str) -> Option<ProofState> {
    match percent_decode_resource_param(value)
        .replace('-', "_")
        .as_str()
    {
        "not_run" => Some(ProofState::NotRun),
        "reached_remote_cargo" => Some(ProofState::ReachedRemoteCargo),
        "source_compile_fail" => Some(ProofState::SourceCompileFail),
        "test_fail" => Some(ProofState::TestFail),
        "pass" => Some(ProofState::Pass),
        "infra_blocked_pre_cargo" => Some(ProofState::InfraBlockedPreCargo),
        "infra_blocked_post_cargo" => Some(ProofState::InfraBlockedPostCargo),
        "local_invalid" => Some(ProofState::LocalInvalid),
        "skipped_not_proven" => Some(ProofState::SkippedNotProven),
        "inconclusive" => Some(ProofState::Inconclusive),
        _ => None,
    }
}

fn percent_decode_resource_param(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((high << 4) | low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(super) struct WaPanesResource {
    filter: PaneFilterConfig,
    db_path: Option<Arc<PathBuf>>,
}

impl WaPanesResource {
    pub(super) fn new(filter: PaneFilterConfig, db_path: Option<Arc<PathBuf>>) -> Self {
        Self { filter, db_path }
    }
}

impl ResourceHandler for WaPanesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://panes".to_string(),
            name: "ft panes".to_string(),
            description: Some("Pane snapshot (same data surface as wa.state)".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "panes".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let tool = WaStateTool::new(self.filter.clone(), self.db_path.as_ref().map(Arc::clone));
        let contents = tool.call(ctx, serde_json::Value::Null)?;
        tool_output_as_resource("wa://panes", contents)
    }
}

pub(super) struct WaEventsResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events".to_string(),
            name: "ft events".to_string(),
            description: Some("Recent detection events (default limit 50)".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_events_resource(ctx, &self.db_path, "wa://events", 50, false)
    }
}

pub(super) struct WaEventsTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events/template".to_string(),
            name: "ft events template".to_string(),
            description: Some("Template for page-sized events resource".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://events/{limit}".to_string(),
            name: "ft events (paged)".to_string(),
            description: Some("Override page size for events resource".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_events_resource(ctx, &self.db_path, "wa://events", 50, false)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let limit = params
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 1000);
        read_events_resource(ctx, &self.db_path, uri, limit, false)
    }
}

pub(super) struct WaEventsUnhandledTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaEventsUnhandledTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaEventsUnhandledTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://events/unhandled/template".to_string(),
            name: "ft events unhandled template".to_string(),
            description: Some("Template for unhandled events resource".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "events".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://events/unhandled/{limit}".to_string(),
            name: "ft events (unhandled)".to_string(),
            description: Some("Read only unhandled events with configurable limit".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "events".to_string(),
                "unhandled".to_string(),
            ],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_events_resource(ctx, &self.db_path, "wa://events/unhandled/50", 50, true)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let limit = params
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 1000);
        read_events_resource(ctx, &self.db_path, uri, limit, true)
    }
}

pub(super) struct WaAccountsResource {
    db_path: Arc<PathBuf>,
}

impl WaAccountsResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaAccountsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://accounts".to_string(),
            name: "ft accounts".to_string(),
            description: Some("Account usage snapshot (default service openai)".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "accounts".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_accounts_resource(ctx, &self.db_path, "wa://accounts", "openai")
    }
}

pub(super) struct WaAccountsByServiceTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaAccountsByServiceTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaAccountsByServiceTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://accounts/template".to_string(),
            name: "ft accounts template".to_string(),
            description: Some("Template for service-specific account snapshots".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "accounts".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://accounts/{service}".to_string(),
            name: "ft accounts by service".to_string(),
            description: Some("Read account snapshot for a specific service".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "accounts".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_accounts_resource(ctx, &self.db_path, "wa://accounts/openai", "openai")
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let service = params
            .get("service")
            .cloned()
            .unwrap_or_else(|| "openai".to_string());
        read_accounts_resource(ctx, &self.db_path, uri, &service)
    }
}

pub(super) struct WaRulesResource;

impl ResourceHandler for WaRulesResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://rules".to_string(),
            name: "ft rules".to_string(),
            description: Some(
                "Rule catalog (same data surface as wa.rules_list with verbose output)".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "rules".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_rules_resource(ctx, "wa://rules", None)
    }
}

pub(super) struct WaRulesByAgentTemplateResource;

impl ResourceHandler for WaRulesByAgentTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://rules/template".to_string(),
            name: "ft rules template".to_string(),
            description: Some("Template for rules filtered by agent type".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "rules".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://rules/{agent_type}".to_string(),
            name: "ft rules by agent".to_string(),
            description: Some("Filter rule catalog by agent type".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "rules".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_rules_resource(ctx, "wa://rules", None)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        read_rules_resource(ctx, uri, params.get("agent_type").map(String::as_str))
    }
}

pub(super) struct WaWorkflowsResource {
    config: Arc<Config>,
}

impl WaWorkflowsResource {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ResourceHandler for WaWorkflowsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://workflows".to_string(),
            name: "ft workflows".to_string(),
            description: Some("Builtin workflow catalog and metadata".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "workflows".to_string()],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let workflows: Vec<McpWorkflowItem> = builtin_workflows(&self.config)
            .iter()
            .map(|workflow| McpWorkflowItem {
                name: workflow.name().to_string(),
                description: workflow.description().to_string(),
                step_count: workflow.step_count(),
                trigger_event_types: workflow
                    .trigger_event_types()
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                trigger_rule_ids: workflow
                    .trigger_rule_ids()
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                supported_agent_types: workflow
                    .supported_agent_types()
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
                requires_pane: workflow.requires_pane(),
                requires_approval: workflow.requires_approval(),
                can_abort: workflow.can_abort(),
                destructive: workflow.is_destructive(),
            })
            .collect();

        let data = McpWorkflowsData {
            total: workflows.len(),
            workflows,
        };
        let envelope = McpEnvelope::success(data, elapsed_ms(start));
        envelope_as_resource("wa://workflows", envelope)
    }
}

pub(super) struct WaHerdWaveResource;

impl ResourceHandler for WaHerdWaveResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://herd-wave".to_string(),
            name: "ft herd wave".to_string(),
            description: Some(
                "Read-only herd-wave contract and dry-run planner surface".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "herd-wave".to_string(),
                "operator".to_string(),
            ],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let generated_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        let report = build_herd_wave_surface_report(
            "mcp.herd_wave",
            generated_at_ms,
            &[],
            HerdWaveEventKind::Wake,
            10,
            60_000,
            HerdWaveMcpResourceSurface::implemented("wa://herd-wave"),
        );
        envelope_as_resource(
            "wa://herd-wave",
            McpEnvelope::success(report, elapsed_ms(start)),
        )
    }
}

pub(super) struct WaRendererInputToPhotonResource;

impl ResourceHandler for WaRendererInputToPhotonResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI.to_string(),
            name: "ft renderer input-to-photon SLO".to_string(),
            description: Some(
                "Read-only input-to-photon renderer SLO status and evidence pointers".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "perf".to_string(),
                "renderer-slo".to_string(),
                "input-to-photon".to_string(),
            ],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let report = renderer_slos_doctor_report();
        let envelope = McpEnvelope::success(report.input_to_photon, elapsed_ms(start));
        envelope_as_resource(RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI, envelope)
    }
}

pub(super) struct WaRendererSsimParityResource;

impl ResourceHandler for WaRendererSsimParityResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: RENDERER_SSIM_PARITY_MCP_RESOURCE_URI.to_string(),
            name: "ft renderer SSIM parity SLO".to_string(),
            description: Some(
                "Read-only SSIM parity renderer SLO status and evidence pointers".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "perf".to_string(),
                "renderer-slo".to_string(),
                "ssim-parity".to_string(),
            ],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let report = renderer_slos_doctor_report();
        let envelope = McpEnvelope::success(report.ssim_parity, elapsed_ms(start));
        envelope_as_resource(RENDERER_SSIM_PARITY_MCP_RESOURCE_URI, envelope)
    }
}

pub(super) struct WaProofHistoryResource {
    config: Arc<Config>,
}

impl WaProofHistoryResource {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ResourceHandler for WaProofHistoryResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: PROOF_HISTORY_RESOURCE_URI.to_string(),
            name: "ft proof history".to_string(),
            description: Some(
                "Read-only proof-history rows with release-blocking summary".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "proof".to_string(),
                "proof-history".to_string(),
            ],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let payload = load_proof_history_query_for_resource(
            &self.config,
            ProofHistoryQuery {
                limit: 100,
                ..ProofHistoryQuery::default()
            },
        )
        .map_err(|err| McpError::internal_error(format!("Load proof history: {err}")))?;
        let envelope = McpEnvelope::success(payload, elapsed_ms(start));
        envelope_as_resource(PROOF_HISTORY_RESOURCE_URI, envelope)
    }
}

pub(super) struct WaProofHistoryReleaseBlockingResource {
    config: Arc<Config>,
}

impl WaProofHistoryReleaseBlockingResource {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ResourceHandler for WaProofHistoryReleaseBlockingResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: PROOF_HISTORY_RELEASE_BLOCKING_URI.to_string(),
            name: "ft proof history release blockers".to_string(),
            description: Some("Read-only proof-history rows blocking release".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "proof".to_string(),
                "proof-history".to_string(),
                "release-blocking".to_string(),
            ],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let payload = load_proof_history_query_for_resource(
            &self.config,
            ProofHistoryQuery {
                release_blocking_only: true,
                limit: 100,
                ..ProofHistoryQuery::default()
            },
        )
        .map_err(|err| McpError::internal_error(format!("Load proof history: {err}")))?;
        let envelope = McpEnvelope::success(payload, elapsed_ms(start));
        envelope_as_resource(PROOF_HISTORY_RELEASE_BLOCKING_URI, envelope)
    }
}

pub(super) struct WaProofHistoryTemplateResource {
    config: Arc<Config>,
}

impl WaProofHistoryTemplateResource {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ResourceHandler for WaProofHistoryTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://proof-history/template".to_string(),
            name: "ft proof history template".to_string(),
            description: Some("Template for filtered proof-history rows".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "proof".to_string(),
                "proof-history".to_string(),
            ],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://proof-history/{filter}/{value}/{limit}".to_string(),
            name: "ft proof history filtered".to_string(),
            description: Some(
                "Filter proof history by bead, category, status, or release-blocking".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "proof".to_string(),
                "proof-history".to_string(),
            ],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        WaProofHistoryResource::new(Arc::clone(&self.config)).read(ctx)
    }

    fn read_with_uri(
        &self,
        _ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let filter = params.get("filter").map(String::as_str).unwrap_or("all");
        let value = params
            .get("value")
            .map(|value| percent_decode_resource_param(value))
            .unwrap_or_default();
        let limit = params
            .get("limit")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100)
            .clamp(1, 1000);
        let mut query = ProofHistoryQuery {
            limit,
            ..ProofHistoryQuery::default()
        };

        match filter {
            "all" => {}
            "bead" => query.bead_id = Some(value),
            "category" => query.proof_category = Some(value),
            "status" => {
                query.status = proof_history_state_from_resource_param(&value);
                if query.status.is_none() {
                    return Err(McpError::invalid_params(
                        "Unsupported proof-history status filter",
                    ));
                }
            }
            "release-blocking" => query.release_blocking_only = true,
            _ => {
                return Err(McpError::invalid_params(
                    "Unsupported proof-history filter. Use all, bead, category, status, or release-blocking.",
                ));
            }
        }

        let start = Instant::now();
        let payload = load_proof_history_query_for_resource(&self.config, query)
            .map_err(|err| McpError::internal_error(format!("Load proof history: {err}")))?;
        let envelope = McpEnvelope::success(payload, elapsed_ms(start));
        envelope_as_resource(uri, envelope)
    }
}

pub(super) struct WaAttestationRetractionsResource {
    config: Arc<Config>,
}

impl WaAttestationRetractionsResource {
    pub(super) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl ResourceHandler for WaAttestationRetractionsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://attestation/retractions".to_string(),
            name: "ft attestation retractions".to_string(),
            description: Some("Active signed attestation retractions".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "attestation".to_string(),
                "retractions".to_string(),
            ],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let envelope = match attestation_retractions_root(&self.config) {
            Ok((workspace_root, root)) => {
                match load_attestation_retractions_for_resource(&workspace_root, &root) {
                    Ok(retractions) => {
                        let payload = serde_json::json!({
                            "schema_version": "ft.attestation.retractions.resource.v1",
                            "root": root.to_string_lossy(),
                            "active_retractions": retractions.len(),
                            "retractions": retractions,
                        });
                        McpEnvelope::success(payload, elapsed_ms(start))
                    }
                    Err(err) => McpEnvelope::<serde_json::Value>::error(
                        MCP_ERR_CONFIG,
                        format!("Failed to load attestation retractions: {err}"),
                        Some("Check docs/attestations/retractions JSON records.".to_string()),
                        elapsed_ms(start),
                    ),
                }
            }
            Err(err) => McpEnvelope::<serde_json::Value>::error(
                MCP_ERR_CONFIG,
                format!("Failed to resolve attestation retractions root: {err}"),
                Some("Set FT_WORKSPACE or run the MCP server from the workspace root.".to_string()),
                elapsed_ms(start),
            ),
        };
        envelope_as_resource("wa://attestation/retractions", envelope)
    }
}

pub(super) struct WaContextHorizonResource {
    db_path: Arc<PathBuf>,
}

impl WaContextHorizonResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaContextHorizonResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://context/horizon".to_string(),
            name: "ft context horizon".to_string(),
            description: Some("Read-only context pressure and handoff risk forecast".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec![
                "wa".to_string(),
                "context".to_string(),
                "horizon".to_string(),
            ],
        }
    }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        let start = Instant::now();
        let generated_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        let envelope = match predict_context_horizon_from_sqlite(
            self.db_path.as_ref().as_path(),
            None,
            generated_at_ms,
            15 * 60 * 1000,
            "mcp.context_horizon",
        ) {
            Ok(report) => McpEnvelope::success(
                serde_json::to_value(report).map_err(|err| {
                    McpError::internal_error(format!("Serialize context horizon: {err}"))
                })?,
                elapsed_ms(start),
            ),
            Err(err) => McpEnvelope::<serde_json::Value>::error(
                MCP_ERR_STORAGE,
                format!("Failed to build context horizon: {err}"),
                Some(
                    "Check the workspace database path; this MCP resource is read-only."
                        .to_string(),
                ),
                elapsed_ms(start),
            ),
        };
        envelope_as_resource("wa://context/horizon", envelope)
    }
}

pub(super) struct WaReservationsResource {
    db_path: Arc<PathBuf>,
}

impl WaReservationsResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaReservationsResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://reservations".to_string(),
            name: "ft reservations".to_string(),
            description: Some(
                "Active pane reservations (same data surface as wa.reservations)".to_string(),
            ),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "reservations".to_string()],
        }
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_reservations_resource(ctx, &self.db_path, "wa://reservations", None)
    }
}

pub(super) struct WaReservationsByPaneTemplateResource {
    db_path: Arc<PathBuf>,
}

impl WaReservationsByPaneTemplateResource {
    pub(super) fn new(db_path: Arc<PathBuf>) -> Self {
        Self { db_path }
    }
}

impl ResourceHandler for WaReservationsByPaneTemplateResource {
    fn definition(&self) -> Resource {
        Resource {
            uri: "wa://reservations/template".to_string(),
            name: "ft reservations template".to_string(),
            description: Some("Template for pane-filtered reservations".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "reservations".to_string()],
        }
    }

    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: "wa://reservations/{pane_id}".to_string(),
            name: "ft reservations by pane".to_string(),
            description: Some("Filter reservations by pane id".to_string()),
            mime_type: Some("application/json".to_string()),
            icon: None,
            version: Some(crate::VERSION.to_string()),
            tags: vec!["wa".to_string(), "reservations".to_string()],
        })
    }

    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        read_reservations_resource(ctx, &self.db_path, "wa://reservations", None)
    }

    fn read_with_uri(
        &self,
        ctx: &McpContext,
        uri: &str,
        params: &HashMap<String, String>,
    ) -> McpResult<Vec<ResourceContent>> {
        let pane_id = params
            .get("pane_id")
            .ok_or_else(|| McpError::invalid_params("Missing pane_id in resource URI"))?
            .parse::<u64>()
            .map_err(|_| McpError::invalid_params("pane_id must be an unsigned integer"))?;
        read_reservations_resource(ctx, &self.db_path, uri, Some(pane_id))
    }
}

#[cfg(test)]
mod tests {
    use super::McpEnvelope;
    use super::{
        WaAccountsByServiceTemplateResource, WaAccountsResource, WaAttestationRetractionsResource,
        WaContextHorizonResource, WaEventsResource, WaEventsTemplateResource,
        WaEventsUnhandledTemplateResource, WaHerdWaveResource, WaPanesResource,
        WaProofHistoryReleaseBlockingResource, WaProofHistoryResource,
        WaProofHistoryTemplateResource, WaRendererInputToPhotonResource,
        WaRendererSsimParityResource, WaReservationsByPaneTemplateResource, WaReservationsResource,
        WaRulesByAgentTemplateResource, WaRulesResource, WaWorkflowsResource, envelope_as_resource,
        load_proof_history_query_from_roots, tool_output_as_resource,
    };
    use crate::config::{Config, PaneFilterConfig};
    use crate::mcp_framework::{
        FrameworkContent as Content, FrameworkResourceHandler as ResourceHandler,
    };
    use crate::proof_lane::{
        ArtifactRetrievalStatus, ProofAttemptRecord, ProofBackend, ProofHistoryQuery,
        ProofRedactionStatus, ProofScope, ProofState,
    };
    use proptest::prelude::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn db_path() -> Arc<PathBuf> {
        Arc::new(PathBuf::from("/tmp/test-mcp.db"))
    }

    fn proof_history_test_record(state: ProofState, bead_id: &str) -> ProofAttemptRecord {
        let mut record = ProofAttemptRecord::new(
            format!("proof-{bead_id}"),
            bead_id,
            state,
            "proof.test",
            "test proof record",
        );
        record.attempted_at_utc = "2026-05-13T00:00:00Z".into();
        record.finished_at_utc = Some("2026-05-13T00:01:00Z".into());
        record.agent_name = "McpTest".into();
        record.cwd = "/repo".into();
        record.command = vec!["rch".into(), "exec".into(), "cargo".into(), "test".into()];
        record.proof_scope = ProofScope::CargoTest;
        record.required_backend = ProofBackend::Rch;
        record.observed_backend = ProofBackend::Rch;
        record
    }

    fn proof_records_to_jsonl(records: &[ProofAttemptRecord]) -> String {
        let mut jsonl = String::new();
        for record in records {
            jsonl.push_str(&serde_json::to_string(record).expect("serialize proof record"));
            jsonl.push('\n');
        }
        jsonl
    }

    // ========================================================================
    // tool_output_as_resource Tests
    // ========================================================================

    #[test]
    fn tool_output_as_resource_extracts_text() {
        let contents = vec![Content::Text {
            text: r#"{"ok":true}"#.to_string(),
        }];
        let result = tool_output_as_resource("wa://test", contents).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].uri, "wa://test");
        assert_eq!(result[0].mime_type.as_deref(), Some("application/json"));
        assert_eq!(result[0].text.as_deref(), Some(r#"{"ok":true}"#));
        assert!(result[0].blob.is_none());
    }

    #[test]
    fn tool_output_as_resource_empty_returns_error() {
        let result = tool_output_as_resource("wa://test", vec![]);
        assert!(result.is_err());
    }

    // ========================================================================
    // envelope_as_resource Tests
    // ========================================================================

    #[test]
    fn envelope_as_resource_serializes_to_json() {
        let envelope = McpEnvelope::success("hello", 42);
        let result = envelope_as_resource("wa://test", envelope).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].uri, "wa://test");
        let parsed: serde_json::Value =
            serde_json::from_str(result[0].text.as_ref().unwrap()).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"], "hello");
    }

    // ========================================================================
    // Resource Definition Stability Tests
    // ========================================================================

    #[test]
    fn panes_resource_definition_uri() {
        let resource = WaPanesResource::new(PaneFilterConfig::default(), None);
        let def = resource.definition();
        assert_eq!(def.uri, "wa://panes");
        assert_eq!(def.mime_type.as_deref(), Some("application/json"));
        assert!(def.tags.contains(&"wa".to_string()));
        assert!(def.tags.contains(&"panes".to_string()));
    }

    #[test]
    fn events_resource_definition_uri() {
        let resource = WaEventsResource::new(db_path());
        let def = resource.definition();
        assert_eq!(def.uri, "wa://events");
        assert!(def.tags.contains(&"events".to_string()));
    }

    #[test]
    fn events_template_resource_has_template() {
        let resource = WaEventsTemplateResource::new(db_path());
        let template = resource.template().expect("should have template");
        assert_eq!(template.uri_template, "wa://events/{limit}");
    }

    #[test]
    fn events_unhandled_template_resource_has_template() {
        let resource = WaEventsUnhandledTemplateResource::new(db_path());
        let template = resource.template().expect("should have template");
        assert_eq!(template.uri_template, "wa://events/unhandled/{limit}");
        assert!(template.tags.contains(&"unhandled".to_string()));
    }

    #[test]
    fn accounts_resource_definition_uri() {
        let resource = WaAccountsResource::new(db_path());
        let def = resource.definition();
        assert_eq!(def.uri, "wa://accounts");
        assert!(def.tags.contains(&"accounts".to_string()));
    }

    #[test]
    fn accounts_by_service_template_has_template() {
        let resource = WaAccountsByServiceTemplateResource::new(db_path());
        let template = resource.template().expect("should have template");
        assert_eq!(template.uri_template, "wa://accounts/{service}");
    }

    #[test]
    fn rules_resource_definition_uri() {
        let def = WaRulesResource.definition();
        assert_eq!(def.uri, "wa://rules");
        assert!(def.tags.contains(&"rules".to_string()));
    }

    #[test]
    fn rules_by_agent_template_has_template() {
        let template = WaRulesByAgentTemplateResource
            .template()
            .expect("should have template");
        assert_eq!(template.uri_template, "wa://rules/{agent_type}");
    }

    #[test]
    fn workflows_resource_definition_uri() {
        let resource = WaWorkflowsResource::new(Arc::new(Config::default()));
        let def = resource.definition();
        assert_eq!(def.uri, "wa://workflows");
        assert!(def.tags.contains(&"workflows".to_string()));
    }

    #[test]
    fn herd_wave_resource_definition_uri() {
        let def = WaHerdWaveResource.definition();
        assert_eq!(def.uri, "wa://herd-wave");
        assert!(def.tags.contains(&"herd-wave".to_string()));
        assert!(def.tags.contains(&"operator".to_string()));
    }

    #[test]
    fn renderer_input_to_photon_resource_definition_uri() {
        let def = WaRendererInputToPhotonResource.definition();
        assert_eq!(
            def.uri,
            crate::render_quality::RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI
        );
        assert!(def.tags.contains(&"perf".to_string()));
        assert!(def.tags.contains(&"renderer-slo".to_string()));
        assert!(def.tags.contains(&"input-to-photon".to_string()));
    }

    #[test]
    fn renderer_ssim_parity_resource_definition_uri() {
        let def = WaRendererSsimParityResource.definition();
        assert_eq!(
            def.uri,
            crate::render_quality::RENDERER_SSIM_PARITY_MCP_RESOURCE_URI
        );
        assert!(def.tags.contains(&"perf".to_string()));
        assert!(def.tags.contains(&"renderer-slo".to_string()));
        assert!(def.tags.contains(&"ssim-parity".to_string()));
    }

    #[test]
    fn attestation_retractions_resource_definition_uri() {
        let resource = WaAttestationRetractionsResource::new(Arc::new(Config::default()));
        let def = resource.definition();
        assert_eq!(def.uri, "wa://attestation/retractions");
        assert!(def.tags.contains(&"attestation".to_string()));
        assert!(def.tags.contains(&"retractions".to_string()));
    }

    #[test]
    fn herd_wave_resource_reads_v1_contract_without_mutation() {
        let resource = WaHerdWaveResource;
        let ctx = crate::mcp_framework::FrameworkMcpContext::new(fastmcp::Cx::for_testing(), 1);
        let contents = resource.read(&ctx).expect("read herd-wave resource");
        let payload: serde_json::Value =
            serde_json::from_str(contents[0].text.as_ref().unwrap()).expect("resource json");

        assert_eq!(payload["ok"].as_bool(), Some(true));
        assert_eq!(
            payload["data"]["contract_id"].as_str(),
            Some(crate::swarm_scheduler::HERD_WAVE_CONTRACT_ID)
        );
        assert_eq!(payload["data"]["source"].as_str(), Some("mcp.herd_wave"));
        assert_eq!(
            payload["data"]["raw_pane_content_stored"].as_bool(),
            Some(false)
        );
        assert_eq!(
            payload["data"]["dry_run_plan"]["live_mutation_allowed"].as_bool(),
            Some(false)
        );
        assert_eq!(
            payload["data"]["mcp_resource"]["implemented"].as_bool(),
            Some(true)
        );
        assert_eq!(
            payload["data"]["mcp_resource"]["uri"].as_str(),
            Some("wa://herd-wave")
        );
    }

    #[test]
    fn renderer_input_to_photon_resource_reads_doctor_contract() {
        let resource = WaRendererInputToPhotonResource;
        let ctx = crate::mcp_framework::FrameworkMcpContext::new(fastmcp::Cx::for_testing(), 1);
        let contents = resource
            .read(&ctx)
            .expect("read renderer input-to-photon resource");
        let payload: serde_json::Value =
            serde_json::from_str(contents[0].text.as_ref().unwrap()).expect("resource json");

        assert_eq!(payload["ok"].as_bool(), Some(true));
        assert_eq!(
            payload["data"]["mcp_resource_uri"].as_str(),
            Some(crate::render_quality::RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI)
        );
        assert_eq!(
            payload["data"]["status"].as_str(),
            Some(crate::render_quality::RENDERER_INPUT_TO_PHOTON_STATUS)
        );
        assert_eq!(
            payload["data"]["structured_log_template"].as_str(),
            Some("target/criterion/slo-input_to_photon_<platform>.jsonl")
        );
    }

    #[test]
    fn renderer_ssim_parity_resource_reads_doctor_contract() {
        let resource = WaRendererSsimParityResource;
        let ctx = crate::mcp_framework::FrameworkMcpContext::new(fastmcp::Cx::for_testing(), 1);
        let contents = resource
            .read(&ctx)
            .expect("read renderer SSIM parity resource");
        let payload: serde_json::Value =
            serde_json::from_str(contents[0].text.as_ref().unwrap()).expect("resource json");

        assert_eq!(payload["ok"].as_bool(), Some(true));
        assert_eq!(
            payload["data"]["mcp_resource_uri"].as_str(),
            Some(crate::render_quality::RENDERER_SSIM_PARITY_MCP_RESOURCE_URI)
        );
        assert_eq!(
            payload["data"]["status"].as_str(),
            Some(crate::render_quality::RENDERER_SSIM_PARITY_STATUS)
        );
        assert_eq!(
            payload["data"]["current_degradation"].as_str(),
            Some(crate::render_quality::RENDERER_SSIM_PARITY_CURRENT_DEGRADATION)
        );
        assert_eq!(
            payload["data"]["corpus_path"].as_str(),
            Some("tests/golden/gpu")
        );
        assert_eq!(
            payload["data"]["default_min_ssim_ppm"].as_u64(),
            Some(u64::from(
                crate::render_quality::RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM,
            ))
        );
        assert_eq!(
            payload["data"]["release_gate_script"].as_str(),
            Some("tests/e2e/test_ssim_parity_release_gate.sh")
        );
    }

    #[test]
    fn proof_history_resources_define_read_only_surfaces() {
        let config = Arc::new(Config::default());

        let def = WaProofHistoryResource::new(Arc::clone(&config)).definition();
        assert_eq!(def.uri, "wa://proof-history");
        assert!(def.tags.contains(&"proof-history".to_string()));

        let blocking = WaProofHistoryReleaseBlockingResource::new(Arc::clone(&config)).definition();
        assert_eq!(blocking.uri, "wa://proof-history/release-blocking");
        assert!(blocking.tags.contains(&"release-blocking".to_string()));

        let template = WaProofHistoryTemplateResource::new(config)
            .template()
            .expect("template definition");
        assert_eq!(
            template.uri_template,
            "wa://proof-history/{filter}/{value}/{limit}"
        );
    }

    #[test]
    fn proof_history_resource_loader_uses_canonical_query_contract() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let artifact_root = root.join("tests").join("e2e").join("artifacts");
        std::fs::create_dir_all(&artifact_root).expect("artifact root");

        let mut pass = proof_history_test_record(ProofState::Pass, "ft-mcp-pass");
        pass.proof_id = "proof-mcp-pass".into();
        pass.selected_worker = Some("vmi-proof".into());
        pass.remote_cargo_reached = true;
        pass.rustc_reached = true;
        pass.test_binary_started = true;
        pass.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        pass.redaction_status = ProofRedactionStatus::NoneNeeded;

        let mut blocked =
            proof_history_test_record(ProofState::InfraBlockedPreCargo, "ft-mcp-blocked");
        blocked.proof_id = "proof-mcp-blocked".into();
        blocked.reason_code = "proof.rch.pre_cargo_timeout_exec_missing".into();

        let proof_path = artifact_root.join("proof-records.jsonl");
        std::fs::write(&proof_path, proof_records_to_jsonl(&[pass, blocked]))
            .expect("write proof records");

        let result = load_proof_history_query_from_roots(
            root,
            vec![artifact_root],
            ProofHistoryQuery {
                release_blocking_only: true,
                limit: 10,
                ..ProofHistoryQuery::default()
            },
        )
        .expect("load proof history");

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.rows[0].bead_id, "ft-mcp-blocked");
        assert_eq!(result.release_blocking_summary.total_blocking_rows, 1);
        assert!(
            result.rows[0]
                .artifact_path
                .ends_with("proof-records.jsonl")
        );
        assert!(!result.rows[0].artifact_path.starts_with('/'));
    }

    #[test]
    fn context_horizon_resource_definition_uri() {
        let resource = WaContextHorizonResource::new(db_path());
        let def = resource.definition();
        assert_eq!(def.uri, "wa://context/horizon");
        assert!(def.tags.contains(&"context".to_string()));
        assert!(def.tags.contains(&"horizon".to_string()));
    }

    #[test]
    fn context_horizon_resource_reads_same_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("context-horizon.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
        conn.execute_batch(
            r"
            CREATE TABLE pane_contexts (
                context_id TEXT PRIMARY KEY NOT NULL,
                pane_id INTEGER NOT NULL,
                state TEXT NOT NULL,
                depth INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL,
                archived_at_ms INTEGER,
                token_budget INTEGER NOT NULL,
                tokens_consumed INTEGER NOT NULL,
                pressure_tier TEXT NOT NULL,
                source TEXT NOT NULL
            );
            CREATE TABLE context_rotations (
                rotation_id TEXT PRIMARY KEY NOT NULL,
                pane_id INTEGER NOT NULL,
                previous_context_id TEXT,
                new_context_id TEXT NOT NULL,
                strategy TEXT NOT NULL,
                reason TEXT,
                caller_idempotency_key TEXT,
                rotated_at_ms INTEGER NOT NULL,
                tokens_before INTEGER NOT NULL,
                tokens_after INTEGER NOT NULL,
                tokens_freed INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL
            );
            INSERT INTO pane_contexts
                (context_id, pane_id, state, depth, created_at_ms, token_budget,
                 tokens_consumed, pressure_tier, source)
            VALUES ('ctx-mcp', 11, 'active', 1, 1700000000000, 1000, 800,
                    'yellow', 'test');
            ",
        )
        .expect("seed context horizon db");
        drop(conn);

        let resource = WaContextHorizonResource::new(Arc::new(db_path));
        let ctx = crate::mcp_framework::FrameworkMcpContext::new(fastmcp::Cx::for_testing(), 1);
        let contents = resource.read(&ctx).expect("read context horizon resource");
        let payload: serde_json::Value =
            serde_json::from_str(contents[0].text.as_ref().unwrap()).expect("resource json");
        assert_eq!(payload["ok"].as_bool(), Some(true));
        assert_eq!(
            payload["data"]["contract_id"].as_str(),
            Some(crate::context_horizon::CONTEXT_HORIZON_CONTRACT_ID)
        );
        assert_eq!(
            payload["data"]["raw_context_content_stored"].as_bool(),
            Some(false)
        );
        assert_eq!(
            payload["data"]["pane_risks"][0]["pane_id"].as_u64(),
            Some(11)
        );
    }

    #[test]
    fn reservations_resource_definition_uri() {
        let resource = WaReservationsResource::new(db_path());
        let def = resource.definition();
        assert_eq!(def.uri, "wa://reservations");
        assert!(def.tags.contains(&"reservations".to_string()));
    }

    #[test]
    fn reservations_by_pane_template_has_template() {
        let resource = WaReservationsByPaneTemplateResource::new(db_path());
        let template = resource.template().expect("should have template");
        assert_eq!(template.uri_template, "wa://reservations/{pane_id}");
    }

    // ========================================================================
    // All resource URIs are unique
    // ========================================================================

    #[test]
    fn all_resource_uris_are_unique() {
        let db = db_path();
        let config = Arc::new(Config::default());
        let uris = [
            WaPanesResource::new(PaneFilterConfig::default(), None)
                .definition()
                .uri,
            WaEventsResource::new(Arc::clone(&db)).definition().uri,
            WaEventsTemplateResource::new(Arc::clone(&db))
                .definition()
                .uri,
            WaEventsUnhandledTemplateResource::new(Arc::clone(&db))
                .definition()
                .uri,
            WaAccountsResource::new(Arc::clone(&db)).definition().uri,
            WaAccountsByServiceTemplateResource::new(Arc::clone(&db))
                .definition()
                .uri,
            WaRulesResource.definition().uri,
            WaRulesByAgentTemplateResource.definition().uri,
            WaWorkflowsResource::new(Arc::new(Config::default()))
                .definition()
                .uri,
            WaRendererInputToPhotonResource.definition().uri,
            WaRendererSsimParityResource.definition().uri,
            WaProofHistoryResource::new(Arc::clone(&config))
                .definition()
                .uri,
            WaProofHistoryReleaseBlockingResource::new(Arc::clone(&config))
                .definition()
                .uri,
            WaProofHistoryTemplateResource::new(Arc::clone(&config))
                .definition()
                .uri,
            WaAttestationRetractionsResource::new(Arc::new(Config::default()))
                .definition()
                .uri,
            WaContextHorizonResource::new(Arc::clone(&db))
                .definition()
                .uri,
            WaReservationsResource::new(Arc::clone(&db))
                .definition()
                .uri,
            WaReservationsByPaneTemplateResource::new(Arc::clone(&db))
                .definition()
                .uri,
        ];
        let mut seen = std::collections::HashSet::new();
        for uri in &uris {
            assert!(seen.insert(uri.as_str()), "Duplicate URI: {uri}");
        }
    }

    // ========================================================================
    // All resource URIs use wa:// scheme
    // ========================================================================

    #[test]
    fn all_resource_uris_use_wa_scheme() {
        let db = db_path();
        let config = Arc::new(Config::default());
        let uris = [
            WaPanesResource::new(PaneFilterConfig::default(), None)
                .definition()
                .uri,
            WaEventsResource::new(Arc::clone(&db)).definition().uri,
            WaRulesResource.definition().uri,
            WaWorkflowsResource::new(Arc::new(Config::default()))
                .definition()
                .uri,
            WaRendererInputToPhotonResource.definition().uri,
            WaRendererSsimParityResource.definition().uri,
            WaProofHistoryResource::new(Arc::clone(&config))
                .definition()
                .uri,
            WaProofHistoryReleaseBlockingResource::new(Arc::clone(&config))
                .definition()
                .uri,
            WaProofHistoryTemplateResource::new(Arc::clone(&config))
                .definition()
                .uri,
            WaAttestationRetractionsResource::new(Arc::new(Config::default()))
                .definition()
                .uri,
            WaContextHorizonResource::new(Arc::clone(&db))
                .definition()
                .uri,
            WaReservationsResource::new(Arc::clone(&db))
                .definition()
                .uri,
        ];
        for uri in &uris {
            assert!(uri.starts_with("wa://"), "URI {uri} missing wa:// scheme");
        }
    }

    // ========================================================================
    // All definitions have JSON mime type
    // ========================================================================

    #[test]
    fn all_definitions_have_json_mime_type() {
        let db = db_path();
        let config = Arc::new(Config::default());
        let defs = [
            WaPanesResource::new(PaneFilterConfig::default(), None).definition(),
            WaEventsResource::new(Arc::clone(&db)).definition(),
            WaRulesResource.definition(),
            WaWorkflowsResource::new(Arc::new(Config::default())).definition(),
            WaRendererInputToPhotonResource.definition(),
            WaRendererSsimParityResource.definition(),
            WaProofHistoryResource::new(Arc::clone(&config)).definition(),
            WaProofHistoryReleaseBlockingResource::new(Arc::clone(&config)).definition(),
            WaProofHistoryTemplateResource::new(Arc::clone(&config)).definition(),
            WaAttestationRetractionsResource::new(Arc::new(Config::default())).definition(),
            WaContextHorizonResource::new(Arc::clone(&db)).definition(),
            WaReservationsResource::new(Arc::clone(&db)).definition(),
        ];
        for def in &defs {
            assert_eq!(
                def.mime_type.as_deref(),
                Some("application/json"),
                "Resource {} missing JSON mime type",
                def.uri
            );
        }
    }

    // ========================================================================
    // All definitions have version
    // ========================================================================

    #[test]
    fn all_definitions_have_version() {
        let db = db_path();
        let config = Arc::new(Config::default());
        let defs = [
            WaPanesResource::new(PaneFilterConfig::default(), None).definition(),
            WaEventsResource::new(Arc::clone(&db)).definition(),
            WaRulesResource.definition(),
            WaWorkflowsResource::new(Arc::new(Config::default())).definition(),
            WaRendererInputToPhotonResource.definition(),
            WaRendererSsimParityResource.definition(),
            WaProofHistoryResource::new(Arc::clone(&config)).definition(),
            WaProofHistoryReleaseBlockingResource::new(Arc::clone(&config)).definition(),
            WaProofHistoryTemplateResource::new(Arc::clone(&config)).definition(),
            WaAttestationRetractionsResource::new(Arc::new(Config::default())).definition(),
            WaContextHorizonResource::new(Arc::clone(&db)).definition(),
            WaReservationsResource::new(Arc::clone(&db)).definition(),
        ];
        for def in &defs {
            assert!(
                def.version.is_some(),
                "Resource {} missing version",
                def.uri
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn prop_tool_output_as_resource_preserves_uri_and_first_text(
            uri in "wa://[A-Za-z0-9/_-]{1,32}",
            first in "[A-Za-z0-9 _.,:/{}\\[\\]\"]{1,64}",
            second in "[A-Za-z0-9 _.,:/{}\\[\\]\"]{1,64}",
        ) {
            let contents = vec![
                Content::Text { text: first.clone() },
                Content::Text { text: second },
            ];
            let result = tool_output_as_resource(&uri, contents).expect("resource output");

            prop_assert_eq!(result.len(), 1);
            prop_assert_eq!(&result[0].uri, &uri);
            prop_assert_eq!(result[0].mime_type.as_deref(), Some("application/json"));
            prop_assert_eq!(result[0].text.as_deref(), Some(first.as_str()));
            prop_assert!(result[0].blob.is_none());
        }

        #[test]
        fn prop_envelope_as_resource_preserves_success_payload(
            uri in "wa://[A-Za-z0-9/_-]{1,32}",
            data in "[A-Za-z0-9 _.,:/-]{1,64}",
            elapsed_ms in any::<u64>(),
        ) {
            let envelope = McpEnvelope::success(data.clone(), elapsed_ms);
            let result = envelope_as_resource(&uri, envelope).expect("envelope resource");
            let parsed: serde_json::Value =
                serde_json::from_str(result[0].text.as_ref().expect("text payload")).expect("json");

            prop_assert_eq!(&result[0].uri, &uri);
            prop_assert_eq!(parsed["ok"].as_bool(), Some(true));
            prop_assert_eq!(parsed["data"].as_str(), Some(data.as_str()));
            prop_assert_eq!(parsed["elapsed_ms"].as_u64(), Some(elapsed_ms));
            prop_assert_eq!(parsed["mcp_version"].as_str(), Some("v1"));
        }

        #[test]
        fn prop_template_resources_keep_json_contract(service in "[A-Za-z0-9_-]{1,24}") {
            let db = db_path();
            let accounts = WaAccountsByServiceTemplateResource::new(Arc::clone(&db));
            let events = WaEventsTemplateResource::new(db);

            let accounts_template = accounts.template().expect("accounts template");
            let events_template = events.template().expect("events template");

            prop_assert_eq!(accounts_template.uri_template, "wa://accounts/{service}");
            prop_assert_eq!(events_template.uri_template, "wa://events/{limit}");
            prop_assert_eq!(accounts_template.mime_type.as_deref(), Some("application/json"));
            prop_assert_eq!(events_template.mime_type.as_deref(), Some("application/json"));
            prop_assert!(accounts_template.tags.contains(&"accounts".to_string()));
            prop_assert!(events_template.tags.contains(&"events".to_string()));
            prop_assert!(!service.is_empty());
        }
    }
}
