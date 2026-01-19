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

    /// Search captured output
    Search {
        /// Search query (FTS5 syntax)
        query: String,

        /// Limit results
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Filter by pane ID
        #[arg(long)]
        pane: Option<u64>,
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

    /// Show system status
    Status {
        /// Output health check as JSON
        #[arg(long)]
        health: bool,
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
enum RobotCommands {
    /// Show robot help as JSON
    Help,

    /// Get all panes as JSON
    State,

    /// Get text from a pane
    GetText {
        /// Pane ID
        pane_id: u64,
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

    /// Wait for a pattern
    WaitFor {
        /// Pane ID
        pane_id: u64,

        /// Pattern rule ID to wait for
        rule_id: String,

        /// Timeout in milliseconds
        #[arg(long, default_value = "30000")]
        timeout: u64,
    },

    /// Search captured output
    Search {
        /// FTS query
        query: String,
    },

    /// Get recent events
    Events {
        /// Limit
        #[arg(long, default_value = "20")]
        limit: usize,
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
        let status = if self.observed {
            "observed"
        } else {
            "ignored"
        };
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
        ],
        global_flags: vec!["--workspace <path>", "--config <path>", "--verbose"],
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


/// Run the observation watcher daemon.
async fn run_watcher(
    layout: &wa_core::config::WorkspaceLayout,
    config: &wa_core::config::Config,
    auto_handle: bool,
    _foreground: bool,
    poll_interval: u64,
    no_patterns: bool,
    disable_lock: bool,
) -> anyhow::Result<()> {
    use std::time::Duration;
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
        tracing::warn!("Single-instance lock DISABLED - data corruption may occur if multiple watchers run");
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
        overlap_size: 4096, // Default overlap window size
        pane_filter: config.ingest.panes.clone(),
        channel_buffer: 1024,
    };

    // Create and start the observation runtime
    let mut runtime = ObservationRuntime::new(runtime_config, storage, pattern_engine);
    let handle = runtime.start().await?;
    tracing::info!("Observation runtime started");

    // Wait for shutdown signal (SIGINT or SIGTERM)
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;

        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT, initiating graceful shutdown");
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, initiating graceful shutdown");
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
async fn main() -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let robot_mode = args.get(1).is_some_and(|arg| arg == "robot");

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
            let command = command.unwrap_or(RobotCommands::Help);

            match command {
                RobotCommands::Help => {
                    let response = RobotResponse::success(build_robot_help(), elapsed_ms(start));
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
                                    let response = RobotResponse::success(states, elapsed_ms(start));
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
                        RobotCommands::GetText { pane_id } => {
                            let response: RobotResponse<String> = RobotResponse::error_with_code(
                                ROBOT_ERR_NOT_IMPLEMENTED,
                                format!("get-text for pane {pane_id} not yet implemented"),
                                None,
                                elapsed_ms(start),
                            );
                            println!("{}", serde_json::to_string_pretty(&response)?);
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
                            rule_id,
                            timeout,
                        } => {
                            let response: RobotResponse<()> = RobotResponse::error_with_code(
                                ROBOT_ERR_NOT_IMPLEMENTED,
                                format!(
                                    "wait-for on pane {pane_id} for rule {rule_id} (timeout {timeout}ms) not yet implemented"
                                ),
                                None,
                                elapsed_ms(start),
                            );
                            println!("{}", serde_json::to_string_pretty(&response)?);
                        }
                        RobotCommands::Search { query } => {
                            let response: RobotResponse<Vec<String>> =
                                RobotResponse::error_with_code(
                                    ROBOT_ERR_NOT_IMPLEMENTED,
                                    format!("search for '{query}' not yet implemented"),
                                    None,
                                    elapsed_ms(start),
                                );
                            println!("{}", serde_json::to_string_pretty(&response)?);
                        }
                        RobotCommands::Events { limit } => {
                            let response: RobotResponse<Vec<wa_core::events::Event>> =
                                RobotResponse::error_with_code(
                                    ROBOT_ERR_NOT_IMPLEMENTED,
                                    format!("events (limit {limit}) not yet implemented"),
                                    None,
                                    elapsed_ms(start),
                                );
                            println!("{}", serde_json::to_string_pretty(&response)?);
                        }
                        RobotCommands::Help => unreachable!("handled above"),
                    }
                }
            }
        }

        Some(Commands::Search { query, limit, pane }) => {
            let redacted_query = redact_for_output(&query);
            tracing::info!(
                "Searching for '{}' (limit={}, pane={:?})",
                redacted_query,
                limit,
                pane
            );
            // TODO: Implement search
            println!("Search not yet implemented");
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
                println!("Panes ({} observed, {} ignored):", observed_count, ignored_count);
                println!("  {:>4}  {:>10}  {:<20}  {:<40}  {}", "ID", "STATUS", "TITLE", "CWD", "DOMAIN");
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

        Some(Commands::Status { health }) => {
            if health {
                println!(r#"{{"status": "ok", "version": "{}"}}"#, wa_core::VERSION);
            } else {
                println!("wa status: OK");
                println!("version: {}", wa_core::VERSION);
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
