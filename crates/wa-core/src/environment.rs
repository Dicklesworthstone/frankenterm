//! Environment detection for wa.
//!
//! Provides best-effort detection of WezTerm, shell configuration, agent panes,
//! remote domains, and system characteristics. All probes are designed to be
//! safe and non-fatal: missing data is represented as `None` or empty lists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::patterns::AgentType;
use crate::setup::{ShellType, has_shell_wa_block, locate_shell_rc};
use crate::wezterm::{PaneInfo, WeztermHandle, default_wezterm_handle};

/// WezTerm capability flags inferred from local probes.
#[derive(Debug, Clone, Serialize)]
pub struct WeztermCapabilities {
    pub cli_available: bool,
    pub json_output: bool,
    pub multiplexing: bool,
    pub osc_133: bool,
    pub osc_7: bool,
    pub image_protocol: bool,
}

impl Default for WeztermCapabilities {
    fn default() -> Self {
        Self {
            cli_available: false,
            json_output: false,
            multiplexing: false,
            osc_133: false,
            osc_7: false,
            image_protocol: false,
        }
    }
}

/// WezTerm detection summary.
#[derive(Debug, Clone, Serialize)]
pub struct WeztermInfo {
    pub version: Option<String>,
    pub socket_path: Option<PathBuf>,
    pub is_running: bool,
    pub capabilities: WeztermCapabilities,
}

/// Shell detection summary.
#[derive(Debug, Clone, Serialize)]
pub struct ShellInfo {
    pub shell_path: Option<String>,
    pub shell_type: Option<String>,
    pub version: Option<String>,
    pub config_file: Option<PathBuf>,
    pub osc_133_enabled: bool,
}

/// Detected agent summary for a pane.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedAgent {
    pub agent_type: AgentType,
    pub pane_id: u64,
    pub confidence: f32,
    pub indicators: Vec<String>,
}

/// Remote host grouping for panes.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteHost {
    pub hostname: String,
    pub connection_type: ConnectionType,
    pub pane_ids: Vec<u64>,
}

/// Connection type inferred from pane metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Ssh,
    Wsl,
    Docker,
    Unknown,
}

/// System detection summary.
#[derive(Debug, Clone, Serialize)]
pub struct SystemInfo {
    pub os: String,
    pub arch: String,
    pub cpu_count: usize,
    pub memory_mb: Option<u64>,
    pub load_average: Option<f64>,
    pub detected_at_epoch_ms: i64,
}

/// Unified detected environment.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedEnvironment {
    pub wezterm: WeztermInfo,
    pub shell: ShellInfo,
    pub agents: Vec<DetectedAgent>,
    pub remotes: Vec<RemoteHost>,
    pub system: SystemInfo,
    pub detected_at: DateTime<Utc>,
}

impl ShellInfo {
    /// Detect shell info from the current process environment.
    #[must_use]
    pub fn detect() -> Self {
        let shell_path = std::env::var("SHELL").ok();
        Self::from_shell_path(shell_path.as_deref())
    }

    /// Construct shell info from an explicit shell path (useful for tests).
    #[must_use]
    pub fn from_shell_path(shell_path: Option<&str>) -> Self {
        let shell_type = shell_path.and_then(ShellType::from_path);
        let shell_name = shell_type.map(|shell| shell.name().to_string());
        let config_file = shell_type.and_then(|shell| locate_shell_rc(shell).ok());
        let osc_133_enabled = config_file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|content| has_shell_wa_block(&content))
            .unwrap_or(false);

        let version = detect_shell_version(shell_type);

        Self {
            shell_path: shell_path.map(str::to_string),
            shell_type: shell_name,
            version,
            config_file,
            osc_133_enabled,
        }
    }
}

impl WeztermInfo {
    /// Detect WezTerm status and capabilities using a WezTerm handle when available.
    pub async fn detect(
        wezterm: Option<&WeztermHandle>,
        shell: &ShellInfo,
    ) -> (Self, Vec<PaneInfo>) {
        let version = detect_wezterm_version();
        let cli_available = version.is_some();
        let socket_path = detect_wezterm_socket();

        let mut panes = Vec::new();
        let mut list_ok = false;

        if cli_available {
            let handle = wezterm.cloned().unwrap_or_else(default_wezterm_handle);
            match handle.list_panes().await {
                Ok(found) => {
                    panes = found;
                    list_ok = true;
                }
                Err(_) => {
                    list_ok = false;
                }
            }
        }

        let osc_7 = list_ok
            && panes.iter().any(|pane| {
                pane.cwd
                    .as_ref()
                    .map(|cwd| !cwd.trim().is_empty())
                    .unwrap_or(false)
            });

        let capabilities = WeztermCapabilities {
            cli_available,
            json_output: list_ok,
            multiplexing: list_ok,
            osc_133: shell.osc_133_enabled,
            osc_7,
            image_protocol: cli_available,
        };

        let info = Self {
            version,
            socket_path,
            is_running: list_ok,
            capabilities,
        };

        (info, panes)
    }
}

impl SystemInfo {
    #[must_use]
    pub fn detect() -> Self {
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let memory_mb = detect_memory_mb();
        let load_average = detect_load_average();
        let detected_at_epoch_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_count,
            memory_mb,
            load_average,
            detected_at_epoch_ms,
        }
    }
}

