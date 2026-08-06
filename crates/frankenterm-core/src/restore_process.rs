//! Process disposition engine for restored panes.
//!
//! Layout restoration already creates a default shell at the restored working
//! directory. This module classifies the captured foreground process and reports
//! whether operator follow-up is required.
//!
//! # Safety
//!
//! Shell switching and agent execution require an argv-isolated mux spawn API.
//! Until that API exists, the planner reports those cases as manual and execution
//! of caller-supplied legacy launch plans fails closed without PTY input.
//!
//! # Data flow
//!
//! ```text
//! PaneStateSnapshot (DB) → ProcessPlan → finite LaunchReport
//! ```

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::patterns::AgentType;
use crate::session_pane_state::{PaneStateSnapshot, ProcessInfo};

// =============================================================================
// Configuration
// =============================================================================

/// Reserved process-restoration configuration.
///
/// The field names are retained temporarily because persisted configuration
/// still exposes them. None of these settings permit PTY command injection or
/// cause this module to start a process.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LaunchConfig {
    /// Reserved historical setting. Ignored by the current planner and executor;
    /// captured shells always require an explicit manual disposition.
    pub launch_shells: bool,
    /// Reserved historical setting. Ignored by the current planner and executor;
    /// captured agents always require an explicit manual disposition.
    pub launch_agents: bool,
    /// Reserved for a future argv-isolated mux spawn implementation. Ignored by
    /// the current planner and executor.
    pub launch_delay_ms: u64,
    /// Reserved command templates for a future argv-isolated mux spawn
    /// implementation. Ignored by the current planner and executor.
    pub agent_commands: HashMap<String, String>,
}

impl fmt::Debug for LaunchConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchConfig")
            .field("reserved_launch_shells", &self.launch_shells)
            .field("reserved_launch_agents", &self.launch_agents)
            .field("reserved_launch_delay_ms", &self.launch_delay_ms)
            .field("reserved_agent_command_count", &self.agent_commands.len())
            .finish()
    }
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

/// Internal action retained between classification and disposition settlement.
///
/// This type deliberately has no serde implementation: it can contain raw
/// persisted commands, paths, or operator hints and is not a wire/report type.
#[derive(Clone, PartialEq, Eq)]
pub enum LaunchAction {
    /// Legacy shell launch request. Execution rejects it until the mux exposes
    /// an argv-isolated spawn channel.
    LaunchShell { shell: String, cwd: PathBuf },
    /// Legacy agent launch request. Execution rejects PTY command injection
    /// until the mux exposes an argv-isolated spawn channel.
    LaunchAgent {
        command: String,
        cwd: PathBuf,
        agent_type: String,
    },
    /// Skip process follow-up for this pane.
    Skip { reason: String },
    /// Manual hint for the user (process needs manual restart).
    Manual {
        hint: String,
        original_process: String,
    },
}

impl fmt::Debug for LaunchAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LaunchShell { .. } => "LaunchShell { redacted: true }",
            Self::LaunchAgent { .. } => "LaunchAgent { redacted: true }",
            Self::Skip { .. } => "Skip { redacted: true }",
            Self::Manual { .. } => "Manual { redacted: true }",
        })
    }
}

/// Finite, content-free disposition retained in execution reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchDisposition {
    Shell,
    Agent,
    Skip,
    Manual,
}

impl LaunchAction {
    const fn disposition(&self) -> LaunchDisposition {
        match self {
            Self::LaunchShell { .. } => LaunchDisposition::Shell,
            Self::LaunchAgent { .. } => LaunchDisposition::Agent,
            Self::Skip { .. } => LaunchDisposition::Skip,
            Self::Manual { .. } => LaunchDisposition::Manual,
        }
    }
}

/// Process-restoration disposition plan for a single pane.
///
/// This type deliberately has no serde implementation and its `Debug` output
/// omits the raw action payload and state warning.
#[derive(Clone)]
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

impl fmt::Debug for ProcessPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessPlan")
            .field("old_pane_id", &self.old_pane_id)
            .field("new_pane_id", &self.new_pane_id)
            .field("action", &self.action.disposition())
            .field("has_state_warning", &self.state_warning.is_some())
            .finish()
    }
}

/// Result of settling a process plan on a single pane.
#[derive(Clone, Serialize, Deserialize)]
pub struct LaunchResult {
    pub old_pane_id: u64,
    pub new_pane_id: u64,
    /// Content-free action category. Commands, paths, hints, and persisted
    /// process strings are deliberately not duplicated into reports.
    pub action: LaunchDisposition,
    pub success: bool,
    pub error: Option<String>,
}

impl fmt::Debug for LaunchResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchResult")
            .field("old_pane_id", &self.old_pane_id)
            .field("new_pane_id", &self.new_pane_id)
            .field("action", &self.action)
            .field("success", &self.success)
            .field("has_error", &self.error.is_some())
            .finish()
    }
}

