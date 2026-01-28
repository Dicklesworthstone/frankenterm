//! WezTerm Automata CLI
//!
//! Terminal hypervisor for AI agent swarms running in WezTerm.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand, ValueEnum};
use wa_core::logging::{LogConfig, LogError, init_logging};
use wa_core::storage::{MigrationPlan, MigrationStatusReport};

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

    /// Robot mode commands (machine-readable I/O)
    Robot {
        /// Output format for robot responses
        #[arg(long, short = 'f', value_enum)]
        format: Option<RobotOutputFormat>,

        /// Show token statistics on stderr (JSON vs TOON)
        #[arg(long)]
        stats: bool,

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

        /// Do not append a trailing newline
        #[arg(long)]
        no_newline: bool,

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

    /// Ingest external events (e.g., WezTerm user-var signals from shell hooks)
    ///
    /// Note: --from-status was removed in v0.2.0 (Lua performance optimization).
    /// Alt-screen detection is now handled via escape sequence parsing.
    Event {
        /// Event source is a WezTerm user-var change (currently the only supported source)
        #[arg(long)]
        from_uservar: bool,

        /// Pane ID that emitted the event
        #[arg(long)]
        pane: u64,

        /// User-var name (e.g., "wa_event") - required for --from-uservar
        #[arg(long, required_if_eq("from_uservar", "true"))]
        name: Option<String>,

        /// Raw user-var value (typically base64-encoded JSON) - required for --from-uservar
        #[arg(long, required_if_eq("from_uservar", "true"))]
        value: Option<String>,
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
        /// List SSH hosts from ~/.ssh/config
        #[arg(long = "list-hosts")]
        list_hosts: bool,

        /// Apply setup changes automatically (non-destructive)
        #[arg(long, global = true)]
        apply: bool,

        /// Show what would change without modifying files
        #[arg(long, global = true)]
        dry_run: bool,

        #[command(subcommand)]
        command: Option<SetupCommands>,
    },

    /// Configuration management commands
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// Database maintenance commands
    Db {
        #[command(subcommand)]
        command: DbCommands,
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

        /// Preview which workflows would handle these events (no execution)
        #[arg(long)]
        would_handle: bool,

        /// Mark output as dry-run preview (implies --would-handle)
        #[arg(long)]
        dry_run: bool,
    },

    /// Workflow management commands
    Workflow {
        #[command(subcommand)]
        command: RobotWorkflowCommands,
    },

    /// Rule management commands (list rules, test text against rules)
    Rules {
        #[command(subcommand)]
        command: RobotRulesCommands,
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
        /// Execution ID (optional when using --pane or --active)
        execution_id: Option<String>,

        /// Filter by pane ID (list all workflows for this pane)
        #[arg(long)]
        pane: Option<u64>,

        /// List all active workflows (status: running or waiting)
        #[arg(long)]
        active: bool,

        /// Include step logs in response
        #[arg(long, short)]
        verbose: bool,
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

/// Robot rules subcommands
#[derive(Subcommand)]
enum RobotRulesCommands {
    /// List all rules with metadata
    List {
        /// Filter by pack name (e.g., "builtin:codex")
        #[arg(long)]
        pack: Option<String>,

        /// Filter by agent type (codex, claude_code, gemini, wezterm)
        #[arg(long)]
        agent_type: Option<String>,

        /// Include rule descriptions in output
        #[arg(long, short)]
        verbose: bool,
    },

    /// Test text against rules and show match trace
    Test {
        /// Text to test against rules
        text: String,

        /// Show full trace evidence for matches
        #[arg(long)]
        trace: bool,

        /// Only test rules from a specific pack
        #[arg(long)]
        pack: Option<String>,
    },

    /// Show details for a specific rule
    Show {
        /// Rule ID (e.g., "codex.usage_reached")
        rule_id: String,
    },

    /// Lint rules: validate IDs, check fixtures, validate regex patterns
    Lint {
        /// Only check a specific pack (e.g., "builtin:codex")
        #[arg(long)]
        pack: Option<String>,

        /// Include fixture coverage check (validates corpus has tests for each rule)
        #[arg(long)]
        fixtures: bool,

        /// Skip regex complexity check
        #[arg(long)]
        skip_regex_check: bool,

        /// Fail on warnings (exit non-zero)
        #[arg(long)]
        strict: bool,
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

        /// Include action plan and step logs
        #[arg(long, short)]
        verbose: bool,
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

        /// Skip interactive confirmation (only with --apply)
        #[arg(long)]
        yes: bool,

        /// Install wa on the remote host
        #[arg(long)]
        install_wa: bool,

        /// Path to local wa binary for scp
        #[arg(long)]
        wa_path: Option<PathBuf>,

        /// Install wa from git (optional tag or revision)
        #[arg(long)]
        wa_version: Option<String>,

        /// Timeout per remote command (seconds)
        #[arg(long, default_value = "30")]
        timeout_secs: u64,
    },

    /// Generate WezTerm config additions
    Config,

    /// Patch wezterm.lua with user-var forwarding (idempotent)
    Patch {
        /// Remove the wa block instead of adding it
        #[arg(long)]
        remove: bool,

        /// Custom path to wezterm.lua (auto-detected if not specified)
        #[arg(long)]
        config_path: Option<PathBuf>,
    },

    /// Install OSC 133 prompt markers in shell rc file (idempotent)
    Shell {
        /// Remove the wa block instead of adding it
        #[arg(long)]
        remove: bool,

        /// Shell type: bash, zsh, or fish (auto-detected from $SHELL if not specified)
        #[arg(long, value_parser = ["bash", "zsh", "fish"])]
        shell: Option<String>,

        /// Custom path to rc file (auto-detected if not specified)
        #[arg(long)]
        rc_path: Option<PathBuf>,
    },
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

#[derive(Subcommand)]
enum DbCommands {
    /// Migrate or inspect the database schema
    Migrate {
        /// Show migration status without applying changes
        #[arg(long)]
        status: bool,

        /// Target schema version (default: current SCHEMA_VERSION)
        #[arg(long)]
        to: Option<i32>,

        /// Apply without prompting for confirmation
        #[arg(long)]
        yes: bool,

        /// Allow downgrades (dangerous)
        #[arg(long)]
        allow_downgrade: bool,

        /// Show the plan without applying migrations
        #[arg(long)]
        dry_run: bool,
    },
}

const ROBOT_ERR_INVALID_ARGS: &str = "robot.invalid_args";
const ROBOT_ERR_UNKNOWN_SUBCOMMAND: &str = "robot.unknown_subcommand";
const ROBOT_ERR_CONFIG: &str = "robot.config_error";
const ROBOT_ERR_FTS_QUERY: &str = "robot.fts_query_error";
const ROBOT_ERR_STORAGE: &str = "robot.storage_error";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RobotOutputFormat {
    Json,
    Toon,
}

fn parse_robot_output_format(s: &str) -> Option<RobotOutputFormat> {
    match s.trim().to_ascii_lowercase().as_str() {
        "json" => Some(RobotOutputFormat::Json),
        "toon" => Some(RobotOutputFormat::Toon),
        _ => None,
    }
}

fn resolve_robot_output_format(cli: Option<RobotOutputFormat>) -> RobotOutputFormat {
    if let Some(format) = cli {
        return format;
    }

    if let Ok(val) = std::env::var("WA_OUTPUT_FORMAT") {
        if let Some(format) = parse_robot_output_format(&val) {
            return format;
        }
    }

    if let Ok(val) = std::env::var("TOON_DEFAULT_FORMAT") {
        if let Some(format) = parse_robot_output_format(&val) {
            return format;
        }
    }

    RobotOutputFormat::Json
}

fn sniff_robot_output_format_from_args() -> Option<RobotOutputFormat> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--format=") {
            return parse_robot_output_format(rest);
        }

        if arg == "--format" || arg == "-f" {
            if let Some(val) = args.next() {
                return parse_robot_output_format(&val);
            }
        }
    }
    None
}