impl DetectedEnvironment {
    /// Detect the environment with an optional WezTerm handle.
    pub async fn detect(wezterm: Option<&WeztermHandle>) -> Self {
        let shell = ShellInfo::detect();
        let (wezterm_info, panes) = WeztermInfo::detect(wezterm, &shell).await;
        let agents = detect_agents_from_panes(&panes);
        let remotes = detect_remotes_from_panes(&panes);
        let system = SystemInfo::detect();

        Self {
            wezterm: wezterm_info,
            shell,
            agents,
            remotes,
            system,
            detected_at: Utc::now(),
        }
    }
}

fn detect_shell_version(shell_type: Option<ShellType>) -> Option<String> {
    match shell_type {
        Some(ShellType::Bash) => std::env::var("BASH_VERSION").ok(),
        Some(ShellType::Zsh) => std::env::var("ZSH_VERSION").ok(),
        Some(ShellType::Fish) => std::env::var("FISH_VERSION").ok(),
        None => None,
    }
}

fn detect_wezterm_socket() -> Option<PathBuf> {
    std::env::var("WEZTERM_UNIX_SOCKET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

fn detect_wezterm_version() -> Option<String> {
    let output = std::process::Command::new("wezterm")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        return None;
    }
    Some(version)
}

/// Detect agents using pane titles and basic heuristics.
#[must_use]
pub fn detect_agents_from_panes(panes: &[PaneInfo]) -> Vec<DetectedAgent> {
    let mut detected = Vec::new();
    for pane in panes {
        let title = pane.title.as_deref().unwrap_or("");
        if let Some((agent_type, indicator)) = detect_agent_from_title(title) {
            detected.push(DetectedAgent {
                agent_type,
                pane_id: pane.pane_id,
                confidence: 0.7,
                indicators: vec![indicator],
            });
        }
    }
    detected
}

fn detect_agent_from_title(title: &str) -> Option<(AgentType, String)> {
    let lower = title.to_lowercase();
    if lower.contains("codex") || lower.contains("openai") {
        return Some((AgentType::Codex, "title:codex".to_string()));
    }
    if lower.contains("claude") {
        return Some((AgentType::ClaudeCode, "title:claude".to_string()));
    }
    if lower.contains("gemini") {
        return Some((AgentType::Gemini, "title:gemini".to_string()));
    }
    None
}

/// Detect remote hosts from pane metadata.
#[must_use]
pub fn detect_remotes_from_panes(panes: &[PaneInfo]) -> Vec<RemoteHost> {
    let mut grouped: HashMap<(ConnectionType, String), Vec<u64>> = HashMap::new();

    for pane in panes {
        let domain = pane.inferred_domain();
        let domain_lower = domain.to_lowercase();
        if domain_lower == "local" {
            continue;
        }

        let cwd_info = pane.parsed_cwd();
        let mut hostname = if cwd_info.is_remote && !cwd_info.host.is_empty() {
            cwd_info.host
        } else {
            domain.clone()
        };

        let connection_type = if domain_lower.starts_with("ssh:") {
            hostname = domain.splitn(2, ':').nth(1).unwrap_or("ssh").to_string();
            ConnectionType::Ssh
        } else if domain_lower.starts_with("wsl:") {
            hostname = domain.splitn(2, ':').nth(1).unwrap_or("wsl").to_string();
            ConnectionType::Wsl
        } else if domain_lower.contains("docker") {
            ConnectionType::Docker
        } else {
            ConnectionType::Unknown
        };

        grouped
            .entry((connection_type, hostname))
            .or_default()
            .push(pane.pane_id);
    }

    grouped
        .into_iter()
        .map(|((connection_type, hostname), pane_ids)| RemoteHost {
            hostname,
            connection_type,
            pane_ids,
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn detect_memory_mb() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb = rest
                .trim()
                .split_whitespace()
                .next()
                .and_then(|val| val.parse::<u64>().ok())?;
            return Some(kb / 1024);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn detect_memory_mb() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn detect_load_average() -> Option<f64> {
    let contents = std::fs::read_to_string("/proc/loadavg").ok()?;
    let first = contents.split_whitespace().next()?;
    first.parse::<f64>().ok()
}

#[cfg(not(target_os = "linux"))]
fn detect_load_average() -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_with_title(id: u64, title: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id,
            tab_id: 1,
            window_id: 1,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: Some(title.to_string()),
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn detects_agents_from_titles() {
        let panes = vec![
            pane_with_title(1, "codex"),
            pane_with_title(2, "Claude Code"),
            pane_with_title(3, "Gemini"),
        ];
        let detected = detect_agents_from_panes(&panes);
        let kinds: Vec<AgentType> = detected.iter().map(|d| d.agent_type).collect();
        assert!(kinds.contains(&AgentType::Codex));
        assert!(kinds.contains(&AgentType::ClaudeCode));
        assert!(kinds.contains(&AgentType::Gemini));
    }

    #[test]
    fn detects_remotes_from_domains() {
        let mut pane = pane_with_title(1, "codex");
        pane.domain_name = Some("ssh:example.com".to_string());
        let remotes = detect_remotes_from_panes(&[pane]);
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].connection_type, ConnectionType::Ssh);
    }

    #[test]
    fn shell_info_from_path_sets_type() {
        let info = ShellInfo::from_shell_path(Some("/bin/bash"));
        assert_eq!(info.shell_type.as_deref(), Some("bash"));
    }
}