/// Report after executing all process plans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchReport {
    pub results: Vec<LaunchResult>,
    /// Successful argv-isolated shell launches. Always zero until that mux
    /// capability exists; retained in the report schema for that future path.
    pub shells_launched: usize,
    /// Successful argv-isolated agent launches. Always zero until that mux
    /// capability exists; retained in the report schema for that future path.
    pub agents_launched: usize,
    pub skipped: usize,
    pub manual: usize,
    pub failed: usize,
    /// Structured reason that execution stopped before every plan settled.
    /// Cancellation stops the sequence immediately; callers use this field to
    /// leave the restore attempt unclean and require reconciliation rather than
    /// treating partial settlement as success.
    pub interruption: Option<LaunchInterruption>,
}

/// Phase at which a process-disposition sequence stopped cooperatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchInterruptionPhase {
    BeforePlan,
    /// Reserved for a future argv-isolated spawn path.
    InterLaunchDelay,
    /// Reserved for a future argv-isolated spawn path.
    ShellSettleDelay,
    /// Reserved for a future argv-isolated spawn path.
    AgentSettleDelay,
    /// Reserved for a future argv-isolated spawn path.
    MuxOperation,
}

/// Typed partial-result marker for a canceled process-disposition sequence.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchInterruption {
    /// Zero-based plan index that did not settle.
    pub plan_index: usize,
    pub phase: LaunchInterruptionPhase,
    /// Capability/runtime detail only; never contains the persisted command.
    pub detail: String,
}

impl fmt::Debug for LaunchInterruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchInterruption")
            .field("plan_index", &self.plan_index)
            .field("phase", &self.phase)
            .field("has_detail", &!self.detail.is_empty())
            .finish()
    }
}

// =============================================================================
// ProcessLauncher
// =============================================================================

/// Classifies and settles process-restoration dispositions for restored panes.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessLauncher;

impl ProcessLauncher {
    /// Create a process-disposition engine. It intentionally takes no mux
    /// handle or launch configuration because the current implementation cannot
    /// launch or inject input and every historical launch setting is inert.
    pub const fn new() -> Self {
        Self
    }

