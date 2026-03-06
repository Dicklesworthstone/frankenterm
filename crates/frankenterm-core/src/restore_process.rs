//! Process re-launch engine — restart shells and agent processes in restored panes.
//!
//! After layout restoration creates panes and scrollback injection restores visual
//! content, this module re-launches the original foreground processes so users can
//! resume work.
//!
//! # Safety
//!
//! Shell processes auto-launch. Agent processes (Claude Code, Codex, Gemini) require
//! explicit opt-in because re-launching starts a **new session** — the agent's
//! in-memory state (conversation history, context window, in-flight operations) is
//! permanently lost.
//!
//! # Data flow
//!
//! ```text
//! PaneStateSnapshot (DB) → ProcessPlan → ProcessLauncher → send_text → pane
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::patterns::AgentType;
use crate::session_pane_state::{AgentMetadata, PaneStateSnapshot, ProcessInfo};
use crate::wezterm::WeztermHandle;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for process re-launch behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchConfig {
    /// Automatically re-launch shell processes.
    pub launch_shells: bool,
    /// Automatically re-launch agent processes (requires explicit opt-in).
    pub launch_agents: bool,
    /// Delay between successive launches in milliseconds.
    pub launch_delay_ms: u64,
    /// Custom agent launch commands keyed by agent type.
    ///
    /// Supports `{cwd}` placeholder in command templates.
    /// Example: `claude_code = "cd {cwd} && claude"`
    pub agent_commands: HashMap<String, String>,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            launch_shells: true,
            launch_agents: false,
            launch_delay_ms: 500,
            agent_commands: HashMap::new(),
        }
    }
}

impl From<crate::config::ProcessRelaunchConfig> for LaunchConfig {
    fn from(cfg: crate::config::ProcessRelaunchConfig) -> Self {
        Self {
            launch_shells: cfg.launch_shells,
            launch_agents: cfg.launch_agents,
            launch_delay_ms: cfg.launch_delay_ms,
            agent_commands: cfg.agent_commands,
        }
    }
}

// =============================================================================
// Plan types
// =============================================================================

/// What action to take for a pane during process re-launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum LaunchAction {
    /// Re-launch a shell process (cd to cwd, exec shell).
    LaunchShell { shell: String, cwd: PathBuf },
    /// Re-launch an agent process with a configured command.
    LaunchAgent {
        command: String,
        cwd: PathBuf,
        agent_type: String,
    },
    /// Skip this pane (process cannot or should not be re-launched).
    Skip { reason: String },
    /// Manual hint for the user (process needs manual restart).
    Manual {
        hint: String,
        original_process: String,
    },
}

/// Re-launch plan for a single pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessPlan {
    /// Original pane ID from the snapshot.
    pub old_pane_id: u64,
    /// New pane ID after layout restoration.
    pub new_pane_id: u64,
    /// The action to take.
    pub action: LaunchAction,
    /// Warning about state loss (for agents).
    pub state_warning: Option<String>,
}

/// Result of executing a process plan on a single pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchResult {
    pub old_pane_id: u64,
    pub new_pane_id: u64,
    pub action: LaunchAction,
    pub success: bool,
    pub error: Option<String>,
}

/// Report after executing all process plans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchReport {
    pub results: Vec<LaunchResult>,
    pub shells_launched: usize,
    pub agents_launched: usize,
    pub skipped: usize,
    pub manual: usize,
    pub failed: usize,
}

// =============================================================================
// ProcessLauncher
// =============================================================================

/// Orchestrates re-launching processes in restored panes.
pub struct ProcessLauncher {
    wezterm: WeztermHandle,
    config: LaunchConfig,
}

impl ProcessLauncher {
    /// Create a new process launcher.
    pub fn new(wezterm: WeztermHandle, config: LaunchConfig) -> Self {
        Self { wezterm, config }
    }

    /// Generate a re-launch plan without executing anything.
    ///
    /// The plan maps each pane from the snapshot to an action based on its
    /// captured process info, agent metadata, and the launch configuration.
    pub fn plan(
        &self,
        pane_id_map: &HashMap<u64, u64>,
        pane_states: &[PaneStateSnapshot],
    ) -> Vec<ProcessPlan> {
        let mut plans = Vec::with_capacity(pane_states.len());

        for state in pane_states {
            let new_pane_id = match pane_id_map.get(&state.pane_id) {
                Some(&id) => id,
                None => {
                    debug!(
                        old_pane_id = state.pane_id,
                        "pane not in id map, skipping plan"
                    );
                    continue;
                }
            };

            let (action, state_warning) = self.resolve_action(state);

            plans.push(ProcessPlan {
                old_pane_id: state.pane_id,
                new_pane_id,
                action,
                state_warning,
            });
        }

        plans
    }

    /// Execute a set of process plans, sending commands to panes.
    ///
    /// Plans are executed sequentially with `launch_delay_ms` between each
    /// to prevent resource spikes.
    pub async fn execute(&self, plans: &[ProcessPlan]) -> LaunchReport {
        let mut report = LaunchReport::default();
        let delay = Duration::from_millis(self.config.launch_delay_ms);

        for (i, plan) in plans.iter().enumerate() {
            if i > 0 && !delay.is_zero() {
                crate::runtime_compat::sleep(delay).await;
            }

            let result = match &plan.action {
                LaunchAction::LaunchShell { shell, cwd } => {
                    self.launch_shell(plan.new_pane_id, shell, cwd).await
                }
                LaunchAction::LaunchAgent {
                    command,
                    cwd,
                    agent_type,
                } => {
                    self.launch_agent(plan.new_pane_id, command, cwd, agent_type)
                        .await
                }
                LaunchAction::Skip { reason } => {
                    debug!(
                        pane = plan.new_pane_id,
                        reason = %reason,
                        "skipping pane"
                    );
                    report.skipped += 1;
                    report.results.push(LaunchResult {
                        old_pane_id: plan.old_pane_id,
                        new_pane_id: plan.new_pane_id,
                        action: plan.action.clone(),
                        success: true,
                        error: None,
                    });
                    continue;
                }
                LaunchAction::Manual { hint, .. } => {
                    info!(
                        pane = plan.new_pane_id,
                        hint = %hint,
                        "manual restart required"
                    );
                    report.manual += 1;
                    report.results.push(LaunchResult {
                        old_pane_id: plan.old_pane_id,
                        new_pane_id: plan.new_pane_id,
                        action: plan.action.clone(),
                        success: true,
                        error: None,
                    });
                    continue;
                }
            };

            match result {
                Ok(()) => {
                    match &plan.action {
                        LaunchAction::LaunchShell { .. } => report.shells_launched += 1,
                        LaunchAction::LaunchAgent { .. } => report.agents_launched += 1,
                        _ => {}
                    }
                    report.results.push(LaunchResult {
                        old_pane_id: plan.old_pane_id,
                        new_pane_id: plan.new_pane_id,
                        action: plan.action.clone(),
                        success: true,
                        error: None,
                    });
                }
                Err(e) => {
                    warn!(
                        pane = plan.new_pane_id,
                        error = %e,
                        "process launch failed"
                    );
                    report.failed += 1;
                    report.results.push(LaunchResult {
                        old_pane_id: plan.old_pane_id,
                        new_pane_id: plan.new_pane_id,
                        action: plan.action.clone(),
                        success: false,
                        error: Some(e),
                    });
                }
            }
        }

        info!(
            shells = report.shells_launched,
            agents = report.agents_launched,
            skipped = report.skipped,
            manual = report.manual,
            failed = report.failed,
            "process re-launch complete"
        );

        report
    }

