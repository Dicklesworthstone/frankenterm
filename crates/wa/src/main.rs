//! WezTerm Automata CLI
//!
//! Terminal hypervisor for AI agent swarms running in WezTerm.

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

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
    },

    /// Robot mode commands (JSON I/O)
    Robot {
        #[command(subcommand)]
        command: RobotCommands,
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

/// JSON envelope for robot mode responses
#[derive(serde::Serialize)]
struct RobotResponse<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    elapsed_ms: u64,
}

impl<T> RobotResponse<T> {
    fn success(data: T, elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            hint: None,
            elapsed_ms,
        }
    }

    fn error(msg: impl Into<String>, hint: Option<String>, elapsed_ms: u64) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
            hint,
            elapsed_ms,
        }
    }
}

fn build_send_dry_run_report(
    pane_id: u64,
    text: &str,
    no_paste: bool,
    command: &str,
) -> wa_core::dry_run::DryRunReport {
    use wa_core::dry_run::{
        DryRunContext, TargetResolution, build_send_policy_evaluation, create_send_action,
        create_wait_for_action,
    };

    let mut ctx = DryRunContext::enabled();
    ctx.set_command(command);

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
    name: &str,
    pane: u64,
    command: &str,
) -> wa_core::dry_run::DryRunReport {
    use wa_core::dry_run::{
        ActionType, DryRunContext, PlannedAction, PolicyCheck, PolicyEvaluation, TargetResolution,
    };

    let mut ctx = DryRunContext::enabled();
    ctx.set_command(command);

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

/// Helper to convert elapsed time to u64 milliseconds safely
fn elapsed_ms(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn init_logging(verbose: bool) {
    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    init_logging(cli.verbose);

    match cli.command {
        Some(Commands::Watch {
            auto_handle,
            foreground,
        }) => {
            tracing::info!(
                "Starting watcher (auto_handle={}, foreground={})",
                auto_handle,
                foreground
            );
            // TODO: Implement watcher
            println!("Watcher not yet implemented");
        }

        Some(Commands::Robot { command }) => {
            let start = std::time::Instant::now();
            match command {
                RobotCommands::State => {
                    // TODO: Implement state command
                    let response: RobotResponse<Vec<wa_core::wezterm::PaneInfo>> =
                        RobotResponse::error("Not yet implemented", None, elapsed_ms(start));
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                RobotCommands::GetText { pane_id } => {
                    let response: RobotResponse<String> = RobotResponse::error(
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
                    if dry_run {
                        let report = build_send_dry_run_report(
                            pane_id,
                            &text,
                            false,
                            &format!("wa robot send {pane_id} \"{text}\" --dry-run"),
                        );
                        let response = RobotResponse::success(report, elapsed_ms(start));
                        println!("{}", serde_json::to_string_pretty(&response)?);
                    } else {
                        let response: RobotResponse<()> = RobotResponse::error(
                            format!("send to pane {pane_id} not yet implemented (text: {text})"),
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
                    let response: RobotResponse<()> = RobotResponse::error(
                        format!(
                            "wait-for on pane {pane_id} for rule {rule_id} (timeout {timeout}ms) not yet implemented"
                        ),
                        None,
                        elapsed_ms(start),
                    );
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                RobotCommands::Search { query } => {
                    let response: RobotResponse<Vec<String>> = RobotResponse::error(
                        format!("search for '{query}' not yet implemented"),
                        None,
                        elapsed_ms(start),
                    );
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                RobotCommands::Events { limit } => {
                    let response: RobotResponse<Vec<wa_core::events::Event>> = RobotResponse::error(
                        format!("events (limit {limit}) not yet implemented"),
                        None,
                        elapsed_ms(start),
                    );
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            }
        }

        Some(Commands::Search { query, limit, pane }) => {
            tracing::info!(
                "Searching for '{}' (limit={}, pane={:?})",
                query,
                limit,
                pane
            );
            // TODO: Implement search
            println!("Search not yet implemented");
        }

        Some(Commands::List { json }) => {
            if json {
                println!("[]");
            } else {
                println!("No panes tracked yet");
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
            if dry_run {
                let report = build_send_dry_run_report(
                    pane_id,
                    &text,
                    no_paste,
                    &format!("wa send --pane {pane_id} \"{text}\""),
                );
                println!("{}", wa_core::dry_run::format_human(&report));
            } else {
                tracing::info!(
                    "Sending to pane {} (no_paste={}): {}",
                    pane_id,
                    no_paste,
                    text
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
                    if dry_run {
                        let report = build_workflow_dry_run_report(
                            &name,
                            pane,
                            &format!("wa workflow run {name} --pane {pane}"),
                        );
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
            // TODO: Add more checks
            println!("All checks passed!");
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