    /// Generate a process-disposition plan without executing anything.
    ///
    /// The plan maps each pane from the snapshot to an action based on its
    /// captured process info and agent metadata. Reserved launch configuration
    /// does not affect the result.
    pub fn plan(
        &self,
        pane_id_map: &HashMap<u64, u64>,
        pane_states: &[PaneStateSnapshot],
    ) -> Vec<ProcessPlan> {
        let mut plans = Vec::with_capacity(pane_states.len());

        for state in pane_states {
            let new_pane_id = match pane_id_map.get(&state.pane_id) {
                Some(&id) => id,
                None => continue,
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

    /// Settle a set of process plans without writing commands to panes.
    ///
    /// Plans are evaluated sequentially. Structural `Skip`/`Manual` actions
    /// succeed; executable legacy actions are rejected with finite errors.
    pub fn execute(&self, plans: &[ProcessPlan]) -> LaunchReport {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.execute_cx(&cx, plans)
    }

    /// Execute process plans under an explicit `&Cx` (ft-xbnl0.2.2 Cx-first
    /// API).
    ///
    /// Caller cancellation is checked before every disposition. Automatic
    /// shell and agent execution currently fails closed because the mux
    /// boundary has no argv-isolated command spawn API.
    pub fn execute_cx(&self, cx: &crate::cx::Cx, plans: &[ProcessPlan]) -> LaunchReport {
        let mut report = LaunchReport {
            results: Vec::with_capacity(plans.len()),
            ..LaunchReport::default()
        };

        for (i, plan) in plans.iter().enumerate() {
            if cx.checkpoint().is_err() {
                report.interruption = Some(LaunchInterruption {
                    plan_index: i,
                    phase: LaunchInterruptionPhase::BeforePlan,
                    detail: "caller capability stopped before plan".to_string(),
                });
                break;
            }
            let error = match &plan.action {
                LaunchAction::LaunchShell { shell, cwd } => {
                    self.reject_legacy_shell_launch(shell, cwd)
                }
                LaunchAction::LaunchAgent {
                    command,
                    cwd,
                    agent_type,
                } => self.reject_legacy_agent_launch(command, cwd, agent_type),
                LaunchAction::Skip { .. } => {
                    report.skipped += 1;
                    report.results.push(LaunchResult {
                        old_pane_id: plan.old_pane_id,
                        new_pane_id: plan.new_pane_id,
                        action: plan.action.disposition(),
                        success: true,
                        error: None,
                    });
                    continue;
                }
                LaunchAction::Manual { .. } => {
                    report.manual += 1;
                    report.results.push(LaunchResult {
                        old_pane_id: plan.old_pane_id,
                        new_pane_id: plan.new_pane_id,
                        action: plan.action.disposition(),
                        success: true,
                        error: None,
                    });
                    continue;
                }
            };

            Self::record_failure(&mut report, plan, error);
        }

        info!(
            shells = report.shells_launched,
            agents = report.agents_launched,
            skipped = report.skipped,
            manual = report.manual,
            failed = report.failed,
            interrupted = report.interruption.is_some(),
            "process restore dispositions settled"
        );

        report
    }

    fn record_failure(report: &mut LaunchReport, plan: &ProcessPlan, error: String) {
        report.failed += 1;
        report.results.push(LaunchResult {
            old_pane_id: plan.old_pane_id,
            new_pane_id: plan.new_pane_id,
            action: plan.action.disposition(),
            success: false,
            error: Some(error),
        });
    }

    // -------------------------------------------------------------------------
    // Internal: action resolution
    // -------------------------------------------------------------------------

    /// Determine what action to take for a pane based on its snapshot.
    fn resolve_action(&self, state: &PaneStateSnapshot) -> (LaunchAction, Option<String>) {
        // Check for agent metadata first
        if state.agent.is_some() {
            let cwd = state.cwd.as_deref().and_then(normalize_restored_cwd);
            return self.resolve_agent_action(cwd.as_deref());
        }

        // Check foreground process
        if let Some(ref process) = state.foreground_process {
            return self.resolve_process_action(process, state.cwd.as_deref());
        }

        // Check shell field
        if state.shell.is_some() {
            return self.resolve_shell_action();
        }

        (
            LaunchAction::Skip {
                reason: "layout restore already created a shell at the restored working directory"
                    .into(),
            },
            None,
        )
    }

    /// Resolve action for a pane with known agent metadata.
    fn resolve_agent_action(&self, cwd: Option<&Path>) -> (LaunchAction, Option<String>) {
        let warning = Some(
            "Agent process state cannot be resumed automatically; conversation context and in-flight work may be unavailable."
                .to_string(),
        );

        if cwd.is_none() {
            return (
                LaunchAction::Manual {
                    hint: "Agent restart requires a verified absolute working directory."
                        .to_string(),
                    original_process: "agent".to_string(),
                },
                warning,
            );
        }

        (
            LaunchAction::Manual {
                hint: "Automatic agent restart requires a mux-native argv spawn channel; restart manually."
                    .to_string(),
                original_process: "agent".to_string(),
            },
            Some(
                "Automatic agent switching is unavailable without a mux-native argv channel."
                    .to_string(),
            ),
        )
    }

    /// Resolve action based on foreground process info.
    fn resolve_process_action(
        &self,
        process: &ProcessInfo,
        cwd: Option<&str>,
    ) -> (LaunchAction, Option<String>) {
        let name = &process.name;

        // Detect agent processes by name
        let agent_type = agent_type_from_process_name(name);
        if agent_type != AgentType::Unknown {
            let cwd = cwd.and_then(normalize_restored_cwd);
            return self.resolve_agent_action(cwd.as_deref());
        }

        // Common shells
        if is_shell(name) {
            return self.resolve_shell_action();
        }

        // Interactive programs that need manual restart
        if is_interactive_program(name) {
            return (
                LaunchAction::Manual {
                    hint: "Interactive process requires manual restart.".to_string(),
                    original_process: "interactive".to_string(),
                },
                None,
            );
        }

        (
            LaunchAction::Manual {
                hint: "Unrecognized foreground process requires manual restart.".to_string(),
                original_process: "unrecognized".to_string(),
            },
            None,
        )
    }

    fn resolve_shell_action(&self) -> (LaunchAction, Option<String>) {
        (
            LaunchAction::Manual {
                hint: "Layout restored the default shell; switching shells requires a mux-native argv spawn and was not typed through PTY input."
                    .to_string(),
                original_process: "shell".to_string(),
            },
            Some("Automatic shell switching is unavailable without a mux-native argv channel.".to_string()),
        )
    }

    // -------------------------------------------------------------------------
    // Internal: execution
    // -------------------------------------------------------------------------

    /// Reject a legacy shell-launch plan without writing to the pane.
    ///
    /// Layout restoration already creates a shell at the restored working
    /// directory. Switching to a different shell requires an argv-isolated
    /// mux spawn API; typing a command into the restored PTY would execute
    /// attacker-controlled persisted state in an interactive shell.
    fn reject_legacy_shell_launch(
        &self,
        shell: &str,
        cwd: &Path,
    ) -> String {
        if let Err(error) = sanitize_restored_command(shell) {
            return error;
        }
        if let Err(error) = validate_restored_cwd(cwd) {
            return error;
        }
        "automatic shell switching requires a mux-native argv spawn channel; PTY command injection is refused"
            .to_string()
    }

    /// Reject a legacy agent-launch plan without writing to the pane.
    fn reject_legacy_agent_launch(
        &self,
        command: &str,
        cwd: &Path,
        agent_type: &str,
    ) -> String {
        if let Err(error) = sanitize_restored_command(command) {
            return error;
        }
        if let Err(error) = validate_restored_cwd(cwd) {
            return error;
        }
        if agent_type.is_empty() {
            return "automatic agent relaunch requires a non-empty agent type".to_string();
        }
        "automatic agent relaunch requires a mux-native argv spawn channel; PTY command injection is refused"
            .to_string()
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

/// Normalize only a local, absolute working directory suitable for a future
/// argv-isolated spawn. A non-local `file://` authority belongs to another mux
/// domain and must never be silently interpreted on this host.
fn normalize_restored_cwd(cwd: &str) -> Option<PathBuf> {
    if let Some(authority_and_path) = cwd.strip_prefix("file://") {
        let is_local = authority_and_path.starts_with('/')
            || authority_and_path == "localhost"
            || authority_and_path.starts_with("localhost/");
        if !is_local {
            return None;
        }
    }
    let path = normalize_cwd(cwd);
    if path.as_os_str().is_empty() || validate_restored_cwd(&path).is_err() {
        return None;
    }
    Some(path)
}

/// Simple percent-decoding for common path characters.
fn percent_decode(s: &str) -> String {
    fn decode_hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let input = s.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        if input[idx] == b'%' && idx + 2 < input.len() {
            if let (Some(high), Some(low)) =
                (decode_hex(input[idx + 1]), decode_hex(input[idx + 2]))
            {
                decoded.push((high << 4) | low);
                idx += 3;
                continue;
            }
        }

        decoded.push(input[idx]);
        idx += 1;
    }

    String::from_utf8(decoded).unwrap_or_else(|err| {
        let bytes = err.into_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Escape a path for use in a shell command.
#[cfg(test)]
fn shell_escape(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.is_empty() {
        return "''".to_string();
    }
    if s.contains(|c: char| c.is_whitespace() || "\"'$`!#&|;(){}[]<>?*~\\".contains(c)) {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.into_owned()
    }
}

fn validate_restored_cwd(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("refusing restored working directory that is not absolute".to_string());
    }
    let value = path.to_string_lossy();
    for (index, ch) in value.char_indices() {
        if ch.is_control() || matches!(ch as u32, 0x7f..=0x9f) {
            return Err(format!(
                "refusing restored working directory with terminal control {:#04x} at byte {index}",
                ch as u32
            ));
        }
    }
    Ok(())
}

/// Validate a command carried by a legacy launch plan before rejecting it.
///
/// FrankenTerm no longer types restored commands into a PTY. The legacy raw
/// plan variants remain quarantined until their callers are removed, and every
/// such plan fails closed. Validation remains defense in depth so a future
/// argv-isolated implementation cannot accidentally inherit terminal-control
/// payloads from persisted state.
///
/// This helper rejects any command that contains a control character
/// that would break the "one command, one CR" assumption:
/// - `\r` / `\n` — the defense-in-depth primary concern.
/// - `\x1b` (ESC) — prevents re-injected ANSI escape sequences that
///   could re-arm Kitty keyboard protocol or trigger OSC handlers.
/// - other C0 controls (`\x00..=\x1f` except `\t`) — reserved/rarely
///   legitimate in a restored agent command; reject to stay conservative.
///
/// Returns `Ok(command)` unchanged on success. Every error is finite and omits
/// the command itself.
fn sanitize_restored_command(command: &str) -> Result<&str, String> {
    for (idx, ch) in command.char_indices() {
        match ch {
            '\r' | '\n' => {
                return Err(format!(
                    "refusing to launch: restored command contains CR/LF at byte {idx} \
                     (ft-kegvt; see `launch_agent` sanitizer docs)",
                ));
            }
            '\x1b' => {
                return Err(format!(
                    "refusing to launch: restored command contains ESC (0x1b) at byte {idx} \
                     (ft-kegvt; ANSI-escape re-injection is refused at the sanitizer)",
                ));
            }
            c if (c as u32) < 0x20 && c != '\t' => {
                return Err(format!(
                    "refusing to launch: restored command contains C0 control {:#04x} at byte {idx} \
                     (ft-kegvt; only TAB is permitted in the C0 range)",
                    c as u32,
                ));
            }
            // ft-asoso: DEL (0x7F) is a control byte that tools reserve for
            // backspace/delete; passing it to launch_agent is operator-facing
            // surprising at minimum and parser-confusing at worst.
            '\x7F' => {
                return Err(format!(
                    "refusing to launch: restored command contains DEL (0x7f) at byte {idx} \
                     (ft-asoso; control byte not permitted in restored commands)",
                ));
            }
            // ft-asoso: C1 controls (0x80-0x9F) include CSI (0x9B), which is
            // the 8-bit equivalent of ESC [ — the actual escape-sequence
            // introducer. Refusing ESC alone leaves this 8-bit injection path
            // open against parsers that accept C1.
            c if matches!(c as u32, 0x80..=0x9F) => {
                return Err(format!(
                    "refusing to launch: restored command contains C1 control {:#04x} at byte {idx} \
                     (ft-asoso; C1 includes CSI 0x9b — the 8-bit equivalent of ESC [, \
                      so accepting C1 would defeat the ESC-rejection above)",
                    c as u32,
                ));
            }
            _ => {}
        }
    }
    Ok(command)
}

/// Get the default shell for the platform.
#[cfg(test)]
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
#[cfg(test)]
fn parse_agent_type(s: &str) -> AgentType {
    match s {
        "claude_code" | "ClaudeCode" => AgentType::ClaudeCode,
        "codex" | "Codex" => AgentType::Codex,
        "gemini" | "Gemini" => AgentType::Gemini,
        _ => AgentType::Unknown,
    }
}

/// Get the default launch command for a known agent type.
#[cfg(test)]
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
    use crate::session_pane_state::{AgentMetadata, TerminalState};

    /// LabRuntime-based determinism test (ft-xbnl0.2.2): prove the Cx-first
    /// `execute_cx` path runs under seed-locked virtual-time scheduling.
    /// We execute an empty plan list so no WezTerm interaction occurs; the
    /// test verifies the execute_cx orchestration loop and its sleep_with_cx
    /// plumbing run under the LabRuntime scheduler without wall-clock
    /// dependence.
    #[test]
    fn execute_cx_runs_under_labruntime() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const SEED: u64 = 0x6E57_0EE0_C410_7E57;
        let wall_start = std::time::Instant::now();
        let completed = std::sync::Arc::new(AtomicBool::new(false));
        let completed_task = std::sync::Arc::clone(&completed);

        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(SEED)
                .with_auto_advance()
                .worker_count(1)
                .max_steps(50_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::for_request();
                let launcher = ProcessLauncher::new();
                let report = launcher.execute_cx(&cx, &[]);
                assert_eq!(report.shells_launched, 0);
                assert_eq!(report.agents_launched, 0);
                assert_eq!(report.failed, 0);
                completed_task.store(true, Ordering::SeqCst);
            })
            .expect("spawn execute_cx task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.step_for_test();
        let _ = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert!(
            completed.load(Ordering::SeqCst),
            "execute_cx must complete under LabRuntime"
        );
        assert!(
            report.oracle_report.all_passed(),
            "LabRuntime oracles must all pass: {report:?}"
        );
        assert!(
            wall_start.elapsed() < std::time::Duration::from_secs(2),
            "Cx-first execute must not burn real seconds; elapsed {:?}",
            wall_start.elapsed()
        );
    }

    #[test]
    fn execute_cx_precancel_stops_before_first_plan() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("restore process pre-cancel regression"),
            );
            let launcher = ProcessLauncher::new();
            let plans = vec![ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 10,
                action: LaunchAction::Manual {
                    hint: "manual".to_string(),
                    original_process: "tool".to_string(),
                },
                state_warning: None,
            }];

            let report = launcher.execute_cx(&cx, &plans);
            let interruption = report
                .interruption
                .expect("pre-cancel must produce a typed interruption");
            assert_eq!(interruption.plan_index, 0);
            assert_eq!(interruption.phase, LaunchInterruptionPhase::BeforePlan);
            assert!(report.results.is_empty());
            assert_eq!(report.manual, 0);
            assert_eq!(report.failed, 0);
        });
    }

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
        ProcessLauncher::new()
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
        use crate::runtime_async::CompatRuntime;

        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
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
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn plan_shell_pane() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();
        let mut state = test_pane_state(1);
        state.shell = Some(default_shell());
        let states = vec![state];

        let plans = launcher.plan(&id_map, &states);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].old_pane_id, 1);
        assert_eq!(plans[0].new_pane_id, 100);
        assert!(plans[0].state_warning.is_some());

        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("mux-native argv"));
            }
            other => panic!("expected shell Manual disposition, got {other:?}"),
        }
    }

    #[test]
    fn plan_agent_pane_is_manual_with_reserved_default_config() {
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
                assert!(hint.contains("mux-native argv"));
                assert!(!hint.contains("claude_code"));
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_agent_pane_is_manual_without_launch_configuration() {
        let launcher = ProcessLauncher::new();
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
            LaunchAction::Manual {
                hint,
                original_process,
            } => {
                assert!(hint.contains("mux-native argv"));
                assert_eq!(original_process, "agent");
            }
            other => panic!("expected safe Manual disposition, got {other:?}"),
        }
    }

    #[test]
    fn plan_agent_manual_hint_contains_no_command_or_cwd() {
        let launcher = ProcessLauncher::new();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "claude_code".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("mux-native argv"));
                assert!(!hint.contains("--resume"));
                assert!(!hint.contains("/home/user/project"));
            }
            other => panic!("expected safe Manual disposition, got {other:?}"),
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
                assert_eq!(hint, "Interactive process requires manual restart.");
                assert_eq!(original_process, "interactive");
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
        assert!(plans[0].state_warning.is_some());
        assert!(matches!(&plans[0].action, LaunchAction::Manual { .. }));
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
    fn plan_shell_is_always_manual() {
        let launcher = ProcessLauncher::new();
        let id_map = test_pane_id_map();
        let states = vec![test_pane_state(1)];

        let plans = launcher.plan(&id_map, &states);
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("mux-native argv"));
            }
            other => panic!("expected Manual, got {other:?}"),
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
        match &plans[0].action {
            LaunchAction::Skip { reason } => {
                assert!(reason.contains("layout restore"));
            }
            other => panic!("expected structural shell Skip, got {other:?}"),
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
    fn shell_escape_empty_path_is_quoted() {
        assert_eq!(shell_escape(&PathBuf::from("")), "''");
    }

    #[test]
    fn shell_escape_roundtrips_as_single_shell_token() {
        for fixture in [
            "",
            "/foo/bar",
            "/foo/my project",
            "/foo/$USER",
            "/foo/it's",
            "/foo/\"bar\"",
            "/foo/(copy)",
            "/foo/~backup",
        ] {
            let escaped = shell_escape(&PathBuf::from(fixture));
            let parsed = shell_words::split(&format!("cd {escaped}"))
                .expect("shell_escape output must remain parseable");
            assert_eq!(
                parsed,
                vec!["cd".to_string(), fixture.to_string()],
                "fixture {fixture:?} escaped to {escaped:?} but parsed as {parsed:?}"
            );
        }
    }

    // ── ft-kegvt sanitizer regression suite ───────────────────────────

    #[test]
    fn sanitize_restored_command_passes_legitimate_input() {
        // Canonical agent restart commands from the built-in packs —
        // none of these should be rejected.
        for fixture in [
            "claude-code",
            "/usr/local/bin/claude-code --resume",
            "codex resume 12345678-1234-1234-1234-123456789012",
            "env CLAUDE_API_KEY=sk-xxxx claude-code",
            "gemini -m gemini-2.5-pro --context session.md",
            "\tindented-but-ok", // TAB is explicitly permitted.
        ] {
            let result = sanitize_restored_command(fixture);
            assert!(
                result.is_ok(),
                "legitimate command {fixture:?} rejected: {:?}",
                result.err()
            );
            assert_eq!(result.unwrap(), fixture);
        }
    }

    #[test]
    fn sanitize_restored_command_rejects_newline_injection() {
        // The concrete attack payload from the bead: attacker stashes
        // a multi-line command in mux_pane_state.command. Pre-fix,
        // the removed send_text path would have typed both lines into the shell.
        let payload = "claude-code\ninnocent_payload\r";
        let err = sanitize_restored_command(payload).expect_err("CR/LF injection must be rejected");
        assert!(
            err.contains("CR/LF") && err.contains("ft-kegvt"),
            "unexpected rejection message: {err}"
        );
    }

    #[test]
    fn sanitize_restored_command_rejects_bare_carriage_return() {
        let err = sanitize_restored_command("cmd\rrm -rf ~").expect_err("bare CR must be rejected");
        assert!(err.contains("CR/LF"));
    }

    #[test]
    fn sanitize_restored_command_rejects_bare_newline() {
        let err = sanitize_restored_command("cmd\nevil").expect_err("bare LF must be rejected");
        assert!(err.contains("CR/LF"));
    }

    #[test]
    fn sanitize_restored_command_rejects_ansi_escape() {
        // ESC (0x1b) can re-arm terminal modes or trigger OSC
        // handlers. Reject so the restored command cannot smuggle
        // in terminal-protocol payloads alongside the agent name.
        let payload = "claude-code\x1b]0;pwned\x07";
        let err = sanitize_restored_command(payload).expect_err("ESC (0x1b) must be rejected");
        assert!(
            err.contains("ESC") && err.contains("ft-kegvt"),
            "unexpected rejection message: {err}"
        );
    }

    #[test]
    fn sanitize_restored_command_rejects_c0_control() {
        // NUL through BS/FF/etc. — anything in the C0 range other
        // than TAB must be rejected.
        let err = sanitize_restored_command("cmd\x07bell").expect_err("BEL must be rejected");
        assert!(err.contains("C0 control"));
        let err = sanitize_restored_command("cmd\x00nul").expect_err("NUL must be rejected");
        assert!(err.contains("C0 control"));
    }

    #[test]
    fn sanitize_restored_command_preserves_utf8() {
        // Multi-byte UTF-8 is fine — reject only the explicit control
        // set, never legitimate text.
        let fixture = "claude-код-代码";
        assert_eq!(sanitize_restored_command(fixture).unwrap(), fixture);
    }

    #[test]
    fn sanitize_restored_command_rejects_lf_then_cr() {
        // Defense in depth: the order of CR/LF shouldn't matter; any
        // occurrence at any byte index rejects.
        let payload = "a\n\rwhatever";
        let err =
            sanitize_restored_command(payload).expect_err("LF-then-CR combo must be rejected");
        assert!(err.contains("CR/LF at byte 1"));
    }

    /// ft-asoso regression guard: previously sanitize_restored_command
    /// only rejected C0 controls (< 0x20) + ESC + CR/LF. Missed DEL
    /// (0x7F) and the C1 control range (0x80-0x9F) which includes CSI
    /// (0x9B), the 8-bit equivalent of ESC [.
    #[test]
    fn sanitize_restored_command_rejects_del() {
        let err =
            sanitize_restored_command("cmd\x7Fbackspace").expect_err("DEL (0x7f) must be rejected");
        assert!(err.contains("DEL"));
        assert!(err.contains("ft-asoso"));
    }

    #[test]
    fn sanitize_restored_command_rejects_csi_8bit_form() {
        // CSI (0x9B) is the single-byte equivalent of ESC [ (0x1B 0x5B).
        // Refusing only the 7-bit ESC leaves the 8-bit CSI injection
        // path open — the most direct ANSI-escape re-injection vector.
        let payload = "claude-code\u{009B}2J";
        let err = sanitize_restored_command(payload).expect_err("CSI (0x9b) must be rejected");
        assert!(err.contains("C1 control"));
        assert!(err.contains("ft-asoso"));
    }

    #[test]
    fn sanitize_restored_command_rejects_all_c1_controls() {
        // Sweep every C1 control byte (0x80-0x9F) and assert all
        // are rejected. Some are operationally inert; refusing
        // uniformly is the conservative posture.
        for cp in 0x80u32..=0x9F {
            let c = char::from_u32(cp).expect("valid Unicode scalar");
            let payload = format!("cmd{c}rest");
            assert!(
                sanitize_restored_command(&payload).is_err(),
                "C1 0x{cp:02x} must be rejected"
            );
        }
    }

    #[test]
    fn sanitize_restored_command_accepts_high_unicode_above_c1() {
        // Sanity: the C1 rejection must NOT extend past 0x9F into
        // legitimate Unicode. Latin-1 supplement letters (0xA0+)
        // and beyond stay accepted.
        let fixture = "claude-Ä-€-中";
        assert_eq!(sanitize_restored_command(fixture).unwrap(), fixture);
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

    #[test]
    fn execute_shell_launch_is_refused_without_pty_input() {
        run_async_test(async {
            let mock = std::sync::Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(100).await;
            let launcher = ProcessLauncher::new();
            let plans = vec![ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::LaunchShell {
                    shell: "bash".into(),
                    cwd: PathBuf::from("/home/user"),
                },
                state_warning: None,
            }];

            let report = launcher.execute(&plans);
            assert_eq!(report.shells_launched, 0);
            assert_eq!(report.failed, 1);
            assert_eq!(report.results.len(), 1);
            assert!(!report.results[0].success);
            assert!(
                report.results[0]
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("PTY command injection is refused"))
            );
            assert_eq!(mock.pane_state(100).await.unwrap().content, "");
        });
    }

    #[test]
    fn execute_mixed_plan() {
        run_async_test(async {
            let launcher = ProcessLauncher::new();
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

            let report = launcher.execute(&plans);
            assert_eq!(report.shells_launched, 0);
            assert_eq!(report.skipped, 1);
            assert_eq!(report.manual, 1);
            assert_eq!(report.failed, 1);
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
        assert!(!cfg2.launch_shells);
        assert!(cfg2.launch_agents);
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
    // LaunchAction / ProcessPlan — redacted diagnostic surfaces
    // =========================================================================

    #[test]
    fn launch_action_debug_redacts_all_raw_payloads() {
        let actions = [
            LaunchAction::LaunchShell {
                shell: "secret-shell".into(),
                cwd: PathBuf::from("/secret/shell/path"),
            },
            LaunchAction::LaunchAgent {
                command: "secret-agent-command".into(),
                cwd: PathBuf::from("/secret/agent/path"),
                agent_type: "secret-agent-type".into(),
            },
            LaunchAction::Skip {
                reason: "secret-skip-reason".into(),
            },
            LaunchAction::Manual {
                hint: "secret-manual-hint".into(),
                original_process: "secret-process".into(),
            },
        ];

        for action in actions {
            let diagnostic = format!("{action:?}");
            assert!(diagnostic.contains("redacted: true"));
            assert!(!diagnostic.contains("secret"));
        }
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
    // ProcessPlan redaction / LaunchResult / LaunchReport serde
    // =========================================================================

    #[test]
    fn process_plan_debug_redacts_action_and_warning() {
        let plan = ProcessPlan {
            old_pane_id: 42,
            new_pane_id: 100,
            action: LaunchAction::LaunchShell {
                shell: "secret-shell".into(),
                cwd: PathBuf::from("/secret/data"),
            },
            state_warning: Some("secret-warning".into()),
        };
        let diagnostic = format!("{plan:?}");
        assert!(diagnostic.contains("ProcessPlan"));
        assert!(diagnostic.contains("old_pane_id: 42"));
        assert!(diagnostic.contains("action: Shell"));
        assert!(diagnostic.contains("has_state_warning: true"));
        assert!(!diagnostic.contains("secret"));
    }

    #[test]
    fn launch_result_serde_roundtrip() {
        let result = LaunchResult {
            old_pane_id: 1,
            new_pane_id: 10,
            action: LaunchDisposition::Skip,
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
            action: LaunchDisposition::Agent,
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
                action: LaunchDisposition::Skip,
                success: true,
                error: None,
            }],
            shells_launched: 3,
            agents_launched: 1,
            skipped: 2,
            manual: 1,
            failed: 0,
            interruption: None,
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
    fn normalize_cwd_percent_encoded_utf8() {
        assert_eq!(
            normalize_cwd("file:///home/user/%E2%9C%93"),
            PathBuf::from("/home/user/\u{2713}")
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
        assert_eq!(percent_decode("test%"), "test%");
    }

    #[test]
    fn percent_decode_single_char_after_percent() {
        assert_eq!(percent_decode("test%4"), "test%4");
    }

    #[test]
    fn percent_decode_utf8_multibyte_sequence() {
        assert_eq!(percent_decode("%E2%9C%93"), "\u{2713}");
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
        // The path should be single-quoted (shell_escape uses single quotes)
        assert!(cmd.contains('\''));
        assert!(cmd.contains("claude"));
    }

    // =========================================================================
    // resolve_action — additional edge cases
    // =========================================================================

    #[test]
    fn plan_no_process_metadata_is_structural_skip() {
        let launcher = ProcessLauncher::new();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.shell = None;
        state.foreground_process = None;
        state.cwd = Some("/home/user/code".into());

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Skip { reason } => {
                assert!(reason.contains("layout restore"));
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
            name: default_shell()
                .rsplit('/')
                .next()
                .unwrap_or("sh")
                .to_string(),
            pid: Some(9999),
            argv: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("mux-native argv"));
            }
            other => panic!("expected shell Manual disposition, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_process_redacts_argv() {
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
                assert_eq!(
                    hint,
                    "Unrecognized foreground process requires manual restart."
                );
                assert_eq!(original_process, "unrecognized");
                assert!(!hint.contains("cargo test --release"));
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
                assert_eq!(
                    hint,
                    "Unrecognized foreground process requires manual restart."
                );
                assert_eq!(original_process, "unrecognized");
                assert!(!hint.contains("mysterious"));
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_no_cwd_does_not_synthesize_process_launch_root() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.cwd = None;
        state.shell = None;
        state.foreground_process = None;

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Skip { reason } => {
                assert!(reason.contains("layout restore"));
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn plan_empty_cwd_shell_requires_manual_verified_spawn() {
        let launcher = test_launcher();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.cwd = Some(String::new());
        state.shell = Some(default_shell());

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("mux-native argv"));
            }
            other => panic!("expected shell Manual disposition, got {other:?}"),
        }
    }

    #[test]
    fn plan_empty_cwd_refuses_agent_restart_instead_of_using_root() {
        let launcher = ProcessLauncher::new();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.cwd = Some(String::new());
        state.agent = Some(AgentMetadata {
            agent_type: "claude_code".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("verified absolute working directory"));
                assert!(!hint.contains('/'));
            }
            other => panic!("expected Manual, got {other:?}"),
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
        assert!(plans[0].state_warning.is_some());
        match &plans[0].action {
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("mux-native argv"));
                assert!(!hint.contains("codex"));
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_gemini_agent_is_manual() {
        let launcher = ProcessLauncher::new();
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
            LaunchAction::Manual { hint, .. } => {
                assert!(hint.contains("mux-native argv"));
                assert!(!hint.contains("Gemini"));
            }
            other => panic!("expected safe Manual disposition, got {other:?}"),
        }
    }

    #[test]
    fn plan_unknown_agent_type_manual() {
        let launcher = ProcessLauncher::new();
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
                assert!(hint.contains("mux-native argv"));
                assert!(!hint.contains("custom_bot"));
            }
            other => panic!("expected Manual, got {other:?}"),
        }
    }

    #[test]
    fn plan_state_warning_is_finite_and_content_free() {
        let launcher = ProcessLauncher::new();
        let id_map = test_pane_id_map();

        let mut state = test_pane_state(1);
        state.agent = Some(AgentMetadata {
            agent_type: "claude_code".into(),
            session_id: None,
            state: None,
        });

        let plans = launcher.plan(&id_map, &[state]);
        let warning = plans[0].state_warning.as_ref().unwrap();
        assert!(warning.contains("cannot be resumed automatically"));
        assert!(warning.contains("in-flight work"));
        assert!(!warning.contains("claude_code"));
    }

    // =========================================================================
    // Execute — additional scenarios
    // =========================================================================

    #[test]
    fn execute_empty_plans() {
        run_async_test(async {
            let launcher = ProcessLauncher::new();
            let report = launcher.execute(&[]);
            assert_eq!(report.results.len(), 0);
            assert_eq!(report.shells_launched, 0);
            assert_eq!(report.failed, 0);
        });
    }

    #[test]
    fn execute_skip_only() {
        run_async_test(async {
            let launcher = ProcessLauncher::new();
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
            let report = launcher.execute(&plans);
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
            let launcher = ProcessLauncher::new();
            let plans = vec![ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::Manual {
                    hint: "Restart vim manually".into(),
                    original_process: "vim".into(),
                },
                state_warning: None,
            }];
            let report = launcher.execute(&plans);
            assert_eq!(report.manual, 1);
            assert_eq!(report.shells_launched, 0);
            assert!(report.results[0].success);
        });
    }

    #[test]
    fn execute_legacy_agent_plan_is_refused() {
        run_async_test(async {
            let mock = std::sync::Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(100).await;
            let launcher = ProcessLauncher::new();
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
            let report = launcher.execute(&plans);
            assert_eq!(report.agents_launched, 0);
            assert_eq!(report.failed, 1);
            assert!(!report.results[0].success);
            assert_eq!(mock.pane_state(100).await.unwrap().content, "");
        });
    }

    #[test]
    fn execute_agent_plan_with_spaced_cwd_requires_mux_native_argv() {
        run_async_test(async {
            let mock = std::sync::Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(100).await;
            let launcher = ProcessLauncher::new();
            let plans = vec![ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::LaunchAgent {
                    command: "claude --resume".into(),
                    cwd: PathBuf::from("/tmp/project with spaces"),
                    agent_type: "claude_code".into(),
                },
                state_warning: Some("new session warning".into()),
            }];

            let report = launcher.execute(&plans);
            assert_eq!(report.agents_launched, 0);
            assert_eq!(report.failed, 1);
            assert!(
                report.results[0]
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("mux-native argv"))
            );

            let content = mock.pane_state(100).await.unwrap().content;
            assert_eq!(content, "");
        });
    }

    #[test]
    fn execute_report_result_order_preserved() {
        run_async_test(async {
            let launcher = ProcessLauncher::new();
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
            let report = launcher.execute(&plans);
            assert_eq!(report.results[0].old_pane_id, 1);
            assert_eq!(report.results[1].old_pane_id, 2);
        });
    }
}