    // -------------------------------------------------------------------------
    // Internal: action resolution
    // -------------------------------------------------------------------------

    /// Determine what action to take for a pane based on its snapshot.
    fn resolve_action(&self, state: &PaneStateSnapshot) -> (LaunchAction, Option<String>) {
        let cwd = state
            .cwd
            .as_deref()
            .map(normalize_cwd)
            .unwrap_or_else(|| PathBuf::from("/"));

        // Check for agent metadata first
        if let Some(ref agent) = state.agent {
            return self.resolve_agent_action(agent, &cwd);
        }

        // Check foreground process
        if let Some(ref process) = state.foreground_process {
            return self.resolve_process_action(process, &cwd);
        }

        // Check shell field
        if let Some(ref shell) = state.shell {
            if self.config.launch_shells {
                return (
                    LaunchAction::LaunchShell {
                        shell: shell.clone(),
                        cwd,
                    },
                    None,
                );
            }
            return (
                LaunchAction::Skip {
                    reason: "shell launch disabled".into(),
                },
                None,
            );
        }

        // No process info at all — just cd to the working directory
        if self.config.launch_shells && *cwd != *"/" {
            return (
                LaunchAction::LaunchShell {
                    shell: default_shell(),
                    cwd,
                },
                None,
            );
        }

        (
            LaunchAction::Skip {
                reason: "no process information available".into(),
            },
            None,
        )
    }

    /// Resolve action for a pane with known agent metadata.
    fn resolve_agent_action(
        &self,
        agent: &AgentMetadata,
        cwd: &Path,
    ) -> (LaunchAction, Option<String>) {
        let agent_type = parse_agent_type(&agent.agent_type);
        let warning = Some(format!(
            "Agent {} will start a NEW session. \
             Conversation history, context, and in-flight work are lost.",
            agent.agent_type
        ));

        if !self.config.launch_agents {
            return (
                LaunchAction::Manual {
                    hint: format!(
                        "Was running {} in {}. Use --launch-agents to auto-restart.",
                        agent.agent_type,
                        cwd.display()
                    ),
                    original_process: agent.agent_type.clone(),
                },
                warning,
            );
        }

        // Check for custom command template
        if let Some(template) = self.config.agent_commands.get(&agent.agent_type) {
            let command = template.replace("{cwd}", &cwd.to_string_lossy());
            return (
                LaunchAction::LaunchAgent {
                    command,
                    cwd: cwd.to_path_buf(),
                    agent_type: agent.agent_type.clone(),
                },
                warning,
            );
        }

        // Use default command for known agent types
        if let Some(cmd) = default_agent_command(agent_type, cwd) {
            return (
                LaunchAction::LaunchAgent {
                    command: cmd,
                    cwd: cwd.to_path_buf(),
                    agent_type: agent.agent_type.clone(),
                },
                warning,
            );
        }

        (
            LaunchAction::Manual {
                hint: format!(
                    "Unknown agent type '{}' in {}. Configure in [snapshots.agent_commands].",
                    agent.agent_type,
                    cwd.display()
                ),
                original_process: agent.agent_type.clone(),
            },
            warning,
        )
    }

    /// Resolve action based on foreground process info.
    fn resolve_process_action(
        &self,
        process: &ProcessInfo,
        cwd: &Path,
    ) -> (LaunchAction, Option<String>) {
        let name = &process.name;

        // Detect agent processes by name
        let agent_type = agent_type_from_process_name(name);
        if agent_type != AgentType::Unknown {
            let agent_str = format!("{agent_type}");
            let meta = AgentMetadata {
                agent_type: agent_str.clone(),
                session_id: None,
                state: None,
            };
            return self.resolve_agent_action(&meta, cwd);
        }

        // Common shells
        if is_shell(name) {
            if self.config.launch_shells {
                return (
                    LaunchAction::LaunchShell {
                        shell: name.clone(),
                        cwd: cwd.to_path_buf(),
                    },
                    None,
                );
            }
            return (
                LaunchAction::Skip {
                    reason: "shell launch disabled".into(),
                },
                None,
            );
        }

        // Interactive programs that need manual restart
        if is_interactive_program(name) {
            return (
                LaunchAction::Manual {
                    hint: format!("Was running {name} in {}. Restart manually.", cwd.display()),
                    original_process: name.clone(),
                },
                None,
            );
        }

        // Unknown process — provide a hint
        let argv_hint = process
            .argv
            .as_ref()
            .map(|args| args.join(" "))
            .unwrap_or_else(|| name.clone());

        (
            LaunchAction::Manual {
                hint: format!("Was running: {argv_hint} in {}", cwd.display()),
                original_process: name.clone(),
            },
            None,
        )
    }

    // -------------------------------------------------------------------------
    // Internal: execution
    // -------------------------------------------------------------------------

