//! WezTerm Automata CLI
//!
//! Terminal hypervisor for AI agent swarms running in WezTerm.

#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use wa_core::logging::{LogConfig, LogError, init_logging};

/// WezTerm Automata - Terminal hypervisor for AI agents
#[derive(Parser)]
#[command(name = "wa")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Configuration file path
    #[arg(short, long, global = true)]
    config: Option<String>,

    /// Workspace root (overrides WA_WORKSPACE)
    #[arg(long, global = true, env = "WA_WORKSPACE")]
    workspace: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the watcher daemon
    Watch {
        /// Enable automatic workflow handling
        #[arg(long)]
        auto_handle: bool,

        /// Run in foreground (don't daemonize)
        #[arg(long)]
        foreground: bool,

        /// Discovery poll interval in milliseconds
        #[arg(long, default_value = "5000")]
        poll_interval: u64,

        /// Disable pattern detection
        #[arg(long)]
        no_patterns: bool,

        /// Disable single-instance lock (DANGEROUS: may corrupt data)
        #[arg(long)]
        dangerous_disable_lock: bool,
    },

    /// Robot mode commands (JSON I/O)
    Robot {
        #[command(subcommand)]
        command: Option<RobotCommands>,
    },

    /// Search captured output (FTS query)
    #[command(alias = "query")]
    Search {
        /// Search query (FTS5 syntax)
        query: String,

        /// Output format: auto, plain, or json
        #[arg(long, short = 'f', default_value = "auto")]
        format: String,

        /// Limit results
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Filter by pane ID
        #[arg(long, short = 'p')]
        pane: Option<u64>,

        /// Only return results since this timestamp (epoch ms or ISO8601)
        #[arg(long, short = 's')]
        since: Option<i64>,
    },

    /// List panes and their status
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show detailed pane information
    Show {
        /// Pane ID to show
        pane_id: u64,

        /// Include recent output
        #[arg(long)]
        output: bool,
    },

    /// Send text to a pane
    Send {
        /// Target pane ID
        pane_id: u64,

        /// Text to send
        text: String,

        /// Send character by character (no paste mode)
        #[arg(long)]
        no_paste: bool,

        /// Preview what would happen without executing
        #[arg(long)]
        dry_run: bool,
    },

    /// Get text from a pane
    GetText {
        /// Target pane ID
        pane_id: u64,

        /// Include escape sequences
        #[arg(long)]
        escapes: bool,
    },

    /// Workflow commands
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommands,
    },

    /// Show system status and pane overview
    Status {
        /// Output health check only (JSON)
        #[arg(long)]
        health: bool,

        /// Output format: auto, plain, or json
        #[arg(long, short = 'f', default_value = "auto")]
        format: String,

        /// Filter by domain (glob pattern)
        #[arg(long, short = 'd')]
        domain: Option<String>,

        /// Filter by agent type
        #[arg(long, short = 'a')]
        agent: Option<String>,

        /// Filter by pane ID
        #[arg(long, short = 'p')]
        pane_id: Option<u64>,
    },

    /// Show recent detection events
    Events {
        /// Output format: auto, plain, or json
        #[arg(long, short = 'f', default_value = "auto")]
        format: String,

        /// Maximum number of events to return
        #[arg(long, short = 'l', default_value = "20")]
        limit: usize,

        /// Filter by pane ID
        #[arg(long, short = 'p')]
        pane_id: Option<u64>,

        /// Filter by rule ID (exact match)
        #[arg(long, short = 'r')]
        rule_id: Option<String>,

        /// Filter by event type (e.g., "compaction_warning")
        #[arg(long, short = 't')]
        event_type: Option<String>,

        /// Only return unhandled events
        #[arg(long, short = 'u')]
        unhandled: bool,
    },

    /// Ingest external events (e.g., WezTerm user-var signals)
    Event {
        /// Event source is a WezTerm user-var change
        #[arg(long)]
        from_uservar: bool,

        /// Pane ID that emitted the user-var
        #[arg(long)]
        pane: u64,

        /// User-var name (e.g., "wa_event")
        #[arg(long)]
        name: String,

        /// Raw user-var value (typically base64-encoded JSON)
        #[arg(long)]
        value: String,
    },

    /// Explain decisions and workflows using built-in templates
    Why {
        /// Template ID to explain (e.g., "deny.alt_screen")
        template_id: Option<String>,

        /// Filter templates by category prefix (deny/workflow/event)
        #[arg(long)]
        category: Option<String>,

        /// Output format: auto, plain, or json
        #[arg(long, short = 'f', default_value = "auto")]
        format: String,

        /// List available templates
        #[arg(long)]
        list: bool,
    },

    /// Run diagnostics
    Doctor,

    /// Setup helpers
    Setup {
        #[command(subcommand)]
        command: SetupCommands,
    },

    /// Configuration management commands
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// Launch the interactive TUI (requires --features tui)
    #[cfg(feature = "tui")]
    Tui {
        /// Enable debug mode
        #[arg(long)]
        debug: bool,

        /// Refresh interval in seconds
        #[arg(long, default_value = "5")]
        refresh: u64,
    },
}

#[derive(Subcommand)]
#[command(disable_help_subcommand = true)]
enum RobotCommands {
    /// Show robot help as JSON
    Help,

    /// Show quick-start guide for agents (default when no subcommand)
    QuickStart,

    /// Get all panes as JSON
    State,

    /// Get text from a pane
    GetText {
        /// Pane ID
        pane_id: u64,

        /// Number of lines to return from the end (tail)
        #[arg(long, default_value = "50")]
        tail: usize,

        /// Include ANSI escape sequences
        #[arg(long)]
        escapes: bool,
    },

    /// Send text to a pane
    Send {
        /// Pane ID
        pane_id: u64,

        /// Text to send
        text: String,

        /// Preview what would happen without executing
        #[arg(long)]
        dry_run: bool,

        /// Verify by waiting for a pattern after sending
        #[arg(long)]
        wait_for: Option<String>,

        /// Timeout for wait-for verification (seconds)
        #[arg(long, default_value = "30")]
        timeout_secs: u64,

        /// Treat wait-for pattern as regex
        #[arg(long)]
        wait_for_regex: bool,
    },

    /// Wait for a pattern in pane output
    WaitFor {
        /// Pane ID
        pane_id: u64,

        /// Pattern to wait for (substring by default, regex with --regex)
        pattern: String,

        /// Timeout in seconds
        #[arg(long, default_value = "30")]
        timeout_secs: u64,

        /// Number of tail lines to consider (0 = full buffer)
        #[arg(long, default_value = "200")]
        tail: usize,

        /// Treat pattern as a regex instead of substring
        #[arg(long)]
        regex: bool,
    },

    /// Search captured output
    Search {
        /// FTS query
        query: String,

        /// Limit number of results
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Filter by pane ID
        #[arg(long)]
        pane: Option<u64>,

        /// Only return results since this timestamp (epoch ms)
        #[arg(long)]
        since: Option<i64>,

        /// Include snippets with highlighted terms
        #[arg(long)]
        snippets: bool,
    },

    /// Get recent events
    Events {
        /// Maximum number of events to return
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Filter by pane ID
        #[arg(long)]
        pane: Option<u64>,

        /// Filter by rule ID (exact match)
        #[arg(long)]
        rule_id: Option<String>,

        /// Filter by event type (e.g., "compaction_warning")
        #[arg(long)]
        event_type: Option<String>,

        /// Only return unhandled events
        #[arg(long, visible_alias = "unhandled-only")]
        unhandled: bool,

        /// Only return events since this timestamp (epoch ms)
        #[arg(long)]
        since: Option<i64>,
    },

    /// Workflow management commands
    Workflow {
        #[command(subcommand)]
        command: RobotWorkflowCommands,
    },

    /// Explain an error code or policy denial
    Why {
        /// Error code or template ID to explain (e.g., "deny.alt_screen", "robot.policy_denied")
        code: String,
    },

    /// Submit an approval code for a pending action
    Approve {
        /// The approval code (8-character alphanumeric)
        code: String,

        /// Target pane ID for fingerprint validation (optional)
        #[arg(long)]
        pane: Option<u64>,

        /// Expected action fingerprint (optional)
        #[arg(long)]
        fingerprint: Option<String>,

        /// Check approval status without consuming
        #[arg(long)]
        dry_run: bool,
    },
}

/// Robot workflow subcommands
#[derive(Subcommand)]
enum RobotWorkflowCommands {
    /// Run a workflow by name on a pane
    Run {
        /// Workflow name
        name: String,

        /// Target pane ID
        pane_id: u64,

        /// Bypass "already handled" checks (still policy-gated)
        #[arg(long)]
        force: bool,

        /// Preview what would happen without executing
        #[arg(long)]
        dry_run: bool,
    },

    /// List available workflows
    List,

    /// Check workflow execution status
    Status {
        /// Execution ID
        execution_id: String,
    },

    /// Abort a running workflow
    Abort {
        /// Execution ID
        execution_id: String,

        /// Reason for aborting
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkflowCommands {
    /// List available workflows
    List,

    /// Run a workflow
    Run {
        /// Workflow name
        name: String,

        /// Target pane ID
        #[arg(long)]
        pane: u64,

        /// Preview what would happen without executing
        #[arg(long)]
        dry_run: bool,
    },

    /// Show workflow execution status
    Status {
        /// Execution ID
        execution_id: String,
    },
}

#[derive(Subcommand)]
enum SetupCommands {
    /// Setup local WezTerm configuration
    Local,

    /// Setup remote host
    Remote {
        /// SSH host (from ~/.ssh/config)
        host: String,
    },

    /// Generate WezTerm config additions
    Config,
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Initialize configuration (creates default config if absent)
    Init {
        /// Overwrite existing config (dangerous)
        #[arg(long)]
        force: bool,

        /// Custom config path (default: ./wa.toml or ~/.config/wa/wa.toml)
        #[arg(long)]
        path: Option<String>,
    },

    /// Validate configuration file (schema + semantic checks)
    Validate {
        /// Custom config path to validate
        #[arg(long)]
        path: Option<String>,

        /// Strict validation (fail on warnings)
        #[arg(long)]
        strict: bool,
    },

    /// Show current configuration
    Show {
        /// Show effective config including resolved paths
        #[arg(long)]
        effective: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Custom config path
        #[arg(long)]
        path: Option<String>,
    },

    /// Set a configuration value
    Set {
        /// Configuration key (dot notation, e.g., "general.log_level")
        key: String,

        /// Value to set
        value: String,

        /// Custom config path
        #[arg(long)]
        path: Option<String>,
    },
}

const ROBOT_ERR_INVALID_ARGS: &str = "robot.invalid_args";
const ROBOT_ERR_UNKNOWN_SUBCOMMAND: &str = "robot.unknown_subcommand";
const ROBOT_ERR_CONFIG: &str = "robot.config_error";
const ROBOT_ERR_FTS_QUERY: &str = "robot.fts_query_error";
const ROBOT_ERR_STORAGE: &str = "robot.storage_error";

/// JSON envelope for robot mode responses
#[derive(serde::Serialize)]
struct RobotResponse<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    elapsed_ms: u64,
    version: String,
    now: u64,
}

impl<T> RobotResponse<T> {
    fn success(data: T, elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms,
            version: wa_core::VERSION.to_string(),
            now: now_ms(),
        }
    }

    fn error_with_code(
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
            version: wa_core::VERSION.to_string(),
            now: now_ms(),
        }
    }
}

#[derive(serde::Serialize)]
struct RobotHelp {
    commands: Vec<RobotCommandInfo>,
    global_flags: Vec<&'static str>,
}

#[derive(serde::Serialize)]
struct RobotCommandInfo {
    name: &'static str,
    description: &'static str,
}

/// Quick-start data for agents
#[derive(serde::Serialize)]
struct RobotQuickStartData {
    description: &'static str,
    global_flags: Vec<QuickStartGlobalFlag>,
    core_loop: Vec<QuickStartStep>,
    commands: Vec<QuickStartCommand>,
    tips: Vec<&'static str>,
    error_handling: QuickStartErrorHandling,
}

#[derive(serde::Serialize)]
struct QuickStartGlobalFlag {
    flag: &'static str,
    env_var: Option<&'static str>,
    description: &'static str,
}

#[derive(serde::Serialize)]
struct QuickStartStep {
    step: u8,
    action: &'static str,
    command: &'static str,
}

#[derive(serde::Serialize)]
struct QuickStartCommand {
    name: &'static str,
    args: &'static str,
    summary: &'static str,
    examples: Vec<&'static str>,
}

#[derive(serde::Serialize)]
struct QuickStartErrorHandling {
    common_codes: Vec<QuickStartErrorCode>,
    safety_notes: Vec<&'static str>,
}

#[derive(serde::Serialize)]
struct QuickStartErrorCode {
    code: &'static str,
    meaning: &'static str,
    recovery: &'static str,
}

/// Pane state for CLI output (list and robot state commands)
#[derive(serde::Serialize)]
struct PaneState {
    pane_id: u64,
    /// Stable pane UUID (assigned at discovery, persists across renames/moves)
    /// Will be null until the pane has been observed by the daemon.
    pane_uuid: Option<String>,
    tab_id: u64,
    window_id: u64,
    domain: String,
    title: Option<String>,
    cwd: Option<String>,
    observed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    ignore_reason: Option<String>,
}

impl PaneState {
    fn from_pane_info(
        info: &wa_core::wezterm::PaneInfo,
        filter: &wa_core::config::PaneFilterConfig,
    ) -> Self {
        let domain = info.inferred_domain();
        let title = info.title.clone().unwrap_or_default();
        let cwd = info.cwd.clone().unwrap_or_default();

        let ignore_reason = filter.check_pane(&domain, &title, &cwd);

        Self {
            pane_id: info.pane_id,
            pane_uuid: None, // Not assigned until pane is observed by daemon
            tab_id: info.tab_id,
            window_id: info.window_id,
            domain,
            title: info.title.clone(),
            cwd: info.cwd.clone(),
            observed: ignore_reason.is_none(),
            ignore_reason,
        }
    }

    fn format_human(&self) -> String {
        let status = if self.observed { "observed" } else { "ignored" };
        let reason = self
            .ignore_reason
            .as_ref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        let title = self.title.as_deref().unwrap_or("(untitled)");
        let cwd = self.cwd.as_deref().unwrap_or("(unknown)");

        format!(
            "  {:>4}  {:>10}  {:<20}  {:<40}  {}{}",
            self.pane_id, status, title, cwd, self.domain, reason
        )
    }
}

/// Robot get-text response data (matches wa-robot-get-text.json schema)
#[derive(serde::Serialize)]
struct RobotGetTextData {
    pane_id: u64,
    text: String,
    tail_lines: usize,
    escapes_included: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation_info: Option<TruncationInfo>,
}

#[derive(serde::Serialize)]
struct TruncationInfo {
    original_bytes: usize,
    returned_bytes: usize,
    original_lines: usize,
    returned_lines: usize,
}

/// Wait-for result data for robot mode
#[derive(serde::Serialize)]
struct RobotWaitForData {
    pane_id: u64,
    pattern: String,
    matched: bool,
    elapsed_ms: u64,
    polls: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_regex: bool,
}

/// Robot send response data
#[derive(serde::Serialize)]
struct RobotSendData {
    pane_id: u64,
    injection: wa_core::policy::InjectionResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_for: Option<RobotWaitForData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_error: Option<String>,
}

/// Search result data for robot mode
#[derive(serde::Serialize)]
struct RobotSearchData {
    query: String,
    results: Vec<RobotSearchHit>,
    total_hits: usize,
    limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_filter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_filter: Option<i64>,
}

