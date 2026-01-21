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

    /// Run diagnostics
    Doctor,

    /// Setup helpers
    Setup {
        #[command(subcommand)]
        command: SetupCommands,
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
        #[arg(long)]
        unhandled_only: bool,

        /// Only return events since this timestamp (epoch ms)
        #[arg(long)]
        since: Option<i64>,
    },

    /// Run a workflow by name on a pane
    Workflow {
        /// Workflow name
        name: String,

        /// Target pane ID
        pane_id: u64,

        /// Bypass "already handled" checks (still policy-gated)
        #[arg(long)]
        force: bool,
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

const ROBOT_ERR_INVALID_ARGS: &str = "robot.invalid_args";
const ROBOT_ERR_UNKNOWN_SUBCOMMAND: &str = "robot.unknown_subcommand";
const ROBOT_ERR_NOT_IMPLEMENTED: &str = "robot.not_implemented";
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
            .map(|r| format!(" ({})", r))
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

fn redact_for_output(text: &str) -> String {
    static REDACTOR: LazyLock<wa_core::policy::Redactor> =
        LazyLock::new(wa_core::policy::Redactor::new);
    REDACTOR.redact(text)
}

fn build_send_dry_run_report(
    command_ctx: &wa_core::dry_run::CommandContext,
    pane_id: u64,
    text: &str,
    no_paste: bool,
) -> wa_core::dry_run::DryRunReport {
    use wa_core::dry_run::{
        TargetResolution, build_send_policy_evaluation, create_send_action, create_wait_for_action,
    };

    let mut ctx = command_ctx.dry_run_context();

    // Target resolution (simulated for now)
    ctx.set_target(
        TargetResolution::new(pane_id, "local")
            .with_title("(pane title)")
            .with_cwd("(current directory)"),
    );

    // Policy evaluation (simulated values)
    let eval = build_send_policy_evaluation(
        (2, 30), // rate limit status
        true,    // is_prompt_active
        true,    // require_prompt_active
        false,   // has_recent_gaps
    );
    ctx.set_policy_evaluation(eval);

    // Expected actions
    ctx.add_action(create_send_action(1, pane_id, text.len()));
    ctx.add_action(create_wait_for_action(2, "prompt boundary", 30000));

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
                name: "workflow",
                description: "Run a workflow by name on a pane",
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
                name: "workflow",
                args: "<name> <pane_id> [--force]",
                summary: "Run a workflow by name on a pane (policy-gated)",
                examples: vec![
                    "wa robot workflow handle_compaction 0",
                    "wa robot workflow handle_usage_limit 1 --force",
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
            actual_mode = format!("{:o}", warning.actual_mode),
            expected_mode = format!("{:o}", warning.expected_mode),
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

/// Run the observation watcher daemon.
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
                    "Another watcher is already running (pid: {}, started: {}). \
                     Use --dangerous-disable-lock to override (NOT RECOMMENDED).",
                    pid,
                    started_at
                );
            }
            Err(wa_core::lock::LockError::AlreadyRunningNoMeta) => {
                anyhow::bail!(
                    "Another watcher is already running (metadata unavailable). \
                     Use --dangerous-disable-lock to override (NOT RECOMMENDED)."
                );
            }
            Err(e) => {
                anyhow::bail!("Failed to acquire watcher lock: {}", e);
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
        handle_fatal_error(err, robot_mode);
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
        config,
        workspace,
        command,
    } = cli;

    let mut overrides = wa_core::config::ConfigOverrides::default();
    if verbose {
        overrides.log_level = Some("debug".to_string());
    }

    let config_path = config.as_deref().map(Path::new);
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
                    let _ctx = match build_robot_context(&config, &workspace_root) {
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
                        } => {
                            let redacted_text = redact_for_output(&text);
                            let command = if dry_run {
                                format!("wa robot send {pane_id} \"{redacted_text}\" --dry-run")
                            } else {
                                format!("wa robot send {pane_id} \"{redacted_text}\"")
                            };
                            let command_ctx =
                                wa_core::dry_run::CommandContext::new(command, dry_run);

                            if command_ctx.is_dry_run() {
                                let report =
                                    build_send_dry_run_report(&command_ctx, pane_id, &text, false);
                                let response = RobotResponse::success(report, elapsed_ms(start));
                                println!("{}", serde_json::to_string_pretty(&response)?);
                            } else {
                                let response: RobotResponse<()> = RobotResponse::error_with_code(
                                    ROBOT_ERR_NOT_IMPLEMENTED,
                                    format!(
                                        "send to pane {pane_id} not yet implemented (text: {redacted_text})"
                                    ),
                                    None,
                                    elapsed_ms(start),
                                );
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
                                            "Timeout waiting for pattern '{}' after {}ms ({} polls)",
                                            pattern, elapsed, polls
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
                            unhandled_only,
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
                                unhandled_only,
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
                                            let pack_id = e
                                                .rule_id
                                                .split('.')
                                                .next()
                                                .map(|agent| format!("builtin:{agent}"))
                                                .unwrap_or_else(|| "builtin:unknown".to_string());
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
                                        unhandled_only,
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
                        RobotCommands::Workflow {
                            name,
                            pane_id,
                            force,
                        } => {
                            use std::sync::Arc;
                            use wa_core::policy::{PolicyEngine, PolicyGatedInjector};
                            use wa_core::storage::StorageHandle;
                            use wa_core::workflows::{
                                PaneWorkflowLockManager, WorkflowEngine, WorkflowExecutionResult,
                                WorkflowRunner, WorkflowRunnerConfig,
                            };

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
                                        println!("{}", serde_json::to_string_pretty(&response)?);
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
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                    return Ok(());
                                }
                            }

                            // Set up workflow infrastructure
                            let db_path = &_ctx.effective.paths.db_path;
                            let storage = match StorageHandle::new(db_path).await {
                                Ok(s) => Arc::new(s),
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotWorkflowData>::error_with_code(
                                            "robot.storage_error",
                                            format!("Failed to open storage: {e}"),
                                            Some(
                                                "Check database path and permissions.".to_string(),
                                            ),
                                            elapsed_ms(start),
                                        );
                                    println!("{}", serde_json::to_string_pretty(&response)?);
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

                            match workflow {
                                Some(wf) => {
                                    // Generate execution ID
                                    let execution_id = format!(
                                        "robot-{}-{}",
                                        name,
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis()
                                    );

                                    // Run the workflow
                                    let result =
                                        runner.run_workflow(pane_id, wf, &execution_id, 0).await;

                                    let (status, message, result_value, steps_executed, step_index) =
                                        match result {
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
                                            format!("Workflow '{}' denied by policy", name),
                                            Some(
                                                "Check safety configuration or use --dry-run."
                                                    .to_string(),
                                            ),
                                            workflow_elapsed,
                                        )
                                    } else {
                                        RobotResponse::<RobotWorkflowData>::error_with_code(
                                            &format!("robot.workflow_{}", status),
                                            format!(
                                                "Workflow '{}' {}",
                                                name,
                                                data.message.as_deref().unwrap_or("failed")
                                            ),
                                            None,
                                            workflow_elapsed,
                                        )
                                    };
                                    println!("{}", serde_json::to_string_pretty(&response)?);
                                }
                                None => {
                                    // No workflow registered with this name
                                    // Note: In standalone robot mode, no workflows are registered.
                                    // The daemon (wa watch --auto-handle) would have workflows
                                    // registered for event-driven execution.
                                    let response =
                                        RobotResponse::<RobotWorkflowData>::error_with_code(
                                            "robot.workflow_not_found",
                                            format!("Workflow '{}' not found", name),
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
                            }

                            // Clean shutdown of storage
                            if let Err(e) = storage.shutdown().await {
                                tracing::warn!("Failed to shutdown storage cleanly: {e}");
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
                println!(
                    "Panes ({} observed, {} ignored):",
                    observed_count, ignored_count
                );
                println!(
                    "  {:>4}  {:>10}  {:<20}  {:<40}  {}",
                    "ID", "STATUS", "TITLE", "CWD", "DOMAIN"
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
                let report = build_send_dry_run_report(&command_ctx, pane_id, &text, no_paste);
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

        Some(Commands::Doctor) => {
            println!("Running diagnostics...");
            println!("  [OK] wa-core loaded");
            if permission_warnings.is_empty() {
                println!("  [OK] filesystem permissions");
                println!("All checks passed!");
            } else {
                for warning in &permission_warnings {
                    println!(
                        "WARNING: {} permissions too open ({:o})",
                        warning.label, warning.actual_mode
                    );
                    println!("  Path: {}", warning.path.display());
                    println!(
                        "  Recommended: chmod {:o} {}",
                        warning.expected_mode,
                        warning.path.display()
                    );
                }
                println!("Diagnostics completed with warnings.");
            }
        }

        Some(Commands::Setup { command }) => match command {
            SetupCommands::Local => {
                println!("Local setup not yet implemented");
            }
            SetupCommands::Remote { host } => {
                println!("Remote setup for '{host}' not yet implemented");
            }
            SetupCommands::Config => {
                println!("Config generation not yet implemented");
            }
        },

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

fn handle_fatal_error(err: anyhow::Error, robot_mode: bool) {
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