    /// Send shell launch commands to a pane.
    async fn launch_shell(&self, pane_id: u64, shell: &str, cwd: &Path) -> Result<(), String> {
        // cd to the working directory first
        let cd_cmd = format!("cd {}\r", shell_escape(cwd));
        self.wezterm
            .send_text(pane_id, &cd_cmd)
            .await
            .map_err(|e| format!("send cd: {e}"))?;

        // Small delay to let the cd complete
        crate::runtime_compat::sleep(Duration::from_millis(50)).await;

        // If the shell is different from default, exec it
        let current_shell = default_shell();
        if shell != current_shell && !shell.is_empty() {
            let exec_cmd = format!("exec {shell}\r");
            self.wezterm
                .send_text(pane_id, &exec_cmd)
                .await
                .map_err(|e| format!("send exec: {e}"))?;
        }

        info!(pane = pane_id, shell = %shell, cwd = %cwd.display(), "shell launched");
        Ok(())
    }

    /// Send agent launch command to a pane.
    async fn launch_agent(
        &self,
        pane_id: u64,
        command: &str,
        cwd: &Path,
        agent_type: &str,
    ) -> Result<(), String> {
        // The command template may include cd, but ensure we're in the right dir
        let full_cmd = format!("{command}\r");
        self.wezterm
            .send_text(pane_id, &full_cmd)
            .await
            .map_err(|e| format!("send agent command: {e}"))?;

        info!(
            pane = pane_id,
            agent = %agent_type,
            cwd = %cwd.display(),
            "agent launched"
        );
        Ok(())
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Normalize a CWD string (strip file:// URI prefix, decode percent-encoding).
fn normalize_cwd(cwd: &str) -> PathBuf {
    let path = if let Some(stripped) = cwd.strip_prefix("file://") {
        // Strip optional hostname (file://hostname/path or file:///path)
        if let Some(abs) = stripped.strip_prefix("localhost") {
            abs
        } else if stripped.starts_with('/') {
            stripped
        } else {
            // file://hostname/path → /path
            stripped.find('/').map_or(stripped, |idx| &stripped[idx..])
        }
    } else {
        cwd
    };

    PathBuf::from(percent_decode(path))
}

/// Simple percent-decoding for common path characters.
fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Escape a path for use in a shell command.
fn shell_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains(|c: char| c.is_whitespace() || "\"'$`!#&|;(){}[]<>?*~\\".contains(c)) {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.into_owned()
    }
}

/// Get the default shell for the platform.
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
}

/// Check if a process name is a known shell.
fn is_shell(name: &str) -> bool {
    let basename = name.rsplit('/').next().unwrap_or(name);
    matches!(
        basename,
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "nushell"
    )
}

/// Check if a process name is a known interactive program that needs manual restart.
fn is_interactive_program(name: &str) -> bool {
    let basename = name.rsplit('/').next().unwrap_or(name);
    matches!(
        basename,
        "vim"
            | "nvim"
            | "vi"
            | "nano"
            | "emacs"
            | "helix"
            | "hx"
            | "htop"
            | "btop"
            | "top"
            | "less"
            | "more"
            | "man"
            | "tmux"
            | "screen"
            | "python"
            | "python3"
            | "ipython"
            | "node"
            | "irb"
            | "ghci"
            | "psql"
            | "mysql"
            | "sqlite3"
    )
}

/// Detect agent type from a process name.
fn agent_type_from_process_name(name: &str) -> AgentType {
    let basename = name.rsplit('/').next().unwrap_or(name);
    match basename {
        "claude" | "claude-code" => AgentType::ClaudeCode,
        "codex" | "codex-cli" => AgentType::Codex,
        "gemini" | "gemini-cli" => AgentType::Gemini,
        _ => AgentType::Unknown,
    }
}

/// Parse an agent type string back to the enum.
fn parse_agent_type(s: &str) -> AgentType {
    match s {
        "claude_code" | "ClaudeCode" => AgentType::ClaudeCode,
        "codex" | "Codex" => AgentType::Codex,
        "gemini" | "Gemini" => AgentType::Gemini,
        _ => AgentType::Unknown,
    }
}

/// Get the default launch command for a known agent type.
fn default_agent_command(agent_type: AgentType, cwd: &Path) -> Option<String> {
    let cwd_escaped = shell_escape(cwd);
    match agent_type {
        AgentType::ClaudeCode => Some(format!("cd {cwd_escaped} && claude")),
        AgentType::Codex => Some(format!("cd {cwd_escaped} && codex")),
        AgentType::Gemini => Some(format!("cd {cwd_escaped} && gemini-cli")),
        AgentType::Wezterm | AgentType::Unknown => None,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_pane_state::TerminalState;

    /// Create a minimal `PaneStateSnapshot` for testing.
    fn test_pane_state(pane_id: u64) -> PaneStateSnapshot {
        PaneStateSnapshot {
            schema_version: 1,
            pane_id,
            captured_at: 1_000_000,
            cwd: Some("/home/user/project".into()),
            foreground_process: None,
            shell: Some("bash".into()),
            terminal: TerminalState {
                rows: 24,
                cols: 80,
                cursor_row: 0,
                cursor_col: 0,
                is_alt_screen: false,
                title: String::new(),
            },
            scrollback_ref: None,
            agent: None,
            env: None,
        }
    }

    fn test_launcher() -> ProcessLauncher {
        let wez = crate::wezterm::mock_wezterm_handle();
        ProcessLauncher::new(wez, LaunchConfig::default())
    }

    fn test_pane_id_map() -> HashMap<u64, u64> {
        let mut map = HashMap::new();
        map.insert(1, 100);
        map.insert(2, 200);
        map.insert(3, 300);
        map
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build restore_process test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn plan_shell_pane() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();
        let states = vec![test_pane_state(1)];

        let plans = launcher.plan(&id_map, &states);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].old_pane_id, 1);
        assert_eq!(plans[0].new_pane_id, 100);
        assert!(plans[0].state_warning.is_none());

        match &plans[0].action {
            LaunchAction::LaunchShell { shell, cwd } => {
                assert_eq!(shell, "bash");
                assert_eq!(cwd, &PathBuf::from("/home/user/project"));
            }
            other => panic!("expected LaunchShell, got {other:?}"),
        }
    }