/// Individual search hit for robot mode
#[derive(serde::Serialize)]
struct RobotSearchHit {
    segment_id: i64,
    pane_id: u64,
    seq: u64,
    captured_at: i64,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

/// Robot events response data
#[derive(serde::Serialize)]
struct RobotEventsData {
    events: Vec<RobotEventItem>,
    total_count: usize,
    limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_filter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule_id_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_type_filter: Option<String>,
    unhandled_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_filter: Option<i64>,
}

/// Individual event for robot mode
#[derive(serde::Serialize)]
struct RobotEventItem {
    id: i64,
    pane_id: u64,
    rule_id: String,
    pack_id: String,
    event_type: String,
    severity: String,
    confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    extracted: Option<serde_json::Value>,
    captured_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    handled_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_id: Option<String>,
}

/// Workflow execution result for robot mode
#[derive(Debug, serde::Serialize)]
struct RobotWorkflowData {
    workflow_name: String,
    pane_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_id: Option<String>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steps_executed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
}

/// Robot why command response data (matches wa-robot-why.json schema)
#[derive(Debug, serde::Serialize)]
struct RobotWhyData {
    code: String,
    category: String,
    title: String,
    explanation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    see_also: Option<Vec<String>>,
}

/// Robot approve command response data (matches wa-robot-approve.json schema)
#[derive(Debug, serde::Serialize)]
struct RobotApproveData {
    code: String,
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    consumed_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
}

#[derive(Debug)]
struct RobotApproveError {
    code: &'static str,
    message: String,
    hint: Option<String>,
}

impl RobotApproveError {
    fn new(code: &'static str, message: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint,
        }
    }
}

/// Robot workflow list response data (matches wa-robot-workflow-list.json schema)
#[derive(Debug, serde::Serialize)]
struct RobotWorkflowListData {
    workflows: Vec<RobotWorkflowInfo>,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_count: Option<usize>,
}

#[derive(Debug, serde::Serialize)]
struct RobotWorkflowInfo {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_event_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requires_pane: Option<bool>,
}

/// Robot workflow status response data (matches wa-robot-workflow-status.json schema)
#[derive(Debug, serde::Serialize)]
struct RobotWorkflowStatusData {
    execution_id: String,
    workflow_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_id: Option<u64>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_step: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_steps: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<u64>,
}

/// Robot workflow abort response data (matches wa-robot-workflow-abort.json schema)
#[derive(Debug, serde::Serialize)]
struct RobotWorkflowAbortData {
    execution_id: String,
    aborted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aborted_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_reason: Option<String>,
}

fn redact_for_output(text: &str) -> String {
    static REDACTOR: LazyLock<wa_core::policy::Redactor> =
        LazyLock::new(wa_core::policy::Redactor::new);
    REDACTOR.redact(text)
}

async fn evaluate_robot_approve(
    storage: &wa_core::storage::StorageHandle,
    workspace_id: &str,
    code: &str,
    pane: Option<u64>,
    fingerprint: Option<&str>,
    dry_run: bool,
) -> Result<RobotApproveData, RobotApproveError> {
    let code_hash = wa_core::approval::hash_allow_once_code(code);

    let token = match storage.get_approval_token(&code_hash).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err(RobotApproveError::new(
                "E_APPROVAL_NOT_FOUND",
                format!("No approval found with code: {code}"),
                Some(
                    "Approval codes are issued when policy requires approval. \
                     Check that you have the correct code."
                        .to_string(),
                ),
            ));
        }
        Err(e) => {
            return Err(RobotApproveError::new(
                ROBOT_ERR_STORAGE,
                format!("Failed to look up approval: {e}"),
                None,
            ));
        }
    };

    // Workspace scope check (explicit to provide E_WRONG_WORKSPACE)
    if token.workspace_id != workspace_id {
        return Err(RobotApproveError::new(
            "E_WRONG_WORKSPACE",
            format!(
                "Approval code is scoped to workspace '{}', but current workspace is '{}'",
                token.workspace_id, workspace_id
            ),
            Some("Use the approval code in the workspace where it was issued.".to_string()),
        ));
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    if token.used_at.is_some() {
        return Err(RobotApproveError::new(
            "E_APPROVAL_CONSUMED",
            "Approval code has already been used".to_string(),
            Some(
                "Approval codes can only be used once. Request a new approval if needed."
                    .to_string(),
            ),
        ));
    }

    if token.expires_at < now_ms {
        return Err(RobotApproveError::new(
            "E_APPROVAL_EXPIRED",
            format!(
                "Approval code expired at {} (now: {})",
                token.expires_at, now_ms
            ),
            Some(
                "Approval codes have a limited validity period. Request a new approval."
                    .to_string(),
            ),
        ));
    }

    if let Some(expected_pane) = pane {
        if token.pane_id != Some(expected_pane) {
            return Err(RobotApproveError::new(
                "E_WRONG_PANE",
                format!(
                    "Approval was for pane {:?}, but --pane {} was specified",
                    token.pane_id, expected_pane
                ),
                Some("Use the correct pane ID or omit --pane.".to_string()),
            ));
        }
    }

    if let Some(expected_fingerprint) = fingerprint {
        if token.action_fingerprint != expected_fingerprint {
            return Err(RobotApproveError::new(
                "E_FINGERPRINT_MISMATCH",
                "Approval fingerprint does not match the requested action".to_string(),
                Some(
                    "The approval was issued for a different action. Request a new approval."
                        .to_string(),
                ),
            ));
        }
    }

    if dry_run {
        return Ok(RobotApproveData {
            code: code.to_string(),
            valid: true,
            action_kind: Some(token.action_kind),
            pane_id: token.pane_id,
            expires_at: Some(token.expires_at as u64),
            consumed_at: None,
            action_fingerprint: Some(token.action_fingerprint),
            dry_run: Some(true),
        });
    }

    let consumed_token = match storage
        .consume_approval_token_by_code(&code_hash, workspace_id)
        .await
    {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err(RobotApproveError::new(
                "E_APPROVAL_CONSUMED",
                "Approval code was consumed by another process".to_string(),
                Some(
                    "The approval was valid but consumed between validation and consumption. \
                     This is a race condition."
                        .to_string(),
                ),
            ));
        }
        Err(e) => {
            return Err(RobotApproveError::new(
                ROBOT_ERR_STORAGE,
                format!("Failed to consume approval: {e}"),
                None,
            ));
        }
    };

    let consumed_at = consumed_token.used_at.unwrap_or(now_ms);

    let audit = wa_core::storage::AuditActionRecord {
        id: 0,
        ts: consumed_at,
        actor_kind: "robot".to_string(),
        actor_id: None,
        pane_id: consumed_token.pane_id,
        domain: None,
        action_kind: "approve_allow_once".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("Robot submitted approval code".to_string()),
        rule_id: None,
        input_summary: Some(format!(
            "wa robot approve {} for {} pane {:?}",
            code, consumed_token.action_kind, consumed_token.pane_id
        )),
        verification_summary: Some(format!(
            "code_hash={}, fingerprint={}",
            code_hash, consumed_token.action_fingerprint
        )),
        decision_context: None,
        result: "success".to_string(),
    };

    if let Err(e) = storage.record_audit_action_redacted(audit).await {
        tracing::warn!("Failed to record approval audit: {e}");
    }

    Ok(RobotApproveData {
        code: code.to_string(),
        valid: true,
        action_kind: Some(consumed_token.action_kind),
        pane_id: consumed_token.pane_id,
        expires_at: Some(consumed_token.expires_at as u64),
        consumed_at: Some(consumed_at as u64),
        action_fingerprint: Some(consumed_token.action_fingerprint),
        dry_run: None,
    })
}

fn build_send_dry_run_report(
    command_ctx: &wa_core::dry_run::CommandContext,
    pane_id: u64,
    pane_info: Option<&wa_core::wezterm::PaneInfo>,
    text: &str,
    no_paste: bool,
    wait_for: Option<&str>,
    timeout_secs: u64,
    config: &wa_core::config::Config,
) -> wa_core::dry_run::DryRunReport {
    use wa_core::dry_run::{
        TargetResolution, build_send_policy_evaluation, create_send_action, create_wait_for_action,
    };
    use wa_core::policy::PaneCapabilities;

    let mut ctx = command_ctx.dry_run_context();

    // Target resolution (best-effort)
    if let Some(info) = pane_info {
        let mut target =
            TargetResolution::new(pane_id, info.inferred_domain()).with_is_active(info.is_active);
        if let Some(title) = &info.title {
            target = target.with_title(title.clone());
        }
        if let Some(cwd) = &info.cwd {
            target = target.with_cwd(cwd.clone());
        }
        ctx.set_target(target);
    } else {
        ctx.set_target(TargetResolution::new(pane_id, "unknown"));
        ctx.add_warning("Pane metadata unavailable; ensure WezTerm is running and pane exists.");
    }

    // Policy evaluation (best-effort; uses config defaults + assumed prompt state)
    let capabilities = PaneCapabilities::prompt();
    let eval = build_send_policy_evaluation(
        (0, config.safety.rate_limit_per_pane),
        capabilities.prompt_active,
        config.safety.require_prompt_active,
        capabilities.has_recent_gap,
    );
    ctx.set_policy_evaluation(eval);

    // Expected actions
    ctx.add_action(create_send_action(1, pane_id, text.len()));
    if let Some(pattern) = wait_for {
        let timeout_ms = timeout_secs.saturating_mul(1000);
        ctx.add_action(create_wait_for_action(2, pattern, timeout_ms));
    }

    if no_paste {
        ctx.add_warning("no_paste mode sends characters individually (slower)");
    }

    ctx.take_report()
}

fn build_workflow_dry_run_report(
    command_ctx: &wa_core::dry_run::CommandContext,
    name: &str,
    pane: u64,
) -> wa_core::dry_run::DryRunReport {
    use wa_core::dry_run::{
        ActionType, PlannedAction, PolicyCheck, PolicyEvaluation, TargetResolution,
    };

    let mut ctx = command_ctx.dry_run_context();

    // Target resolution
    ctx.set_target(
        TargetResolution::new(pane, "local")
            .with_title("(pane title)")
            .with_agent_type("(detected agent)"),
    );

    // Policy evaluation for workflow
    let mut eval = PolicyEvaluation::new();
    eval.add_check(PolicyCheck::passed(
        "workflow_enabled",
        format!("Workflow '{name}' is enabled"),
    ));
    eval.add_check(PolicyCheck::passed("pane_state", "Pane is in valid state"));
    eval.add_check(PolicyCheck::passed("policy", "Workflow execution allowed"));
    ctx.set_policy_evaluation(eval);

    // Expected workflow steps (example for handle_compaction)
    ctx.add_action(PlannedAction::new(
        1,
        ActionType::AcquireLock,
        format!("Acquire workflow lock for pane {pane}"),
    ));
    ctx.add_action(PlannedAction::new(
        2,
        ActionType::WaitFor,
        "Stabilize: wait for tail stability (no new deltas for N polls; max 2s)".to_string(),
    ));
    ctx.add_action(PlannedAction::new(
        3,
        ActionType::SendText,
        "Send re-read instruction to agent".to_string(),
    ));
    ctx.add_action(PlannedAction::new(
        4,
        ActionType::WaitFor,
        "Verify: wait for prompt boundary".to_string(),
    ));
    ctx.add_action(PlannedAction::new(
        5,
        ActionType::MarkEventHandled,
        "Mark triggering event as handled".to_string(),
    ));
    ctx.add_action(PlannedAction::new(
        6,
        ActionType::ReleaseLock,
        "Release workflow lock".to_string(),
    ));

    ctx.take_report()
}

#[allow(dead_code)]
struct RobotContext {
    effective: wa_core::config::EffectiveConfig,
}

fn build_robot_context(
    config: &wa_core::config::Config,
    workspace_root: &Path,
) -> anyhow::Result<RobotContext> {
    let effective = config.effective_config(Some(workspace_root))?;
    Ok(RobotContext { effective })
}

fn build_robot_help() -> RobotHelp {
    RobotHelp {
        commands: vec![
            RobotCommandInfo {
                name: "help",
                description: "Show this help as JSON",
            },
            RobotCommandInfo {
                name: "state",
                description: "List panes with metadata",
            },
            RobotCommandInfo {
                name: "get-text",
                description: "Fetch recent pane output",
            },
            RobotCommandInfo {
                name: "send",
                description: "Send text to a pane",
            },
            RobotCommandInfo {
                name: "wait-for",
                description: "Wait for a pattern on a pane",
            },
            RobotCommandInfo {
                name: "search",
                description: "Search captured output",
            },
            RobotCommandInfo {
                name: "events",
                description: "Fetch recent events",
            },
            RobotCommandInfo {
                name: "workflow run",
                description: "Run a workflow by name on a pane",
            },
            RobotCommandInfo {
                name: "workflow list",
                description: "List available workflows",
            },
            RobotCommandInfo {
                name: "workflow status",
                description: "Check workflow execution status",
            },
            RobotCommandInfo {
                name: "workflow abort",
                description: "Abort a running workflow",
            },
            RobotCommandInfo {
                name: "why",
                description: "Explain an error code or policy denial",
            },
            RobotCommandInfo {
                name: "approve",
                description: "Submit an approval code for a pending action",
            },
        ],
        global_flags: vec!["--workspace <path>", "--config <path>", "--verbose"],
    }
}