fn sniff_robot_mode_from_args() -> bool {
    let mut args = std::env::args();
    // Skip argv[0]
    let _ = args.next();

    let mut skip_next_value = false;

    while let Some(arg) = args.next() {
        if skip_next_value {
            skip_next_value = false;
            continue;
        }

        if arg == "--" {
            return args.next().is_some_and(|sub| sub == "robot");
        }

        // Known global options that take values.
        if arg == "--config" || arg == "-c" || arg == "--workspace" {
            skip_next_value = true;
            continue;
        }
        if arg.starts_with("--config=") || arg.starts_with("--workspace=") {
            continue;
        }

        // Any other long option (including clap-provided --help/--version).
        if arg.starts_with("--") {
            continue;
        }

        // Handle short option clusters like `-vc foo robot` and attached values like `-cfoo`.
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            if arg == "-v" {
                continue;
            }
            if let Some(rest) = arg.strip_prefix("-c") {
                // `-cVALUE` consumes the rest of the arg as the value.
                if rest.is_empty() {
                    skip_next_value = true;
                }
                continue;
            }

            let mut chars = arg[1..].chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    'v' => {}
                    'c' => {
                        // `-cVALUE` (attached) or `-c VALUE` (next token).
                        if chars.peek().is_none() {
                            skip_next_value = true;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            continue;
        }

        // First positional token is the subcommand.
        return arg == "robot";
    }

    false
}

fn should_show_toon_stats(cli_stats: bool) -> bool {
    cli_stats || std::env::var("TOON_STATS").is_ok()
}

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

fn estimate_tokens(s: &str) -> usize {
    let chars = s.len();
    let words = s.split_whitespace().count();
    std::cmp::max(chars / 4, words)
}

fn emit_toon_stats(json: &str, toon: &str) {
    let json_tokens = estimate_tokens(json);
    let toon_tokens = estimate_tokens(toon);
    #[allow(clippy::cast_possible_wrap)]
    let savings_pct = if json_tokens > 0 {
        // TOON can be larger than JSON for very small payloads; use signed math so we don't underflow.
        // Token counts are small enough that wrap is impossible in practice.
        100_i64 - ((toon_tokens as i64) * 100 / (json_tokens as i64))
    } else {
        0
    };

    eprintln!(
        "[stats] JSON: {json_tokens} tokens, TOON: {toon_tokens} tokens ({savings_pct}% savings)"
    );
}

fn print_robot_response<T: serde::Serialize>(
    response: &RobotResponse<T>,
    format: RobotOutputFormat,
    cli_stats: bool,
) -> anyhow::Result<()> {
    let show_stats = should_show_toon_stats(cli_stats);

    match format {
        RobotOutputFormat::Json => {
            if show_stats {
                // Use compact JSON for stats to avoid counting whitespace.
                let json_compact = serde_json::to_string(response)?;
                let toon = toon_rust::encode(serde_json::to_value(response)?, None);
                emit_toon_stats(&json_compact, &toon);
            }

            println!("{}", serde_json::to_string_pretty(response)?);
        }
        RobotOutputFormat::Toon => {
            let toon = toon_rust::encode(serde_json::to_value(response)?, None);

            if show_stats {
                let json_compact = serde_json::to_string(response)?;
                emit_toon_stats(&json_compact, &toon);
            }

            println!("{toon}");
        }
    }

    Ok(())
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

/// Human send response data (stable JSON when non-TTY)
#[derive(serde::Serialize)]
struct HumanSendData {
    pane_id: u64,
    injection: wa_core::policy::InjectionResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_for: Option<RobotWaitForData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_error: Option<String>,
    no_paste: bool,
    no_newline: bool,
}

#[derive(Debug, serde::Deserialize)]
struct IpcPaneState {
    pane_id: u64,
    known: bool,
    #[serde(default)]
    observed: Option<bool>,
    #[serde(default)]
    alt_screen: Option<bool>,
    #[serde(default)]
    last_status_at: Option<i64>,
    #[serde(default)]
    in_gap: Option<bool>,
    #[serde(default)]
    cursor_alt_screen: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    would_handle: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    would_handle_with: Option<RobotEventWouldHandle>,
}

/// Workflow preview for robot events dry-run
#[derive(serde::Serialize)]
struct RobotEventWouldHandle {
    workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    estimated_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
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

/// Robot rules list response data
#[derive(Debug, serde::Serialize)]
struct RobotRulesListData {
    rules: Vec<RobotRuleItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pack_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_type_filter: Option<String>,
}

/// Individual rule item for robot mode
#[derive(Debug, serde::Serialize)]
struct RobotRuleItem {
    id: String,
    agent_type: String,
    event_type: String,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
    anchor_count: usize,
    has_regex: bool,
}

/// Robot rules test response data
#[derive(Debug, serde::Serialize)]
struct RobotRulesTestData {
    text_length: usize,
    match_count: usize,
    matches: Vec<RobotRuleMatchItem>,
}

/// Individual rule match item for robot mode
#[derive(Debug, serde::Serialize)]
struct RobotRuleMatchItem {
    rule_id: String,
    agent_type: String,
    event_type: String,
    severity: String,
    confidence: f64,
    matched_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    extracted: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<RobotRuleTraceInfo>,
}

/// Trace info for rule match
#[derive(Debug, serde::Serialize)]
struct RobotRuleTraceInfo {
    anchors_checked: bool,
    regex_matched: bool,
}

/// Robot rules show response data (full rule details)
#[derive(Debug, serde::Serialize)]
struct RobotRuleDetailData {
    id: String,
    agent_type: String,
    event_type: String,
    severity: String,
    description: String,
    anchors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manual_fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    learn_more_url: Option<String>,
}

/// Robot rules lint response data
#[derive(Debug, serde::Serialize)]
struct RobotRulesLintData {
    total_rules: usize,
    rules_checked: usize,
    errors: Vec<RobotLintIssue>,
    warnings: Vec<RobotLintIssue>,
    /// Fixture coverage statistics (present when --fixtures flag used)
    #[serde(skip_serializing_if = "Option::is_none")]
    fixture_coverage: Option<RobotFixtureCoverage>,
    passed: bool,
}

/// Individual lint issue
#[derive(Debug, serde::Serialize)]
struct RobotLintIssue {
    rule_id: String,
    category: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<String>,
}

/// Fixture coverage statistics
#[derive(Debug, serde::Serialize)]
#[allow(clippy::struct_field_names)]
struct RobotFixtureCoverage {
    rules_with_fixtures: usize,
    rules_without_fixtures: Vec<String>,
    total_fixtures: usize,
}

const RULE_ID_PREFIXES: &[&str] = &["codex.", "claude_code.", "gemini.", "wezterm."];

fn collect_fixture_rule_ids(
    dir: &Path,
    rules_with_fixtures: &mut HashSet<String>,
    total_fixtures: &mut usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_fixture_rule_ids(&path, rules_with_fixtures, total_fixtures);
        } else if path.extension().is_some_and(|ext| ext == "json")
            && path.to_string_lossy().contains(".expect.")
        {
            *total_fixtures += 1;
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(arr) = val.as_array() {
                        for item in arr {
                            if let Some(rule_id) =
                                item.get("rule_id").and_then(serde_json::Value::as_str)
                            {
                                rules_with_fixtures.insert(rule_id.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger_event_id: Option<i64>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_step_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_step: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_steps: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_condition: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_logs: Option<Vec<RobotWorkflowStepLog>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_plan: Option<RobotWorkflowActionPlan>,
}

/// Robot workflow step log entry (matches wa-robot-workflow-status.json schema)
#[derive(Debug, serde::Serialize)]
struct RobotWorkflowStepLog {
    step_index: usize,
    step_name: String,
    result_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_summary: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_refs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<i64>,
}

/// Robot workflow action plan entry
#[derive(Debug, serde::Serialize)]
struct RobotWorkflowActionPlan {
    plan_id: String,
    plan_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
}

/// Robot workflow status list response (for --pane or --active)
#[derive(Debug, serde::Serialize)]
struct RobotWorkflowStatusListData {
    executions: Vec<RobotWorkflowStatusData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_filter: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_only: Option<bool>,
    count: usize,
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
        .map_or(0, |d| d.as_millis() as i64);

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

const SEND_OSC_SEGMENT_LIMIT: usize = 200;

struct CapabilityResolution {
    capabilities: wa_core::policy::PaneCapabilities,
    warnings: Vec<String>,
}

async fn derive_osc_state_from_storage(
    storage: &wa_core::storage::StorageHandle,
    pane_id: u64,
) -> Result<Option<wa_core::ingest::Osc133State>, String> {
    let segments = storage
        .get_segments(pane_id, SEND_OSC_SEGMENT_LIMIT)
        .await
        .map_err(|e| format!("failed to read segments: {e}"))?;
    if segments.is_empty() {
        return Ok(None);
    }

    let mut state = wa_core::ingest::Osc133State::new();
    for segment in segments.iter().rev() {
        wa_core::ingest::process_osc133_output(&mut state, &segment.content);
    }

    if state.markers_seen == 0 {
        return Ok(None);
    }

    Ok(Some(state))
}

#[cfg(unix)]
async fn fetch_pane_state_from_ipc(
    socket_path: &Path,
    pane_id: u64,
) -> Result<Option<IpcPaneState>, String> {
    let client = wa_core::ipc::IpcClient::new(socket_path);
    match client.pane_state(pane_id).await {
        Ok(response) => {
            if !response.ok {
                let detail = response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string());
                return Err(detail);
            }
            if let Some(data) = response.data {
                serde_json::from_value::<IpcPaneState>(data)
                    .map(Some)
                    .map_err(|e| format!("invalid pane state payload: {e}"))
            } else {
                Ok(None)
            }
        }
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(not(unix))]
async fn fetch_pane_state_from_ipc(
    _socket_path: &Path,
    _pane_id: u64,
) -> Result<Option<IpcPaneState>, String> {
    Err("IPC not supported on this platform".to_string())
}

fn resolve_alt_screen_state(state: &IpcPaneState) -> Option<bool> {
    if !state.known {
        return None;
    }
    if let Some(cursor_state) = state.cursor_alt_screen {
        return Some(cursor_state);
    }
    if state.last_status_at.is_some() {
        return state.alt_screen;
    }
    None
}

async fn resolve_pane_capabilities(
    pane_id: u64,
    storage: Option<&wa_core::storage::StorageHandle>,
    ipc_socket_path: Option<&Path>,
) -> CapabilityResolution {
    let mut warnings = Vec::new();
    let mut osc_state = None;

    if let Some(storage) = storage {
        match derive_osc_state_from_storage(storage, pane_id).await {
            Ok(state) => osc_state = state,
            Err(err) => warnings.push(format!("OSC 133 state unavailable: {err}")),
        }
    } else {
        warnings.push("Storage unavailable; prompt state unknown.".to_string());
    }

    let mut alt_screen = None;
    let mut in_gap = true;
    let mut gap_known = false;

    if let Some(socket_path) = ipc_socket_path {
        match fetch_pane_state_from_ipc(socket_path, pane_id).await {
            Ok(Some(state)) => {
                if state.pane_id != pane_id {
                    warnings.push(format!(
                        "Watcher returned state for pane {} (expected {})",
                        state.pane_id, pane_id
                    ));
                }
                if !state.known {
                    let reason = state.reason.as_deref().unwrap_or("unknown");
                    warnings.push(format!("Watcher has no state for this pane ({reason})."));
                } else if state.observed == Some(false) {
                    warnings.push(
                        "Pane is not observed by watcher; state may be incomplete.".to_string(),
                    );
                }
                alt_screen = resolve_alt_screen_state(&state);
                if state.in_gap.is_some() {
                    gap_known = true;
                    in_gap = state.in_gap.unwrap_or(true);
                }
                if alt_screen.is_none() {
                    warnings
                        .push("Alt-screen state unknown; approval may be required.".to_string());
                }
                if in_gap {
                    if gap_known {
                        warnings.push(
                            "Recent capture gap detected; approval may be required.".to_string(),
                        );
                    } else {
                        warnings.push(
                            "Capture continuity unknown; treating as recent gap.".to_string(),
                        );
                    }
                } else if !gap_known {
                    warnings
                        .push("Capture continuity unknown; treating as recent gap.".to_string());
                }
            }
            Ok(None) => {
                warnings.push("Watcher IPC returned no pane state.".to_string());
            }
            Err(err) => {
                warnings.push(format!("Watcher IPC unavailable: {err}"));
            }
        }
    } else {
        warnings.push("IPC socket unavailable; alt-screen/gap unknown.".to_string());
    }

    let capabilities = wa_core::policy::PaneCapabilities::from_ingest_state(
        osc_state.as_ref(),
        alt_screen,
        in_gap,
    );

    CapabilityResolution {
        capabilities,
        warnings,
    }
}

fn build_send_dry_run_report(
    command_ctx: &wa_core::dry_run::CommandContext,
    pane_id: u64,
    pane_info: Option<&wa_core::wezterm::PaneInfo>,
    capabilities: Option<&wa_core::policy::PaneCapabilities>,
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
    let capabilities_opt = capabilities;
    let capabilities = capabilities_opt
        .cloned()
        .unwrap_or_else(PaneCapabilities::prompt);
    let eval = build_send_policy_evaluation(
        (0, config.safety.rate_limit_per_pane),
        capabilities.prompt_active,
        config.safety.require_prompt_active,
        capabilities.has_recent_gap,
    );
    ctx.set_policy_evaluation(eval);

    if let Some(provided_caps) = capabilities_opt {
        if provided_caps.alt_screen == Some(true) {
            ctx.add_warning("Pane is in alt-screen; send will be denied by policy.");
        } else if provided_caps.alt_screen.is_none() {
            ctx.add_warning("Alt-screen state unknown; approval may be required.");
        }
        if provided_caps.has_recent_gap {
            ctx.add_warning("Recent capture gap detected; approval may be required.");
        }
        if config.safety.require_prompt_active && !provided_caps.prompt_active {
            ctx.add_warning("Prompt not active; approval or denial likely.");
        }
    }

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

fn format_send_result_human(
    data: &HumanSendData,
    redacted_text: &str,
    redacted_wait_for: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    use wa_core::policy::{InjectionResult, PolicyDecision};

    let mut output = String::new();
    let status = match data.injection {
        InjectionResult::Allowed { .. } => "allowed",
        InjectionResult::Denied { .. } => "denied",
        InjectionResult::RequiresApproval { .. } => "approval_required",
        InjectionResult::Error { .. } => "error",
    };

    let mut reason = None;
    let mut approval = None;
    if let InjectionResult::Denied { decision, .. }
    | InjectionResult::RequiresApproval { decision, .. } = &data.injection
    {
        match decision {
            PolicyDecision::Deny { reason: r, .. } => reason = Some(r.as_str()),
            PolicyDecision::RequireApproval {
                reason: r,
                approval: a,
                ..
            } => {
                reason = Some(r.as_str());
                approval = a.as_ref();
            }
            PolicyDecision::Allow { .. } => {}
        }
    }

    if let InjectionResult::Error { error, .. } = &data.injection {
        reason = Some(error.as_str());
    }

    let _ = writeln!(output, "Send result");
    let _ = writeln!(output, "  Pane: {}", data.pane_id);
    let _ = writeln!(output, "  Status: {status}");
    let _ = writeln!(output, "  Text: {redacted_text}");

    if data.no_paste {
        let _ = writeln!(output, "  Mode: no-paste");
    }
    if data.no_newline {
        let _ = writeln!(output, "  Newline: suppressed");
    }

    if let Some(rule_id) = data.injection.rule_id() {
        let _ = writeln!(output, "  Rule: {rule_id}");
    }
    if let Some(reason) = reason {
        let _ = writeln!(output, "  Reason: {reason}");
    }
    if let Some(approval) = approval {
        let _ = writeln!(output, "  Approval: {}", approval.summary);
        let _ = writeln!(output, "  Command: {}", approval.command);
    }
    if let Some(audit_id) = data.injection.audit_action_id() {
        let _ = writeln!(output, "  Audit ID: {audit_id}");
    }

    if let Some(wait_for) = &data.wait_for {
        let status = if wait_for.matched {
            "matched"
        } else {
            "timed out"
        };
        let regex = if wait_for.is_regex { " (regex)" } else { "" };
        let _ = writeln!(
            output,
            "  Wait-for{regex}: {} ({status}, {} ms, {} polls)",
            wait_for.pattern, wait_for.elapsed_ms, wait_for.polls
        );
    } else if redacted_wait_for.is_some() && !data.injection.is_allowed() {
        let _ = writeln!(output, "  Wait-for: skipped (send not executed)");
    }

    if let Some(err) = data.verification_error.as_deref() {
        let _ = writeln!(output, "  Verification: {err}");
    }

    output
}

fn resolve_workflow(name: &str) -> Option<std::sync::Arc<dyn wa_core::workflows::Workflow>> {
    match name {
        "handle_compaction" => Some(std::sync::Arc::new(
            wa_core::workflows::HandleCompaction::default(),
        )),
        "handle_usage_limits" => Some(std::sync::Arc::new(
            wa_core::workflows::HandleUsageLimits::new(),
        )),
        _ => None,
    }
}

fn infer_workflow_action_type(step_name: &str) -> wa_core::dry_run::ActionType {
    let name = step_name.to_lowercase();
    if name.contains("send") {
        wa_core::dry_run::ActionType::SendText
    } else if name.contains("wait") || name.contains("stabilize") || name.contains("verify") {
        wa_core::dry_run::ActionType::WaitFor
    } else if name.contains("lock") {
        wa_core::dry_run::ActionType::AcquireLock
    } else {
        wa_core::dry_run::ActionType::WorkflowStep
    }
}

fn build_workflow_dry_run_report(
    command_ctx: &wa_core::dry_run::CommandContext,
    name: &str,
    pane: u64,
    pane_info: Option<&wa_core::wezterm::PaneInfo>,
) -> wa_core::dry_run::DryRunReport {
    use wa_core::dry_run::{
        ActionType, PlannedAction, PolicyCheck, PolicyEvaluation, TargetResolution,
    };

    let mut ctx = command_ctx.dry_run_context();

    // Target resolution
    if let Some(info) = pane_info {
        let mut target =
            TargetResolution::new(pane, info.inferred_domain()).with_is_active(info.is_active);
        if let Some(title) = &info.title {
            target = target.with_title(title.clone());
        }
        if let Some(cwd) = &info.cwd {
            target = target.with_cwd(cwd.clone());
        }
        ctx.set_target(target);
    } else {
        ctx.set_target(TargetResolution::new(pane, "unknown"));
        ctx.add_warning("Pane metadata unavailable; verify pane ID and daemon state.");
    }

    // Policy evaluation for workflow
    let mut eval = PolicyEvaluation::new();
    let workflow = resolve_workflow(name);
    match workflow.as_ref() {
        Some(wf) => {
            eval.add_check(PolicyCheck::passed(
                "workflow",
                format!("Workflow '{name}' loaded"),
            ));
            if wf.is_enabled() {
                eval.add_check(PolicyCheck::passed(
                    "workflow_enabled",
                    "Workflow is enabled",
                ));
            } else {
                eval.add_check(PolicyCheck::failed(
                    "workflow_enabled",
                    "Workflow is disabled",
                ));
            }

            if wf.requires_approval() {
                eval.add_check(PolicyCheck::failed(
                    "approval",
                    "Workflow requires approval",
                ));
            } else {
                eval.add_check(PolicyCheck::passed("approval", "No approval required"));
            }

            if wf.is_destructive() {
                eval.add_check(PolicyCheck::failed(
                    "destructive",
                    "Workflow marked destructive",
                ));
            } else {
                eval.add_check(PolicyCheck::passed(
                    "destructive",
                    "Workflow marked non-destructive",
                ));
            }
        }
        None => {
            eval.add_check(PolicyCheck::failed(
                "workflow",
                format!("Workflow '{name}' not found"),
            ));
        }
    }

    if pane_info.is_some() {
        eval.add_check(PolicyCheck::passed("pane", "Pane found"));
    } else {
        eval.add_check(PolicyCheck::failed(
            "pane",
            "Pane not found (dry-run uses best-effort resolution)",
        ));
    }

    eval.add_check(
        PolicyCheck::passed("pane_state", "Pane state not inspected during dry-run")
            .with_details("Verify prompt/alt-screen state before execution."),
    );
    eval.add_check(
        PolicyCheck::passed("policy", "Policy checks deferred to execution")
            .with_details("Send steps remain policy-gated at runtime."),
    );
    ctx.set_policy_evaluation(eval);

    // Expected workflow steps
    if let Some(wf) = workflow.as_ref() {
        let mut step = 1u32;

        ctx.add_action(PlannedAction::new(
            step,
            ActionType::AcquireLock,
            format!("Acquire workflow lock for pane {pane}"),
        ));
        step += 1;

        for workflow_step in wf.steps() {
            let action_type = infer_workflow_action_type(&workflow_step.name);
            let mut description = format!("{}: {}", workflow_step.name, workflow_step.description);
            if action_type == ActionType::SendText {
                description.push_str(" [policy-gated]");
            }
            let mut action = PlannedAction::new(step, action_type, description);
            if action_type == ActionType::SendText {
                action = action.with_metadata(serde_json::json!({
                    "policy_gated": true,
                }));
            }
            ctx.add_action(action);
            step += 1;
        }

        let trigger_event_types = wf.trigger_event_types();
        let trigger_rule_ids = wf.trigger_rule_ids();
        if !trigger_event_types.is_empty() || !trigger_rule_ids.is_empty() {
            let mut details = Vec::new();
            if !trigger_event_types.is_empty() {
                details.push(format!("event types: {}", trigger_event_types.join(", ")));
            }
            if !trigger_rule_ids.is_empty() {
                details.push(format!("rule ids: {}", trigger_rule_ids.join(", ")));
            }
            let suffix = details.join("; ");
            ctx.add_action(PlannedAction::new(
                step,
                ActionType::MarkEventHandled,
                format!("Mark triggering event handled ({suffix})"),
            ));
            step += 1;
        }

        ctx.add_action(PlannedAction::new(
            step,
            ActionType::ReleaseLock,
            "Release workflow lock".to_string(),
        ));
    } else {
        ctx.add_warning("No workflow steps available; check workflow name.");
    }

    ctx.take_report()
}

fn workflow_enabled(workflow: &str, config: &wa_core::config::WorkflowsConfig) -> bool {
    if config.enabled.is_empty() {
        return true;
    }
    config.enabled.iter().any(|name| name == workflow)
}

fn workflow_auto_run(workflow: &str, config: &wa_core::config::WorkflowsConfig) -> bool {
    if config.auto_run_denylist.iter().any(|name| name == workflow) {
        return false;
    }
    if config.auto_run_allowlist.is_empty() {
        return true;
    }
    config
        .auto_run_allowlist
        .iter()
        .any(|name| name == workflow)
}

fn workflow_first_step(name: &str) -> Option<String> {
    use wa_core::workflows::{HandleCompaction, Workflow};

    match name {
        "handle_compaction" => HandleCompaction::new()
            .steps()
            .first()
            .map(|step| step.name.clone()),
        _ => None,
    }
}

fn build_event_would_handle(
    event: &wa_core::storage::StoredEvent,
    rule: Option<&wa_core::patterns::RuleDef>,
    config: &wa_core::config::Config,
) -> Option<RobotEventWouldHandle> {
    let rule = rule?;
    let workflow = rule.workflow.as_ref()?;

    let enabled = workflow_enabled(workflow, &config.workflows);
    let auto_run = workflow_auto_run(workflow, &config.workflows);
    let already_handled = event.handled_at.is_some();
    let would_run = enabled && auto_run && !already_handled;

    let reason = if already_handled {
        Some("event already handled".to_string())
    } else if !enabled {
        Some("workflow disabled by config".to_string())
    } else if !auto_run {
        Some("workflow not in auto-run allowlist".to_string())
    } else {
        None
    };

    Some(RobotEventWouldHandle {
        workflow: workflow.clone(),
        preview_command: rule.get_preview_command(event.pane_id, Some(event.id)),
        first_step: workflow_first_step(workflow),
        estimated_duration_ms: None,
        would_run: Some(would_run),
        reason,
    })
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
                description: "Fetch recent events (optional workflow preview)",
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
        global_flags: vec![
            "--workspace <path>",
            "--config <path>",
            "--verbose",
            "--format <json|toon>",
            "--stats",
        ],
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
            QuickStartGlobalFlag {
                flag: "--format <json|toon>",
                env_var: Some("WA_OUTPUT_FORMAT"),
                description: "Robot stdout format (default: json); also consults TOON_DEFAULT_FORMAT",
            },
            QuickStartGlobalFlag {
                flag: "--stats",
                env_var: Some("TOON_STATS"),
                description: "Emit token comparison stats to stderr (JSON vs TOON)",
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
                args: "[--limit N] [--pane ID] [--rule-id ID] [--event-type TYPE] [--unhandled-only] [--would-handle] [--dry-run]",
                summary: "Fetch recent pattern detection events (with optional workflow preview)",
                examples: vec![
                    "wa robot events",
                    "wa robot events --rule-id codex.usage_reached",
                    "wa robot events --would-handle --dry-run",
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
            "Use --format toon for compact output when piping robot results between agents",
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
        .map_or(0, |dur| u64::try_from(dur.as_millis()).unwrap_or(u64::MAX))
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

    fancy_regex::Regex::new(&regex_pattern).is_ok_and(|re| re.is_match(value).unwrap_or(false))
}

fn is_structured_uservar_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("wa-") || lower.starts_with("wa_") || lower == "wa_event"
}

fn validate_uservar_request(pane_id: u64, name: &str, value: &str) -> Result<(), String> {
    // Maximum message size for IPC (128KB) - mirrors wa_core::ipc::MAX_MESSAGE_SIZE
    const MAX_MESSAGE_SIZE: usize = 131_072;

    if name.trim().is_empty() {
        return Err("user-var name cannot be empty".to_string());
    }
    if value.is_empty() {
        return Err("user-var value cannot be empty".to_string());
    }

    // Estimate request size (JSON overhead is ~50 bytes for field names + pane_id)
    // Format: {"type":"user_var","pane_id":N,"name":"...","value":"..."}
    let request_size = 50 + pane_id.to_string().len() + name.len() + value.len();
    if request_size > MAX_MESSAGE_SIZE {
        return Err(format!(
            "user-var payload too large: {request_size} bytes (max {MAX_MESSAGE_SIZE})"
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
    use wa_core::events::EventBus;
    use wa_core::lock::WatcherLock;
    use wa_core::patterns::PatternEngine;
    use wa_core::policy::{PolicyEngine, PolicyGatedInjector};
    use wa_core::runtime::{ObservationRuntime, RuntimeConfig};
    use wa_core::storage::StorageHandle;
    use wa_core::workflows::{
        HandleCompaction, PaneWorkflowLockManager, WorkflowEngine, WorkflowRunner,
        WorkflowRunnerConfig,
    };

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
    let pattern_engine = Arc::new(if no_patterns {
        tracing::info!("Pattern detection: disabled");
        PatternEngine::default()
    } else {
        tracing::info!("Pattern detection: enabled with builtin packs");
        PatternEngine::new()
    });

    // Create event bus for publishing detections to workflow runners
    let event_bus = Arc::new(EventBus::new(1000));

    // Set up workflow runner if auto_handle is enabled
    let _workflow_runner_handle = if auto_handle {
        // Create shared storage for workflow runner (recreate config since it doesn't impl Clone)
        let workflow_storage_config = wa_core::storage::StorageConfig {
            write_queue_size: config.storage.writer_queue_size as usize,
        };
        let storage_for_workflows =
            Arc::new(StorageHandle::with_config(&db_path, workflow_storage_config).await?);

        // Create policy engine (permissive defaults for auto-handling)
        let policy_engine = PolicyEngine::permissive();
        let wezterm_client = wa_core::wezterm::WeztermClient::new();
        let injector = Arc::new(tokio::sync::Mutex::new(PolicyGatedInjector::with_storage(
            policy_engine,
            wezterm_client,
            storage_for_workflows.as_ref().clone(),
        )));

        // Create workflow engine and lock manager
        let workflow_engine = WorkflowEngine::new(config.workflows.max_concurrent as usize);
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());

        // Create workflow runner
        let runner_config = WorkflowRunnerConfig {
            max_concurrent: config.workflows.max_concurrent as usize,
            step_timeout_ms: config.workflows.default_step_timeout_ms,
            ..Default::default()
        };
        let workflow_runner = WorkflowRunner::new(
            workflow_engine,
            lock_manager,
            storage_for_workflows,
            injector,
            runner_config,
        );

        // Register built-in workflows
        workflow_runner.register_workflow(Arc::new(HandleCompaction::default()));
        workflow_runner.register_workflow(Arc::new(wa_core::workflows::HandleUsageLimits::new()));
        tracing::info!("Registered workflows: handle_compaction, handle_usage_limits");

        // Spawn workflow runner event loop
        let event_bus_clone = Arc::clone(&event_bus);
        let runner_handle = tokio::spawn(async move {
            tracing::info!("Workflow runner started, listening for detection events");
            workflow_runner.run(&event_bus_clone).await;
            tracing::info!("Workflow runner stopped");
        });

        Some(runner_handle)
    } else {
        None
    };

    // Configure the runtime
    let runtime_config = RuntimeConfig {
        discovery_interval: Duration::from_millis(poll_interval),
        capture_interval: Duration::from_millis(config.ingest.poll_interval_ms),
        min_capture_interval: Duration::from_millis(config.ingest.min_poll_interval_ms),
        overlap_size: 4096, // Default overlap window size
        pane_filter: config.ingest.panes.clone(),
        channel_buffer: 1024,
        max_concurrent_captures: config.ingest.max_concurrent_captures as usize,
        retention_days: config.storage.retention_days,
        retention_max_mb: config.storage.retention_max_mb,
        checkpoint_interval_secs: config.storage.checkpoint_interval_secs,
    };

    // Create and start the observation runtime (with event bus for workflow integration)
    let mut runtime =
        ObservationRuntime::new(runtime_config, storage, pattern_engine).with_event_bus(event_bus);
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
    let robot_mode = sniff_robot_mode_from_args();
    if let Err(err) = Box::pin(run(robot_mode)).await {
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
                let format = resolve_robot_output_format(sniff_robot_output_format_from_args());
                match err.kind() {
                    clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayVersion => {
                        let response = RobotResponse::success(build_robot_help(), elapsed);
                        print_robot_response(&response, format, false)?;
                    }
                    clap::error::ErrorKind::InvalidSubcommand => {
                        let response = RobotResponse::<()>::error_with_code(
                            ROBOT_ERR_UNKNOWN_SUBCOMMAND,
                            "Unknown robot subcommand",
                            Some("Use `wa robot help` for available commands.".to_string()),
                            elapsed,
                        );
                        print_robot_response(&response, format, false)?;
                    }
                    _ => {
                        let response = RobotResponse::<()>::error_with_code(
                            ROBOT_ERR_INVALID_ARGS,
                            "Invalid robot arguments",
                            Some("Use `wa robot help` for usage.".to_string()),
                            elapsed,
                        );
                        print_robot_response(&response, format, false)?;
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
                let (format, stats) = match command.as_ref() {
                    Some(Commands::Robot { format, stats, .. }) => {
                        (resolve_robot_output_format(*format), *stats)
                    }
                    _ => (
                        resolve_robot_output_format(sniff_robot_output_format_from_args()),
                        false,
                    ),
                };
                let response = RobotResponse::<()>::error_with_code(
                    ROBOT_ERR_CONFIG,
                    format!("Failed to load config: {err}"),
                    Some("Check --config/--workspace or WA_WORKSPACE.".to_string()),
                    elapsed_ms(start),
                );
                print_robot_response(&response, format, stats)?;
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

        Some(Commands::Robot {
            format,
            stats,
            command,
        }) => {
            let start = std::time::Instant::now();
            let command = command.unwrap_or(RobotCommands::QuickStart);
            let format = resolve_robot_output_format(format);

            match command {
                RobotCommands::Help => {
                    let response = RobotResponse::success(build_robot_help(), elapsed_ms(start));
                    print_robot_response(&response, format, stats)?;
                }
                RobotCommands::QuickStart => {
                    let response =
                        RobotResponse::success(build_robot_quick_start(), elapsed_ms(start));
                    print_robot_response(&response, format, stats)?;
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
                            print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
                                }
                                Err(e) => {
                                    let response = RobotResponse::<Vec<PaneState>>::error_with_code(
                                        "robot.wezterm_error",
                                        format!("Failed to list panes: {e}"),
                                        Some("Is WezTerm running?".to_string()),
                                        elapsed_ms(start),
                                    );
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                let storage = wa_core::storage::StorageHandle::new(
                                    &ctx.effective.paths.db_path,
                                )
                                .await
                                .ok();
                                let ipc_socket = Path::new(&ctx.effective.paths.ipc_socket_path);
                                let resolution = resolve_pane_capabilities(
                                    pane_id,
                                    storage.as_ref(),
                                    Some(ipc_socket),
                                )
                                .await;
                                let mut report = build_send_dry_run_report(
                                    &command_ctx,
                                    pane_id,
                                    pane_info.as_ref(),
                                    Some(&resolution.capabilities),
                                    &text,
                                    false,
                                    wait_for.as_deref(),
                                    timeout_secs,
                                    &config,
                                );
                                report.warnings.extend(resolution.warnings);
                                let response =
                                    RobotResponse::success(report.redacted(), elapsed_ms(start));
                                print_robot_response(&response, format, stats)?;
                            } else {
                                use wa_core::approval::ApprovalStore;
                                use wa_core::policy::{
                                    ActionKind, ActorKind, InjectionResult, PolicyDecision,
                                    PolicyEngine, PolicyInput,
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
                                        print_robot_response(&response, format, stats)?;
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
                                        print_robot_response(&response, format, stats)?;
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

                                let ipc_socket = Path::new(&ctx.effective.paths.ipc_socket_path);
                                let resolution = resolve_pane_capabilities(
                                    pane_id,
                                    Some(&storage),
                                    Some(ipc_socket),
                                )
                                .await;
                                let capabilities = resolution.capabilities;

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
                                            print_robot_response(&response, format, stats)?;
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
                                                audit_action_id: None,
                                            },
                                            Err(e) => InjectionResult::Error {
                                                error: e.to_string(),
                                                pane_id,
                                                action: ActionKind::SendText,
                                                audit_action_id: None,
                                            },
                                        }
                                    }
                                    PolicyDecision::Deny { .. } => InjectionResult::Denied {
                                        decision,
                                        summary: summary.clone(),
                                        pane_id,
                                        action: ActionKind::SendText,
                                        audit_action_id: None,
                                    },
                                    PolicyDecision::RequireApproval { .. } => {
                                        InjectionResult::RequiresApproval {
                                            decision,
                                            summary: summary.clone(),
                                            pane_id,
                                            action: ActionKind::SendText,
                                            audit_action_id: None,
                                        }
                                    }
                                };

                                let mut audit_record = injection.to_audit_record(
                                    ActorKind::Robot,
                                    None,
                                    Some(domain.clone()),
                                );
                                audit_record.input_summary =
                                    Some(wa_core::policy::build_send_text_audit_summary(
                                        &text, None, None,
                                    ));
                                if let Err(e) =
                                    storage.record_audit_action_redacted(audit_record).await
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
                                print_robot_response(&response, format, stats)?;
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
                                        print_robot_response(&response, format, stats)?;
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
                                        print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
                                }
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotWaitForData>::error_with_code(
                                            "WA-ROBOT-GET-TEXT-FAILED",
                                            format!("Failed to get pane text: {e}"),
                                            None,
                                            elapsed_ms(start),
                                        );
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                            would_handle,
                            dry_run,
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    let include_preview = would_handle || dry_run;
                                    let rule_index = if include_preview {
                                        let engine = wa_core::patterns::PatternEngine::new();
                                        let mut index = std::collections::HashMap::new();
                                        for rule in engine.rules() {
                                            index.insert(rule.id.clone(), rule.clone());
                                        }
                                        Some(index)
                                    } else {
                                        None
                                    };
                                    let total_count = events.len();
                                    let items: Vec<RobotEventItem> = events
                                        .into_iter()
                                        .map(|e| {
                                            // Derive pack_id from rule_id (e.g., "codex.usage.reached" -> "builtin:codex")
                                            let pack_id = e.rule_id.split('.').next().map_or_else(
                                                || "builtin:unknown".to_string(),
                                                |agent| format!("builtin:{agent}"),
                                            );
                                            let preview = if include_preview {
                                                let rule = rule_index
                                                    .as_ref()
                                                    .and_then(|index| index.get(&e.rule_id));
                                                build_event_would_handle(&e, rule, &config)
                                            } else {
                                                None
                                            };
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
                                                would_handle_with: preview,
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
                                        would_handle: include_preview,
                                        dry_run,
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    print_robot_response(&response, format, stats)?;
                                }
                                Err(e) => {
                                    let response =
                                        RobotResponse::<RobotEventsData>::error_with_code(
                                            ROBOT_ERR_STORAGE,
                                            format!("Failed to query events: {e}"),
                                            None,
                                            elapsed_ms(start),
                                        );
                                    print_robot_response(&response, format, stats)?;
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
                                        let pane_info = wa_core::wezterm::WeztermClient::new()
                                            .get_pane(pane_id)
                                            .await
                                            .ok();
                                        let report = build_workflow_dry_run_report(
                                            &command_ctx,
                                            &name,
                                            pane_id,
                                            pane_info.as_ref(),
                                        );
                                        let response = RobotResponse::success(
                                            report.redacted(),
                                            elapsed_ms(start),
                                        );
                                        print_robot_response(&response, format, stats)?;
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
                                                print_robot_response(&response, format, stats)?;
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
                                            print_robot_response(&response, format, stats)?;
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
                                            print_robot_response(&response, format, stats)?;
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
                                        PolicyGatedInjector::with_storage(
                                            policy_engine,
                                            wezterm_client,
                                            storage.as_ref().clone(),
                                        ),
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
                                        print_robot_response(&response, format, stats)?;
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
                                        print_robot_response(&response, format, stats)?;
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
                                            name: "handle_usage_limits".to_string(),
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
                                    print_robot_response(&response, format, stats)?;
                                }
                                RobotWorkflowCommands::Status {
                                    execution_id,
                                    pane,
                                    active,
                                    verbose,
                                } => {
                                    // Get workspace layout for DB path
                                    let layout =
                                        match config.workspace_layout(Some(&workspace_root)) {
                                            Ok(l) => l,
                                            Err(e) => {
                                                let response = RobotResponse::<
                                                    RobotWorkflowStatusData,
                                                >::error_with_code(
                                                    ROBOT_ERR_CONFIG,
                                                    format!("Failed to get workspace layout: {e}"),
                                                    None,
                                                    elapsed_ms(start),
                                                );
                                                print_robot_response(&response, format, stats)?;
                                                return Ok(());
                                            }
                                        };

                                    // Open storage handle
                                    let db_path = layout.db_path.to_string_lossy();
                                    let storage = match wa_core::storage::StorageHandle::new(
                                        &db_path,
                                    )
                                    .await
                                    {
                                        Ok(s) => s,
                                        Err(e) => {
                                            let response = RobotResponse::<
                                                    RobotWorkflowStatusData,
                                                >::error_with_code(
                                                    ROBOT_ERR_STORAGE,
                                                    format!("Failed to open storage: {e}"),
                                                    Some(
                                                        "Is the database initialized? Run 'wa watch' first."
                                                            .to_string(),
                                                    ),
                                                    elapsed_ms(start),
                                                );
                                            print_robot_response(&response, format, stats)?;
                                            return Ok(());
                                        }
                                    };

                                    // Validate arguments
                                    if execution_id.is_none() && !active && pane.is_none() {
                                        let response = RobotResponse::<
                                            RobotWorkflowStatusData,
                                        >::error_with_code(
                                            "E_MISSING_ARGUMENT",
                                            "Must provide execution_id, --pane, or --active"
                                                .to_string(),
                                            Some(
                                                "Specify an execution ID, or use --pane <id> to list workflows for a pane, or --active to list all active workflows."
                                                    .to_string(),
                                            ),
                                            elapsed_ms(start),
                                        );
                                        print_robot_response(&response, format, stats)?;
                                        return Ok(());
                                    }

                                    // Query by execution_id if provided
                                    if let Some(exec_id) = &execution_id {
                                        match storage.get_workflow(exec_id).await {
                                            Ok(Some(record)) => {
                                                // Get step logs if verbose
                                                let step_logs = if verbose {
                                                    match storage.get_step_logs(exec_id).await {
                                                        Ok(logs) => Some(
                                                            logs.into_iter()
                                                                .map(|log| RobotWorkflowStepLog {
                                                                    step_index: log.step_index,
                                                                    step_name: log.step_name,
                                                                    result_type: log.result_type,
                                                                    step_id: log.step_id,
                                                                    step_kind: log.step_kind,
                                                                    result_data: log
                                                                        .result_data
                                                                        .and_then(|s| {
                                                                            serde_json::from_str(&s)
                                                                                .ok()
                                                                        }),
                                                                    policy_summary: log
                                                                        .policy_summary
                                                                        .and_then(|s| {
                                                                            serde_json::from_str(&s)
                                                                                .ok()
                                                                        }),
                                                                    verification_refs: log
                                                                        .verification_refs
                                                                        .and_then(|s| {
                                                                            serde_json::from_str(&s)
                                                                                .ok()
                                                                        }),
                                                                    error_code: log.error_code,
                                                                    started_at: log.started_at,
                                                                    completed_at: Some(
                                                                        log.completed_at,
                                                                    ),
                                                                    duration_ms: Some(
                                                                        log.duration_ms,
                                                                    ),
                                                                })
                                                                .collect(),
                                                        ),
                                                        Err(_) => None,
                                                    }
                                                } else {
                                                    None
                                                };
                                                let action_plan = if verbose {
                                                    match storage.get_action_plan(exec_id).await {
                                                        Ok(Some(record)) => {
                                                            Some(RobotWorkflowActionPlan {
                                                                plan_id: record.plan_id,
                                                                plan_hash: record.plan_hash,
                                                                plan: serde_json::from_str(
                                                                    &record.plan_json,
                                                                )
                                                                .ok(),
                                                                created_at: Some(record.created_at),
                                                            })
                                                        }
                                                        _ => None,
                                                    }
                                                } else {
                                                    None
                                                };

                                                // Calculate elapsed_ms
                                                let now = std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .unwrap_or_default()
                                                    .as_millis()
                                                    as i64;
                                                let elapsed = if record.completed_at.is_some() {
                                                    record
                                                        .completed_at
                                                        .map(|c| (c - record.started_at) as u64)
                                                } else {
                                                    Some((now - record.started_at) as u64)
                                                };

                                                let data = RobotWorkflowStatusData {
                                                    execution_id: record.id,
                                                    workflow_name: record.workflow_name,
                                                    pane_id: Some(record.pane_id),
                                                    trigger_event_id: record.trigger_event_id,
                                                    status: record.status,
                                                    step_name: None, // Would need workflow definition to get step names
                                                    elapsed_ms: elapsed,
                                                    last_step_result: None, // Would need step logs to derive
                                                    current_step: Some(record.current_step),
                                                    total_steps: None, // Would need workflow definition
                                                    wait_condition: record.wait_condition,
                                                    context: record.context,
                                                    result: record.result,
                                                    error: record.error,
                                                    started_at: Some(record.started_at as u64),
                                                    updated_at: Some(record.updated_at as u64),
                                                    completed_at: record
                                                        .completed_at
                                                        .map(|c| c as u64),
                                                    step_logs,
                                                    action_plan,
                                                };

                                                let response =
                                                    RobotResponse::success(data, elapsed_ms(start));
                                                print_robot_response(&response, format, stats)?;
                                            }
                                            Ok(None) => {
                                                let response = RobotResponse::<
                                                    RobotWorkflowStatusData,
                                                >::error_with_code(
                                                    "E_EXECUTION_NOT_FOUND",
                                                    format!(
                                                        "No workflow execution found with ID: {exec_id}"
                                                    ),
                                                    Some(
                                                        "Check the execution ID is correct. Use 'wa robot workflow status --active' to list running workflows."
                                                            .to_string(),
                                                    ),
                                                    elapsed_ms(start),
                                                );
                                                print_robot_response(&response, format, stats)?;
                                            }
                                            Err(e) => {
                                                let response = RobotResponse::<
                                                    RobotWorkflowStatusData,
                                                >::error_with_code(
                                                    ROBOT_ERR_STORAGE,
                                                    format!("Failed to query workflow: {e}"),
                                                    None,
                                                    elapsed_ms(start),
                                                );
                                                print_robot_response(&response, format, stats)?;
                                            }
                                        }
                                    } else {
                                        // Query active/by-pane workflows
                                        let records = if active {
                                            storage.find_incomplete_workflows().await
                                        } else {
                                            // Note: pane filter would require a custom query
                                            // For now, filter in-memory after getting incomplete workflows
                                            storage.find_incomplete_workflows().await
                                        };

                                        match records {
                                            Ok(mut workflows) => {
                                                // Filter by pane if specified
                                                if let Some(pane_id) = pane {
                                                    workflows.retain(|w| w.pane_id == pane_id);
                                                }

                                                let executions: Vec<RobotWorkflowStatusData> =
                                                    workflows
                                                        .into_iter()
                                                        .map(|record| {
                                                            let now = std::time::SystemTime::now()
                                                                .duration_since(
                                                                    std::time::UNIX_EPOCH,
                                                                )
                                                                .unwrap_or_default()
                                                                .as_millis()
                                                                as i64;
                                                            let elapsed =
                                                                if record.completed_at.is_some() {
                                                                    record.completed_at.map(|c| {
                                                                        (c - record.started_at)
                                                                            as u64
                                                                    })
                                                                } else {
                                                                    Some(
                                                                        (now - record.started_at)
                                                                            as u64,
                                                                    )
                                                                };

                                                            RobotWorkflowStatusData {
                                                                execution_id: record.id,
                                                                workflow_name: record.workflow_name,
                                                                pane_id: Some(record.pane_id),
                                                                trigger_event_id: record
                                                                    .trigger_event_id,
                                                                status: record.status,
                                                                step_name: None,
                                                                elapsed_ms: elapsed,
                                                                last_step_result: None,
                                                                current_step: Some(
                                                                    record.current_step,
                                                                ),
                                                                total_steps: None,
                                                                wait_condition: record
                                                                    .wait_condition,
                                                                context: record.context,
                                                                result: record.result,
                                                                error: record.error,
                                                                started_at: Some(
                                                                    record.started_at as u64,
                                                                ),
                                                                updated_at: Some(
                                                                    record.updated_at as u64,
                                                                ),
                                                                completed_at: record
                                                                    .completed_at
                                                                    .map(|c| c as u64),
                                                                step_logs: None, // Not included in list mode
                                                                action_plan: None,
                                                            }
                                                        })
                                                        .collect();

                                                let count = executions.len();
                                                let data = RobotWorkflowStatusListData {
                                                    executions,
                                                    pane_filter: pane,
                                                    active_only: if active {
                                                        Some(true)
                                                    } else {
                                                        None
                                                    },
                                                    count,
                                                };

                                                let response =
                                                    RobotResponse::success(data, elapsed_ms(start));
                                                print_robot_response(&response, format, stats)?;
                                            }
                                            Err(e) => {
                                                let response = RobotResponse::<
                                                    RobotWorkflowStatusListData,
                                                >::error_with_code(
                                                    ROBOT_ERR_STORAGE,
                                                    format!("Failed to query workflows: {e}"),
                                                    None,
                                                    elapsed_ms(start),
                                                );
                                                print_robot_response(&response, format, stats)?;
                                            }
                                        }
                                    }

                                    // Clean shutdown of storage
                                    if let Err(e) = storage.shutdown().await {
                                        tracing::warn!("Failed to shutdown storage cleanly: {e}");
                                    }
                                }
                                RobotWorkflowCommands::Abort {
                                    execution_id,
                                    reason,
                                } => {
                                    use wa_core::policy::{PolicyEngine, PolicyGatedInjector};
                                    use wa_core::storage::StorageHandle;
                                    use wa_core::workflows::{
                                        PaneWorkflowLockManager, WorkflowEngine, WorkflowRunner,
                                        WorkflowRunnerConfig,
                                    };

                                    // Set up storage
                                    let db_path = &ctx.effective.paths.db_path;
                                    let storage = match StorageHandle::new(db_path).await {
                                        Ok(s) => Arc::new(s),
                                        Err(e) => {
                                            let response =
                                                RobotResponse::<RobotWorkflowAbortData>::error_with_code(
                                                    ROBOT_ERR_STORAGE,
                                                    format!("Failed to open storage: {e}"),
                                                    Some(
                                                        "Check database path and permissions."
                                                            .to_string(),
                                                    ),
                                                    elapsed_ms(start),
                                                );
                                            print_robot_response(&response, format, stats)?;
                                            return Ok(());
                                        }
                                    };

                                    // Set up minimal workflow infrastructure for abort
                                    let engine = WorkflowEngine::new(10);
                                    let lock_manager = Arc::new(PaneWorkflowLockManager::new());
                                    let policy_engine = PolicyEngine::new(
                                        config.safety.rate_limit_per_pane,
                                        config.safety.rate_limit_global,
                                        false,
                                    );
                                    let wezterm_client = wa_core::wezterm::WeztermClient::new();
                                    let injector = Arc::new(tokio::sync::Mutex::new(
                                        PolicyGatedInjector::with_storage(
                                            policy_engine,
                                            wezterm_client,
                                            storage.as_ref().clone(),
                                        ),
                                    ));
                                    let runner_config = WorkflowRunnerConfig::default();
                                    let runner = WorkflowRunner::new(
                                        engine,
                                        lock_manager,
                                        Arc::clone(&storage),
                                        injector,
                                        runner_config,
                                    );

                                    // Execute the abort
                                    match runner
                                        .abort_execution(
                                            &execution_id,
                                            reason.as_deref(),
                                            false, // force
                                        )
                                        .await
                                    {
                                        Ok(result) => {
                                            let data = RobotWorkflowAbortData {
                                                execution_id: result.execution_id,
                                                aborted: result.aborted,
                                                workflow_name: Some(result.workflow_name),
                                                previous_status: Some(result.previous_status),
                                                reason: result.reason,
                                                aborted_at: result.aborted_at,
                                                error_reason: result.error_reason,
                                            };
                                            let response =
                                                RobotResponse::success(data, elapsed_ms(start));
                                            print_robot_response(&response, format, stats)?;
                                        }
                                        Err(e) => {
                                            let (code, hint) = match &e {
                                                wa_core::Error::Workflow(
                                                    wa_core::error::WorkflowError::NotFound(_),
                                                ) => (
                                                    "E_EXECUTION_NOT_FOUND",
                                                    Some(
                                                        "Check the execution ID is correct. Use \
                                                         'wa robot workflow status --active' to \
                                                         list running workflows."
                                                            .to_string(),
                                                    ),
                                                ),
                                                _ => (ROBOT_ERR_STORAGE, None),
                                            };
                                            let response =
                                                RobotResponse::<RobotWorkflowAbortData>::error_with_code(
                                                    code,
                                                    format!("Failed to abort workflow: {e}"),
                                                    hint,
                                                    elapsed_ms(start),
                                                );
                                            print_robot_response(&response, format, stats)?;
                                        }
                                    }

                                    // Clean shutdown of storage
                                    if let Err(e) = storage.shutdown().await {
                                        tracing::warn!("Failed to shutdown storage cleanly: {e}");
                                    }
                                }
                            }
                        }
                        RobotCommands::Rules { command } => {
                            use wa_core::patterns::{AgentType, PatternEngine};

                            let engine = PatternEngine::new();

                            match command {
                                RobotRulesCommands::List {
                                    pack,
                                    agent_type,
                                    verbose,
                                } => {
                                    // Parse agent type filter if provided
                                    let agent_filter: Option<AgentType> =
                                        agent_type.as_ref().and_then(|s| match s.as_str() {
                                            "codex" => Some(AgentType::Codex),
                                            "claude_code" => Some(AgentType::ClaudeCode),
                                            "gemini" => Some(AgentType::Gemini),
                                            "wezterm" => Some(AgentType::Wezterm),
                                            _ => None,
                                        });

                                    let rules: Vec<RobotRuleItem> = engine
                                        .rules()
                                        .iter()
                                        .filter(|rule| {
                                            // Filter by pack if specified
                                            if let Some(ref pack_filter) = pack {
                                                // Rules don't store pack directly, but we can derive from id prefix
                                                // For now, show all rules (pack filtering would need library access)
                                                let _ = pack_filter;
                                            }
                                            // Filter by agent type if specified
                                            if let Some(ref agent) = agent_filter {
                                                if rule.agent_type != *agent {
                                                    return false;
                                                }
                                            }
                                            true
                                        })
                                        .map(|rule| RobotRuleItem {
                                            id: rule.id.clone(),
                                            agent_type: format!("{}", rule.agent_type),
                                            event_type: rule.event_type.clone(),
                                            severity: format!("{:?}", rule.severity).to_lowercase(),
                                            description: if verbose {
                                                Some(rule.description.clone())
                                            } else {
                                                None
                                            },
                                            workflow: rule.workflow.clone(),
                                            anchor_count: rule.anchors.len(),
                                            has_regex: rule.regex.is_some(),
                                        })
                                        .collect();

                                    let data = RobotRulesListData {
                                        rules,
                                        pack_filter: pack,
                                        agent_type_filter: agent_type,
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    print_robot_response(&response, format, stats)?;
                                }
                                RobotRulesCommands::Test { text, trace, pack } => {
                                    let _ = pack; // Pack filtering not implemented yet

                                    // Run detection on the provided text
                                    let detections = engine.detect(&text);

                                    let matches: Vec<RobotRuleMatchItem> = detections
                                        .iter()
                                        .map(|d| RobotRuleMatchItem {
                                            rule_id: d.rule_id.clone(),
                                            agent_type: format!("{}", d.agent_type),
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
                                            trace: if trace {
                                                // Trace output would include match evidence
                                                // For now, provide basic info
                                                Some(RobotRuleTraceInfo {
                                                    anchors_checked: true,
                                                    regex_matched: !d.matched_text.is_empty(),
                                                })
                                            } else {
                                                None
                                            },
                                        })
                                        .collect();

                                    let data = RobotRulesTestData {
                                        text_length: text.len(),
                                        match_count: matches.len(),
                                        matches,
                                    };
                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    print_robot_response(&response, format, stats)?;
                                }
                                RobotRulesCommands::Show { rule_id } => {
                                    // Find the specific rule
                                    if let Some(rule) =
                                        engine.rules().iter().find(|r| r.id == rule_id)
                                    {
                                        let data = RobotRuleDetailData {
                                            id: rule.id.clone(),
                                            agent_type: format!("{}", rule.agent_type),
                                            event_type: rule.event_type.clone(),
                                            severity: format!("{:?}", rule.severity).to_lowercase(),
                                            description: rule.description.clone(),
                                            anchors: rule.anchors.clone(),
                                            regex: rule.regex.clone(),
                                            workflow: rule.workflow.clone(),
                                            remediation: rule.remediation.clone(),
                                            manual_fix: rule.manual_fix.clone(),
                                            learn_more_url: rule.learn_more_url.clone(),
                                        };
                                        let response =
                                            RobotResponse::success(data, elapsed_ms(start));
                                        print_robot_response(&response, format, stats)?;
                                    } else {
                                        let response =
                                            RobotResponse::<RobotRuleDetailData>::error_with_code(
                                                "robot.rule_not_found",
                                                format!("Rule '{}' not found", rule_id),
                                                Some("Use 'wa robot rules list' to see available rules".to_string()),
                                                elapsed_ms(start),
                                            );
                                        print_robot_response(&response, format, stats)?;
                                    }
                                }
                                RobotRulesCommands::Lint {
                                    pack,
                                    fixtures,
                                    skip_regex_check,
                                    strict,
                                } => {
                                    let _ = pack; // Pack filtering not yet implemented

                                    let rules = engine.rules();
                                    let mut errors: Vec<RobotLintIssue> = Vec::new();
                                    let mut warnings: Vec<RobotLintIssue> = Vec::new();

                                    // 1. Validate rule ID naming conventions
                                    for rule in rules {
                                        let has_valid_prefix =
                                            RULE_ID_PREFIXES.iter().any(|p| rule.id.starts_with(p));
                                        if !has_valid_prefix {
                                            errors.push(RobotLintIssue {
                                                rule_id: rule.id.clone(),
                                                category: "naming".to_string(),
                                                message: format!(
                                                    "Rule ID must start with one of: {}",
                                                    RULE_ID_PREFIXES.join(", ")
                                                ),
                                                suggestion: Some(format!(
                                                    "Rename to '{}.{}'",
                                                    rule.agent_type,
                                                    rule.id
                                                        .split('.')
                                                        .skip(1)
                                                        .collect::<Vec<_>>()
                                                        .join(".")
                                                )),
                                            });
                                        }

                                        // Check ID matches agent_type
                                        let expected_prefix = format!("{}.", rule.agent_type);
                                        if !rule.id.starts_with(&expected_prefix) {
                                            errors.push(RobotLintIssue {
                                                rule_id: rule.id.clone(),
                                                category: "naming".to_string(),
                                                message: format!(
                                                    "Rule ID prefix '{}' does not match agent_type '{}'",
                                                    rule.id.split('.').next().unwrap_or(""),
                                                    rule.agent_type
                                                ),
                                                suggestion: Some(format!(
                                                    "Use '{}{}'",
                                                    expected_prefix,
                                                    rule.id.split('.').skip(1).collect::<Vec<_>>().join(".")
                                                )),
                                            });
                                        }

                                        // 2. Validate regex patterns (if not skipped)
                                        if !skip_regex_check {
                                            if let Some(ref regex_str) = rule.regex {
                                                // Check for dangerous patterns
                                                if regex_str.contains(".*.*.*") {
                                                    warnings.push(RobotLintIssue {
                                                        rule_id: rule.id.clone(),
                                                        category: "regex".to_string(),
                                                        message: "Regex contains nested wildcards (potential ReDoS)".to_string(),
                                                        suggestion: Some("Consider using non-greedy quantifiers or simplifying pattern".to_string()),
                                                    });
                                                }
                                                // Check for excessive length
                                                if regex_str.len() > 500 {
                                                    warnings.push(RobotLintIssue {
                                                        rule_id: rule.id.clone(),
                                                        category: "regex".to_string(),
                                                        message: format!("Regex is {} chars (consider splitting)", regex_str.len()),
                                                        suggestion: Some("Break into multiple rules or simplify the pattern".to_string()),
                                                    });
                                                }
                                                // Check for unescaped special chars that might be mistakes
                                                if regex_str.contains("  ") {
                                                    warnings.push(RobotLintIssue {
                                                        rule_id: rule.id.clone(),
                                                        category: "regex".to_string(),
                                                        message: "Regex contains consecutive spaces (intentional?)".to_string(),
                                                        suggestion: Some("Use \\s+ for flexible whitespace matching".to_string()),
                                                    });
                                                }
                                            }
                                        }
                                    }

                                    // 3. Check fixture coverage if requested
                                    let fixture_coverage = if fixtures {
                                        let corpus_base =
                                            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                                                .parent()
                                                .unwrap_or_else(|| std::path::Path::new("."))
                                                .join("wa-core")
                                                .join("tests")
                                                .join("corpus");

                                        let mut rules_with_fixtures: HashSet<String> =
                                            HashSet::new();
                                        let mut total_fixtures = 0;

                                        collect_fixture_rule_ids(
                                            &corpus_base,
                                            &mut rules_with_fixtures,
                                            &mut total_fixtures,
                                        );

                                        // Find rules without any fixture coverage
                                        let rules_without: Vec<String> = rules
                                            .iter()
                                            .filter(|r| !rules_with_fixtures.contains(&r.id))
                                            .map(|r| r.id.clone())
                                            .collect();

                                        // Add warnings for rules without fixtures
                                        for rule_id in &rules_without {
                                            warnings.push(RobotLintIssue {
                                                rule_id: rule_id.clone(),
                                                category: "fixture".to_string(),
                                                message: "No corpus fixture found for this rule".to_string(),
                                                suggestion: Some(format!(
                                                    "Add tests/corpus/<agent>/{}.txt and .expect.json",
                                                    rule_id.split('.').next_back().unwrap_or("unknown")
                                                )),
                                            });
                                        }

                                        Some(RobotFixtureCoverage {
                                            rules_with_fixtures: rules_with_fixtures.len(),
                                            rules_without_fixtures: rules_without,
                                            total_fixtures,
                                        })
                                    } else {
                                        None
                                    };

                                    let passed =
                                        errors.is_empty() && (!strict || warnings.is_empty());

                                    let data = RobotRulesLintData {
                                        total_rules: rules.len(),
                                        rules_checked: rules.len(),
                                        errors,
                                        warnings,
                                        fixture_coverage,
                                        passed,
                                    };

                                    let response = RobotResponse::success(data, elapsed_ms(start));
                                    print_robot_response(&response, format, stats)?;

                                    // Exit non-zero if not passed
                                    if !passed {
                                        std::process::exit(1);
                                    }
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
                                print_robot_response(&response, format, stats)?;
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
                                print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
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
                                    print_robot_response(&response, format, stats)?;
                                }
                                Err(err) => {
                                    let response =
                                        RobotResponse::<RobotApproveData>::error_with_code(
                                            err.code,
                                            err.message,
                                            err.hint,
                                            elapsed_ms(start),
                                        );
                                    print_robot_response(&response, format, stats)?;
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
            no_newline,
            dry_run,
            wait_for,
            timeout_secs,
            wait_for_regex,
        }) => {
            use wa_core::output::{OutputFormat, detect_format};

            let output_format = detect_format();
            let emit_json = matches!(output_format, OutputFormat::Json)
                || (matches!(output_format, OutputFormat::Auto)
                    && !std::io::stdout().is_terminal());
            let redacted_text = redact_for_output(&text);
            let redacted_wait_for = wait_for.as_ref().map(|p| redact_for_output(p));

            let mut command = if dry_run {
                format!("wa send --pane {pane_id} \"{redacted_text}\" --dry-run")
            } else {
                format!("wa send --pane {pane_id} \"{redacted_text}\"")
            };
            if no_paste {
                command.push_str(" --no-paste");
            }
            if no_newline {
                command.push_str(" --no-newline");
            }
            if let Some(pattern) = &redacted_wait_for {
                command.push_str(" --wait-for \"");
                command.push_str(pattern);
                command.push('"');
                if wait_for_regex {
                    command.push_str(" --wait-for-regex");
                }
                command.push_str(&format!(" --timeout-secs {timeout_secs}"));
            }
            let command_ctx = wa_core::dry_run::CommandContext::new(command, dry_run);

            if command_ctx.is_dry_run() {
                let wezterm = wa_core::wezterm::WeztermClient::new();
                let pane_info = wezterm.get_pane(pane_id).await.ok();
                let db_path = layout.db_path.to_string_lossy();
                let storage = wa_core::storage::StorageHandle::new(&db_path).await.ok();
                let resolution = resolve_pane_capabilities(
                    pane_id,
                    storage.as_ref(),
                    Some(layout.ipc_socket_path.as_path()),
                )
                .await;
                let mut report = build_send_dry_run_report(
                    &command_ctx,
                    pane_id,
                    pane_info.as_ref(),
                    Some(&resolution.capabilities),
                    &text,
                    no_paste,
                    wait_for.as_deref(),
                    timeout_secs,
                    &config,
                );
                report.warnings.extend(resolution.warnings);
                if emit_json {
                    println!("{}", serde_json::to_string_pretty(&report.redacted())?);
                } else {
                    println!("{}", wa_core::dry_run::format_human(&report));
                }
            } else {
                tracing::info!(
                    "Sending to pane {} (no_paste={}): {}",
                    pane_id,
                    no_paste,
                    redacted_text
                );
                let emit_error = |message: &str, hint: Option<&str>| {
                    if emit_json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "ok": false,
                                "error": message,
                                "hint": hint,
                                "version": wa_core::VERSION,
                            })
                        );
                    } else {
                        eprintln!("Error: {message}");
                        if let Some(hint) = hint {
                            eprintln!("{hint}");
                        }
                    }
                };

                let db_path = layout.db_path.to_string_lossy();
                let storage = match wa_core::storage::StorageHandle::new(&db_path).await {
                    Ok(s) => s,
                    Err(e) => {
                        emit_error(
                            &format!("Failed to open storage: {e}"),
                            Some("Is the database initialized? Run 'wa watch' first."),
                        );
                        return Ok(());
                    }
                };

                let wezterm = wa_core::wezterm::WeztermClient::new();
                let pane_info = match wezterm.get_pane(pane_id).await {
                    Ok(info) => info,
                    Err(e) => {
                        let (_code, hint) = map_wezterm_error_to_robot(&e);
                        emit_error(&format!("{e}"), hint.as_deref());
                        return Ok(());
                    }
                };
                let domain = pane_info.inferred_domain();

                let mut engine = wa_core::policy::PolicyEngine::new(
                    config.safety.rate_limit_per_pane,
                    config.safety.rate_limit_global,
                    config.safety.require_prompt_active,
                )
                .with_command_gate_config(config.safety.command_gate.clone())
                .with_policy_rules(config.safety.rules.clone());

                let resolution = resolve_pane_capabilities(
                    pane_id,
                    Some(&storage),
                    Some(layout.ipc_socket_path.as_path()),
                )
                .await;
                let capabilities = resolution.capabilities;

                let summary = engine.redact_secrets(&text);
                let input = wa_core::policy::PolicyInput::new(
                    wa_core::policy::ActionKind::SendText,
                    wa_core::policy::ActorKind::Human,
                )
                .with_pane(pane_id)
                .with_domain(domain.clone())
                .with_capabilities(capabilities)
                .with_text_summary(&summary)
                .with_command_text(&text);

                let mut decision = engine.authorize(&input);
                if decision.requires_approval() {
                    let store = wa_core::approval::ApprovalStore::new(
                        &storage,
                        config.safety.approval.clone(),
                        workspace_root.to_string_lossy().to_string(),
                    );
                    decision = match store
                        .attach_to_decision(decision, &input, Some(summary.clone()))
                        .await
                    {
                        Ok(updated) => updated,
                        Err(e) => {
                            emit_error(&format!("Failed to issue approval token: {e}"), None);
                            return Ok(());
                        }
                    };
                }

                let mut injection = match decision {
                    wa_core::policy::PolicyDecision::Allow { .. } => {
                        let send_result = wezterm
                            .send_text_with_options(pane_id, &text, no_paste, no_newline)
                            .await;
                        match send_result {
                            Ok(()) => wa_core::policy::InjectionResult::Allowed {
                                decision,
                                summary: summary.clone(),
                                pane_id,
                                action: wa_core::policy::ActionKind::SendText,
                                audit_action_id: None,
                            },
                            Err(e) => wa_core::policy::InjectionResult::Error {
                                error: e.to_string(),
                                pane_id,
                                action: wa_core::policy::ActionKind::SendText,
                                audit_action_id: None,
                            },
                        }
                    }
                    wa_core::policy::PolicyDecision::Deny { .. } => {
                        wa_core::policy::InjectionResult::Denied {
                            decision,
                            summary: summary.clone(),
                            pane_id,
                            action: wa_core::policy::ActionKind::SendText,
                            audit_action_id: None,
                        }
                    }
                    wa_core::policy::PolicyDecision::RequireApproval { .. } => {
                        wa_core::policy::InjectionResult::RequiresApproval {
                            decision,
                            summary: summary.clone(),
                            pane_id,
                            action: wa_core::policy::ActionKind::SendText,
                            audit_action_id: None,
                        }
                    }
                };

                let mut audit_record = injection.to_audit_record(
                    wa_core::policy::ActorKind::Human,
                    None,
                    Some(domain.clone()),
                );
                audit_record.input_summary = Some(wa_core::policy::build_send_text_audit_summary(
                    &text, None, None,
                ));
                match storage.record_audit_action_redacted(audit_record).await {
                    Ok(audit_id) => {
                        injection.set_audit_action_id(audit_id);
                    }
                    Err(e) => {
                        tracing::warn!(pane_id, "Failed to record audit: {e}");
                    }
                }

                let mut wait_for_data = None;
                let mut verification_error = None;
                if injection.is_allowed() {
                    if let Some(pattern) = &wait_for {
                        let matcher = if wait_for_regex {
                            match fancy_regex::Regex::new(pattern) {
                                Ok(compiled) => {
                                    Some(wa_core::wezterm::WaitMatcher::regex(compiled))
                                }
                                Err(e) => {
                                    verification_error =
                                        Some(format!("Invalid wait-for regex: {e}"));
                                    None
                                }
                            }
                        } else {
                            Some(wa_core::wezterm::WaitMatcher::substring(pattern))
                        };

                        if let Some(matcher) = matcher {
                            let options = wa_core::wezterm::WaitOptions {
                                tail_lines: 200,
                                escapes: false,
                                ..wa_core::wezterm::WaitOptions::default()
                            };
                            let waiter =
                                wa_core::wezterm::PaneWaiter::new(&wezterm).with_options(options);
                            let timeout = std::time::Duration::from_secs(timeout_secs);
                            match waiter.wait_for(pane_id, &matcher, timeout).await {
                                Ok(wa_core::wezterm::WaitResult::Matched { elapsed_ms, polls }) => {
                                    let pattern_out = redacted_wait_for
                                        .clone()
                                        .unwrap_or_else(|| pattern.clone());
                                    wait_for_data = Some(RobotWaitForData {
                                        pane_id,
                                        pattern: pattern_out,
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
                                    let pattern_out = redacted_wait_for
                                        .clone()
                                        .unwrap_or_else(|| pattern.clone());
                                    wait_for_data = Some(RobotWaitForData {
                                        pane_id,
                                        pattern: pattern_out,
                                        matched: false,
                                        elapsed_ms,
                                        polls,
                                        is_regex: wait_for_regex,
                                    });
                                    verification_error =
                                        Some(format!("Timeout waiting for pattern '{pattern}'"));
                                }
                                Err(e) => {
                                    verification_error = Some(format!("wait-for failed: {e}"));
                                }
                            }
                        }
                    }
                }

                let data = HumanSendData {
                    pane_id,
                    injection,
                    wait_for: wait_for_data,
                    verification_error,
                    no_paste,
                    no_newline,
                };

                if emit_json {
                    println!("{}", serde_json::to_string_pretty(&data)?);
                } else {
                    println!(
                        "{}",
                        format_send_result_human(
                            &data,
                            &redacted_text,
                            redacted_wait_for.as_deref()
                        )
                    );
                }
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
                        let wezterm = wa_core::wezterm::WeztermClient::new();
                        let pane_info = wezterm.get_pane(pane).await.ok();
                        let report = build_workflow_dry_run_report(
                            &command_ctx,
                            &name,
                            pane,
                            pane_info.as_ref(),
                        );
                        println!("{}", wa_core::dry_run::format_human(&report));
                    } else {
                        tracing::info!("Running workflow '{}' on pane {}", name, pane);
                        // TODO: Implement workflow run
                        println!("Workflow run not yet implemented");
                    }
                }
                WorkflowCommands::Status {
                    execution_id,
                    verbose,
                } => {
                    tracing::info!("Getting status for execution {}", execution_id);

                    let layout = match config.workspace_layout(Some(&workspace_root)) {
                        Ok(l) => l,
                        Err(e) => {
                            eprintln!("Failed to get workspace layout: {e}");
                            return Ok(());
                        }
                    };

                    let db_path = layout.db_path.to_string_lossy();
                    let storage = match wa_core::storage::StorageHandle::new(&db_path).await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("Failed to open storage: {e}");
                            return Ok(());
                        }
                    };

                    match storage.get_workflow(&execution_id).await {
                        Ok(Some(record)) => {
                            println!("Workflow: {} ({})", record.workflow_name, record.id);
                            println!("Status: {}", record.status);
                            println!("Pane: {}", record.pane_id);
                            if let Some(event_id) = record.trigger_event_id {
                                println!("Trigger event: {event_id}");
                            }
                            println!("Current step: {}", record.current_step);
                            println!("Started at: {}", record.started_at);
                            println!("Updated at: {}", record.updated_at);
                            if let Some(completed_at) = record.completed_at {
                                println!("Completed at: {completed_at}");
                            }
                            if let Some(error) = record.error.as_ref() {
                                println!("Error: {error}");
                            }

                            if verbose {
                                match storage.get_action_plan(&execution_id).await {
                                    Ok(Some(plan)) => {
                                        println!("\nAction plan:");
                                        println!("  plan_id: {}", plan.plan_id);
                                        println!("  plan_hash: {}", plan.plan_hash);
                                        println!("  created_at: {}", plan.created_at);
                                        match serde_json::from_str::<serde_json::Value>(
                                            &plan.plan_json,
                                        ) {
                                            Ok(value) => {
                                                if let Ok(pretty) =
                                                    serde_json::to_string_pretty(&value)
                                                {
                                                    println!("{pretty}");
                                                } else {
                                                    println!("{}", plan.plan_json);
                                                }
                                            }
                                            Err(_) => {
                                                println!("{}", plan.plan_json);
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        println!("\nAction plan: <none>");
                                    }
                                    Err(e) => {
                                        println!("\nAction plan: <error: {e}>");
                                    }
                                }

                                match storage.get_step_logs(&execution_id).await {
                                    Ok(logs) => {
                                        if logs.is_empty() {
                                            println!("\nStep logs: <none>");
                                        } else {
                                            println!("\nStep logs:");
                                            for log in logs {
                                                println!(
                                                    "  [{}] {} ({})",
                                                    log.step_index, log.step_name, log.result_type
                                                );
                                                if let Some(step_id) = log.step_id.as_ref() {
                                                    println!("    step_id: {step_id}");
                                                }
                                                if let Some(step_kind) = log.step_kind.as_ref() {
                                                    println!("    step_kind: {step_kind}");
                                                }
                                                if let Some(code) = log.error_code.as_ref() {
                                                    println!("    error_code: {code}");
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        println!("\nStep logs: <error: {e}>");
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            eprintln!("No workflow execution found with ID: {}", execution_id);
                        }
                        Err(e) => {
                            eprintln!("Failed to query workflow: {e}");
                        }
                    }

                    if let Err(e) = storage.shutdown().await {
                        tracing::warn!("Failed to shutdown storage cleanly: {e}");
                    }
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
                let wezterm = wa_core::wezterm::WeztermClient::new();
                let payload = serde_json::json!({
                    "status": "ok",
                    "version": wa_core::VERSION,
                    "wezterm_circuit": wezterm.circuit_status(),
                });
                println!(
                    "{}",
                    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
                );
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
                        if let wa_core::Error::Wezterm(wa_core::error::WeztermError::CircuitOpen {
                            retry_after_ms,
                        }) = &e
                        {
                            if output_format.is_json() {
                                let payload = serde_json::json!({
                                    "ok": false,
                                    "error": format!(
                                        "WezTerm circuit breaker open; retry in {retry_after_ms} ms"
                                    ),
                                    "circuit": wezterm.circuit_status(),
                                    "version": wa_core::VERSION,
                                });
                                println!(
                                    "{}",
                                    serde_json::to_string(&payload)
                                        .unwrap_or_else(|_| "{}".to_string())
                                );
                            } else {
                                eprintln!(
                                    "Error: WezTerm circuit breaker open; retry in {retry_after_ms} ms."
                                );
                                eprintln!("Is WezTerm running and responsive?");
                            }
                            std::process::exit(1);
                        }
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
            // NOTE: --from-status was removed in v0.2.0 (Lua performance optimization)
            // Alt-screen detection is now handled via escape sequence parsing.
            if !from_uservar {
                eprintln!("Error: must specify --from-uservar.");
                eprintln!(
                    "Hint: use `wa event --from-uservar --pane <id> --name <name> --value <value>`"
                );
                std::process::exit(1);
            }

            #[cfg(unix)]
            {
                let client = wa_core::ipc::IpcClient::new(&layout.ipc_socket_path);

                // Handle user-var event
                let name = name.expect("name required for --from-uservar");
                let value = value.expect("value required for --from-uservar");
                let name_for_log = name.clone();
                let value_len = value.len();

                if let Err(message) = validate_uservar_request(pane, &name, &value) {
                    eprintln!("Error: {message}");
                    eprintln!(
                        "Context: pane_id={pane} name=\"{name_for_log}\" value_len={value_len}"
                    );
                    std::process::exit(1);
                }

                tracing::debug!(
                    pane_id = pane,
                    name = %name_for_log,
                    value_len,
                    "Forwarding user-var event to watcher"
                );

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
                                eprintln!(
                                    "Hint: start the watcher with `wa watch` in this workspace."
                                );
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

            #[cfg(not(unix))]
            {
                eprintln!("Error: IPC event forwarding is only supported on Unix platforms.");
                eprintln!("Context: pane_id={pane}");
                std::process::exit(1);
            }
        }

        Some(Commands::Doctor) => {
            println!("wa doctor - Running diagnostics...\n");

            let checks = run_diagnostics(&permission_warnings, &config, &layout);

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
                println!(
                    "Diagnostics completed with warnings. wa should work but performance may be affected."
                );
            } else {
                println!("All checks passed! wa is ready to use.");
            }
        }

        Some(Commands::Setup {
            list_hosts,
            apply,
            dry_run,
            command,
        }) => {
            if list_hosts {
                use wa_core::setup;

                println!("wa setup --list-hosts\n");

                match setup::locate_ssh_config() {
                    Ok(path) => match setup::load_ssh_hosts(&path) {
                        Ok(hosts) => {
                            if hosts.is_empty() {
                                println!("No SSH hosts found in {}.", path.display());
                            } else {
                                println!(
                                    "Found {} SSH host(s) in {}:\n",
                                    hosts.len(),
                                    path.display()
                                );
                                for host in hosts {
                                    println!("• {}", host.alias);
                                    if let Some(hostname) = host.hostname.as_deref() {
                                        println!("  HostName: {hostname}");
                                    }
                                    if let Some(user) = host.user.as_deref() {
                                        println!("  User: {user}");
                                    }
                                    if let Some(port) = host.port {
                                        println!("  Port: {port}");
                                    }
                                    let identities = host.redacted_identity_files();
                                    if !identities.is_empty() {
                                        println!("  IdentityFile(s): {}", identities.join(", "));
                                    }
                                    println!();
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!("Error: {err}");
                            std::process::exit(1);
                        }
                    },
                    Err(err) => {
                        eprintln!("Error: {err}");
                        std::process::exit(1);
                    }
                }
            } else if let Some(command) = command {
                match command {
                    SetupCommands::Local => {
                        println!("wa setup local - WezTerm Configuration Guide\n");
                        println!("wa requires WezTerm to be configured with adequate scrollback.");
                        println!("Without sufficient scrollback, wa may miss terminal output.\n");

                        // Check current state
                        if let Ok((lines, path)) = check_wezterm_scrollback() {
                            if lines >= RECOMMENDED_SCROLLBACK_LINES {
                                println!("✓ Your WezTerm scrollback is already configured!");
                                println!("  Current: {} lines in {}", lines, path.display());
                                println!(
                                    "  Recommended minimum: {RECOMMENDED_SCROLLBACK_LINES} lines"
                                );
                                println!("\nNo changes needed.");
                            } else {
                                println!("⚠ Your WezTerm scrollback is below recommended minimum.");
                                println!("  Current: {} lines in {}", lines, path.display());
                                println!("  Recommended: {RECOMMENDED_SCROLLBACK_LINES} lines\n");
                                println!("Add this line to your wezterm.lua:");
                                println!(
                                    "  config.scrollback_lines = {RECOMMENDED_SCROLLBACK_LINES}"
                                );
                            }
                        } else {
                            println!("Could not find or parse WezTerm config.\n");
                            println!("Add the following to your ~/.config/wezterm/wezterm.lua:\n");
                            println!(
                                "  config.scrollback_lines = {RECOMMENDED_SCROLLBACK_LINES}\n"
                            );
                            println!(
                                "This ensures wa can capture all terminal output without gaps."
                            );
                        }

                        println!("\n--- Why {RECOMMENDED_SCROLLBACK_LINES} lines? ---");
                        println!("• AI coding agents can produce substantial output");
                        println!("• wa uses delta extraction to capture only new content");
                        println!(
                            "• Insufficient scrollback causes capture gaps (EVENT_GAP_DETECTED)"
                        );
                        println!(
                            "• 50k lines ≈ 2-5 MB memory per pane (negligible on modern systems)"
                        );
                        println!("\nRun 'wa doctor' to verify your configuration.");
                    }
                    SetupCommands::Remote {
                        host,
                        yes,
                        install_wa,
                        wa_path,
                        wa_version,
                        timeout_secs,
                    } => {
                        let options = RemoteSetupOptions {
                            apply,
                            dry_run,
                            yes,
                            install_wa,
                            wa_path: wa_path.as_deref(),
                            wa_version: wa_version.as_deref(),
                            timeout_secs,
                            verbose: cli.verbose,
                        };
                        run_remote_setup(&host, &options)?;
                    }
                    SetupCommands::Config => {
                        use wa_core::setup;

                        println!("wa setup config - Generate WezTerm Config Additions\n");
                        let mut hosts = Vec::new();
                        let mut ssh_path = None;
                        match setup::locate_ssh_config() {
                            Ok(path) => match setup::load_ssh_hosts(&path) {
                                Ok(loaded) => {
                                    hosts = loaded;
                                    ssh_path = Some(path);
                                }
                                Err(err) => {
                                    eprintln!("Warning: {err}");
                                }
                            },
                            Err(err) => {
                                eprintln!("Warning: {err}");
                            }
                        }

                        let wa_block =
                            setup::generate_ssh_domains_lua(&hosts, RECOMMENDED_SCROLLBACK_LINES);

                        if dry_run {
                            match setup::locate_wezterm_config() {
                                Ok(config_path) => match fs::read_to_string(&config_path) {
                                    Ok(content) => {
                                        if setup::has_wa_block(&content) {
                                            let existing = setup::extract_wa_block(&content)
                                                .unwrap_or_default();
                                            if existing.trim_end() == wa_block.trim_end() {
                                                println!(
                                                    "✓ wa block already up to date in {}",
                                                    config_path.display()
                                                );
                                            } else {
                                                println!(
                                                    "• wa block would be updated in {}",
                                                    config_path.display()
                                                );
                                            }
                                        } else {
                                            println!(
                                                "• wa block would be added to {}",
                                                config_path.display()
                                            );
                                        }
                                    }
                                    Err(err) => {
                                        eprintln!(
                                            "Error: Failed to read {}: {}",
                                            config_path.display(),
                                            err
                                        );
                                        std::process::exit(1);
                                    }
                                },
                                Err(err) => {
                                    eprintln!("Warning: {err}");
                                }
                            }
                            println!("\n--- Generated block (dry-run) ---\n");
                            println!("{wa_block}");
                            println!("Place this block before any `return config` line.");
                            println!("Run with --apply to patch your config.");
                        } else if apply {
                            let config_path = match setup::locate_wezterm_config() {
                                Ok(path) => path,
                                Err(err) => {
                                    eprintln!("Error: {err}");
                                    std::process::exit(1);
                                }
                            };
                            match setup::patch_wezterm_config_block_at(&config_path, &wa_block) {
                                Ok(patch_result) => {
                                    if patch_result.modified {
                                        println!("✓ {}", patch_result.message);
                                        if let Some(backup) = patch_result.backup_path {
                                            println!("  Backup: {}", backup.display());
                                        }
                                        println!("\nRestart WezTerm to apply the changes.");
                                    } else {
                                        println!("✓ {}", patch_result.message);
                                    }
                                }
                                Err(e) => {
                                    eprintln!("Error: {e}");
                                    std::process::exit(1);
                                }
                            }
                        } else {
                            println!("Add the following to your wezterm.lua:\n");
                            if let Some(path) = ssh_path {
                                println!(
                                    "-- Derived {} SSH host(s) from {}",
                                    hosts.len(),
                                    path.display()
                                );
                            } else {
                                println!("-- SSH config not found; update ssh_domains manually");
                            }
                            println!();
                            println!("{wa_block}");
                            println!("Place this block before any `return config` line.");
                            println!("Tip: run with --apply to patch automatically.");
                        }
                    }
                    SetupCommands::Patch {
                        remove,
                        config_path,
                    } => {
                        use wa_core::setup;

                        println!("wa setup patch - WezTerm User-Var Forwarding\n");

                        let result = if remove {
                            if let Some(path) = config_path {
                                setup::unpatch_wezterm_config_at(&path)
                            } else {
                                let path = setup::locate_wezterm_config()?;
                                setup::unpatch_wezterm_config_at(&path)
                            }
                        } else if let Some(path) = config_path {
                            setup::patch_wezterm_config_at(&path)
                        } else {
                            setup::patch_wezterm_config()
                        };

                        match result {
                            Ok(patch_result) => {
                                if patch_result.modified {
                                    println!("✓ {}", patch_result.message);
                                    if let Some(backup) = patch_result.backup_path {
                                        println!("  Backup: {}", backup.display());
                                    }
                                    println!("\nRestart WezTerm to apply the changes.");
                                } else {
                                    println!("✓ {}", patch_result.message);
                                }
                            }
                            Err(e) => {
                                eprintln!("Error: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    SetupCommands::Shell {
                        remove,
                        shell,
                        rc_path,
                    } => {
                        use wa_core::setup::{self, ShellType};

                        // Determine shell type
                        let shell_type = match shell.as_deref() {
                            Some("bash") => ShellType::Bash,
                            Some("zsh") => ShellType::Zsh,
                            Some("fish") => ShellType::Fish,
                            Some(other) => {
                                eprintln!("Error: Unsupported shell: {other}");
                                eprintln!("Supported shells: bash, zsh, fish");
                                std::process::exit(1);
                            }
                            None => {
                                // Auto-detect from $SHELL
                                if let Some(st) = ShellType::detect() {
                                    println!("Detected shell: {}\n", st.name());
                                    st
                                } else {
                                    eprintln!("Error: Could not detect shell from $SHELL");
                                    eprintln!("Please specify --shell bash|zsh|fish");
                                    std::process::exit(1);
                                }
                            }
                        };

                        println!(
                            "wa setup shell - OSC 133 Prompt Markers ({})\n",
                            shell_type.name()
                        );

                        let result = if remove {
                            if let Some(path) = rc_path {
                                setup::unpatch_shell_rc_at(&path)
                            } else {
                                let path = setup::locate_shell_rc(shell_type)?;
                                setup::unpatch_shell_rc_at(&path)
                            }
                        } else if let Some(path) = rc_path {
                            setup::patch_shell_rc_at(&path, shell_type)
                        } else {
                            setup::patch_shell_rc(shell_type)
                        };

                        match result {
                            Ok(patch_result) => {
                                if patch_result.modified {
                                    println!("✓ {}", patch_result.message);
                                    if let Some(backup) = patch_result.backup_path {
                                        println!("  Backup: {}", backup.display());
                                    }
                                    println!(
                                        "\nSource your shell config or restart your shell to apply:"
                                    );
                                    println!("  source {}", patch_result.config_path.display());
                                } else {
                                    println!("✓ {}", patch_result.message);
                                }
                            }
                            Err(e) => {
                                eprintln!("Error: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                }
            } else {
                run_guided_setup(apply, dry_run, cli.verbose)?;
            }
        }

        Some(Commands::Config { command }) => {
            handle_config_command(command, cli_config_arg.as_deref(), workspace.as_deref()).await?;
        }

        Some(Commands::Db { command }) => {
            handle_db_command(command, &layout).await?;
        }

        #[cfg(feature = "tui")]
        Some(Commands::Tui { debug, refresh }) => {
            use std::time::Duration;
            use wa_core::tui::{AppConfig, ProductionQueryClient, run_tui};

            let db_path = layout.db_path.to_string_lossy();
            let storage = match wa_core::storage::StorageHandle::new(&db_path).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to open storage: {e}");
                    return Err(e.into());
                }
            };

            let query_client = ProductionQueryClient::with_storage(layout.clone(), storage);
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
    use wa_core::output::{ErrorRenderer, OutputFormat, detect_format};

    if robot_mode {
        // In robot mode, output structured JSON error
        if let Some(core_err) = err.downcast_ref::<wa_core::Error>() {
            let renderer = ErrorRenderer::new(OutputFormat::Json);
            eprintln!("{}", renderer.render(core_err));
        } else {
            eprintln!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    "error": err.to_string(),
                })
            );
        }
        return;
    }

    // In human mode, use rich formatting with error codes
    if let Some(core_err) = err.downcast_ref::<wa_core::Error>() {
        let format = detect_format();
        let renderer = ErrorRenderer::new(format);
        eprintln!("{}", renderer.render(core_err));
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
            } else {
                cli_config.map(std::path::PathBuf::from)
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
                .map_or_else(|| "(default)".to_string(), |p| p.display().to_string());

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
            } else {
                cli_config.map(std::path::PathBuf::from)
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

/// Handle `wa db` subcommands
async fn handle_db_command(
    command: DbCommands,
    layout: &wa_core::config::WorkspaceLayout,
) -> anyhow::Result<()> {
    use wa_core::storage::{
        MigrationDirection, SCHEMA_VERSION, migrate_database_to_version, migration_plan_for_path,
        migration_status_for_path,
    };

    let db_path = &layout.db_path;

    match command {
        DbCommands::Migrate {
            status,
            to,
            yes,
            allow_downgrade,
            dry_run,
        } => {
            let target_version = to.unwrap_or(SCHEMA_VERSION);

            if status {
                let report = migration_status_for_path(db_path)?;
                print_migration_status(&report, db_path);
                return Ok(());
            }

            if dry_run {
                let report = migration_status_for_path(db_path)?;
                if report.needs_initialization {
                    println!("Database is uninitialized at {}.", db_path.display());
                    println!("Would initialize to schema v{}.", SCHEMA_VERSION);
                    return Ok(());
                }
                let plan = migration_plan_for_path(db_path, target_version)?;
                print_migration_plan(&plan);
                return Ok(());
            }

            let report = migration_status_for_path(db_path)?;
            if report.needs_initialization {
                if target_version != SCHEMA_VERSION {
                    anyhow::bail!(
                        "Database is uninitialized; can only initialize to schema v{}.",
                        SCHEMA_VERSION
                    );
                }
                println!(
                    "Database is uninitialized; will initialize to schema v{}.",
                    SCHEMA_VERSION
                );
                if !yes && !prompt_confirm("Proceed? [y/N]: ")? {
                    println!("Aborted.");
                    return Ok(());
                }
                let plan = migrate_database_to_version(db_path, target_version)?;
                println!("Migration complete.");
                print_migration_plan(&plan);
                return Ok(());
            }

            let plan = migration_plan_for_path(db_path, target_version)?;
            if plan.steps.is_empty() {
                println!("Database already at schema v{}.", plan.to_version);
                return Ok(());
            }

            if plan.direction == MigrationDirection::Down && !allow_downgrade {
                anyhow::bail!(
                    "Refusing to downgrade from v{} to v{} without --allow-downgrade.",
                    plan.from_version,
                    plan.to_version
                );
            }

            print_migration_plan(&plan);
            if !yes && !prompt_confirm("Apply migrations? [y/N]: ")? {
                println!("Aborted.");
                return Ok(());
            }

            let applied_plan = migrate_database_to_version(db_path, target_version)?;
            println!("Migration complete.");
            print_migration_plan(&applied_plan);
        }
    }

    Ok(())
}

fn print_migration_status(report: &MigrationStatusReport, db_path: &Path) {
    println!("Database: {}", db_path.display());
    println!("Exists: {}", report.db_exists);
    println!("Needs initialization: {}", report.needs_initialization);
    println!("Current schema version: {}", report.current_version);
    println!("Target schema version: {}", report.target_version);
    println!();
    println!("Migrations:");
    for entry in &report.entries {
        let applied = if entry.applied { "[x]" } else { "[ ]" };
        let rollback = if entry.rollback_supported {
            "rollback: yes"
        } else {
            "rollback: no"
        };
        println!(
            "  {applied} v{version:02} {desc} ({rollback})",
            version = entry.version,
            desc = entry.description
        );
    }
}

fn print_migration_plan(plan: &MigrationPlan) {
    println!(
        "Migration plan ({direction}): v{from} -> v{to}",
        direction = plan.direction.as_str(),
        from = plan.from_version,
        to = plan.to_version
    );

    if plan.steps.is_empty() {
        println!("  (no steps)");
        return;
    }

    for step in &plan.steps {
        println!(
            "  - {dir} v{from} -> v{to}: {desc}",
            dir = step.direction.as_str(),
            from = step.migration_version,
            to = step.resulting_version,
            desc = step.description
        );
    }
}

fn prompt_confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::{self, Write};

    print!("{prompt}");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim().to_ascii_lowercase();
    Ok(matches!(value.as_str(), "y" | "yes"))
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
# Enabled workflows (by name)
enabled = ["handle_compaction", "handle_usage_limits"]
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
                if !table.contains_key(part) {
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
                    return Err(format!("Failed to read {}: {}", config_path.display(), e));
                }
            }
        }
    }

    Err(
        "No WezTerm config file found (~/.config/wezterm/wezterm.lua or ~/.wezterm.lua)"
            .to_string(),
    )
}

fn detect_wezterm_version() -> Option<String> {
    let output = std::process::Command::new("wezterm")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = stdout.trim();
    if text.is_empty() {
        let text = stderr.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    } else {
        Some(text.to_string())
    }
}

fn format_backup_hint(path: &Path) -> String {
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    let backup_name = format!("{filename}.bak.<timestamp>");
    path.with_file_name(backup_name).display().to_string()
}

fn run_guided_setup(apply: bool, dry_run: bool, verbose: bool) -> anyhow::Result<()> {
    use wa_core::setup::{self, ShellType};

    let apply_changes = apply && !dry_run;

    println!("wa setup - Guided setup\n");
    if dry_run {
        println!("(dry run) No changes will be made.\n");
    } else if apply_changes {
        println!("Applying non-destructive changes where needed.\n");
    } else {
        println!("Run with --apply to make changes automatically.\n");
    }

    // Step 1: WezTerm CLI availability
    if let Some(version) = detect_wezterm_version() {
        println!("✓ WezTerm CLI detected: {version}");
    } else {
        println!("⚠ WezTerm CLI not detected in PATH.");
        println!("  Install WezTerm and ensure `wezterm` is available.");
    }

    // Step 2: scrollback configuration
    match check_wezterm_scrollback() {
        Ok((lines, path)) => {
            if verbose {
                println!("  WezTerm config: {}", path.display());
            }
            if lines >= RECOMMENDED_SCROLLBACK_LINES {
                println!("✓ scrollback_lines = {} (ok)", lines);
            } else {
                println!(
                    "⚠ scrollback_lines = {} (recommended ≥ {})",
                    lines, RECOMMENDED_SCROLLBACK_LINES
                );
                println!(
                    "  Add to {}: config.scrollback_lines = {}",
                    path.display(),
                    RECOMMENDED_SCROLLBACK_LINES
                );
            }
        }
        Err(err) => {
            println!("⚠ scrollback_lines check: {err}");
            println!("  Add: config.scrollback_lines = {RECOMMENDED_SCROLLBACK_LINES}");
        }
    }

    // Step 3: WezTerm user-var forwarding + status update block
    match setup::locate_wezterm_config() {
        Ok(path) => {
            if verbose {
                println!("  WezTerm config path: {}", path.display());
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if setup::has_wa_block(&content) {
                        println!("✓ wezterm.lua already patched (wa block present)");
                    } else if apply_changes {
                        let result = setup::patch_wezterm_config_at(&path)?;
                        println!("✓ {}", result.message);
                        if let Some(backup) = result.backup_path {
                            println!("  Backup: {}", backup.display());
                        }
                    } else {
                        println!("⚠ wezterm.lua missing wa block: {}", path.display());
                        println!(
                            "  Would patch and create backup: {}",
                            format_backup_hint(&path)
                        );
                        println!("  Run: wa setup patch");
                    }
                }
                Err(err) => {
                    println!("⚠ Failed to read {}: {}", path.display(), err);
                }
            }
        }
        Err(err) => {
            println!("⚠ WezTerm config not found: {err}");
            println!("  Create ~/.config/wezterm/wezterm.lua then run: wa setup patch");
        }
    }

    // Step 4: Shell OSC 133 integration
    match ShellType::detect() {
        Some(shell_type) => match setup::locate_shell_rc(shell_type) {
            Ok(rc_path) => {
                if verbose {
                    println!("  Shell rc path: {}", rc_path.display());
                }
                let content = if rc_path.exists() {
                    std::fs::read_to_string(&rc_path).unwrap_or_default()
                } else {
                    String::new()
                };
                if setup::has_shell_wa_block(&content) {
                    println!("✓ shell rc already patched ({})", shell_type.name());
                } else if apply_changes {
                    let result = setup::patch_shell_rc_at(&rc_path, shell_type)?;
                    println!("✓ {}", result.message);
                    if let Some(backup) = result.backup_path {
                        println!("  Backup: {}", backup.display());
                    }
                } else {
                    println!("⚠ shell rc missing OSC 133 markers ({})", shell_type.name());
                    if rc_path.exists() {
                        println!(
                            "  Would patch and create backup: {}",
                            format_backup_hint(&rc_path)
                        );
                    } else {
                        println!("  Would create {}", rc_path.display());
                    }
                    println!("  Run: wa setup shell");
                }
            }
            Err(err) => {
                println!("⚠ Shell rc not found: {err}");
            }
        },
        None => {
            println!("⚠ Could not detect shell from $SHELL (skip OSC 133 setup)");
        }
    }

    // Step 5: SSH hosts (optional)
    match setup::locate_ssh_config() {
        Ok(path) => match setup::load_ssh_hosts(&path) {
            Ok(hosts) => {
                println!(
                    "✓ SSH config found: {} ({} host(s))",
                    path.display(),
                    hosts.len()
                );
                println!("  Run: wa setup --list-hosts");
            }
            Err(err) => {
                println!("⚠ Failed to parse SSH config: {err}");
            }
        },
        Err(_) => {
            println!("• No SSH config detected (optional).");
        }
    }

    println!("\nNext steps:");
    println!("  wa daemon start");
    println!("  wa status");
    println!("  wa robot state");

    Ok(())
}

const REMOTE_MUX_SERVICE_UNIT: &str = r"[Unit]
Description=WezTerm Mux Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/bin/wezterm-mux-server --daemonize=false
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
";

struct RemoteCommandOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    duration_ms: u128,
}

struct RemoteSetupOptions<'a> {
    apply: bool,
    dry_run: bool,
    yes: bool,
    install_wa: bool,
    wa_path: Option<&'a Path>,
    wa_version: Option<&'a str>,
    timeout_secs: u64,
    verbose: bool,
}

fn run_remote_command(
    host: &str,
    command: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<RemoteCommandOutput> {
    use std::process::{Command, Stdio};
    use std::thread::sleep;

    let start = std::time::Instant::now();
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={}", timeout.as_secs()))
        .arg(host)
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    loop {
        if let Some(_status) = child.try_wait()? {
            break;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            anyhow::bail!("timeout after {}s", timeout.as_secs());
        }
        sleep(std::time::Duration::from_millis(100));
    }

    let output = child.wait_with_output()?;
    Ok(RemoteCommandOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        duration_ms: start.elapsed().as_millis(),
    })
}

fn print_remote_output(
    redactor: &wa_core::policy::Redactor,
    label: &str,
    text: &str,
    verbose: bool,
) {
    if !verbose {
        return;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let redacted = redactor.redact(trimmed);
    println!("  {label}: {redacted}");
}

fn run_remote_step<F>(
    name: &str,
    host: &str,
    command: &str,
    timeout: std::time::Duration,
    runner: &F,
    redactor: &wa_core::policy::Redactor,
    verbose: bool,
    require_success: bool,
) -> anyhow::Result<RemoteCommandOutput>
where
    F: Fn(&str, &str, std::time::Duration) -> anyhow::Result<RemoteCommandOutput>,
{
    if verbose {
        let redacted_cmd = redactor.redact(command);
        println!("• {name}");
        println!("  cmd: {redacted_cmd}");
    } else {
        println!("• {name}");
    }

    let output = runner(host, command, timeout)?;
    if require_success && !output.status.success() {
        print_remote_output(redactor, "stdout", &output.stdout, true);
        print_remote_output(redactor, "stderr", &output.stderr, true);
        anyhow::bail!(
            "{} failed (exit {:?})",
            name,
            output.status.code().unwrap_or(-1)
        );
    }

    print_remote_output(redactor, "stdout", &output.stdout, verbose);
    print_remote_output(redactor, "stderr", &output.stderr, verbose);
    println!("  ✓ done in {} ms", output.duration_ms);
    Ok(output)
}

fn run_remote_setup(host: &str, options: &RemoteSetupOptions<'_>) -> anyhow::Result<()> {
    run_remote_setup_with_runner(host, options, &run_remote_command)
}

fn run_remote_setup_with_runner<F>(
    host: &str,
    options: &RemoteSetupOptions<'_>,
    runner: &F,
) -> anyhow::Result<()>
where
    F: Fn(&str, &str, std::time::Duration) -> anyhow::Result<RemoteCommandOutput>,
{
    use std::io::Write;
    use wa_core::policy::Redactor;

    let timeout = std::time::Duration::from_secs(options.timeout_secs.max(5));
    let apply_changes = options.apply && !options.dry_run;
    let redactor = Redactor::new();

    println!("wa setup remote - Remote Host Setup for '{host}'\n");
    if options.dry_run || !apply_changes {
        println!("(dry run) No changes will be made.\n");
    } else {
        println!("Applying non-destructive changes.\n");
    }

    if apply_changes && !options.yes {
        println!("This will run remote commands on {host}.");
        print!("Proceed? [y/N]: ");
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let confirmed = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Step 1: Connectivity check
    run_remote_step(
        "Check SSH connectivity",
        host,
        "true",
        timeout,
        runner,
        &redactor,
        options.verbose,
        true,
    )?;

    // Step 2: Detect package manager
    let pkg_output = run_remote_step(
        "Detect package manager",
        host,
        "command -v apt-get || command -v dnf || command -v yum || command -v pacman || true",
        timeout,
        runner,
        &redactor,
        options.verbose,
        false,
    )?;
    let pkg_manager = pkg_output
        .stdout
        .lines()
        .next()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty());

    // Step 3: Detect WezTerm
    let wezterm_output = run_remote_step(
        "Detect WezTerm",
        host,
        "command -v wezterm || true",
        timeout,
        runner,
        &redactor,
        options.verbose,
        false,
    )?;
    let wezterm_installed = !wezterm_output.stdout.trim().is_empty();
    if wezterm_installed {
        let version_output = run_remote_step(
            "WezTerm version",
            host,
            "wezterm --version || true",
            timeout,
            runner,
            &redactor,
            options.verbose,
            false,
        )?;
        if !version_output.stdout.trim().is_empty() {
            println!("  Version: {}", version_output.stdout.trim());
        }
    }

    if !wezterm_installed {
        match pkg_manager.as_deref() {
            Some(path) if path.contains("apt-get") => {
                if apply_changes {
                    run_remote_step(
                        "Install WezTerm (apt)",
                        host,
                        "sudo apt-get update && sudo apt-get install -y wezterm",
                        timeout,
                        runner,
                        &redactor,
                        options.verbose,
                        true,
                    )?;
                } else {
                    println!("• Would install WezTerm via apt");
                    println!("  cmd: sudo apt-get update && sudo apt-get install -y wezterm");
                }
            }
            Some(path) if path.contains("dnf") => {
                if apply_changes {
                    run_remote_step(
                        "Install WezTerm (dnf)",
                        host,
                        "sudo dnf install -y wezterm",
                        timeout,
                        runner,
                        &redactor,
                        options.verbose,
                        true,
                    )?;
                } else {
                    println!("• Would install WezTerm via dnf");
                    println!("  cmd: sudo dnf install -y wezterm");
                }
            }
            Some(path) if path.contains("yum") => {
                if apply_changes {
                    run_remote_step(
                        "Install WezTerm (yum)",
                        host,
                        "sudo yum install -y wezterm",
                        timeout,
                        runner,
                        &redactor,
                        options.verbose,
                        true,
                    )?;
                } else {
                    println!("• Would install WezTerm via yum");
                    println!("  cmd: sudo yum install -y wezterm");
                }
            }
            Some(path) if path.contains("pacman") => {
                if apply_changes {
                    run_remote_step(
                        "Install WezTerm (pacman)",
                        host,
                        "sudo pacman -Sy --noconfirm wezterm",
                        timeout,
                        runner,
                        &redactor,
                        options.verbose,
                        true,
                    )?;
                } else {
                    println!("• Would install WezTerm via pacman");
                    println!("  cmd: sudo pacman -Sy --noconfirm wezterm");
                }
            }
            _ => {
                println!("⚠ No supported package manager detected; install WezTerm manually.");
            }
        }
    }

    // Step 4: Install mux-server user service unit
    let service_path = "~/.config/systemd/user/wezterm-mux-server.service";
    let check_service_cmd = format!("cat {service_path} 2>/dev/null || true");
    let service_output = run_remote_step(
        "Check mux service unit",
        host,
        &check_service_cmd,
        timeout,
        runner,
        &redactor,
        options.verbose,
        false,
    )?;
    let existing_service = service_output.stdout.trim();
    let expected_service = REMOTE_MUX_SERVICE_UNIT.trim();
    if existing_service == expected_service {
        println!("✓ mux service unit already up to date");
    } else if existing_service.is_empty() {
        if apply_changes {
            let install_cmd = format!(
                "mkdir -p ~/.config/systemd/user && cat > {service_path} <<'EOF'\n{REMOTE_MUX_SERVICE_UNIT}EOF"
            );
            run_remote_step(
                "Install mux service unit",
                host,
                &install_cmd,
                timeout,
                runner,
                &redactor,
                options.verbose,
                true,
            )?;
        } else {
            println!("• Would install mux service unit at {service_path}");
        }
    } else {
        println!("⚠ mux service unit exists but differs; leaving unchanged.");
        if apply_changes {
            println!("  Remove or update {service_path} manually if desired.");
        }
    }

    if apply_changes {
        run_remote_step(
            "systemctl --user daemon-reload",
            host,
            "systemctl --user daemon-reload",
            timeout,
            runner,
            &redactor,
            options.verbose,
            true,
        )?;
        run_remote_step(
            "Enable mux service",
            host,
            "systemctl --user enable --now wezterm-mux-server",
            timeout,
            runner,
            &redactor,
            options.verbose,
            true,
        )?;
    } else {
        println!("• Would run: systemctl --user daemon-reload");
        println!("• Would run: systemctl --user enable --now wezterm-mux-server");
    }

    let status_output = run_remote_step(
        "Check mux service status",
        host,
        "systemctl --user is-active wezterm-mux-server || true",
        timeout,
        runner,
        &redactor,
        options.verbose,
        false,
    )?;
    let status = status_output.stdout.trim();
    if status == "active" {
        println!("✓ mux service is active");
    } else if !status.is_empty() {
        println!("⚠ mux service status: {status}");
    }

    // Step 5: Enable linger
    let linger_output = run_remote_step(
        "Check linger",
        host,
        "loginctl show-user $USER -p Linger || true",
        timeout,
        runner,
        &redactor,
        options.verbose,
        false,
    )?;
    let linger_enabled = linger_output
        .stdout
        .lines()
        .any(|line| line.trim_end() == "Linger=yes");
    if linger_enabled {
        println!("✓ linger already enabled");
    } else if apply_changes {
        run_remote_step(
            "Enable linger",
            host,
            "sudo loginctl enable-linger $USER",
            timeout,
            runner,
            &redactor,
            options.verbose,
            true,
        )?;
    } else {
        println!("• Would run: sudo loginctl enable-linger $USER");
    }

    // Step 6: Optional wa install
    if options.install_wa {
        if apply_changes {
            run_remote_step(
                "Ensure ~/.local/bin exists",
                host,
                "mkdir -p ~/.local/bin",
                timeout,
                runner,
                &redactor,
                options.verbose,
                true,
            )?;

            if let Some(path) = options.wa_path {
                if !path.exists() {
                    anyhow::bail!("wa_path does not exist: {}", path.display());
                }
                let scp_status = std::process::Command::new("scp")
                    .arg(path)
                    .arg(format!("{host}:~/.local/bin/wa"))
                    .status()?;
                if !scp_status.success() {
                    anyhow::bail!("scp failed with status {:?}", scp_status.code());
                }
                run_remote_step(
                    "chmod +x ~/.local/bin/wa",
                    host,
                    "chmod +x ~/.local/bin/wa",
                    timeout,
                    runner,
                    &redactor,
                    options.verbose,
                    true,
                )?;
            } else if let Some(version) = options.wa_version {
                let install_cmd = if version.eq_ignore_ascii_case("git") {
                    "cargo install --git https://github.com/Dicklesworthstone/wezterm_automata.git wa"
                        .to_string()
                } else {
                    format!(
                        "cargo install --git https://github.com/Dicklesworthstone/wezterm_automata.git --tag {} wa",
                        version
                    )
                };
                run_remote_step(
                    "Install wa via cargo",
                    host,
                    &install_cmd,
                    timeout,
                    runner,
                    &redactor,
                    options.verbose,
                    true,
                )?;
            } else {
                println!("⚠ --install-wa set but no --wa-path or --wa-version provided.");
            }
        } else {
            println!("• Would install wa on remote host");
            if let Some(path) = options.wa_path {
                println!("  Would scp {}", path.display());
            } else if let Some(version) = options.wa_version {
                println!("  Would cargo install wa ({version})");
            } else {
                println!("  Provide --wa-path or --wa-version to install wa");
            }
        }
    }

    println!("\nRemote setup summary:");
    println!("  Host: {host}");
    println!(
        "  Mode: {}",
        if apply_changes { "apply" } else { "dry-run" }
    );
    println!("  Next: verify with `ssh {host} 'systemctl --user status wezterm-mux-server'`");

    Ok(())
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

    fn warning(
        name: &'static str,
        detail: impl Into<String>,
        recommendation: impl Into<String>,
    ) -> Self {
        Self {
            name,
            status: DiagnosticStatus::Warning,
            detail: Some(detail.into()),
            recommendation: Some(recommendation.into()),
        }
    }

    fn error(
        name: &'static str,
        detail: impl Into<String>,
        recommendation: impl Into<String>,
    ) -> Self {
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
            println!("       → {rec}");
        }
    }
}

/// Run all diagnostic checks and return results
fn run_diagnostics(
    permission_warnings: &[wa_core::config::PermissionWarning],
    config: &wa_core::config::Config,
    layout: &wa_core::config::WorkspaceLayout,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check 1: wa-core loaded with version
    checks.push(DiagnosticCheck::ok_with_detail(
        "wa-core loaded",
        format!("v{}", env!("CARGO_PKG_VERSION")),
    ));

    // Check 2: Workspace resolution
    checks.push(DiagnosticCheck::ok_with_detail(
        "workspace root",
        layout.root.display().to_string(),
    ));

    // Check 3: .wa directory
    if layout.wa_dir.exists() {
        checks.push(DiagnosticCheck::ok_with_detail(
            ".wa directory",
            layout.wa_dir.display().to_string(),
        ));
    } else {
        checks.push(DiagnosticCheck::warning(
            ".wa directory",
            format!("{} does not exist", layout.wa_dir.display()),
            "Will be created on first daemon start",
        ));
    }

    // Check 4: Database path and status
    if layout.db_path.exists() {
        // Try to open and check DB
        match rusqlite::Connection::open_with_flags(
            &layout.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(conn) => {
                // Check schema version
                match conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0)) {
                    Ok(version) => {
                        let target = wa_core::storage::SCHEMA_VERSION;
                        match version.cmp(&target) {
                            std::cmp::Ordering::Equal => {
                                // Check WAL mode
                                let wal_mode: String = conn
                                    .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                                    .unwrap_or_else(|_| "unknown".to_string());
                                checks.push(DiagnosticCheck::ok_with_detail(
                                    "database",
                                    format!(
                                        "schema v{}, journal={} ({})",
                                        version,
                                        wal_mode,
                                        layout.db_path.display()
                                    ),
                                ));
                            }
                            std::cmp::Ordering::Less => {
                                checks.push(DiagnosticCheck::warning(
                                    "database",
                                    format!("schema v{} (needs migration to v{})", version, target),
                                    "Run 'wa daemon start' to auto-migrate",
                                ));
                            }
                            std::cmp::Ordering::Greater => {
                                checks.push(DiagnosticCheck::error(
                                    "database",
                                    format!(
                                        "schema v{} is newer than wa supports (v{})",
                                        version, target
                                    ),
                                    "Update wa to a newer version",
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        checks.push(DiagnosticCheck::warning(
                            "database",
                            format!("could not read schema version: {}", e),
                            "Database may be corrupt or locked",
                        ));
                    }
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::error(
                    "database",
                    format!("could not open: {}", e),
                    "Check file permissions or if another process has locked it",
                ));
            }
        }
    } else {
        checks.push(DiagnosticCheck::warning(
            "database",
            format!("{} does not exist", layout.db_path.display()),
            "Will be created on first daemon start",
        ));
    }

    // Check 5: Lock file (watcher status)
    if layout.lock_path.exists() {
        // Try to read lock file to see if daemon is running
        match std::fs::read_to_string(&layout.lock_path) {
            Ok(content) => {
                let pid = content.trim();
                // Check if process is actually running
                let is_running = std::process::Command::new("kill")
                    .args(["-0", pid])
                    .output()
                    .is_ok_and(|o| o.status.success());

                if is_running {
                    checks.push(DiagnosticCheck::ok_with_detail(
                        "daemon status",
                        format!("running (PID {})", pid),
                    ));
                } else {
                    checks.push(DiagnosticCheck::warning(
                        "daemon status",
                        format!("stale lock (PID {} not running)", pid),
                        format!("Remove lock: rm {}", layout.lock_path.display()),
                    ));
                }
            }
            Err(_) => {
                checks.push(DiagnosticCheck::warning(
                    "daemon status",
                    "lock file exists but unreadable",
                    format!("Check permissions: {}", layout.lock_path.display()),
                ));
            }
        }
    } else {
        checks.push(DiagnosticCheck::ok_with_detail(
            "daemon status",
            "not running",
        ));
    }

    // Check 6: Logs directory writability
    if layout.logs_dir.exists() {
        let test_file = layout.logs_dir.join(".wa_doctor_test");
        match std::fs::write(&test_file, "test") {
            Ok(()) => {
                let _ = std::fs::remove_file(&test_file);
                checks.push(DiagnosticCheck::ok_with_detail(
                    "logs directory",
                    layout.logs_dir.display().to_string(),
                ));
            }
            Err(e) => {
                checks.push(DiagnosticCheck::error(
                    "logs directory",
                    format!("not writable: {}", e),
                    format!("Check permissions: chmod 755 {}", layout.logs_dir.display()),
                ));
            }
        }
    } else {
        checks.push(DiagnosticCheck::warning(
            "logs directory",
            format!("{} does not exist", layout.logs_dir.display()),
            "Will be created on first daemon start",
        ));
    }

    // Check 7: Feature flags
    #[allow(unused_mut)]
    let mut features: Vec<&str> = Vec::new();
    #[cfg(feature = "tui")]
    features.push("tui");
    #[cfg(feature = "browser")]
    features.push("browser");
    #[cfg(feature = "mcp")]
    features.push("mcp");
    #[cfg(feature = "web")]
    features.push("web");
    #[cfg(feature = "metrics")]
    features.push("metrics");

    if features.is_empty() {
        checks.push(DiagnosticCheck::ok_with_detail(
            "features",
            "default (no optional features)",
        ));
    } else {
        checks.push(DiagnosticCheck::ok_with_detail(
            "features",
            features.join(", "),
        ));
    }

    // Check 8: Config source (without exposing secrets)
    let config_source = if config.general.log_level.is_empty() {
        "defaults".to_string()
    } else {
        format!("log_level={}", config.general.log_level)
    };
    checks.push(DiagnosticCheck::ok_with_detail("config", config_source));

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
                    format!(
                        "{lines} lines (below {RECOMMENDED_SCROLLBACK_LINES} recommended)"
                    ),
                    format!(
                        "Add to wezterm.lua: config.scrollback_lines = {RECOMMENDED_SCROLLBACK_LINES}"
                    ),
                ));
            }
        }
        Err(msg) => {
            checks.push(DiagnosticCheck::warning(
                "WezTerm scrollback",
                msg,
                format!(
                    "Add to wezterm.lua: config.scrollback_lines = {RECOMMENDED_SCROLLBACK_LINES}"
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
                format!("CLI error: {e}"),
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
    use wa_core::storage::{ApprovalTokenRecord, PaneRecord, StorageHandle};

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
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
        // approval_tokens.pane_id has a FK to panes(pane_id).
        if let Some(pid) = pane_id {
            let now = now_ms();
            let pane = PaneRecord {
                pane_id: pid,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: now,
                last_seen_at: now,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            };
            storage.upsert_pane(pane).await.unwrap();
        }

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
    fn workflow_dry_run_report_includes_steps() {
        let command_ctx = wa_core::dry_run::CommandContext::new("workflow run", true);
        let report = build_workflow_dry_run_report(&command_ctx, "handle_compaction", 42, None);

        assert!(!report.expected_actions.is_empty());
        assert!(
            report
                .expected_actions
                .iter()
                .any(|action| action.action_type == wa_core::dry_run::ActionType::AcquireLock)
        );

        let send_actions: Vec<_> = report
            .expected_actions
            .iter()
            .filter(|action| action.action_type == wa_core::dry_run::ActionType::SendText)
            .collect();
        assert!(!send_actions.is_empty());
        assert!(
            send_actions
                .iter()
                .all(|action| action.description.contains("policy-gated"))
        );
        assert!(send_actions.iter().all(|action| {
            action
                .metadata
                .as_ref()
                .and_then(|value| value.get("policy_gated"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        }));

        let workflow_check = report
            .policy_evaluation
            .as_ref()
            .and_then(|policy| policy.checks.iter().find(|check| check.name == "workflow"));
        assert!(workflow_check.is_some());
        assert!(workflow_check.unwrap().passed);
    }

    #[test]
    fn workflow_dry_run_report_unknown_workflow() {
        let command_ctx = wa_core::dry_run::CommandContext::new("workflow run", true);
        let report = build_workflow_dry_run_report(&command_ctx, "unknown_workflow", 7, None);

        assert!(report.expected_actions.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("No workflow steps available"))
        );

        let workflow_check = report
            .policy_evaluation
            .as_ref()
            .and_then(|policy| policy.checks.iter().find(|check| check.name == "workflow"));
        assert!(workflow_check.is_some());
        assert!(!workflow_check.unwrap().passed);
    }

    #[cfg(unix)]
    #[test]
    fn remote_setup_dry_run_avoids_install_commands() {
        use std::os::unix::process::ExitStatusExt;
        use std::sync::Mutex;

        let commands = Mutex::new(Vec::new());
        let runner = |_: &str, command: &str, _timeout: std::time::Duration| {
            commands.lock().unwrap().push(command.to_string());
            let stdout = if command.contains("command -v apt-get") {
                "/usr/bin/apt-get\n".to_string()
            } else if command.contains("command -v wezterm") {
                String::new()
            } else if command.contains("systemctl --user is-active") {
                "inactive\n".to_string()
            } else if command.contains("loginctl show-user") {
                "Linger=no\n".to_string()
            } else {
                String::new()
            };

            Ok(RemoteCommandOutput {
                status: std::process::ExitStatus::from_raw(0),
                stdout,
                stderr: String::new(),
                duration_ms: 5,
            })
        };

        let options = RemoteSetupOptions {
            apply: false,
            dry_run: true,
            yes: true,
            install_wa: true,
            wa_path: None,
            wa_version: Some("git"),
            timeout_secs: 5,
            verbose: false,
        };

        run_remote_setup_with_runner("example", &options, &runner).unwrap();

        let cmds = { commands.lock().unwrap().clone() };
        assert!(cmds.iter().any(|cmd| cmd == "true"));
        assert!(cmds.iter().any(|cmd| cmd.contains("command -v apt-get")));
        assert!(cmds.iter().any(|cmd| cmd.contains("command -v wezterm")));
        assert!(!cmds.iter().any(|cmd| cmd.contains("apt-get install")));
        assert!(!cmds.iter().any(|cmd| cmd.contains("cargo install")));
        assert!(!cmds.iter().any(|cmd| cmd.contains("scp ")));
    }

    #[test]
    fn structured_uservar_name_detection() {
        assert!(is_structured_uservar_name("wa_event"));
        assert!(is_structured_uservar_name("wa-foo"));
        assert!(is_structured_uservar_name("WA_FOO"));
        assert!(!is_structured_uservar_name("other_event"));
    }

    #[test]
    fn robot_help_toon_roundtrip() {
        fn normalize_numbers_to_int(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Array(items) => {
                    for item in items {
                        normalize_numbers_to_int(item);
                    }
                }
                serde_json::Value::Object(map) => {
                    for (_, value) in map.iter_mut() {
                        normalize_numbers_to_int(value);
                    }
                }
                serde_json::Value::Number(n) => {
                    // toon_rust currently decodes numbers as floats; normalize integral floats back
                    // to integers so comparisons against serde_json::to_value(...) are stable.
                    if let Some(f) = n.as_f64() {
                        #[allow(clippy::cast_precision_loss)]
                        let max_u64 = u64::MAX as f64;
                        if f.fract() == 0.0 && f >= 0.0 && f <= max_u64 {
                            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                            let as_u64 = f as u64;
                            *v = serde_json::Value::Number(serde_json::Number::from(as_u64));
                        }
                    }
                }
                serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::String(_) => {}
            }
        }

        let resp = RobotResponse::success(build_robot_help(), 0);
        let toon = toon_rust::encode(serde_json::to_value(&resp).unwrap(), None);

        let decoded = toon_rust::try_decode(&toon, None).unwrap();
        let json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");

        let mut decoded_value: serde_json::Value = serde_json::from_str(&json).unwrap();
        normalize_numbers_to_int(&mut decoded_value);
        let expected_value: serde_json::Value = serde_json::to_value(&resp).unwrap();

        assert_eq!(decoded_value, expected_value);
    }

    #[test]
    fn send_dry_run_report_includes_wait_for_and_no_paste_warning() {
        let config = wa_core::config::Config::default();
        let command_ctx = wa_core::dry_run::CommandContext::new("wa send", true);
        let report = build_send_dry_run_report(
            &command_ctx,
            1,
            None,
            None,
            "echo hi",
            true,
            Some("READY"),
            5,
            &config,
        );

        assert!(
            report
                .expected_actions
                .iter()
                .any(|action| action.action_type == wa_core::dry_run::ActionType::SendText)
        );
        assert!(
            report
                .expected_actions
                .iter()
                .any(|action| action.action_type == wa_core::dry_run::ActionType::WaitFor)
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("no_paste"))
        );
    }

    #[test]
    fn format_send_result_human_includes_approval_and_wait_for_skip() {
        let approval = wa_core::policy::ApprovalRequest {
            allow_once_code: "ABC123".to_string(),
            allow_once_full_hash: "sha256:deadbeef".to_string(),
            expires_at: 0,
            summary: "Needs approval".to_string(),
            command: "wa approve ABC123".to_string(),
        };
        let decision = wa_core::policy::PolicyDecision::RequireApproval {
            reason: "Alt-screen unknown".to_string(),
            rule_id: Some("policy.alt_screen_unknown".to_string()),
            approval: Some(approval),
            context: None,
        };
        let injection = wa_core::policy::InjectionResult::RequiresApproval {
            decision,
            summary: "[redacted]".to_string(),
            pane_id: 7,
            action: wa_core::policy::ActionKind::SendText,
            audit_action_id: Some(42),
        };
        let data = HumanSendData {
            pane_id: 7,
            injection,
            wait_for: None,
            verification_error: None,
            no_paste: false,
            no_newline: false,
        };

        let output = format_send_result_human(&data, "[redacted]", Some("READY"));

        assert!(output.contains("approval_required"));
        assert!(output.contains("wa approve ABC123"));
        assert!(output.contains("Audit ID: 42"));
        assert!(output.contains("Wait-for: skipped"));
    }

    #[test]
    fn format_send_result_human_includes_wait_for_match() {
        let decision = wa_core::policy::PolicyDecision::Allow {
            rule_id: Some("policy.allow".to_string()),
            context: None,
        };
        let injection = wa_core::policy::InjectionResult::Allowed {
            decision,
            summary: "[redacted]".to_string(),
            pane_id: 1,
            action: wa_core::policy::ActionKind::SendText,
            audit_action_id: Some(7),
        };
        let wait_for = RobotWaitForData {
            pane_id: 1,
            pattern: "READY".to_string(),
            matched: true,
            elapsed_ms: 10,
            polls: 2,
            is_regex: false,
        };
        let data = HumanSendData {
            pane_id: 1,
            injection,
            wait_for: Some(wait_for),
            verification_error: None,
            no_paste: false,
            no_newline: false,
        };

        let output = format_send_result_human(&data, "[redacted]", None);

        assert!(output.contains("matched"));
        assert!(output.contains("Wait-for"));
        assert!(output.contains("Audit ID: 7"));
    }

    #[test]
    fn format_send_result_human_includes_denial_and_verification_error() {
        let decision = wa_core::policy::PolicyDecision::Deny {
            reason: "Alt-screen active".to_string(),
            rule_id: Some("policy.alt_screen".to_string()),
            context: None,
        };
        let injection = wa_core::policy::InjectionResult::Denied {
            decision,
            summary: "[redacted]".to_string(),
            pane_id: 9,
            action: wa_core::policy::ActionKind::SendText,
            audit_action_id: Some(99),
        };
        let data = HumanSendData {
            pane_id: 9,
            injection,
            wait_for: None,
            verification_error: Some("Timeout waiting for pattern 'READY'".to_string()),
            no_paste: false,
            no_newline: false,
        };

        let output = format_send_result_human(&data, "[redacted]", Some("READY"));

        assert!(output.contains("Status: denied"));
        assert!(output.contains("policy.alt_screen"));
        assert!(output.contains("Alt-screen active"));
        assert!(output.contains("Wait-for: skipped"));
        assert!(output.contains("Verification"));
        assert!(output.contains("Audit ID: 99"));
    }

    #[test]
    fn send_dry_run_report_redacts_command() {
        let config = wa_core::config::Config::default();
        let command_ctx = wa_core::dry_run::CommandContext::new(
            "wa send 1 \"sk-abc123456789012345678901234567890123456789012345678901\"",
            true,
        );
        let report = build_send_dry_run_report(
            &command_ctx,
            1,
            None,
            None,
            "sk-abc123456789012345678901234567890123456789012345678901",
            false,
            None,
            10,
            &config,
        );

        let redacted = report.redacted();
        assert!(redacted.command.contains("[REDACTED]"));
        assert!(!redacted.command.contains("sk-abc"));
    }

    #[test]
    fn validate_uservar_rejects_empty_fields() {
        assert!(validate_uservar_request(1, "", "x").is_err());
        assert!(validate_uservar_request(1, "wa_event", "").is_err());
    }

    #[test]
    fn validate_uservar_rejects_oversize_payload() {
        // MAX_MESSAGE_SIZE is 131072 (128KB) - must match validate_uservar_request
        const MAX_MESSAGE_SIZE: usize = 131_072;
        let name = "wa_event";
        let value = "a".repeat(MAX_MESSAGE_SIZE);
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
