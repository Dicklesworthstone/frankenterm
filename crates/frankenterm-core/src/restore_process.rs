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
use std::path::PathBuf;
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
                tokio::time::sleep(delay).await;
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
        cwd: &PathBuf,
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
                    cwd: cwd.clone(),
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
                    cwd: cwd.clone(),
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
        cwd: &PathBuf,
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
                        cwd: cwd.clone(),
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
    async fn launch_shell(&self, pane_id: u64, shell: &str, cwd: &PathBuf) -> Result<(), String> {
        // cd to the working directory first
        let cd_cmd = format!("cd {}\r", shell_escape(cwd));
        self.wezterm
            .send_text(pane_id, &cd_cmd)
            .await
            .map_err(|e| format!("send cd: {e}"))?;

        // Small delay to let the cd complete
        tokio::time::sleep(Duration::from_millis(50)).await;

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
        cwd: &PathBuf,
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
fn shell_escape(path: &PathBuf) -> String {
    let s = path.to_string_lossy();
    if s.contains(|c: char| c.is_whitespace() || "\"'$`!#&|;(){}[]<>?*~".contains(c)) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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
fn default_agent_command(agent_type: AgentType, cwd: &PathBuf) -> Option<String> {
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
            "\"/foo/my project\""
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

    #[tokio::test]
    async fn execute_shell_launch() {
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
    }

    #[tokio::test]
    async fn execute_mixed_plan() {
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
    }
}