fn build_robot_quick_start() -> RobotQuickStartData {
    RobotQuickStartData {
        description: "wa robot mode: JSON API for AI agents to observe and control WezTerm panes",
        global_flags: vec![
            QuickStartGlobalFlag {
                flag: "--workspace <path>",
                env_var: Some("WA_WORKSPACE"),
                description: "Project root directory for config and database",
            },
            QuickStartGlobalFlag {
                flag: "--config <path>",
                env_var: None,
                description: "Override config file location",
            },
        ],
        core_loop: vec![
            QuickStartStep {
                step: 1,
                action: "Discover panes",
                command: "wa robot state",
            },
            QuickStartStep {
                step: 2,
                action: "Select target pane_id from state output",
                command: "(parse JSON, pick pane_id)",
            },
            QuickStartStep {
                step: 3,
                action: "Read pane output",
                command: "wa robot get-text <pane_id> --tail 100",
            },
            QuickStartStep {
                step: 4,
                action: "Preview send (safety-first)",
                command: "wa robot send <pane_id> \"text\" --dry-run",
            },
            QuickStartStep {
                step: 5,
                action: "Execute send (if policy allows)",
                command: "wa robot send <pane_id> \"text\"",
            },
        ],
        commands: vec![
            QuickStartCommand {
                name: "state",
                args: "",
                summary: "List all panes with metadata (pane_id, title, cwd, observed)",
                examples: vec!["wa robot state"],
            },
            QuickStartCommand {
                name: "get-text",
                args: "<pane_id> [--tail N] [--escapes]",
                summary: "Fetch recent output from a pane",
                examples: vec!["wa robot get-text 0", "wa robot get-text 0 --tail 200"],
            },
            QuickStartCommand {
                name: "send",
                args: "<pane_id> \"<text>\" [--dry-run]",
                summary: "Send text input to a pane (policy-gated)",
                examples: vec![
                    "wa robot send 0 \"/help\" --dry-run",
                    "wa robot send 0 \"/compact\"",
                ],
            },
            QuickStartCommand {
                name: "wait-for",
                args: "<pane_id> <pattern> [--timeout-secs N] [--regex]",
                summary: "Wait for a pattern to appear in pane output",
                examples: vec![
                    "wa robot wait-for 0 \"ready>\"",
                    "wa robot wait-for 0 \"error|failed\" --regex --timeout-secs 60",
                ],
            },
            QuickStartCommand {
                name: "search",
                args: "\"<query>\" [--limit N] [--pane ID]",
                summary: "Full-text search across captured output",
                examples: vec![
                    "wa robot search \"compilation failed\"",
                    "wa robot search \"error\" --pane 0 --limit 10",
                ],
            },
            QuickStartCommand {
                name: "events",
                args: "[--limit N] [--pane ID] [--rule-id ID]",
                summary: "Fetch recent pattern detection events",
                examples: vec![
                    "wa robot events",
                    "wa robot events --rule-id codex.usage_reached",
                ],
            },
            QuickStartCommand {
                name: "workflow run",
                args: "<name> <pane_id> [--force] [--dry-run]",
                summary: "Run a workflow by name on a pane (policy-gated)",
                examples: vec![
                    "wa robot workflow run handle_compaction 0",
                    "wa robot workflow run handle_usage_limit 1 --force",
                ],
            },
            QuickStartCommand {
                name: "workflow list",
                args: "",
                summary: "List available workflows",
                examples: vec!["wa robot workflow list"],
            },
            QuickStartCommand {
                name: "workflow status",
                args: "<execution_id>",
                summary: "Check workflow execution status",
                examples: vec!["wa robot workflow status robot-handle_compaction-1234567890"],
            },
            QuickStartCommand {
                name: "workflow abort",
                args: "<execution_id> [--reason \"<reason>\"]",
                summary: "Abort a running workflow",
                examples: vec![
                    "wa robot workflow abort robot-handle_compaction-1234567890 --reason \"User requested\"",
                ],
            },
            QuickStartCommand {
                name: "why",
                args: "<code>",
                summary: "Explain an error code or policy denial",
                examples: vec![
                    "wa robot why deny.alt_screen",
                    "wa robot why robot.policy_denied",
                ],
            },
            QuickStartCommand {
                name: "approve",
                args: "<code> [--pane <id>] [--dry-run] [--fingerprint <hash>]",
                summary: "Submit an approval code for a pending action",
                examples: vec![
                    "wa robot approve ABC12345",
                    "wa robot approve ABC12345 --dry-run",
                ],
            },
            QuickStartCommand {
                name: "help",
                args: "",
                summary: "Show command list as JSON",
                examples: vec!["wa robot help"],
            },
            QuickStartCommand {
                name: "quick-start",
                args: "",
                summary: "Show this quick-start guide",
                examples: vec!["wa robot quick-start", "wa robot"],
            },
        ],
        tips: vec![
            "Always use --dry-run before send to preview policy decisions",
            "The 'ok' field in responses indicates success (true) or failure (false)",
            "Check 'error_code' field for programmatic error handling",
            "Use 'now' timestamp in responses to track freshness",
            "Pane IDs are stable within a WezTerm session but may change across restarts",
        ],
        error_handling: QuickStartErrorHandling {
            common_codes: vec![
                QuickStartErrorCode {
                    code: "robot.pane_not_found",
                    meaning: "The specified pane_id does not exist",
                    recovery: "Use 'wa robot state' to list valid pane IDs",
                },
                QuickStartErrorCode {
                    code: "robot.wezterm_not_running",
                    meaning: "WezTerm is not running or not accessible",
                    recovery: "Start WezTerm before running wa commands",
                },
                QuickStartErrorCode {
                    code: "robot.policy_denied",
                    meaning: "Action blocked by safety policy",
                    recovery: "Use --dry-run to see policy details; adjust config if needed",
                },
                QuickStartErrorCode {
                    code: "robot.require_approval",
                    meaning: "Action requires human approval",
                    recovery: "A human must approve via 'wa approve <token>' or interactive prompt",
                },
                QuickStartErrorCode {
                    code: "robot.timeout",
                    meaning: "Wait operation timed out",
                    recovery: "Increase --timeout-secs or check if pattern is correct",
                },
            ],
            safety_notes: vec![
                "All send operations are policy-gated by default",
                "--dry-run shows what would happen without executing",
                "RequireApproval decisions surface a token for human approval",
                "Approval can be granted via 'wa approve <token>' command",
            ],
        },
    }
}

/// Helper to convert elapsed time to u64 milliseconds safely
fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| u64::try_from(dur.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Apply tail truncation to text, returning the truncated text and truncation info
fn apply_tail_truncation(text: &str, tail_lines: usize) -> (String, bool, Option<TruncationInfo>) {
    let lines: Vec<&str> = text.lines().collect();
    let original_lines = lines.len();
    let original_bytes = text.len();

    if lines.len() <= tail_lines {
        // No truncation needed
        return (text.to_string(), false, None);
    }

    // Take the last N lines
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

/// Map wa_core errors to stable robot error codes
fn map_wezterm_error_to_robot(error: &wa_core::Error) -> (&'static str, Option<String>) {
    use wa_core::error::WeztermError;

    match error {
        wa_core::Error::Wezterm(wezterm_err) => match wezterm_err {
            WeztermError::CliNotFound => (
                "robot.wezterm_not_found",
                Some("Install WezTerm or ensure 'wezterm' is in PATH.".to_string()),
            ),
            WeztermError::NotRunning => (
                "robot.wezterm_not_running",
                Some("Start WezTerm before running wa commands.".to_string()),
            ),
            WeztermError::PaneNotFound(pane_id) => (
                "robot.pane_not_found",
                Some(format!(
                    "Pane {pane_id} does not exist. Use 'wa robot state' to list available panes."
                )),
            ),
            WeztermError::SocketNotFound(_) => (
                "robot.wezterm_socket_not_found",
                Some("WezTerm socket not found. Is WezTerm running?".to_string()),
            ),
            WeztermError::CommandFailed(_) => (
                "robot.wezterm_command_failed",
                Some("The WezTerm CLI command failed. Check WezTerm logs for details.".to_string()),
            ),
            WeztermError::ParseError(_) => (
                "robot.wezterm_parse_error",
                Some(
                    "Failed to parse WezTerm output. This may indicate a version mismatch."
                        .to_string(),
                ),
            ),
            WeztermError::Timeout(_) => (
                "robot.wezterm_timeout",
                Some("WezTerm command timed out. The terminal may be unresponsive.".to_string()),
            ),
        },
        _ => (
            "robot.internal_error",
            Some("An unexpected internal error occurred.".to_string()),
        ),
    }
}

fn init_logging_from_config(
    config: &wa_core::config::Config,
    workspace_root: Option<&Path>,
) -> anyhow::Result<()> {
    let log_file = config
        .general
        .log_file
        .as_ref()
        .map(|path| resolve_log_path(path, workspace_root));

    let log_config = LogConfig {
        level: config.general.log_level.clone(),
        format: config.general.log_format,
        file: log_file,
    };

    match init_logging(&log_config) {
        Ok(()) | Err(LogError::AlreadyInitialized) => {}
        Err(err) => return Err(err.into()),
    }

    if let Some(root) = workspace_root {
        tracing::info!(workspace = %root.display(), "Workspace resolved");
    }

    Ok(())
}

fn resolve_log_path(path: &str, workspace_root: Option<&Path>) -> std::path::PathBuf {
    let candidate = std::path::PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else if let Some(root) = workspace_root {
        root.join(candidate)
    } else {
        candidate
    }
}

fn emit_permission_warnings(warnings: &[wa_core::config::PermissionWarning]) {
    for warning in warnings {
        tracing::warn!(
            label = warning.label,
            path = %warning.path.display(),
            actual_mode = format!("{actual_mode:o}", actual_mode = warning.actual_mode),
            expected_mode = format!("{expected_mode:o}", expected_mode = warning.expected_mode),
            "Permissions too open"
        );
    }
}

/// Simple glob pattern matching for CLI filters.
///
/// Supports `*` for any sequence and `?` for single character.
fn glob_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        // Exact match
        return value == pattern;
    }

    // Convert glob to regex-style matching
    let mut regex_pattern = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex_pattern.push_str(".*"),
            '?' => regex_pattern.push('.'),
            '.' | '+' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                regex_pattern.push('\\');
                regex_pattern.push(ch);
            }
            _ => regex_pattern.push(ch),
        }
    }
    regex_pattern.push('$');

    fancy_regex::Regex::new(&regex_pattern)
        .map(|re| re.is_match(value).unwrap_or(false))
        .unwrap_or(false)
}

fn is_structured_uservar_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("wa-") || lower.starts_with("wa_") || lower == "wa_event"
}

fn validate_uservar_request(pane_id: u64, name: &str, value: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("user-var name cannot be empty".to_string());
    }
    if value.is_empty() {
        return Err("user-var value cannot be empty".to_string());
    }

    let request = wa_core::ipc::IpcRequest::UserVar {
        pane_id,
        name: name.to_string(),
        value: value.to_string(),
    };
    let request_json = serde_json::to_string(&request)
        .map_err(|e| format!("failed to serialize IPC request for validation: {e}"))?;
    let request_size = request_json.len();
    if request_size > wa_core::ipc::MAX_MESSAGE_SIZE {
        return Err(format!(
            "user-var payload too large: {request_size} bytes (max {})",
            wa_core::ipc::MAX_MESSAGE_SIZE
        ));
    }

    if is_structured_uservar_name(name) {
        wa_core::events::UserVarPayload::decode(value, false)
            .map_err(|e| format!("invalid structured user-var payload for '{name}': {e}"))?;
    }

    Ok(())
}