    #[test]
    fn plan_agent_pane_no_opt_in() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "claude_code".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].state_warning.is_some());

        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("claude_code"));
                assert!(hint.contains("--launch-agents"));
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_agent_pane_with_opt_in() {
        let config = LaunchConfig {
            launch_agents: true,
            ..Default::default()
        };
        let wez = crate::wezterm::mock_wezterm_handle();
        let launcher = ProcessLauncher::new(wez, config);
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "claude_code".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].state_warning.is_some());

        match &plans[0].action {
            LaunchAction::LaunchAgent {
                command,
                agent_type,
                ..
            } => {
                assert!(command.contains("claude"));
                assert_eq!(agent_type, "claude_code");
            }
            other => panic!("expected LaunchAgent, got {other:?}"),
        }
    }

    #[test]
    fn plan_agent_with_custom_command() {
        let mut agent_commands = HashMap::new();
        agent_commands.insert("claude_code".into(), "cd {cwd} && claude --resume".into());
        let config = LaunchConfig {
            launch_agents: true,
            agent_commands,
            ..Default::default()
        };
        let wez = crate::wezterm::mock_wezterm_handle();
        let launcher = ProcessLauncher::new(wez, config);
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "claude_code".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::LaunchAgent { command, .. } => {
                assert!(command.contains("--resume"));
                assert!(command.contains("/home/user/project"));
            }
            other => panic!("expected LaunchAgent, got {other:?}"),
        }
    }

    #[test]
    fn plan_interactive_program() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = Some(ProcessInfo {
            name: "vim".into(),
            pid: Some(1234),
            argv: Some(vec!["vim".into(), "src/main.rs".into()]),
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual {
                hint,
                original_process,
            } => {
                assert!(hint.contains("vim"));
                assert_eq!(original_process, "vim");
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_process_detected_as_agent() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = Some(ProcessInfo {
            name: "claude".into(),
            pid: Some(5678),
            argv: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        // Agents default to Manual without opt-in
        assert!(plans[0].state_warning.is_some());
        matches!(&plans[0].action, LaunchAction::Manual { .. });
    }

    #[test]
    fn plan_skips_unmapped_panes() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();
        // Pane ID 999 is not in the map
        let states = vec![test_pane_state(999)];

        let plans = launcher.plan(&id_map, &states);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_multiple_panes() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut states = vec![test_pane_state(1), test_pane_state(2)];
        states[1].pane_id = 2;
        states[1].cwd = Some("/tmp".into());

        let plans = launcher.plan(&id_map, &states);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].new_pane_id, 100);
        assert_eq!(plans[1].new_pane_id, 200);
    }

    #[test]
    fn plan_shells_disabled() {
        let config = LaunchConfig {
            launch_shells: false,
            ..Default::default()
        };
        let wez = crate::wezterm::mock_wezterm_handle();
        let launcher = ProcessLauncher::new(wez, config);
        let id_map = test_pane_id_map();
        let states = vec![test_pane_state(1)];

        let plans = launcher.plan(&id_map, &states);
        match &plans[0].action {
            LaunchAction::Skip { reason } => {
                assert!(reason.contains("disabled"));
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn plan_no_process_info() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = None;

        let plans = launcher.plan(&id_map, &[state]);
        // Should still launch a shell to cd to the working dir
        match &plans[0].action {
            LaunchAction::LaunchShell { cwd, .. } => {
                assert_eq!(cwd, &PathBuf::from("/home/user/project"));
            }
            other => panic!("expected LaunchShell, got {other:?}"),
        }
    }

    #[test]
    fn normalize_cwd_file_uri() {
        assert_eq!(
            normalize_cwd("file:///home/user/project"),
            PathBuf::from("/home/user/project")
        );
        assert_eq!(
            normalize_cwd("file://localhost/home/user"),
            PathBuf::from("/home/user")
        );
        assert_eq!(
            normalize_cwd("/home/user/plain"),
            PathBuf::from("/home/user/plain")
        );
    }

    #[test]
    fn normalize_cwd_percent_encoded() {
        assert_eq!(
            normalize_cwd("file:///home/user/my%20project"),
            PathBuf::from("/home/user/my project")
        );
    }

    #[test]
    fn shell_escape_plain() {
        assert_eq!(shell_escape(&PathBuf::from("/foo/bar")), "/foo/bar");
    }

    #[test]
    fn shell_escape_spaces() {
        assert_eq!(
            shell_escape(&PathBuf::from("/foo/my project")),
            "'/foo/my project'"
        );
    }

    #[test]
    fn is_shell_detection() {
        assert!(is_shell("bash"));
        assert!(is_shell("/usr/bin/zsh"));
        assert!(is_shell("fish"));
        assert!(!is_shell("vim"));
        assert!(!is_shell("claude"));
    }

    #[test]
    fn agent_type_detection() {
        assert_eq!(
            agent_type_from_process_name("claude"),
            AgentType::ClaudeCode
        );
        assert_eq!(agent_type_from_process_name("codex-cli"), AgentType::Codex);
        assert_eq!(
            agent_type_from_process_name("gemini-cli"),
            AgentType::Gemini
        );
        assert_eq!(agent_type_from_process_name("bash"), AgentType::Unknown);
    }

    #[test]
    fn default_agent_commands_populated() {
        let cwd = PathBuf::from("/project");
        assert!(
            default_agent_command(AgentType::ClaudeCode, &cwd)
                .unwrap()
                .contains("claude")
        );
        assert!(
            default_agent_command(AgentType::Codex, &cwd)
                .unwrap()
                .contains("codex")
        );
        assert!(default_agent_command(AgentType::Unknown, &cwd).is_none());
    }

    #[test]
    fn plan_deterministic() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let states: Vec<_> = (1..=3).map(test_pane_state).collect();

        let plans1 = launcher.plan(&id_map, &states);
        let plans2 = launcher.plan(&id_map, &states);

        assert_eq!(plans1.len(), plans2.len());
        for (a, b) in plans1.iter().zip(plans2.iter()) {
            assert_eq!(a.old_pane_id, b.old_pane_id);
            assert_eq!(a.new_pane_id, b.new_pane_id);
            assert_eq!(a.action, b.action);
            assert_eq!(a.state_warning, b.state_warning);
        }
    }

    #[test]
    fn all_cwds_absolute() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut states = vec![test_pane_state(1), test_pane_state(2), test_pane_state(3)];
        states[1].pane_id = 2;
        states[2].pane_id = 3;
        states[2].cwd = None; // No cwd

        let plans = launcher.plan(&id_map, &states);
        for plan in &plans {
            match &plan.action {
                LaunchAction::LaunchShell { cwd, .. } | LaunchAction::LaunchAgent { cwd, .. } => {
                    assert!(
                        cwd.is_absolute(),
                        "cwd must be absolute, got: {}",
                        cwd.display()
                    );
                }
                _ => {}
            }
        }
    }

    /// Create a mock with panes pre-registered at the given IDs.
    async fn mock_with_panes(pane_ids: &[u64]) -> WeztermHandle {
        let mock = crate::wezterm::MockWezterm::new();
        for &id in pane_ids {
            mock.add_default_pane(id).await;
        }
        std::sync::Arc::new(mock) as WeztermHandle
    }

    #[test]
    fn execute_shell_launch() {
        run_async_test(async {
            let wez = mock_with_panes(&[100]).await;
            let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
            let plans = vec![ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::LaunchShell {
                    shell: "bash".into(),
                    cwd: PathBuf::from("/home/user"),
                },
                state_warning: None,
            }];

            let report = launcher.execute(&plans).await;
            assert_eq!(report.shells_launched, 1);
            assert_eq!(report.failed, 0);
        });
    }

    #[test]
    fn execute_mixed_plan() {
        run_async_test(async {
            let wez = mock_with_panes(&[100, 200, 300]).await;
            let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
            let plans = vec![
                ProcessPlan {
                    old_pane_id: 1,
                    new_pane_id: 100,
                    action: LaunchAction::LaunchShell {
                        shell: "zsh".into(),
                        cwd: PathBuf::from("/project"),
                    },
                    state_warning: None,
                },
                ProcessPlan {
                    old_pane_id: 2,
                    new_pane_id: 200,
                    action: LaunchAction::Skip {
                        reason: "no process info".into(),
                    },
                    state_warning: None,
                },
                ProcessPlan {
                    old_pane_id: 3,
                    new_pane_id: 300,
                    action: LaunchAction::Manual {
                        hint: "Was running vim".into(),
                        original_process: "vim".into(),
                    },
                    state_warning: None,
                },
            ];

            let report = launcher.execute(&plans).await;
            assert_eq!(report.shells_launched, 1);
            assert_eq!(report.skipped, 1);
            assert_eq!(report.manual, 1);
            assert_eq!(report.failed, 0);
            assert_eq!(report.results.len(), 3);
        });
    }

    // =========================================================================
    // LaunchConfig — defaults & serde
    // =========================================================================

    #[test]
    fn launch_config_default_values() {
        let cfg = LaunchConfig::default();
        assert!(cfg.launch_shells);
        assert!(!cfg.launch_agents);
        assert_eq!(cfg.launch_delay_ms, 500);
        assert!(cfg.agent_commands.is_empty());
    }

    #[test]
    fn launch_config_serde_roundtrip() {
        let mut commands = HashMap::new();
        commands.insert("claude_code".into(), "cd {cwd} && claude --resume".into());
        let cfg = LaunchConfig {
            launch_shells: false,
            launch_agents: true,
            launch_delay_ms: 250,
            agent_commands: commands,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg2.launch_shells, false);
        assert_eq!(cfg2.launch_agents, true);
        assert_eq!(cfg2.launch_delay_ms, 250);
        assert_eq!(
            cfg2.agent_commands.get("claude_code").unwrap(),
            "cd {cwd} && claude --resume"
        );
    }

    #[test]
    fn launch_config_clone() {
        let cfg = LaunchConfig {
            launch_shells: false,
            launch_agents: true,
            launch_delay_ms: 100,
            agent_commands: HashMap::new(),
        };
        let cfg2 = cfg.clone();
        assert_eq!(cfg2.launch_shells, cfg.launch_shells);
        assert_eq!(cfg2.launch_agents, cfg.launch_agents);
        assert_eq!(cfg2.launch_delay_ms, cfg.launch_delay_ms);
    }

    // =========================================================================
    // LaunchAction — serde tagged enum
    // =========================================================================

    #[test]
    fn launch_action_serde_launch_shell() {
        let action = LaunchAction::LaunchShell {
            shell: "zsh".into(),
            cwd: PathBuf::from("/tmp"),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"action\":\"launch_shell\""));
        let roundtrip: LaunchAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    #[test]
    fn launch_action_serde_launch_agent() {
        let action = LaunchAction::LaunchAgent {
            command: "cd /proj && claude".into(),
            cwd: PathBuf::from("/proj"),
            agent_type: "claude_code".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"action\":\"launch_agent\""));
        let roundtrip: LaunchAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    #[test]
    fn launch_action_serde_skip() {
        let action = LaunchAction::Skip {
            reason: "no info".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"action\":\"skip\""));
        let roundtrip: LaunchAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    #[test]
    fn launch_action_serde_manual() {
        let action = LaunchAction::Manual {
            hint: "Was running vim".into(),
            original_process: "vim".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"action\":\"manual\""));
        let roundtrip: LaunchAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    #[test]
    fn launch_action_equality() {
        let a = LaunchAction::LaunchShell {
            shell: "bash".into(),
            cwd: PathBuf::from("/home"),
        };
        let b = LaunchAction::LaunchShell {
            shell: "bash".into(),
            cwd: PathBuf::from("/home"),
        };
        let c = LaunchAction::LaunchShell {
            shell: "zsh".into(),
            cwd: PathBuf::from("/home"),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);

        let skip1 = LaunchAction::Skip { reason: "x".into() };
        let skip2 = LaunchAction::Skip { reason: "y".into() };
        assert_ne!(skip1, skip2);
    }

    // =========================================================================
    // ProcessPlan / LaunchResult / LaunchReport — serde
    // =========================================================================

    #[test]
    fn process_plan_serde_roundtrip() {
        let plan = ProcessPlan {
            old_pane_id: 42,
            new_pane_id: 100,
            action: LaunchAction::LaunchShell {
                shell: "fish".into(),
                cwd: PathBuf::from("/data"),
            },
            state_warning: Some("careful!".into()),
        };
        let json = serde_json::to_string(&plan).unwrap();
        let plan2: ProcessPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan2.old_pane_id, 42);
        assert_eq!(plan2.new_pane_id, 100);
        assert_eq!(plan2.state_warning.as_deref(), Some("careful!"));
        assert_eq!(plan2.action, plan.action);
    }

    #[test]
    fn launch_result_serde_roundtrip() {
        let result = LaunchResult {
            old_pane_id: 1,
            new_pane_id: 10,
            action: LaunchAction::Skip {
                reason: "test".into(),
            },
            success: true,
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let result2: LaunchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result2.old_pane_id, 1);
        assert_eq!(result2.new_pane_id, 10);
        assert!(result2.success);
        assert!(result2.error.is_none());
    }

    #[test]
    fn launch_result_with_error() {
        let result = LaunchResult {
            old_pane_id: 5,
            new_pane_id: 50,
            action: LaunchAction::LaunchAgent {
                command: "claude".into(),
                cwd: PathBuf::from("/x"),
                agent_type: "claude_code".into(),
            },
            success: false,
            error: Some("connection refused".into()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let result2: LaunchResult = serde_json::from_str(&json).unwrap();
        assert!(!result2.success);
        assert_eq!(result2.error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn launch_report_default() {
        let report = LaunchReport::default();
        assert!(report.results.is_empty());
        assert_eq!(report.shells_launched, 0);
        assert_eq!(report.agents_launched, 0);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.manual, 0);
        assert_eq!(report.failed, 0);
    }

    #[test]
    fn launch_report_serde_roundtrip() {
        let report = LaunchReport {
            results: vec![LaunchResult {
                old_pane_id: 1,
                new_pane_id: 10,
                action: LaunchAction::Skip { reason: "r".into() },
                success: true,
                error: None,
            }],
            shells_launched: 3,
            agents_launched: 1,
            skipped: 2,
            manual: 1,
            failed: 0,
        };
        let json = serde_json::to_string(&report).unwrap();
        let report2: LaunchReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report2.shells_launched, 3);
        assert_eq!(report2.agents_launched, 1);
        assert_eq!(report2.results.len(), 1);
    }

    // =========================================================================
    // normalize_cwd — edge cases
    // =========================================================================

    #[test]
    fn normalize_cwd_bare_path() {
        assert_eq!(
            normalize_cwd("/usr/local/bin"),
            PathBuf::from("/usr/local/bin")
        );
    }

    #[test]
    fn normalize_cwd_file_triple_slash() {
        assert_eq!(normalize_cwd("file:///var/log"), PathBuf::from("/var/log"));
    }

    #[test]
    fn normalize_cwd_file_hostname_path() {
        // file://myhost/share/data → /share/data
        assert_eq!(
            normalize_cwd("file://myhost/share/data"),
            PathBuf::from("/share/data")
        );
    }

    #[test]
    fn normalize_cwd_multiple_percent_encoded() {
        assert_eq!(
            normalize_cwd("file:///home/user/my%20big%20project"),
            PathBuf::from("/home/user/my big project")
        );
    }

    #[test]
    fn normalize_cwd_root_only() {
        assert_eq!(normalize_cwd("/"), PathBuf::from("/"));
    }

    #[test]
    fn normalize_cwd_empty_string() {
        // Empty string → empty path
        assert_eq!(normalize_cwd(""), PathBuf::from(""));
    }

    // =========================================================================
    // percent_decode — edge cases
    // =========================================================================

    #[test]
    fn percent_decode_empty() {
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn percent_decode_no_encoding() {
        assert_eq!(percent_decode("hello world"), "hello world");
    }

    #[test]
    fn percent_decode_space() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
    }

    #[test]
    fn percent_decode_multiple_sequences() {
        assert_eq!(percent_decode("a%20b%20c%20d"), "a b c d");
    }

    #[test]
    fn percent_decode_special_chars() {
        // %23 = '#', %26 = '&', %3D = '='
        assert_eq!(
            percent_decode("key%3Dvalue%26other%23tag"),
            "key=value&other#tag"
        );
    }

    #[test]
    fn percent_decode_invalid_hex() {
        // Invalid hex after % → preserved as-is
        assert_eq!(percent_decode("100%XY"), "100%XY");
    }

    #[test]
    fn percent_decode_trailing_percent() {
        // Trailing % with nothing after → incomplete but we still get output
        let result = percent_decode("test%");
        // With only 0 chars taken, from_str_radix("", 16) fails → preserved
        assert!(result.contains("test"));
    }

    #[test]
    fn percent_decode_single_char_after_percent() {
        // Only 1 hex char after % → from_str_radix with 1 char
        let result = percent_decode("test%4");
        // With only 1 char, it may parse as 4 (valid hex) or error depending on take(2)
        assert!(result.starts_with("test"));
    }

    // =========================================================================
    // shell_escape — additional special characters
    // =========================================================================

    #[test]
    fn shell_escape_dollar() {
        let result = shell_escape(&PathBuf::from("/home/$USER"));
        assert!(result.starts_with('\''));
        assert!(result.contains("$USER"));
    }

    #[test]
    fn shell_escape_backtick() {
        let result = shell_escape(&PathBuf::from("/foo/`bar`"));
        assert!(result.starts_with('\''));
    }

    #[test]
    fn shell_escape_exclamation() {
        let result = shell_escape(&PathBuf::from("/foo/bar!"));
        assert!(result.starts_with('\''));
    }

    #[test]
    fn shell_escape_ampersand() {
        let result = shell_escape(&PathBuf::from("/foo&bar"));
        assert!(result.starts_with('\''));
    }

    #[test]
    fn shell_escape_hash() {
        let result = shell_escape(&PathBuf::from("/foo#bar"));
        assert!(result.starts_with('\''));
    }

    #[test]
    fn shell_escape_quotes_in_path() {
        let result = shell_escape(&PathBuf::from("/foo/it's"));
        assert!(result.starts_with('\''));
    }

    #[test]
    fn shell_escape_double_quote_escaped() {
        let path = PathBuf::from("/foo/\"bar\"");
        let result = shell_escape(&path);
        // Double quotes inside single quotes do not need escaping
        assert!(result.contains("\"bar\""));
    }

    #[test]
    fn shell_escape_parentheses() {
        let result = shell_escape(&PathBuf::from("/foo/(copy)"));
        assert!(result.starts_with('\''));
    }

    #[test]
    fn shell_escape_tilde() {
        let result = shell_escape(&PathBuf::from("/foo/~backup"));
        assert!(result.starts_with('\''));
    }

    // =========================================================================
    // is_shell — comprehensive coverage
    // =========================================================================

    #[test]
    fn is_shell_all_recognized_names() {
        let shells = [
            "bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "csh", "nu", "nushell",
        ];
        for shell in &shells {
            assert!(
                is_shell(shell),
                "expected {} to be detected as shell",
                shell
            );
        }
    }

    #[test]
    fn is_shell_with_full_paths() {
        assert!(is_shell("/usr/bin/bash"));
        assert!(is_shell("/bin/zsh"));
        assert!(is_shell("/usr/local/bin/fish"));
        assert!(is_shell("/usr/bin/nu"));
    }

    #[test]
    fn is_shell_rejects_non_shells() {
        assert!(!is_shell("basher"));
        assert!(!is_shell("zshrc"));
        assert!(!is_shell("fishing"));
        assert!(!is_shell("vim"));
        assert!(!is_shell("claude"));
        assert!(!is_shell("cargo"));
        assert!(!is_shell(""));
    }

    // =========================================================================
    // is_interactive_program — comprehensive coverage
    // =========================================================================

    #[test]
    fn is_interactive_all_recognized_programs() {
        let programs = [
            "vim", "nvim", "vi", "nano", "emacs", "helix", "hx", "htop", "btop", "top", "less",
            "more", "man", "tmux", "screen", "python", "python3", "ipython", "node", "irb", "ghci",
            "psql", "mysql", "sqlite3",
        ];
        for prog in &programs {
            assert!(
                is_interactive_program(prog),
                "expected {} to be detected as interactive",
                prog
            );
        }
    }

    #[test]
    fn is_interactive_with_paths() {
        assert!(is_interactive_program("/usr/bin/vim"));
        assert!(is_interactive_program("/usr/local/bin/nvim"));
        assert!(is_interactive_program("/usr/bin/python3"));
    }

    #[test]
    fn is_interactive_rejects_non_interactive() {
        assert!(!is_interactive_program("bash"));
        assert!(!is_interactive_program("cargo"));
        assert!(!is_interactive_program("gcc"));
        assert!(!is_interactive_program("ls"));
        assert!(!is_interactive_program("cat"));
    }

    // =========================================================================
    // agent_type_from_process_name — comprehensive
    // =========================================================================

    #[test]
    fn agent_type_all_recognized_names() {
        assert_eq!(
            agent_type_from_process_name("claude"),
            AgentType::ClaudeCode
        );
        assert_eq!(
            agent_type_from_process_name("claude-code"),
            AgentType::ClaudeCode
        );
        assert_eq!(agent_type_from_process_name("codex"), AgentType::Codex);
        assert_eq!(agent_type_from_process_name("codex-cli"), AgentType::Codex);
        assert_eq!(agent_type_from_process_name("gemini"), AgentType::Gemini);
        assert_eq!(
            agent_type_from_process_name("gemini-cli"),
            AgentType::Gemini
        );
    }

    #[test]
    fn agent_type_with_full_paths() {
        assert_eq!(
            agent_type_from_process_name("/usr/local/bin/claude"),
            AgentType::ClaudeCode
        );
        assert_eq!(
            agent_type_from_process_name("/home/user/.local/bin/codex"),
            AgentType::Codex
        );
        assert_eq!(
            agent_type_from_process_name("/opt/bin/gemini"),
            AgentType::Gemini
        );
    }

    #[test]
    fn agent_type_unknown_names() {
        assert_eq!(agent_type_from_process_name("bash"), AgentType::Unknown);
        assert_eq!(agent_type_from_process_name("vim"), AgentType::Unknown);
        assert_eq!(agent_type_from_process_name(""), AgentType::Unknown);
        assert_eq!(agent_type_from_process_name("gpt"), AgentType::Unknown);
    }

    // =========================================================================
    // parse_agent_type — all mappings
    // =========================================================================

    #[test]
    fn parse_agent_type_all_variants() {
        assert_eq!(parse_agent_type("claude_code"), AgentType::ClaudeCode);
        assert_eq!(parse_agent_type("ClaudeCode"), AgentType::ClaudeCode);
        assert_eq!(parse_agent_type("codex"), AgentType::Codex);
        assert_eq!(parse_agent_type("Codex"), AgentType::Codex);
        assert_eq!(parse_agent_type("gemini"), AgentType::Gemini);
        assert_eq!(parse_agent_type("Gemini"), AgentType::Gemini);
    }

    #[test]
    fn parse_agent_type_unknown_strings() {
        assert_eq!(parse_agent_type(""), AgentType::Unknown);
        assert_eq!(parse_agent_type("gpt4"), AgentType::Unknown);
        assert_eq!(parse_agent_type("CLAUDE_CODE"), AgentType::Unknown);
        assert_eq!(parse_agent_type("wezterm"), AgentType::Unknown);
    }

    // =========================================================================
    // default_agent_command — comprehensive
    // =========================================================================

    #[test]
    fn default_agent_command_gemini() {
        let cwd = PathBuf::from("/project");
        let cmd = default_agent_command(AgentType::Gemini, &cwd).unwrap();
        assert!(cmd.contains("gemini-cli"));
        assert!(cmd.contains("/project"));
    }

    #[test]
    fn default_agent_command_wezterm_returns_none() {
        let cwd = PathBuf::from("/project");
        assert!(default_agent_command(AgentType::Wezterm, &cwd).is_none());
    }

    #[test]
    fn default_agent_command_unknown_returns_none() {
        let cwd = PathBuf::from("/project");
        assert!(default_agent_command(AgentType::Unknown, &cwd).is_none());
    }

    #[test]
    fn default_agent_command_escapes_spaces_in_path() {
        let cwd = PathBuf::from("/my project/code");
        let cmd = default_agent_command(AgentType::ClaudeCode, &cwd).unwrap();
        // The path should be quoted
        assert!(cmd.contains('"'));
        assert!(cmd.contains("claude"));
    }

    // =========================================================================
    // resolve_action — additional edge cases
    // =========================================================================

    #[test]
    fn plan_no_info_shells_disabled() {
        let config = LaunchConfig {
            launch_shells: false,
            ..Default::default()
        };
        let wez = crate::wezterm::mock_wezterm_handle();
        let launcher = ProcessLauncher::new(wez, config);
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = None;
        state.cwd = Some("/home/user/code".into());

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Skip { reason } => {
                assert!(reason.contains("no process information"));
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn plan_foreground_shell_process() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = Some(ProcessInfo {
            name: "zsh".into(),
            pid: Some(9999),
            argv: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::LaunchShell { shell, .. } => {
                assert_eq!(shell, "zsh");
            }
            other => panic!("expected LaunchShell, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_process_with_argv_hint() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = Some(ProcessInfo {
            name: "cargo".into(),
            pid: Some(2222),
            argv: Some(vec!["cargo".into(), "test".into(), "--release".into()]),
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual {
                hint,
                original_process,
            } => {
                assert!(hint.contains("cargo test --release"));
                assert_eq!(original_process, "cargo");
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_process_without_argv() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = Some(ProcessInfo {
            name: "mysterious".into(),
            pid: None,
            argv: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual {
                hint,
                original_process,
            } => {
                // Without argv, hint uses the name
                assert!(hint.contains("mysterious"));
                assert_eq!(original_process, "mysterious");
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_no_cwd_defaults_to_root() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.cwd = None;
        state.shell = None;
        state.foreground_process = None;

        let plans = launcher.plan(&id_map, &[state]);
        // With no cwd (defaults to "/") and shells enabled but cwd == "/",
        // should skip
        match &plans[0].action {
            LaunchAction::Skip { reason } => {
                assert!(reason.contains("no process information"));
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn plan_codex_agent_detected_from_process() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = Some(ProcessInfo {
            name: "codex-cli".into(),
            pid: Some(5555),
            argv: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        // Agents default to Manual without opt-in
        assert!(plans[0].state_warning.is_some());
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("codex"));
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_gemini_agent_with_opt_in() {
        let config = LaunchConfig {
            launch_agents: true,
            ..Default::default()
        };
        let wez = crate::wezterm::mock_wezterm_handle();
        let launcher = ProcessLauncher::new(wez, config);
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "Gemini".into(),
            session_id: Some("sess-abc".into()),
            state: Some("idle".into()),
        });

        let plans = launcher.plan(&id_map, &[state]);
        assert!(plans[0].state_warning.is_some());
        match &plans[0].action {
            LaunchAction::LaunchAgent {
                command,
                agent_type,
                ..
            } => {
                assert!(command.contains("gemini-cli"));
                assert_eq!(agent_type, "Gemini");
            }
            other => panic!("expected LaunchAgent, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_agent_type_manual() {
        let config = LaunchConfig {
            launch_agents: true,
            ..Default::default()
        };
        let wez = crate::wezterm::mock_wezterm_handle();
        let launcher = ProcessLauncher::new(wez, config);
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "custom_bot".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("Unknown agent type"));
                assert!(hint.contains("custom_bot"));
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_state_warning_contains_agent_name() {
        let config = LaunchConfig {
            launch_agents: true,
            ..Default::default()
        };
        let wez = crate::wezterm::mock_wezterm_handle();
        let launcher = ProcessLauncher::new(wez, config);
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "claude_code".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        let warning = plans[0].state_warning.as_ref().unwrap();
        assert!(warning.contains("claude_code"));
        assert!(warning.contains("NEW session"));
        assert!(warning.contains("lost"));
    }

    // =========================================================================
    // Execute — additional scenarios
    // =========================================================================

    #[test]
    fn execute_empty_plans() {
        run_async_test(async {
            let wez = mock_with_panes(&[]).await;
            let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
            let report = launcher.execute(&[]).await;
            assert_eq!(report.results.len(), 0);
            assert_eq!(report.shells_launched, 0);
            assert_eq!(report.failed, 0);
        });
    }

    #[test]
    fn execute_skip_only() {
        run_async_test(async {
            let wez = mock_with_panes(&[]).await;
            let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
            let plans = vec![
                ProcessPlan {
                    old_pane_id: 1,
                    new_pane_id: 100,
                    action: LaunchAction::Skip {
                        reason: "disabled".into(),
                    },
                    state_warning: None,
                },
                ProcessPlan {
                    old_pane_id: 2,
                    new_pane_id: 200,
                    action: LaunchAction::Skip {
                        reason: "no info".into(),
                    },
                    state_warning: None,
                },
            ];
            let report = launcher.execute(&plans).await;
            assert_eq!(report.skipped, 2);
            assert_eq!(report.shells_launched, 0);
            assert_eq!(report.agents_launched, 0);
            assert_eq!(report.results.len(), 2);
            assert!(report.results.iter().all(|r| r.success));
        });
    }

    #[test]
    fn execute_manual_only() {
        run_async_test(async {
            let wez = mock_with_panes(&[]).await;
            let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
            let plans = vec![ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::Manual {
                    hint: "Restart vim manually".into(),
                    original_process: "vim".into(),
                },
                state_warning: None,
            }];
            let report = launcher.execute(&plans).await;
            assert_eq!(report.manual, 1);
            assert_eq!(report.shells_launched, 0);
            assert!(report.results[0].success);
        });
    }

    #[test]
    fn execute_agent_launch() {
        run_async_test(async {
            let wez = mock_with_panes(&[100]).await;
            let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
            let plans = vec![ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::LaunchAgent {
                    command: "cd /proj && claude".into(),
                    cwd: PathBuf::from("/proj"),
                    agent_type: "claude_code".into(),
                },
                state_warning: Some("new session warning".into()),
            }];
            let report = launcher.execute(&plans).await;
            assert_eq!(report.agents_launched, 1);
            assert_eq!(report.failed, 0);
            assert!(report.results[0].success);
        });
    }

    #[test]
    fn execute_report_result_order_preserved() {
        run_async_test(async {
            let wez = mock_with_panes(&[100, 200]).await;
            let launcher = ProcessLauncher::new(
                wez,
                LaunchConfig {
                    launch_delay_ms: 0,
                    ..Default::default()
                },
            );
            let plans = vec![
                ProcessPlan {
                    old_pane_id: 1,
                    new_pane_id: 100,
                    action: LaunchAction::LaunchShell {
                        shell: "bash".into(),
                        cwd: PathBuf::from("/a"),
                    },
                    state_warning: None,
                },
                ProcessPlan {
                    old_pane_id: 2,
                    new_pane_id: 200,
                    action: LaunchAction::Skip {
                        reason: "skip".into(),
                    },
                    state_warning: None,
                },
            ];
            let report = launcher.execute(&plans).await;
            assert_eq!(report.results[0].old_pane_id, 1);
            assert_eq!(report.results[1].old_pane_id, 2);
        });
    }
}