/// Run the observation watcher daemon.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
async fn run_watcher(
    layout: &wa_core::config::WorkspaceLayout,
    config: &wa_core::config::Config,
    config_path: Option<&Path>,
    auto_handle: bool,
    _foreground: bool,
    poll_interval: u64,
    no_patterns: bool,
    disable_lock: bool,
) -> anyhow::Result<()> {
    use std::time::Duration;
    use wa_core::config::{Config, ConfigOverrides, HotReloadableConfig};
    use wa_core::lock::WatcherLock;
    use wa_core::patterns::PatternEngine;
    use wa_core::runtime::{ObservationRuntime, RuntimeConfig};
    use wa_core::storage::StorageHandle;

    // Print resolved paths for diagnostic visibility
    tracing::info!(
        workspace = %layout.root.display(),
        db_path = %layout.db_path.display(),
        lock_path = %layout.lock_path.display(),
        ipc_socket = %layout.ipc_socket_path.display(),
        log_file = %layout.log_path.display(),
        "Watcher starting with resolved paths"
    );

    if auto_handle {
        tracing::info!("Automatic workflow handling: enabled");
    }

    // Acquire single-instance lock (unless disabled)
    let _lock_guard = if disable_lock {
        tracing::warn!(
            "Single-instance lock DISABLED - data corruption may occur if multiple watchers run"
        );
        None
    } else {
        match WatcherLock::acquire(&layout.lock_path) {
            Ok(lock) => {
                tracing::info!(
                    lock_path = %layout.lock_path.display(),
                    "Acquired single-instance lock"
                );
                Some(lock)
            }
            Err(wa_core::lock::LockError::AlreadyRunning { pid, started_at }) => {
                anyhow::bail!(
                    "Another watcher is already running (pid: {pid}, started: {started_at}). \
                     Use --dangerous-disable-lock to override (NOT RECOMMENDED)."
                );
            }
            Err(wa_core::lock::LockError::AlreadyRunningNoMeta) => {
                anyhow::bail!(
                    "Another watcher is already running (metadata unavailable). \
                     Use --dangerous-disable-lock to override (NOT RECOMMENDED)."
                );
            }
            Err(e) => {
                anyhow::bail!("Failed to acquire watcher lock: {e}");
            }
        }
    };

    // Create storage handle
    let db_path = layout.db_path.to_string_lossy();
    let storage_config = wa_core::storage::StorageConfig {
        write_queue_size: config.storage.writer_queue_size as usize,
    };
    let storage = StorageHandle::with_config(&db_path, storage_config).await?;
    tracing::info!(db_path = %db_path, "Storage initialized");

    // Create pattern engine
    let pattern_engine = if no_patterns {
        tracing::info!("Pattern detection: disabled");
        PatternEngine::default()
    } else {
        tracing::info!("Pattern detection: enabled with builtin packs");
        PatternEngine::new()
    };

    // Configure the runtime
    let runtime_config = RuntimeConfig {
        discovery_interval: Duration::from_millis(poll_interval),
        capture_interval: Duration::from_millis(config.ingest.poll_interval_ms),
        min_capture_interval: Duration::from_millis(50),
        overlap_size: 4096, // Default overlap window size
        pane_filter: config.ingest.panes.clone(),
        channel_buffer: 1024,
        max_concurrent_captures: 10,
    };

    // Create and start the observation runtime
    let mut runtime = ObservationRuntime::new(runtime_config, storage, pattern_engine);
    let handle = runtime.start().await?;
    tracing::info!("Observation runtime started");

    // Track current config for hot reload
    let mut current_config = config.clone();

    // Wait for signals (SIGINT/SIGTERM to shutdown, SIGHUP to reload config)
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sighup = signal(SignalKind::hangup())?;

        loop {
            tokio::select! {
                _ = sigint.recv() => {
                    tracing::info!("Received SIGINT, initiating graceful shutdown");
                    break;
                }
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM, initiating graceful shutdown");
                    break;
                }
                _ = sighup.recv() => {
                    tracing::info!("Received SIGHUP, attempting config reload");

                    // Reload config from disk
                    match Config::load_with_overrides(config_path, false, &ConfigOverrides::default()) {
                        Ok(new_config) => {
                            // Check what changed
                            let diff = current_config.diff_for_hot_reload(&new_config);

                            if !diff.allowed {
                                tracing::warn!(
                                    "Config reload blocked: forbidden changes detected"
                                );
                                for fc in &diff.forbidden {
                                    tracing::warn!(
                                        setting = %fc.name,
                                        reason = %fc.reason,
                                        "Forbidden config change"
                                    );
                                }
                                tracing::warn!(
                                    "Restart the watcher to apply these changes"
                                );
                            } else if diff.changes.is_empty() {
                                tracing::info!("No configuration changes detected");
                            } else {
                                // Log and apply changes
                                for change in &diff.changes {
                                    tracing::info!(
                                        setting = %change.name,
                                        old = %change.old_value,
                                        new = %change.new_value,
                                        "Applying config change"
                                    );
                                }

                                // Create hot-reloadable config and apply to runtime
                                let hot_config = HotReloadableConfig::from_config(&new_config);
                                if let Err(e) = handle.apply_config_update(hot_config) {
                                    tracing::error!(error = %e, "Failed to apply config update");
                                } else {
                                    current_config = new_config;
                                    tracing::info!("Config reload complete");
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to reload config");
                        }
                    }
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        tracing::info!("Received Ctrl+C, initiating graceful shutdown");
    }

    // Graceful shutdown
    tracing::info!("Shutting down observation runtime...");
    handle.shutdown().await;
    tracing::info!("Watcher shutdown complete");

    Ok(())
}
#[tokio::main]
async fn main() {
    let robot_mode = std::env::args().nth(1).is_some_and(|arg| arg == "robot");
    if let Err(err) = run(robot_mode).await {
        handle_fatal_error(&err, robot_mode);
        std::process::exit(1);
    }
}

async fn run(robot_mode: bool) -> anyhow::Result<()> {
    let start = std::time::Instant::now();

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if robot_mode {
                let elapsed = elapsed_ms(start);
                match err.kind() {
                    clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayVersion => {
                        let response = RobotResponse::success(build_robot_help(), elapsed);
                        println!("{}", serde_json::to_string_pretty(&response)?);
                    }
                    clap::error::ErrorKind::InvalidSubcommand => {
                        let response = RobotResponse::<()>::error_with_code(
                            ROBOT_ERR_UNKNOWN_SUBCOMMAND,
                            "Unknown robot subcommand",
                            Some("Use `wa robot help` for available commands.".to_string()),
                            elapsed,
                        );
                        println!("{}", serde_json::to_string_pretty(&response)?);
                    }
                    _ => {
                        let response = RobotResponse::<()>::error_with_code(
                            ROBOT_ERR_INVALID_ARGS,
                            "Invalid robot arguments",
                            Some("Use `wa robot help` for usage.".to_string()),
                            elapsed,
                        );
                        println!("{}", serde_json::to_string_pretty(&response)?);
                    }
                }
                return Ok(());
            }
            err.exit();
        }
    };

    let Cli {
        verbose,
        config: cli_config_arg,
        workspace,
        command,
    } = cli;

    let mut overrides = wa_core::config::ConfigOverrides::default();
    if verbose {
        overrides.log_level = Some("debug".to_string());
    }

    let config_path = cli_config_arg.as_deref().map(Path::new);
    let config = match wa_core::config::Config::load_with_overrides(
        config_path,
        config_path.is_some(),
        &overrides,
    ) {
        Ok(config) => config,
        Err(err) => {
            if robot_mode {
                let response = RobotResponse::<()>::error_with_code(
                    ROBOT_ERR_CONFIG,
                    format!("Failed to load config: {err}"),
                    Some("Check --config/--workspace or WA_WORKSPACE.".to_string()),
                    elapsed_ms(start),
                );
                println!("{}", serde_json::to_string_pretty(&response)?);
                return Ok(());
            }
            return Err(err.into());
        }
    };

    let workspace_path = workspace.as_deref().map(Path::new);
    let workspace_root = config.resolve_workspace_root(workspace_path)?;
    let layout = config.workspace_layout(Some(&workspace_root))?;
    let resolved_config_path = wa_core::config::resolve_config_path(config_path);
    let log_file_path = config
        .general
        .log_file
        .as_ref()
        .map(|path| resolve_log_path(path, Some(&workspace_root)));

    init_logging_from_config(&config, Some(&workspace_root))?;
    layout.ensure_directories()?;
    let permission_warnings = wa_core::config::collect_permission_warnings(
        &layout,
        resolved_config_path.as_deref(),
        log_file_path.as_deref(),
    );
    emit_permission_warnings(&permission_warnings);

    match command {
        Some(Commands::Watch {
            auto_handle,
            foreground,
            poll_interval,
            no_patterns,
            dangerous_disable_lock,
        }) => {
            run_watcher(
                &layout,
                &config,
                resolved_config_path.as_deref(),
                auto_handle,
                foreground,
                poll_interval,
                no_patterns,
                dangerous_disable_lock,
            )
            .await?;
        }

        Some(Commands::Robot { command }) => {
            let start = std::time::Instant::now();
            let command = command.unwrap_or(RobotCommands::QuickStart);

            match command {
                RobotCommands::Help => {
                    let response = RobotResponse::success(build_robot_help(), elapsed_ms(start));
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                RobotCommands::QuickStart => {
                    let response =
                        RobotResponse::success(build_robot_quick_start(), elapsed_ms(start));
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                other => {
                    let ctx = match build_robot_context(&config, &workspace_root) {
                        Ok(ctx) => ctx,
                        Err(err) => {
                            let response = RobotResponse::<()>::error_with_code(
                                ROBOT_ERR_CONFIG,
                                format!("Failed to load config: {err}"),
                                Some("Check --config/--workspace or WA_WORKSPACE.".to_string()),
                                elapsed_ms(start),
                            );
                            println!("{}", serde_json::to_string_pretty(&response)?);
                            return Ok(());
                        }
                    };

                    match other {
                        RobotCommands::State => {
                            let wezterm = wa_core::wezterm::WeztermClient::new();
                            match wezterm.list_panes().await {
                                Ok(panes) => {
                                    let filter = &config.ingest.panes;
                                    let states: Vec<PaneState> = panes
                                        .iter()
                                        .map(|p| PaneState::from_pane_info(p, filter))
                                        .collect();
                                    let response =
                                        RobotResponse::success(states, elapsed_ms(start));
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                Err(e) => {
                                    let response = RobotResponse::<Vec<PaneState>>::error_with_code(
                                        "robot.wezterm_error",
                                        format!("Failed to list panes: {e}"),
                                        Some("Is WezTerm running?".to_string()),
                                        elapsed_ms(start),
                                    );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                            }
                        }
                        RobotCommands::GetText {
                            pane_id,
                            tail,
                            escapes,
                        } => {
                            let wezterm = wa_core::wezterm::WeztermClient::new();
                            match wezterm.get_text(pane_id, escapes).await {
                                Ok(full_text) => {
                                    // Apply tail truncation
                                    let (text, truncated, truncation_info) =
                                        apply_tail_truncation(&full_text, tail);

                                    let data = RobotGetTextData {
                                        pane_id,
                                        text,
                                        tail_lines: tail,
                                        escapes_included: escapes,
                                        truncated,
                                        truncation_info,
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                Err(e) => {
                                    // Map errors to stable codes
                                    let (code, hint) = map_wezterm_error_to_robot(&e);
                                    let response =
                                        RobotResponse::<RobotGetTextData>::error_with_code(
                                            code,
                                            format!("{e}"),
                                            hint,
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                            }
                        }
                        RobotCommands::Send {
                            pane_id,
                            text,
                            dry_run,
                            wait_for,
                            timeout_secs,
                            wait_for_regex,
                        } => {
                            use std::fmt::Write as _;

                            let redacted_text = redact_for_output(&text);
                            let mut command = if dry_run {
                                format!("wa robot send {pane_id} \"{redacted_text}\" --dry-run")
                            } else {
                                format!("wa robot send {pane_id} \"{redacted_text}\"")
                            };
                            if let Some(pattern) = &wait_for {
                                let redacted_pattern = redact_for_output(pattern);
                                let _ = write!(command, " --wait-for \"{redacted_pattern}\"");
                                if wait_for_regex {
                                    command.push_str(" --wait-for-regex");
                                }
                                let _ = write!(command, " --timeout-secs {timeout_secs}");
                            }
                            let command_ctx =
                                wa_core::dry_run::CommandContext::new(command, dry_run);

                            if command_ctx.is_dry_run() {
                                let wezterm = wa_core::wezterm::WeztermClient::new();
                                let pane_info = wezterm.get_pane(pane_id).await.ok();
                                let report = build_send_dry_run_report(
                                    &command_ctx,
                                    pane_id,
                                    pane_info.as_ref(),
                                    &text,
                                    false,
                                    wait_for.as_deref(),
                                    timeout_secs,
                                    &config,
                                );
                                let response =
                                    RobotResponse::success(report.redacted(), elapsed_ms(start));
                                println!("{}", serde_json::to_string_pretty(&response)?);
                            } else {
                                use wa_core::approval::ApprovalStore;
                                use wa_core::policy::{
                                    ActionKind, ActorKind, InjectionResult, PaneCapabilities,
                                    PolicyDecision, PolicyEngine, PolicyInput,
                                };
                                use wa_core::wezterm::{PaneWaiter, WaitMatcher, WaitOptions};

                                let db_path = &ctx.effective.paths.db_path;
                                let storage = match wa_core::storage::StorageHandle::new(db_path)
                                    .await
                                {
                                    Ok(s) => s,
                                    Err(e) => {
                                        let response =
                                                RobotResponse::<RobotSendData>::error_with_code(
                                                    ROBOT_ERR_STORAGE,
                                                    format!("Failed to open storage: {e}"),
                                                    Some(
                                                        "Is the database initialized? Run 'wa watch' first."
                                                            .to_string(),
                                                    ),
                                                    elapsed_ms(start),
                                                );
                                        println!("{}", serde_json::to_string_pretty(&response)?);
                                        return Ok(());
                                    }
                                };

                                let wezterm = wa_core::wezterm::WeztermClient::new();
                                let pane_info = match wezterm.get_pane(pane_id).await {
                                    Ok(info) => info,
                                    Err(e) => {
                                        let (code, hint) = map_wezterm_error_to_robot(&e);
                                        let response =
                                            RobotResponse::<RobotSendData>::error_with_code(
                                                code,
                                                format!("{e}"),
                                                hint,
                                                elapsed_ms(start),
                                            );
                                        println!("{}", serde_json::to_string_pretty(&response)?);
                                        return Ok(());
                                    }
                                };
                                let domain = pane_info.inferred_domain();

                                let mut engine = PolicyEngine::new(
                                    config.safety.rate_limit_per_pane,
                                    config.safety.rate_limit_global,
                                    config.safety.require_prompt_active,
                                )
                                .with_command_gate_config(config.safety.command_gate.clone())
                                .with_policy_rules(config.safety.rules.clone());

                                // NOTE: Until ingest state is wired into CLI, assume prompt state.
                                let capabilities = PaneCapabilities::prompt();

                                let summary = engine.redact_secrets(&text);
                                let input =
                                    PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
                                        .with_pane(pane_id)
                                        .with_domain(domain.clone())
                                        .with_capabilities(capabilities)
                                        .with_text_summary(&summary)
                                        .with_command_text(&text);

                                let mut decision = engine.authorize(&input);
                                if decision.requires_approval() {
                                    let store = ApprovalStore::new(
                                        &storage,
                                        config.safety.approval.clone(),
                                        ctx.effective.paths.workspace_root.clone(),
                                    );
                                    decision = match store
                                        .attach_to_decision(decision, &input, Some(summary.clone()))
                                        .await
                                    {
                                        Ok(updated) => updated,
                                        Err(e) => {
                                            let response =
                                                RobotResponse::<RobotSendData>::error_with_code(
                                                    "robot.approval_error",
                                                    format!("Failed to issue approval token: {e}"),
                                                    None,
                                                    elapsed_ms(start),
                                                );
                                            println!(
                                                "{}",
                                                serde_json::to_string_pretty(&response)?
                                            );
                                            return Ok(());
                                        }
                                    };
                                }

                                let injection = match decision {
                                    PolicyDecision::Allow { .. } => {
                                        let send_result = wezterm.send_text(pane_id, &text).await;
                                        match send_result {
                                            Ok(()) => InjectionResult::Allowed {
                                                decision,
                                                summary: summary.clone(),
                                                pane_id,
                                                action: ActionKind::SendText,
                                            },
                                            Err(e) => InjectionResult::Error {
                                                error: e.to_string(),
                                                pane_id,
                                                action: ActionKind::SendText,
                                            },
                                        }
                                    }
                                    PolicyDecision::Deny { .. } => InjectionResult::Denied {
                                        decision,
                                        summary: summary.clone(),
                                        pane_id,
                                        action: ActionKind::SendText,
                                    },
                                    PolicyDecision::RequireApproval { .. } => {
                                        InjectionResult::RequiresApproval {
                                            decision,
                                            summary: summary.clone(),
                                            pane_id,
                                            action: ActionKind::SendText,
                                        }
                                    }
                                };

                                if let Err(e) = storage
                                    .record_audit_action_redacted(injection.to_audit_record(
                                        ActorKind::Robot,
                                        None,
                                        Some(domain.clone()),
                                    ))
                                    .await
                                {
                                    tracing::warn!(pane_id, "Failed to record audit: {e}");
                                }

                                let mut wait_for_data = None;
                                let mut verification_error = None;
                                if injection.is_allowed() {
                                    if let Some(pattern) = &wait_for {
                                        let matcher = if wait_for_regex {
                                            match fancy_regex::Regex::new(pattern) {
                                                Ok(compiled) => Some(WaitMatcher::regex(compiled)),
                                                Err(e) => {
                                                    verification_error = Some(format!(
                                                        "Invalid wait-for regex: {e}"
                                                    ));
                                                    None
                                                }
                                            }
                                        } else {
                                            Some(WaitMatcher::substring(pattern))
                                        };

                                        if let Some(matcher) = matcher {
                                            let options = WaitOptions {
                                                tail_lines: 200,
                                                escapes: false,
                                                ..WaitOptions::default()
                                            };
                                            let waiter =
                                                PaneWaiter::new(&wezterm).with_options(options);
                                            let timeout =
                                                std::time::Duration::from_secs(timeout_secs);
                                            match waiter.wait_for(pane_id, &matcher, timeout).await
                                            {
                                                Ok(wa_core::wezterm::WaitResult::Matched {
                                                    elapsed_ms,
                                                    polls,
                                                }) => {
                                                    wait_for_data = Some(RobotWaitForData {
                                                        pane_id,
                                                        pattern: pattern.clone(),
                                                        matched: true,
                                                        elapsed_ms,
                                                        polls,
                                                        is_regex: wait_for_regex,
                                                    });
                                                }
                                                Ok(wa_core::wezterm::WaitResult::TimedOut {
                                                    elapsed_ms,
                                                    polls,
                                                    ..
                                                }) => {
                                                    wait_for_data = Some(RobotWaitForData {
                                                        pane_id,
                                                        pattern: pattern.clone(),
                                                        matched: false,
                                                        elapsed_ms,
                                                        polls,
                                                        is_regex: wait_for_regex,
                                                    });
                                                    verification_error = Some(format!(
                                                        "Timeout waiting for pattern '{pattern}'"
                                                    ));
                                                }
                                                Err(e) => {
                                                    verification_error =
                                                        Some(format!("wait-for failed: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }

                                let data = RobotSendData {
                                    pane_id,
                                    injection,
                                    wait_for: wait_for_data,
                                    verification_error,
                                };
                                let response = RobotResponse::success(data, elapsed_ms(start));
                                println!("{}", serde_json::to_string_pretty(&response)?);
                            }
                        }
                        RobotCommands::WaitFor {
                            pane_id,
                            pattern,
                            timeout_secs,
                            tail,
                            regex,
                        } => {
                            use std::time::Duration;
                            use wa_core::wezterm::{
                                PaneWaiter, WaitMatcher, WaitOptions, WaitResult, WeztermClient,
                            };

                            // Build the matcher
                            let matcher = if regex {
                                match fancy_regex::Regex::new(&pattern) {
                                    Ok(compiled) => WaitMatcher::regex(compiled),
                                    Err(e) => {
                                        let response =
                                            RobotResponse::<RobotWaitForData>::error_with_code(
                                                "WA-ROBOT-INVALID-REGEX",
                                                format!("Invalid regex pattern: {e}"),
                                                Some("Check the regex syntax".to_string()),
                                                elapsed_ms(start),
                                            );
                                        println!("{}", serde_json::to_string_pretty(&response)?);
                                        return Ok(());
                                    }
                                }
                            } else {
                                WaitMatcher::substring(&pattern)
                            };

                            // Create WezTerm client
                            let wezterm = WeztermClient::new();

                            // First verify the pane exists
                            match wezterm.list_panes().await {
                                Ok(panes) => {
                                    if !panes.iter().any(|p| p.pane_id == pane_id) {
                                        let response =
                                            RobotResponse::<RobotWaitForData>::error_with_code(
                                                "WA-ROBOT-PANE-NOT-FOUND",
                                                format!("Pane {pane_id} not found"),
                                                Some(
                                                    "Use 'wa robot state' to list available panes"
                                                        .to_string(),
                                                ),
                                                elapsed_ms(start),
                                            );
                                        println!("{}", serde_json::to_string_pretty(&response)?);
                                        return Ok(());
                                    }
                                }
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotWaitForData>::error_with_code(
                                            "WA-ROBOT-WEZTERM-ERROR",
                                            format!("Failed to list panes: {e}"),
                                            None,
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            }

                            // Configure wait options
                            let options = WaitOptions {
                                tail_lines: tail,
                                escapes: false,
                                ..WaitOptions::default()
                            };

                            // Create waiter and wait
                            let waiter = PaneWaiter::new(&wezterm).with_options(options);
                            let timeout = Duration::from_secs(timeout_secs);

                            tracing::info!(
                                pane_id,
                                pattern = %pattern,
                                timeout_secs,
                                is_regex = regex,
                                "Starting wait-for"
                            );

                            match waiter.wait_for(pane_id, &matcher, timeout).await {
                                Ok(WaitResult::Matched {
                                    elapsed_ms: elapsed,
                                    polls,
                                }) => {
                                    let data = RobotWaitForData {
                                        pane_id,
                                        pattern,
                                        matched: true,
                                        elapsed_ms: elapsed,
                                        polls,
                                        is_regex: regex,
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                Ok(WaitResult::TimedOut {
                                    elapsed_ms: elapsed,
                                    polls,
                                    ..
                                }) => {
                                    let response = RobotResponse::<RobotWaitForData>::error_with_code(
                                        "WA-ROBOT-TIMEOUT",
                                        format!(
                                            "Timeout waiting for pattern '{pattern}' after {elapsed}ms ({polls} polls)"
                                        ),
                                        Some("Increase --timeout-secs or check if the pattern is correct".to_string()),
                                        elapsed_ms(start),
                                    );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotWaitForData>::error_with_code(
                                            "WA-ROBOT-GET-TEXT-FAILED",
                                            format!("Failed to get pane text: {e}"),
                                            None,
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                            }
                        }
                        RobotCommands::Search {
                            query,
                            limit,
                            pane,
                            since,
                            snippets,
                        } => {
                            // Get workspace layout for db path
                            let layout = match config.workspace_layout(Some(&workspace_root)) {
                                Ok(l) => l,
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotSearchData>::error_with_code(
                                            ROBOT_ERR_CONFIG,
                                            format!("Failed to get workspace layout: {e}"),
                                            Some("Check --workspace or WA_WORKSPACE".to_string()),
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            };

                            // Open storage handle
                            let db_path = layout.db_path.to_string_lossy();
                            let storage = match wa_core::storage::StorageHandle::new(&db_path).await
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    let response = RobotResponse::<RobotSearchData>::error_with_code(
                                        ROBOT_ERR_STORAGE,
                                        format!("Failed to open storage: {e}"),
                                        Some("Is the database initialized? Run 'wa watch' first.".to_string()),
                                        elapsed_ms(start),
                                    );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            };

                            // Build search options
                            let options = wa_core::storage::SearchOptions {
                                limit: Some(limit),
                                pane_id: pane,
                                since,
                                until: None,
                                include_snippets: Some(snippets),
                                snippet_max_tokens: Some(30), // Reasonable default for terminal output
                                highlight_prefix: Some(">>".to_string()),
                                highlight_suffix: Some("<<".to_string()),
                            };

                            // Perform search
                            match storage.search_with_results(&query, options).await {
                                Ok(results) => {
                                    let total_hits = results.len();
                                    let hits: Vec<RobotSearchHit> = results
                                        .into_iter()
                                        .map(|r| RobotSearchHit {
                                            segment_id: r.segment.id,
                                            pane_id: r.segment.pane_id,
                                            seq: r.segment.seq,
                                            captured_at: r.segment.captured_at,
                                            score: r.score,
                                            snippet: r.snippet,
                                            content: if snippets {
                                                None // Don't include full content when snippets are requested
                                            } else {
                                                Some(r.segment.content)
                                            },
                                        })
                                        .collect();

                                    let data = RobotSearchData {
                                        query,
                                        results: hits,
                                        total_hits,
                                        limit,
                                        pane_filter: pane,
                                        since_filter: since,
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                Err(e) => {
                                    // Map storage errors to robot error codes
                                    let (code, hint) = match &e {
                                        wa_core::Error::Storage(
                                            wa_core::StorageError::FtsQueryError(_),
                                        ) => (
                                            ROBOT_ERR_FTS_QUERY,
                                            Some("Check FTS5 query syntax. Supported: words, \"phrases\", prefix*, AND/OR/NOT".to_string()),
                                        ),
                                        _ => (ROBOT_ERR_STORAGE, None),
                                    };
                                    let response =
                                        RobotResponse::<RobotSearchData>::error_with_code(
                                            code,
                                            format!("{e}"),
                                            hint,
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                            }
                        }
                        RobotCommands::Events {
                            limit,
                            pane,
                            rule_id,
                            event_type,
                            unhandled,
                            since,
                        } => {
                            // Get workspace layout for DB path
                            let layout = match config.workspace_layout(Some(&workspace_root)) {
                                Ok(l) => l,
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotEventsData>::error_with_code(
                                            ROBOT_ERR_CONFIG,
                                            format!("Failed to get workspace layout: {e}"),
                                            Some("Check --workspace or WA_WORKSPACE".to_string()),
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            };

                            // Open storage handle
                            let db_path = layout.db_path.to_string_lossy();
                            let storage = match wa_core::storage::StorageHandle::new(&db_path).await
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    let response = RobotResponse::<RobotEventsData>::error_with_code(
                                        ROBOT_ERR_STORAGE,
                                        format!("Failed to open storage: {e}"),
                                        Some("Is the database initialized? Run 'wa watch' first.".to_string()),
                                        elapsed_ms(start),
                                    );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            };

                            // Build event query
                            let query = wa_core::storage::EventQuery {
                                limit: Some(limit),
                                pane_id: pane,
                                rule_id: rule_id.clone(),
                                event_type: event_type.clone(),
                                unhandled_only: unhandled,
                                since,
                                until: None,
                            };

                            // Query events
                            match storage.get_events(query).await {
                                Ok(events) => {
                                    let total_count = events.len();
                                    let items: Vec<RobotEventItem> = events
                                        .into_iter()
                                        .map(|e| {
                                            // Derive pack_id from rule_id (e.g., "codex.usage.reached" -> "builtin:codex")
                                            let pack_id = e.rule_id.split('.').next().map_or_else(
                                                || "builtin:unknown".to_string(),
                                                |agent| format!("builtin:{agent}"),
                                            );
                                            RobotEventItem {
                                                id: e.id,
                                                pane_id: e.pane_id,
                                                rule_id: e.rule_id,
                                                pack_id,
                                                event_type: e.event_type,
                                                severity: e.severity,
                                                confidence: e.confidence,
                                                extracted: e.extracted,
                                                captured_at: e.detected_at,
                                                handled_at: e.handled_at,
                                                workflow_id: e.handled_by_workflow_id,
                                            }
                                        })
                                        .collect();

                                    let data = RobotEventsData {
                                        events: items,
                                        total_count,
                                        limit,
                                        pane_filter: pane,
                                        rule_id_filter: rule_id,
                                        event_type_filter: event_type,
                                        unhandled_only: unhandled,
                                        since_filter: since,
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotEventsData>::error_with_code(
                                            ROBOT_ERR_STORAGE,
                                            format!("Failed to query events: {e}"),
                                            None,
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                            }
                        }
                        RobotCommands::Workflow { command } => {
                            match command {
                                RobotWorkflowCommands::Run {
                                    name,
                                    pane_id,
                                    force,
                                    dry_run,
                                } => {
                                    use std::sync::Arc;
                                    use wa_core::policy::{PolicyEngine, PolicyGatedInjector};
                                    use wa_core::storage::StorageHandle;
                                    use wa_core::workflows::{
                                        PaneWorkflowLockManager, WorkflowEngine,
                                        WorkflowExecutionResult, WorkflowRunner,
                                        WorkflowRunnerConfig,
                                    };

                                    // Handle dry-run mode
                                    if dry_run {
                                        let command_ctx = wa_core::dry_run::CommandContext::new(
                                            "workflow run",
                                            true,
                                        );
                                        let report = build_workflow_dry_run_report(
                                            &command_ctx,
                                            &name,
                                            pane_id,
                                        );
                                        let response =
                                            RobotResponse::success(report, elapsed_ms(start));
                                        println!("{}", serde_json::to_string_pretty(&response)?);
                                        return Ok(());
                                    }

                                    // Verify pane exists
                                    let wezterm = wa_core::wezterm::WeztermClient::new();
                                    match wezterm.list_panes().await {
                                        Ok(panes) => {
                                            if !panes.iter().any(|p| p.pane_id == pane_id) {
                                                let response =
                                                    RobotResponse::<RobotWorkflowData>::error_with_code(
                                                        "robot.pane_not_found",
                                                        format!("Pane {pane_id} does not exist"),
                                                        Some(
                                                            "Use 'wa robot state' to list available panes."
                                                                .to_string(),
                                                        ),
                                                        elapsed_ms(start),
                                                    );
                                                println!(
                                                    "{}",
                                                    serde_json::to_string_pretty(&response)?
                                                );
                                                return Ok(());
                                            }
                                        }
                                        Err(e) => {
                                            let (code, hint) = map_wezterm_error_to_robot(&e);
                                            let response =
                                                RobotResponse::<RobotWorkflowData>::error_with_code(
                                                    code,
                                                    format!("{e}"),
                                                    hint,
                                                    elapsed_ms(start),
                                                );
                                            println!(
                                                "{}",
                                                serde_json::to_string_pretty(&response)?
                                            );
                                            return Ok(());
                                        }
                                    }

                                    // Set up workflow infrastructure
                                    let db_path = &ctx.effective.paths.db_path;
                                    let storage = match StorageHandle::new(db_path).await {
                                        Ok(s) => Arc::new(s),
                                        Err(e) => {
                                            let response =
                                                RobotResponse::<RobotWorkflowData>::error_with_code(
                                                    "robot.storage_error",
                                                    format!("Failed to open storage: {e}"),
                                                    Some(
                                                        "Check database path and permissions."
                                                            .to_string(),
                                                    ),
                                                    elapsed_ms(start),
                                                );
                                            println!(
                                                "{}",
                                                serde_json::to_string_pretty(&response)?
                                            );
                                            return Ok(());
                                        }
                                    };

                                    let engine = WorkflowEngine::new(10);
                                    let lock_manager = Arc::new(PaneWorkflowLockManager::new());

                                    // Create policy engine from safety config
                                    let policy_engine = PolicyEngine::new(
                                        config.safety.rate_limit_per_pane,
                                        config.safety.rate_limit_global,
                                        false, // Don't require prompt active for robot mode
                                    );

                                    // Create policy-gated injector with WezTerm client
                                    let wezterm_client = wa_core::wezterm::WeztermClient::new();
                                    let injector = Arc::new(tokio::sync::Mutex::new(
                                        PolicyGatedInjector::new(policy_engine, wezterm_client),
                                    ));
                                    let runner_config = WorkflowRunnerConfig::default();
                                    let runner = WorkflowRunner::new(
                                        engine,
                                        lock_manager,
                                        Arc::clone(&storage),
                                        injector,
                                        runner_config,
                                    );

                                    // Look up workflow by name
                                    let workflow = runner.find_workflow_by_name(&name);
                                    let _ = force; // Will be used when implementing "already handled" bypass

                                    if let Some(wf) = workflow {
                                        // Generate execution ID
                                        let now_ms = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis();
                                        let execution_id = format!("robot-{name}-{now_ms}");

                                        // Run the workflow
                                        let result = runner
                                            .run_workflow(pane_id, wf, &execution_id, 0)
                                            .await;

                                        let (
                                            status,
                                            message,
                                            result_value,
                                            steps_executed,
                                            step_index,
                                        ) = match result {
                                            WorkflowExecutionResult::Completed {
                                                result,
                                                steps_executed,
                                                ..
                                            } => (
                                                "completed",
                                                None,
                                                Some(result),
                                                Some(steps_executed),
                                                None,
                                            ),
                                            WorkflowExecutionResult::Aborted {
                                                reason,
                                                step_index,
                                                ..
                                            } => (
                                                "aborted",
                                                Some(reason),
                                                None,
                                                None,
                                                Some(step_index),
                                            ),
                                            WorkflowExecutionResult::PolicyDenied {
                                                reason,
                                                step_index,
                                                ..
                                            } => (
                                                "policy_denied",
                                                Some(reason),
                                                None,
                                                None,
                                                Some(step_index),
                                            ),
                                            WorkflowExecutionResult::Error { error, .. } => (
                                                "error",
                                                Some(error),
                                                None,
                                                None,
                                                None, // Error variant doesn't track step_index
                                            ),
                                        };

                                        let workflow_elapsed = elapsed_ms(start);
                                        let data = RobotWorkflowData {
                                            workflow_name: name.clone(),
                                            pane_id,
                                            execution_id: Some(execution_id),
                                            status: status.to_string(),
                                            message,
                                            result: result_value,
                                            steps_executed,
                                            step_index,
                                            elapsed_ms: Some(workflow_elapsed),
                                        };

                                        let response = if status == "completed" {
                                            RobotResponse::success(data, workflow_elapsed)
                                        } else if status == "policy_denied" {
                                            RobotResponse::<RobotWorkflowData>::error_with_code(
                                                "robot.policy_denied",
                                                format!("Workflow '{name}' denied by policy"),
                                                Some(
                                                    "Check safety configuration or use --dry-run."
                                                        .to_string(),
                                                ),
                                                workflow_elapsed,
                                            )
                                        } else {
                                            let status_message =
                                                data.message.as_deref().unwrap_or("failed");
                                            RobotResponse::<RobotWorkflowData>::error_with_code(
                                                &format!("robot.workflow_{status}"),
                                                format!("Workflow '{name}' {status_message}"),
                                                None,
                                                workflow_elapsed,
                                            )
                                        };
                                        println!("{}", serde_json::to_string_pretty(&response)?);
                                    } else {
                                        // No workflow registered with this name
                                        let response =
                                            RobotResponse::<RobotWorkflowData>::error_with_code(
                                                "robot.workflow_not_found",
                                                format!("Workflow '{name}' not found"),
                                                Some(
                                                    "No workflows registered in standalone mode. \
                                                     Use 'wa watch --auto-handle' for event-driven workflows."
                                                        .to_string(),
                                                ),
                                                elapsed_ms(start),
                                            );
                                        tracing::debug!(
                                            workflow = %name,
                                            pane_id,
                                            "Workflow not found"
                                        );
                                        println!("{}", serde_json::to_string_pretty(&response)?);
                                    }

                                    // Clean shutdown of storage
                                    if let Err(e) = storage.shutdown().await {
                                        tracing::warn!("Failed to shutdown storage cleanly: {e}");
                                    }
                                }
                                RobotWorkflowCommands::List => {
                                    // List available workflows
                                    // In standalone robot mode, workflows are defined but not
                                    // registered. List the built-in workflow definitions.
                                    let workflows: Vec<RobotWorkflowInfo> = vec![
                                        RobotWorkflowInfo {
                                            name: "handle_compaction".to_string(),
                                            description: Some(
                                                "Re-inject critical context after conversation \
                                                 compaction"
                                                    .to_string(),
                                            ),
                                            enabled: true,
                                            trigger_event_types: Some(vec![
                                                "compaction_warning".to_string(),
                                            ]),
                                            requires_pane: Some(true),
                                        },
                                        RobotWorkflowInfo {
                                            name: "handle_usage_limit".to_string(),
                                            description: Some(
                                                "Handle API usage limit reached events".to_string(),
                                            ),
                                            enabled: true,
                                            trigger_event_types: Some(vec![
                                                "usage_limit".to_string(),
                                            ]),
                                            requires_pane: Some(true),
                                        },
                                    ];

                                    let total = workflows.len();
                                    let data = RobotWorkflowListData {
                                        workflows,
                                        total,
                                        enabled_count: Some(total),
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                RobotWorkflowCommands::Status { execution_id } => {
                                    // Workflow status lookup is not yet persisted
                                    // Return a not-implemented response for now
                                    let response =
                                        RobotResponse::<RobotWorkflowStatusData>::error_with_code(
                                            "robot.not_implemented",
                                            format!(
                                                "Workflow status lookup not yet implemented \
                                                 (execution_id: {execution_id})"
                                            ),
                                            Some(
                                                "Workflow execution status is currently only \
                                                 available during execution. Persistent status \
                                                 tracking is planned for a future release."
                                                    .to_string(),
                                            ),
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                RobotWorkflowCommands::Abort {
                                    execution_id,
                                    reason,
                                } => {
                                    // Workflow abort is not yet implemented
                                    // Return a not-implemented response for now
                                    let _ = reason; // Will be used when abort is implemented
                                    let response =
                                        RobotResponse::<RobotWorkflowAbortData>::error_with_code(
                                            "robot.not_implemented",
                                            format!(
                                                "Workflow abort not yet implemented \
                                                 (execution_id: {execution_id})"
                                            ),
                                            Some(
                                                "Workflow abort requires persistent execution \
                                                 tracking. This feature is planned for a future \
                                                 release."
                                                    .to_string(),
                                            ),
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                            }
                        }
                        RobotCommands::Why { code } => {
                            // Explain an error code using built-in templates
                            use wa_core::explanations::{get_explanation, list_template_ids};

                            if let Some(tmpl) = get_explanation(&code) {
                                // Extract category from code (e.g., "deny.alt_screen" -> "deny")
                                let category =
                                    code.split('.').next().unwrap_or("unknown").to_string();

                                let data = RobotWhyData {
                                    code: code.clone(),
                                    category,
                                    title: tmpl.scenario.to_string(),
                                    explanation: tmpl.detailed.to_string(),
                                    suggestions: if tmpl.suggestions.is_empty() {
                                        None
                                    } else {
                                        Some(
                                            tmpl.suggestions
                                                .iter()
                                                .map(|s| (*s).to_string())
                                                .collect(),
                                        )
                                    },
                                    see_also: if tmpl.see_also.is_empty() {
                                        None
                                    } else {
                                        Some(
                                            tmpl.see_also
                                                .iter()
                                                .map(|s| (*s).to_string())
                                                .collect(),
                                        )
                                    },
                                };
                                let response = RobotResponse::success(data, elapsed_ms(start));
                                println!("{}", serde_json::to_string_pretty(&response)?);
                            } else {
                                // Code not found - list available codes as hint
                                let available = list_template_ids();
                                let hint = if available.is_empty() {
                                    "No explanation templates available.".to_string()
                                } else {
                                    format!(
                                        "Available codes: {}",
                                        available[..available.len().min(10)].join(", ")
                                    )
                                };
                                let response = RobotResponse::<RobotWhyData>::error_with_code(
                                    "robot.code_not_found",
                                    format!("Unknown error code: {code}"),
                                    Some(hint),
                                    elapsed_ms(start),
                                );
                                println!("{}", serde_json::to_string_pretty(&response)?);
                            }
                        }
                        RobotCommands::Approve {
                            code,
                            pane,
                            fingerprint,
                            dry_run,
                        } => {
                            // Get workspace layout for db path and workspace_id
                            let layout = match config.workspace_layout(Some(&workspace_root)) {
                                Ok(l) => l,
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotApproveData>::error_with_code(
                                            ROBOT_ERR_CONFIG,
                                            format!("Failed to get workspace layout: {e}"),
                                            Some("Check --workspace or WA_WORKSPACE".to_string()),
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            };

                            // Open storage handle
                            let db_path = layout.db_path.to_string_lossy();
                            let storage = match wa_core::storage::StorageHandle::new(&db_path).await
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotApproveData>::error_with_code(
                                            ROBOT_ERR_STORAGE,
                                            format!("Failed to open storage: {e}"),
                                            Some(
                                                "Is the database initialized? Run 'wa watch' first."
                                                    .to_string(),
                                            ),
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            };

                            // Get workspace ID for scoping
                            let workspace_id = layout.root.to_string_lossy().to_string();

                            match evaluate_robot_approve(
                                &storage,
                                &workspace_id,
                                &code,
                                pane,
                                fingerprint.as_deref(),
                                dry_run,
                            )
                            .await
                            {
                                Ok(data) => {
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                Err(err) => {
                                    let response =
                                        RobotResponse::<RobotApproveData>::error_with_code(
                                            err.code,
                                            err.message,
                                            err.hint,
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                            }
                        }
                        RobotCommands::Help | RobotCommands::QuickStart => {
                            unreachable!("handled above")
                        }
                    }
                }
            }
        }

        Some(Commands::Search {
            query,
            format,
            limit,
            pane,
            since,
        }) => {
            use wa_core::output::{
                OutputFormat, RenderContext, SearchResultRenderer, detect_format,
            };

            let output_format = match format.to_lowercase().as_str() {
                "json" => OutputFormat::Json,
                "plain" => OutputFormat::Plain,
                _ => detect_format(),
            };

            let redacted_query = redact_for_output(&query);
            tracing::info!(
                "Searching for '{}' (limit={}, pane={:?})",
                redacted_query,
                limit,
                pane
            );

            // Get workspace layout for DB path
            let layout = match config.workspace_layout(Some(&workspace_root)) {
                Ok(l) => l,
                Err(e) => {
                    if output_format.is_json() {
                        println!(
                            r#"{{"ok": false, "error": "Failed to get workspace layout: {}", "version": "{}"}}"#,
                            e,
                            wa_core::VERSION
                        );
                    } else {
                        eprintln!("Error: Failed to get workspace layout: {e}");
                        eprintln!("Check --workspace or WA_WORKSPACE");
                    }
                    std::process::exit(1);
                }
            };

            // Open storage handle
            let db_path = layout.db_path.to_string_lossy();
            let storage = match wa_core::storage::StorageHandle::new(&db_path).await {
                Ok(s) => s,
                Err(e) => {
                    if output_format.is_json() {
                        println!(
                            r#"{{"ok": false, "error": "Failed to open storage: {}", "version": "{}"}}"#,
                            e,
                            wa_core::VERSION
                        );
                    } else {
                        eprintln!("Error: Failed to open storage: {e}");
                        eprintln!("Is the database initialized? Run 'wa watch' first.");
                    }
                    std::process::exit(1);
                }
            };

            // Build search options
            let options = wa_core::storage::SearchOptions {
                limit: Some(limit),
                pane_id: pane,
                since,
                until: None,
                include_snippets: Some(true),
                snippet_max_tokens: Some(30),
                highlight_prefix: Some(">>".to_string()),
                highlight_suffix: Some("<<".to_string()),
            };

            // Perform search
            match storage.search_with_results(&query, options).await {
                Ok(results) => {
                    let ctx = RenderContext::new(output_format)
                        .verbose(cli.verbose)
                        .limit(limit);
                    let output = SearchResultRenderer::render(&results, &query, &ctx);
                    print!("{output}");
                }
                Err(e) => {
                    if output_format.is_json() {
                        println!(
                            r#"{{"ok": false, "error": "Search failed: {}", "version": "{}"}}"#,
                            e,
                            wa_core::VERSION
                        );
                    } else {
                        eprintln!("Error: Search failed: {e}");
                        if e.to_string().contains("fts5") || e.to_string().contains("syntax") {
                            eprintln!("Check your FTS query syntax.");
                        }
                    }
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::List { json }) => {
            let wezterm = wa_core::wezterm::WeztermClient::new();
            let panes = match wezterm.list_panes().await {
                Ok(panes) => panes,
                Err(e) => {
                    eprintln!("Failed to list panes: {e}");
                    return Err(e.into());
                }
            };

            let filter = &config.ingest.panes;
            let states: Vec<PaneState> = panes
                .iter()
                .map(|p| PaneState::from_pane_info(p, filter))
                .collect();

            if json {
                println!("{}", serde_json::to_string_pretty(&states)?);
            } else if states.is_empty() {
                println!("No panes found");
            } else {
                let observed_count = states.iter().filter(|s| s.observed).count();
                let ignored_count = states.len() - observed_count;
                println!("Panes ({observed_count} observed, {ignored_count} ignored):");
                println!(
                    "  {:>4}  {:>10}  {:<20}  {:<40}  DOMAIN",
                    "ID", "STATUS", "TITLE", "CWD"
                );
                println!("  {}", "-".repeat(100));
                for state in &states {
                    println!("{}", state.format_human());
                }
            }
        }

        Some(Commands::Show { pane_id, output }) => {
            tracing::info!("Showing pane {} (output={})", pane_id, output);
            // TODO: Implement show
            println!("Show not yet implemented");
        }

        Some(Commands::Send {
            pane_id,
            text,
            no_paste,
            dry_run,
        }) => {
            let redacted_text = redact_for_output(&text);
            let command = if dry_run {
                format!("wa send --pane {pane_id} \"{redacted_text}\" --dry-run")
            } else {
                format!("wa send --pane {pane_id} \"{redacted_text}\"")
            };
            let command_ctx = wa_core::dry_run::CommandContext::new(command, dry_run);

            if command_ctx.is_dry_run() {
                let wezterm = wa_core::wezterm::WeztermClient::new();
                let pane_info = wezterm.get_pane(pane_id).await.ok();
                let report = build_send_dry_run_report(
                    &command_ctx,
                    pane_id,
                    pane_info.as_ref(),
                    &text,
                    no_paste,
                    None,
                    30,
                    &config,
                );
                println!("{}", wa_core::dry_run::format_human(&report));
            } else {
                tracing::info!(
                    "Sending to pane {} (no_paste={}): {}",
                    pane_id,
                    no_paste,
                    redacted_text
                );
                // TODO: Implement send
                println!("Send not yet implemented");
            }
        }

        Some(Commands::GetText { pane_id, escapes }) => {
            tracing::info!("Getting text from pane {} (escapes={})", pane_id, escapes);
            // TODO: Implement get-text
            println!("Get-text not yet implemented");
        }

        Some(Commands::Workflow { command }) => {
            match command {
                WorkflowCommands::List => {
                    println!("Available workflows:");
                    println!("  - handle_compaction");
                    println!("  - handle_usage_limits");
                }
                WorkflowCommands::Run {
                    name,
                    pane,
                    dry_run,
                } => {
                    let command = if dry_run {
                        format!("wa workflow run {name} --pane {pane} --dry-run")
                    } else {
                        format!("wa workflow run {name} --pane {pane}")
                    };
                    let command_ctx = wa_core::dry_run::CommandContext::new(command, dry_run);

                    if command_ctx.is_dry_run() {
                        let report = build_workflow_dry_run_report(&command_ctx, &name, pane);
                        println!("{}", wa_core::dry_run::format_human(&report));
                    } else {
                        tracing::info!("Running workflow '{}' on pane {}", name, pane);
                        // TODO: Implement workflow run
                        println!("Workflow run not yet implemented");
                    }
                }
                WorkflowCommands::Status { execution_id } => {
                    tracing::info!("Getting status for execution {}", execution_id);
                    // TODO: Implement workflow status
                    println!("Workflow status not yet implemented");
                }
            }
        }

        Some(Commands::Status {
            health,
            format,
            domain,
            agent,
            pane_id,
        }) => {
            if health {
                // Health check mode: simple JSON status
                println!(r#"{{"status": "ok", "version": "{}"}}"#, wa_core::VERSION);
            } else {
                // Rich status mode: pane table + summary
                use wa_core::output::{
                    OutputFormat, PaneTableRenderer, RenderContext, detect_format,
                };

                let output_format = match format.to_lowercase().as_str() {
                    "json" => OutputFormat::Json,
                    "plain" => OutputFormat::Plain,
                    _ => detect_format(),
                };

                let wezterm = wa_core::wezterm::WeztermClient::new();
                match wezterm.list_panes().await {
                    Ok(panes) => {
                        let filter = &config.ingest.panes;

                        // Convert to PaneRecord format for rendering
                        #[allow(clippy::cast_possible_truncation)]
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64;

                        let mut records: Vec<wa_core::storage::PaneRecord> = panes
                            .iter()
                            .filter_map(|p| {
                                let pane_domain = p.inferred_domain();
                                let pane_title = p.title.as_deref().unwrap_or("");
                                let pane_cwd = p.cwd.as_deref().unwrap_or("");

                                // Apply filters
                                if let Some(ref filter_pane_id) = pane_id {
                                    if p.pane_id != *filter_pane_id {
                                        return None;
                                    }
                                }

                                if let Some(ref domain_filter) = domain {
                                    if !glob_match(domain_filter, &pane_domain) {
                                        return None;
                                    }
                                }

                                // Agent filter would go here when agent inference is implemented
                                if agent.is_some() {
                                    // TODO: Filter by inferred agent type once available
                                }

                                let ignore_reason =
                                    filter.check_pane(&pane_domain, pane_title, pane_cwd);

                                Some(wa_core::storage::PaneRecord {
                                    pane_id: p.pane_id,
                                    pane_uuid: None,
                                    domain: pane_domain,
                                    window_id: Some(p.window_id),
                                    tab_id: Some(p.tab_id),
                                    title: p.title.clone(),
                                    cwd: p.cwd.clone(),
                                    tty_name: p.tty_name.clone(),
                                    first_seen_at: now,
                                    last_seen_at: now,
                                    observed: ignore_reason.is_none(),
                                    ignore_reason,
                                    last_decision_at: None,
                                })
                            })
                            .collect();

                        // Sort by pane_id for deterministic output
                        records.sort_by_key(|r| r.pane_id);

                        let ctx = RenderContext::new(output_format).verbose(cli.verbose);
                        let output = PaneTableRenderer::render(&records, &ctx);
                        print!("{output}");
                    }
                    Err(e) => {
                        if output_format.is_json() {
                            println!(
                                r#"{{"ok": false, "error": "Failed to list panes: {}", "version": "{}"}}"#,
                                e,
                                wa_core::VERSION
                            );
                        } else {
                            eprintln!("Error: Failed to list panes: {e}");
                            eprintln!("Is WezTerm running?");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Some(Commands::Events {
            format,
            limit,
            pane_id,
            rule_id,
            event_type,
            unhandled,
        }) => {
            use wa_core::output::{EventListRenderer, OutputFormat, RenderContext, detect_format};

            let output_format = match format.to_lowercase().as_str() {
                "json" => OutputFormat::Json,
                "plain" => OutputFormat::Plain,
                _ => detect_format(),
            };

            // Get workspace layout for DB path
            let layout = match config.workspace_layout(Some(&workspace_root)) {
                Ok(l) => l,
                Err(e) => {
                    if output_format.is_json() {
                        println!(
                            r#"{{"ok": false, "error": "Failed to get workspace layout: {}", "version": "{}"}}"#,
                            e,
                            wa_core::VERSION
                        );
                    } else {
                        eprintln!("Error: Failed to get workspace layout: {e}");
                        eprintln!("Check --workspace or WA_WORKSPACE");
                    }
                    std::process::exit(1);
                }
            };

            // Open storage handle
            let db_path = layout.db_path.to_string_lossy();
            let storage = match wa_core::storage::StorageHandle::new(&db_path).await {
                Ok(s) => s,
                Err(e) => {
                    if output_format.is_json() {
                        println!(
                            r#"{{"ok": false, "error": "Failed to open storage: {}", "version": "{}"}}"#,
                            e,
                            wa_core::VERSION
                        );
                    } else {
                        eprintln!("Error: Failed to open storage: {e}");
                        eprintln!("Is the database initialized? Run 'wa watch' first.");
                    }
                    std::process::exit(1);
                }
            };

            // Build event query
            let query = wa_core::storage::EventQuery {
                limit: Some(limit),
                pane_id,
                rule_id: rule_id.clone(),
                event_type: event_type.clone(),
                unhandled_only: unhandled,
                since: None,
                until: None,
            };

            // Query events
            match storage.get_events(query).await {
                Ok(events) => {
                    let ctx = RenderContext::new(output_format)
                        .verbose(cli.verbose)
                        .limit(limit);
                    let output = EventListRenderer::render(&events, &ctx);
                    print!("{output}");
                }
                Err(e) => {
                    if output_format.is_json() {
                        println!(
                            r#"{{"ok": false, "error": "Failed to query events: {}", "version": "{}"}}"#,
                            e,
                            wa_core::VERSION
                        );
                    } else {
                        eprintln!("Error: Failed to query events: {e}");
                    }
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Why {
            template_id,
            category,
            format,
            list,
        }) => {
            use wa_core::explanations::{
                format_explanation, get_explanation, list_template_ids, list_templates_by_category,
            };
            use wa_core::output::{OutputFormat, detect_format};

            let output_format = match format.to_lowercase().as_str() {
                "json" => OutputFormat::Json,
                "plain" => OutputFormat::Plain,
                _ => detect_format(),
            };

            let should_list = list || template_id.is_none();

            if should_list {
                let mut ids: Vec<String> = category.as_deref().map_or_else(
                    || {
                        list_template_ids()
                            .into_iter()
                            .map(ToString::to_string)
                            .collect()
                    },
                    |prefix| {
                        let mut items: Vec<String> = list_templates_by_category(prefix)
                            .into_iter()
                            .map(|t| t.id.to_string())
                            .collect();
                        items.sort();
                        items
                    },
                );

                if output_format.is_json() {
                    #[derive(serde::Serialize)]
                    struct WhyListResponse {
                        ok: bool,
                        templates: Vec<String>,
                        count: usize,
                        category: Option<String>,
                        version: &'static str,
                    }

                    let response = WhyListResponse {
                        ok: true,
                        count: ids.len(),
                        templates: std::mem::take(&mut ids),
                        category,
                        version: wa_core::VERSION,
                    };
                    println!("{}", serde_json::to_string_pretty(&response)?);
                } else {
                    if let Some(prefix) = category.as_deref() {
                        println!("Templates (category={prefix}):");
                    } else {
                        println!("Available explanations:");
                    }
                    if ids.is_empty() {
                        println!("  (none)");
                    } else {
                        for id in ids {
                            println!("  - {id}");
                        }
                    }
                    println!();
                    println!("Usage: wa why <template_id>");
                }
                return Ok(());
            }

            let id = template_id.unwrap_or_default();
            if let Some(template) = get_explanation(&id) {
                if output_format.is_json() {
                    #[derive(serde::Serialize)]
                    struct WhyTemplateResponse<'a> {
                        ok: bool,
                        template: &'a wa_core::explanations::ExplanationTemplate,
                        version: &'static str,
                    }

                    let response = WhyTemplateResponse {
                        ok: true,
                        template,
                        version: wa_core::VERSION,
                    };
                    println!("{}", serde_json::to_string_pretty(&response)?);
                } else {
                    let formatted = format_explanation(template, None);
                    println!("{formatted}");
                }
            } else {
                if output_format.is_json() {
                    println!(
                        r#"{{"ok": false, "error": "Unknown explanation id: {}", "hint": "Use 'wa why --list' to see available templates.", "version": "{}"}}"#,
                        id,
                        wa_core::VERSION
                    );
                } else {
                    eprintln!("Error: Unknown explanation id: {id}");
                    eprintln!("Use 'wa why --list' to see available templates.");
                }
                std::process::exit(1);
            }
        }

        Some(Commands::Event {
            from_uservar,
            pane,
            name,
            value,
        }) => {
            if !from_uservar {
                eprintln!("Error: only --from-uservar is supported for now.");
                eprintln!(
                    "Hint: use `wa event --from-uservar --pane <id> --name <name> --value <value>`"
                );
                std::process::exit(1);
            }

            let name_for_log = name.clone();
            let value_len = value.len();

            if let Err(message) = validate_uservar_request(pane, &name, &value) {
                eprintln!("Error: {message}");
                eprintln!("Context: pane_id={pane} name=\"{name_for_log}\" value_len={value_len}");
                std::process::exit(1);
            }

            tracing::debug!(
                pane_id = pane,
                name = %name_for_log,
                value_len,
                "Forwarding user-var event to watcher"
            );

            let client = wa_core::ipc::IpcClient::new(&layout.ipc_socket_path);
            match client.send_user_var(pane, name, value).await {
                Ok(response) => {
                    if !response.ok {
                        let detail = response
                            .error
                            .unwrap_or_else(|| "unknown error".to_string());
                        eprintln!("Error: watcher rejected user-var event: {detail}");
                        eprintln!(
                            "Context: pane_id={pane} name=\"{name_for_log}\" value_len={value_len}"
                        );
                        std::process::exit(1);
                    }
                }
                Err(err) => {
                    match err {
                        wa_core::events::UserVarError::WatcherNotRunning { .. } => {
                            eprintln!("Error: {err}");
                            eprintln!("Hint: start the watcher with `wa watch` in this workspace.");
                        }
                        _ => {
                            eprintln!("Error: failed to forward user-var event: {err}");
                        }
                    }
                    eprintln!(
                        "Context: pane_id={pane} name=\"{name_for_log}\" value_len={value_len}"
                    );
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Doctor) => {
            println!("wa doctor - Running diagnostics...\n");

            let checks = run_diagnostics(&permission_warnings);

            for check in &checks {
                check.print();
            }

            let has_errors = checks.iter().any(|c| c.status == DiagnosticStatus::Error);
            let has_warnings = checks.iter().any(|c| c.status == DiagnosticStatus::Warning);

            println!();
            if has_errors {
                println!("Diagnostics completed with errors. Fix issues above before using wa.");
                std::process::exit(1);
            } else if has_warnings {
                println!("Diagnostics completed with warnings. wa should work but performance may be affected.");
            } else {
                println!("All checks passed! wa is ready to use.");
            }
        }

        Some(Commands::Setup { command }) => match command {
            SetupCommands::Local => {
                println!("wa setup local - WezTerm Configuration Guide\n");
                println!("wa requires WezTerm to be configured with adequate scrollback.");
                println!("Without sufficient scrollback, wa may miss terminal output.\n");

                // Check current state
                match check_wezterm_scrollback() {
                    Ok((lines, path)) => {
                        if lines >= RECOMMENDED_SCROLLBACK_LINES {
                            println!("✓ Your WezTerm scrollback is already configured!");
                            println!("  Current: {} lines in {}", lines, path.display());
                            println!("  Recommended minimum: {} lines", RECOMMENDED_SCROLLBACK_LINES);
                            println!("\nNo changes needed.");
                        } else {
                            println!("⚠ Your WezTerm scrollback is below recommended minimum.");
                            println!("  Current: {} lines in {}", lines, path.display());
                            println!("  Recommended: {} lines\n", RECOMMENDED_SCROLLBACK_LINES);
                            println!("Add this line to your wezterm.lua:");
                            println!("  config.scrollback_lines = {}", RECOMMENDED_SCROLLBACK_LINES);
                        }
                    }
                    Err(_) => {
                        println!("Could not find or parse WezTerm config.\n");
                        println!("Add the following to your ~/.config/wezterm/wezterm.lua:\n");
                        println!("  config.scrollback_lines = {}\n", RECOMMENDED_SCROLLBACK_LINES);
                        println!("This ensures wa can capture all terminal output without gaps.");
                    }
                }

                println!("\n--- Why {} lines? ---", RECOMMENDED_SCROLLBACK_LINES);
                println!("• AI coding agents can produce substantial output");
                println!("• wa uses delta extraction to capture only new content");
                println!("• Insufficient scrollback causes capture gaps (EVENT_GAP_DETECTED)");
                println!("• 50k lines ≈ 2-5 MB memory per pane (negligible on modern systems)");
                println!("\nRun 'wa doctor' to verify your configuration.");
            }
            SetupCommands::Remote { host } => {
                println!("wa setup remote - Remote Host Setup for '{host}'\n");
                println!("Remote setup is not yet implemented.\n");
                println!("For now, ensure the remote host has:");
                println!("  1. WezTerm SSH domain configured in your wezterm.lua");
                println!("  2. Adequate scrollback_lines setting");
                println!("\nExample SSH domain configuration:");
                println!("  config.ssh_domains = {{");
                println!("    {{");
                println!("      name = '{}',", host);
                println!("      remote_address = '{}.example.com',", host);
                println!("      username = 'your_user',");
                println!("    }},");
                println!("  }}");
            }
            SetupCommands::Config => {
                println!("wa setup config - Generate WezTerm Config Additions\n");
                println!("Add the following to your ~/.config/wezterm/wezterm.lua:\n");
                println!("-- wa (WezTerm Automata) recommended settings");
                println!("-- Ensure adequate scrollback for terminal capture");
                println!("config.scrollback_lines = {}", RECOMMENDED_SCROLLBACK_LINES);
                println!();
                println!("-- Optional: Enable hyperlinks for file navigation");
                println!("config.hyperlink_rules = wezterm.default_hyperlink_rules()");
                println!();
                println!("-- Optional: Quick-access key for wa status");
                println!("-- config.keys = {{");
                println!("--   {{ key = 'w', mods = 'CTRL|SHIFT', action = wezterm.action.SpawnCommandInNewTab {{");
                println!("--     args = {{ 'wa', 'status' }},");
                println!("--   }} }},");
                println!("-- }}");
            }
        },

        Some(Commands::Config { command }) => {
            handle_config_command(command, cli_config_arg.as_deref(), workspace.as_deref()).await?;
        }

        #[cfg(feature = "tui")]
        Some(Commands::Tui { debug, refresh }) => {
            use wa_core::tui::{run_tui, AppConfig, ProductionQueryClient};
            use std::time::Duration;

            let query_client = ProductionQueryClient::new(layout.clone());
            let tui_config = AppConfig {
                refresh_interval: Duration::from_secs(refresh),
                debug,
            };

            if let Err(e) = run_tui(query_client, tui_config) {
                eprintln!("TUI error: {e}");
                return Err(e.into());
            }
        }

        None => {
            println!("wa - WezTerm Automata");
            println!();
            println!("Terminal hypervisor for AI agent swarms.");
            println!();
            println!("Use --help to see available commands.");
        }
    }

    Ok(())
}

fn handle_fatal_error(err: &anyhow::Error, robot_mode: bool) {
    if robot_mode {
        eprintln!("Error: {err}");
        return;
    }

    if let Some(core_err) = err.downcast_ref::<wa_core::Error>() {
        eprintln!(
            "{}",
            wa_core::error::format_error_with_remediation(core_err)
        );
    } else {
        eprintln!("Error: {err}");
    }
}

/// Handle `wa config` subcommands
async fn handle_config_command(
    command: ConfigCommands,
    cli_config: Option<&str>,
    cli_workspace: Option<&str>,
) -> anyhow::Result<()> {
    use wa_core::config::{Config, ConfigOverrides};

    match command {
        ConfigCommands::Init { force, path } => {
            // Determine config path
            let config_path = if let Some(p) = path {
                std::path::PathBuf::from(p)
            } else if let Some(p) = cli_config {
                std::path::PathBuf::from(p)
            } else {
                // Default: ./wa.toml, fallback to ~/.config/wa/wa.toml
                let cwd_config = std::path::PathBuf::from("wa.toml");
                if cwd_config.exists() && !force {
                    anyhow::bail!(
                        "Config file already exists at {}. Use --force to overwrite.",
                        cwd_config.display()
                    );
                }
                if cwd_config.exists() || std::env::current_dir().is_ok() {
                    cwd_config
                } else {
                    dirs::config_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
                        .join("wa")
                        .join("wa.toml")
                }
            };

            // Check if exists
            if config_path.exists() && !force {
                anyhow::bail!(
                    "Config file already exists at {}. Use --force to overwrite.",
                    config_path.display()
                );
            }

            // Create parent directories
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Write default config
            let _default_config = Config::default();
            let toml_content = generate_default_config_toml();
            std::fs::write(&config_path, toml_content)?;

            println!("Created config at: {}", config_path.display());
            println!();
            println!("Edit this file to customize wa behavior.");
            println!("Run `wa config validate` to check for errors.");
        }

        ConfigCommands::Validate { path, strict } => {
            // Find config
            let config_path = if let Some(p) = path {
                Some(std::path::PathBuf::from(p))
            } else if let Some(p) = cli_config {
                Some(std::path::PathBuf::from(p))
            } else {
                None
            };

            let config = Config::load_with_overrides(
                config_path.as_deref(),
                strict,
                &ConfigOverrides::default(),
            )?;

            // Run validation
            config.validate()?;

            let path_display = config_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(default)".to_string());

            println!("✓ Config is valid: {path_display}");

            // Show any warnings (non-fatal)
            let warnings = validate_config_warnings(&config);
            if !warnings.is_empty() {
                println!();
                println!("Warnings:");
                for warning in &warnings {
                    println!("  ⚠ {warning}");
                }
                if strict {
                    anyhow::bail!("{} warning(s) found in strict mode", warnings.len());
                }
            }
        }

        ConfigCommands::Show {
            effective,
            json,
            path,
        } => {
            // Find config
            let config_path = if let Some(p) = path {
                Some(std::path::PathBuf::from(p))
            } else if let Some(p) = cli_config {
                Some(std::path::PathBuf::from(p))
            } else {
                None
            };

            let config = Config::load_with_overrides(
                config_path.as_deref(),
                false,
                &ConfigOverrides::default(),
            )?;

            if effective {
                // Show effective config with resolved paths
                let workspace_root = cli_workspace.map(std::path::PathBuf::from);
                let effective_config = config.effective_config(workspace_root.as_deref())?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&effective_config)?);
                } else {
                    println!("Effective Configuration");
                    println!("=======================");
                    println!();
                    println!("Paths:");
                    println!(
                        "  workspace_root: {}",
                        effective_config.paths.workspace_root
                    );
                    println!("  wa_dir:         {}", effective_config.paths.wa_dir);
                    println!("  db_path:        {}", effective_config.paths.db_path);
                    println!("  lock_path:      {}", effective_config.paths.lock_path);
                    println!(
                        "  ipc_socket:     {}",
                        effective_config.paths.ipc_socket_path
                    );
                    println!("  logs_dir:       {}", effective_config.paths.logs_dir);
                    println!("  log_path:       {}", effective_config.paths.log_path);
                    println!();
                    println!("Settings:");
                    println!(
                        "  log_level:      {}",
                        effective_config.config.general.log_level
                    );
                    println!(
                        "  log_format:     {}",
                        effective_config.config.general.log_format
                    );
                    println!(
                        "  poll_interval:  {}ms",
                        effective_config.config.ingest.poll_interval_ms
                    );
                    println!(
                        "  retention_days: {}",
                        effective_config.config.storage.retention_days
                    );
                }
            } else {
                // Show raw config
                if json {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                } else {
                    // Re-serialize to TOML for display
                    let toml_str =
                        toml::to_string_pretty(&config).unwrap_or_else(|_| format!("{config:?}"));
                    println!("{toml_str}");
                }
            }
        }

        ConfigCommands::Set { key, value, path } => {
            // Find config path
            let config_path = if let Some(p) = path {
                std::path::PathBuf::from(p)
            } else if let Some(p) = cli_config {
                std::path::PathBuf::from(p)
            } else {
                // Default to ./wa.toml or user config
                let cwd_config = std::path::PathBuf::from("wa.toml");
                if cwd_config.exists() {
                    cwd_config
                } else {
                    dirs::config_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
                        .join("wa")
                        .join("wa.toml")
                }
            };

            // Read existing config
            let content = if config_path.exists() {
                std::fs::read_to_string(&config_path)?
            } else {
                generate_default_config_toml()
            };

            // Parse as TOML value for modification
            let mut doc = content
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| anyhow::anyhow!("Failed to parse config: {e}"))?;

            // Split key by dots
            let parts: Vec<&str> = key.split('.').collect();
            if parts.is_empty() {
                anyhow::bail!("Invalid key: empty");
            }

            // Navigate to the target and set value
            set_toml_value(&mut doc, &parts, &value)?;

            // Create parent directory if needed
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Write back
            std::fs::write(&config_path, doc.to_string())?;

            println!("Set {key} = {value}");
            println!("Config file: {}", config_path.display());
        }
    }

    Ok(())
}

/// Generate default config TOML with comments
fn generate_default_config_toml() -> String {
    r#"# wa configuration file
# See: https://github.com/your-org/wezterm-automata for documentation

[general]
# Log level: trace, debug, info, warn, error
log_level = "info"
# Log format: pretty (human) or json (machine)
log_format = "pretty"
# Optional log file path
# log_file = "~/.wa/wa.log"

[ingest]
# Base poll interval in milliseconds
poll_interval_ms = 200
# Minimum poll interval (adaptive polling lower bound)
min_poll_interval_ms = 50
# Maximum concurrent pane captures
max_concurrent_captures = 10
# Enable gap detection
gap_detection = true

[storage]
# Database file name (relative to workspace .wa/ directory)
db_path = "wa.db"
# Retention period in days (0 = unlimited)
retention_days = 30
# Maximum database size in MB (0 = unlimited)
retention_max_mb = 1000

[patterns]
# Enabled pattern packs
enabled_packs = ["builtin:core"]

[workflows]
# Enable workflow execution
enabled = true
# Maximum concurrent workflows
max_concurrent = 3

[safety]
# Require prompt to be active for send operations
require_prompt_active = true
# Block sends during alt-screen
block_alt_screen = true
# Block sends when there's a recent capture gap
block_recent_gap = true
"#
    .to_string()
}

/// Validate config and return warnings (non-fatal issues)
fn validate_config_warnings(config: &wa_core::config::Config) -> Vec<String> {
    let mut warnings = Vec::new();

    if config.ingest.poll_interval_ms < 50 {
        warnings.push(format!(
            "poll_interval_ms ({}) is very low; may cause high CPU usage",
            config.ingest.poll_interval_ms
        ));
    }

    if config.storage.retention_days == 0 && config.storage.retention_max_mb == 0 {
        warnings.push(
            "Both retention_days and retention_max_mb are 0; database may grow unbounded"
                .to_string(),
        );
    }

    if config.ingest.max_concurrent_captures > 50 {
        warnings.push(format!(
            "max_concurrent_captures ({}) is very high; may cause system instability",
            config.ingest.max_concurrent_captures
        ));
    }

    warnings
}

/// Set a value in a TOML document using dot-notation path
fn set_toml_value(
    doc: &mut toml_edit::DocumentMut,
    path: &[&str],
    value: &str,
) -> anyhow::Result<()> {
    if path.is_empty() {
        anyhow::bail!("Empty path");
    }

    // Navigate to the parent table
    let mut current: &mut toml_edit::Item = doc.as_item_mut();
    for (i, part) in path.iter().enumerate() {
        if i == path.len() - 1 {
            // Last element - set the value
            if let Some(table) = current.as_table_mut() {
                // Try to parse the value as appropriate type
                let toml_value = parse_toml_value(value);
                table[*part] = toml_value;
            } else {
                anyhow::bail!("Cannot set value: parent is not a table");
            }
        } else {
            // Intermediate element - navigate or create table
            if let Some(table) = current.as_table_mut() {
                if !table.contains_key(*part) {
                    table[*part] = toml_edit::Item::Table(toml_edit::Table::new());
                }
                current = &mut table[*part];
            } else {
                anyhow::bail!("Cannot navigate: {} is not a table", path[..=i].join("."));
            }
        }
    }

    Ok(())
}

/// Recommended minimum scrollback lines for wa to function reliably
const RECOMMENDED_SCROLLBACK_LINES: u64 = 50_000;

/// Check WezTerm scrollback configuration
///
/// Returns (scrollback_lines, config_path) if found, or an error message.
fn check_wezterm_scrollback() -> Result<(u64, std::path::PathBuf), String> {
    // Check common WezTerm config locations
    let config_paths: Vec<std::path::PathBuf> = vec![
        dirs::config_dir()
            .map(|p| p.join("wezterm/wezterm.lua"))
            .unwrap_or_default(),
        dirs::home_dir()
            .map(|p| p.join(".wezterm.lua"))
            .unwrap_or_default(),
        dirs::home_dir()
            .map(|p| p.join(".config/wezterm/wezterm.lua"))
            .unwrap_or_default(),
    ];

    for config_path in config_paths {
        if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(content) => {
                    // Parse scrollback_lines from Lua config
                    // Patterns: config.scrollback_lines = N or scrollback_lines = N
                    for line in content.lines() {
                        let line = line.trim();
                        // Skip comments
                        if line.starts_with("--") {
                            continue;
                        }
                        // Match: config.scrollback_lines = 50000 or scrollback_lines = 50000
                        if line.contains("scrollback_lines") && line.contains('=') {
                            // Extract the number after '='
                            if let Some(value_part) = line.split('=').nth(1) {
                                let value_str = value_part.trim().trim_end_matches(',');
                                if let Ok(lines) = value_str.parse::<u64>() {
                                    return Ok((lines, config_path));
                                }
                            }
                        }
                    }
                    return Err(format!(
                        "scrollback_lines not set in {}",
                        config_path.display()
                    ));
                }
                Err(e) => {
                    return Err(format!(
                        "Failed to read {}: {}",
                        config_path.display(),
                        e
                    ));
                }
            }
        }
    }

    Err("No WezTerm config file found (~/.config/wezterm/wezterm.lua or ~/.wezterm.lua)".to_string())
}

/// Diagnostic result for a single check
struct DiagnosticCheck {
    name: &'static str,
    status: DiagnosticStatus,
    detail: Option<String>,
    recommendation: Option<String>,
}

#[derive(PartialEq)]
enum DiagnosticStatus {
    Ok,
    Warning,
    Error,
}

impl DiagnosticCheck {
    fn ok(name: &'static str) -> Self {
        Self {
            name,
            status: DiagnosticStatus::Ok,
            detail: None,
            recommendation: None,
        }
    }

    fn ok_with_detail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DiagnosticStatus::Ok,
            detail: Some(detail.into()),
            recommendation: None,
        }
    }

    fn warning(name: &'static str, detail: impl Into<String>, recommendation: impl Into<String>) -> Self {
        Self {
            name,
            status: DiagnosticStatus::Warning,
            detail: Some(detail.into()),
            recommendation: Some(recommendation.into()),
        }
    }

    fn error(name: &'static str, detail: impl Into<String>, recommendation: impl Into<String>) -> Self {
        Self {
            name,
            status: DiagnosticStatus::Error,
            detail: Some(detail.into()),
            recommendation: Some(recommendation.into()),
        }
    }

    fn print(&self) {
        let status_icon = match self.status {
            DiagnosticStatus::Ok => "[OK]",
            DiagnosticStatus::Warning => "[WARN]",
            DiagnosticStatus::Error => "[ERR]",
        };

        if let Some(detail) = &self.detail {
            println!("  {} {} - {}", status_icon, self.name, detail);
        } else {
            println!("  {} {}", status_icon, self.name);
        }

        if let Some(rec) = &self.recommendation {
            println!("       → {}", rec);
        }
    }
}

/// Run all diagnostic checks and return results
fn run_diagnostics(permission_warnings: &[wa_core::config::PermissionWarning]) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check 1: wa-core loaded
    checks.push(DiagnosticCheck::ok("wa-core loaded"));

    // Check 2: WezTerm CLI available
    match std::process::Command::new("wezterm")
        .arg("--version")
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            let version = version.trim();
            checks.push(DiagnosticCheck::ok_with_detail("WezTerm CLI", version));
        }
        Ok(_) => {
            checks.push(DiagnosticCheck::error(
                "WezTerm CLI",
                "wezterm command failed",
                "Ensure WezTerm is installed and in PATH",
            ));
        }
        Err(_) => {
            checks.push(DiagnosticCheck::error(
                "WezTerm CLI",
                "wezterm not found",
                "Install WezTerm from https://wezfurlong.org/wezterm/",
            ));
        }
    }

    // Check 3: WezTerm scrollback configuration
    match check_wezterm_scrollback() {
        Ok((lines, path)) => {
            if lines >= RECOMMENDED_SCROLLBACK_LINES {
                checks.push(DiagnosticCheck::ok_with_detail(
                    "WezTerm scrollback",
                    format!("{} lines ({})", lines, path.display()),
                ));
            } else {
                checks.push(DiagnosticCheck::warning(
                    "WezTerm scrollback",
                    format!("{} lines (below {} recommended)", lines, RECOMMENDED_SCROLLBACK_LINES),
                    format!(
                        "Add to wezterm.lua: config.scrollback_lines = {}",
                        RECOMMENDED_SCROLLBACK_LINES
                    ),
                ));
            }
        }
        Err(msg) => {
            checks.push(DiagnosticCheck::warning(
                "WezTerm scrollback",
                msg,
                format!(
                    "Add to wezterm.lua: config.scrollback_lines = {}",
                    RECOMMENDED_SCROLLBACK_LINES
                ),
            ));
        }
    }

    // Check 4: Filesystem permissions
    if permission_warnings.is_empty() {
        checks.push(DiagnosticCheck::ok("filesystem permissions"));
    } else {
        for warning in permission_warnings {
            checks.push(DiagnosticCheck::warning(
                "filesystem permissions",
                format!(
                    "{} permissions too open ({:o})",
                    warning.label, warning.actual_mode
                ),
                format!(
                    "chmod {:o} {}",
                    warning.expected_mode,
                    warning.path.display()
                ),
            ));
        }
    }

    // Check 5: WezTerm running and responding
    match std::process::Command::new("wezterm")
        .args(["cli", "list", "--format", "json"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<Vec<serde_json::Value>>(&stdout) {
                Ok(panes) => {
                    checks.push(DiagnosticCheck::ok_with_detail(
                        "WezTerm connection",
                        format!("{} pane(s) detected", panes.len()),
                    ));
                }
                Err(_) => {
                    checks.push(DiagnosticCheck::warning(
                        "WezTerm connection",
                        "Could not parse pane list",
                        "Check WezTerm version compatibility",
                    ));
                }
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            checks.push(DiagnosticCheck::error(
                "WezTerm connection",
                format!("CLI failed: {}", stderr.trim()),
                "Ensure WezTerm GUI is running",
            ));
        }
        Err(e) => {
            checks.push(DiagnosticCheck::error(
                "WezTerm connection",
                format!("CLI error: {}", e),
                "Ensure WezTerm is installed and running",
            ));
        }
    }

    checks
}

/// Parse a string value into appropriate TOML type
fn parse_toml_value(value: &str) -> toml_edit::Item {
    // Try integer
    if let Ok(n) = value.parse::<i64>() {
        return toml_edit::value(n);
    }

    // Try float
    if let Ok(f) = value.parse::<f64>() {
        return toml_edit::value(f);
    }

    // Try boolean
    match value.to_lowercase().as_str() {
        "true" => return toml_edit::value(true),
        "false" => return toml_edit::value(false),
        _ => {}
    }

    // Default to string
    toml_edit::value(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_core::approval::hash_allow_once_code;
    use wa_core::storage::{ApprovalTokenRecord, StorageHandle};

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    async fn setup_storage(label: &str) -> (StorageHandle, String) {
        let temp_dir = std::env::temp_dir();
        let unique = now_ms();
        let db_path = temp_dir.join(format!(
            "wa_robot_approve_{label}_{}_{}.db",
            std::process::id(),
            unique
        ));
        let db_path_str = db_path.to_string_lossy().to_string();
        let storage = StorageHandle::new(&db_path_str).await.unwrap();
        (storage, db_path_str)
    }

    async fn cleanup_storage(storage: StorageHandle, db_path: &str) {
        let _ = storage.shutdown().await;
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(format!("{db_path}-wal"));
        let _ = std::fs::remove_file(format!("{db_path}-shm"));
    }

    async fn insert_token(
        storage: &StorageHandle,
        workspace_id: &str,
        code: &str,
        pane_id: Option<u64>,
        expires_at: i64,
        used_at: Option<i64>,
        fingerprint: &str,
    ) {
        let token = ApprovalTokenRecord {
            id: 0,
            code_hash: hash_allow_once_code(code),
            created_at: now_ms(),
            expires_at,
            used_at,
            workspace_id: workspace_id.to_string(),
            action_kind: "send_text".to_string(),
            pane_id,
            action_fingerprint: fingerprint.to_string(),
        };
        storage.insert_approval_token(token).await.unwrap();
    }

    #[test]
    fn structured_uservar_name_detection() {
        assert!(is_structured_uservar_name("wa_event"));
        assert!(is_structured_uservar_name("wa-foo"));
        assert!(is_structured_uservar_name("WA_FOO"));
        assert!(!is_structured_uservar_name("other_event"));
    }

    #[test]
    fn validate_uservar_rejects_empty_fields() {
        assert!(validate_uservar_request(1, "", "x").is_err());
        assert!(validate_uservar_request(1, "wa_event", "").is_err());
    }

    #[test]
    fn validate_uservar_rejects_oversize_payload() {
        let name = "wa_event";
        let value = "a".repeat(wa_core::ipc::MAX_MESSAGE_SIZE);
        let err = validate_uservar_request(1, name, &value).unwrap_err();
        assert!(err.contains("too large"));
    }

    #[test]
    fn validate_uservar_accepts_base64_json() {
        let name = "wa_event";
        let value = "eyJraW5kIjoicHJvbXB0In0="; // {"kind":"prompt"}
        assert!(validate_uservar_request(1, name, value).is_ok());
    }

    #[tokio::test]
    async fn robot_approve_valid_code_consumes() {
        let (storage, db_path) = setup_storage("valid").await;
        let workspace_id = "ws-valid";
        let code = "ABC12345";
        let fingerprint = "sha256:valid";
        let expires_at = now_ms() + 60_000;

        insert_token(
            &storage,
            workspace_id,
            code,
            Some(5),
            expires_at,
            None,
            fingerprint,
        )
        .await;

        let data = evaluate_robot_approve(
            &storage,
            workspace_id,
            code,
            Some(5),
            Some(fingerprint),
            false,
        )
        .await
        .unwrap();

        assert!(data.valid);
        assert_eq!(data.code, code);
        assert_eq!(data.pane_id, Some(5));
        assert!(data.consumed_at.is_some());

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn robot_approve_dry_run_does_not_consume() {
        let (storage, db_path) = setup_storage("dry_run").await;
        let workspace_id = "ws-dry";
        let code = "DRY12345";
        let fingerprint = "sha256:dry";
        let expires_at = now_ms() + 60_000;

        insert_token(
            &storage,
            workspace_id,
            code,
            Some(1),
            expires_at,
            None,
            fingerprint,
        )
        .await;

        let data = evaluate_robot_approve(
            &storage,
            workspace_id,
            code,
            Some(1),
            Some(fingerprint),
            true,
        )
        .await
        .unwrap();

        assert_eq!(data.dry_run, Some(true));
        assert!(data.consumed_at.is_none());

        let data2 = evaluate_robot_approve(
            &storage,
            workspace_id,
            code,
            Some(1),
            Some(fingerprint),
            false,
        )
        .await
        .unwrap();

        assert!(data2.consumed_at.is_some());

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn robot_approve_expired_code() {
        let (storage, db_path) = setup_storage("expired").await;
        let workspace_id = "ws-expired";
        let code = "EXP12345";
        let fingerprint = "sha256:expired";

        insert_token(
            &storage,
            workspace_id,
            code,
            Some(2),
            now_ms() - 1,
            None,
            fingerprint,
        )
        .await;

        let err = evaluate_robot_approve(
            &storage,
            workspace_id,
            code,
            Some(2),
            Some(fingerprint),
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, "E_APPROVAL_EXPIRED");

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn robot_approve_consumed_code() {
        let (storage, db_path) = setup_storage("consumed").await;
        let workspace_id = "ws-consumed";
        let code = "CON12345";
        let fingerprint = "sha256:consumed";
        let used_at = now_ms() - 10;

        insert_token(
            &storage,
            workspace_id,
            code,
            Some(3),
            now_ms() + 60_000,
            Some(used_at),
            fingerprint,
        )
        .await;

        let err = evaluate_robot_approve(
            &storage,
            workspace_id,
            code,
            Some(3),
            Some(fingerprint),
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, "E_APPROVAL_CONSUMED");

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn robot_approve_wrong_pane() {
        let (storage, db_path) = setup_storage("wrong_pane").await;
        let workspace_id = "ws-pane";
        let code = "PANE1234";
        let fingerprint = "sha256:pane";
        let expires_at = now_ms() + 60_000;

        insert_token(
            &storage,
            workspace_id,
            code,
            Some(9),
            expires_at,
            None,
            fingerprint,
        )
        .await;

        let err = evaluate_robot_approve(
            &storage,
            workspace_id,
            code,
            Some(7),
            Some(fingerprint),
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, "E_WRONG_PANE");

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn robot_approve_wrong_workspace() {
        let (storage, db_path) = setup_storage("wrong_ws").await;
        let code = "WS123456";
        let fingerprint = "sha256:ws";
        let expires_at = now_ms() + 60_000;

        insert_token(&storage, "ws-a", code, None, expires_at, None, fingerprint).await;

        let err = evaluate_robot_approve(&storage, "ws-b", code, None, Some(fingerprint), false)
            .await
            .unwrap_err();

        assert_eq!(err.code, "E_WRONG_WORKSPACE");

        cleanup_storage(storage, &db_path).await;
    }

    #[tokio::test]
    async fn robot_approve_fingerprint_mismatch() {
        let (storage, db_path) = setup_storage("fingerprint").await;
        let workspace_id = "ws-fp";
        let code = "FP123456";
        let expires_at = now_ms() + 60_000;

        insert_token(
            &storage,
            workspace_id,
            code,
            Some(4),
            expires_at,
            None,
            "sha256:expected",
        )
        .await;

        let err = evaluate_robot_approve(
            &storage,
            workspace_id,
            code,
            Some(4),
            Some("sha256:other"),
            false,
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, "E_FINGERPRINT_MISMATCH");

        cleanup_storage(storage, &db_path).await;
    }
}
