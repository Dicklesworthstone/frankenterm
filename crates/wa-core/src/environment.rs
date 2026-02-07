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
            hostname = domain
                .split_once(':')
                .map(|(_, host)| host)
                .unwrap_or("ssh")
                .to_string();
            ConnectionType::Ssh
        } else if domain_lower.starts_with("wsl:") {
            hostname = domain
                .split_once(':')
                .map(|(_, host)| host)
                .unwrap_or("wsl")
                .to_string();
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

// ---------------------------------------------------------------------------
// Auto-configuration engine
// ---------------------------------------------------------------------------

/// Source of a configuration value in the resolution chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    /// Hard-coded default.
    Default,
    /// Auto-detected from the environment.
    AutoDetected,
    /// Explicitly set in the config file.
    ConfigFile,
}

/// A single configuration recommendation produced by the auto-config engine.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigRecommendation {
    /// Config key path (e.g. `ingest.poll_interval_ms`).
    pub key: String,
    /// Recommended value (human-readable).
    pub value: String,
    /// Why this value was chosen.
    pub reason: String,
    /// Where the recommendation came from.
    pub source: ConfigSource,
}

/// Aggregated auto-configuration output from environment detection.
#[derive(Debug, Clone, Serialize)]
pub struct AutoConfig {
    /// Recommended poll interval in milliseconds.
    pub poll_interval_ms: u64,
    /// Recommended minimum poll interval in milliseconds.
    pub min_poll_interval_ms: u64,
    /// Recommended maximum concurrent captures.
    pub max_concurrent_captures: u32,
    /// Recommended pattern packs based on detected agents.
    pub pattern_packs: Vec<String>,
    /// Whether safety should be stricter (remote/production detected).
    pub strict_safety: bool,
    /// Recommended per-pane rate limit (actions/min).
    pub rate_limit_per_pane: u32,
    /// Human-readable recommendations for `wa doctor` output.
    pub recommendations: Vec<ConfigRecommendation>,
}

impl AutoConfig {
    /// Derive optimal configuration from a detected environment.
    #[must_use]
    pub fn from_environment(env: &DetectedEnvironment) -> Self {
        let mut recs = Vec::new();

        // --- Poll interval ---
        let poll_interval_ms = auto_poll_interval(env, &mut recs);
        let min_poll_interval_ms = auto_min_poll_interval(env);

        // --- Concurrency ---
        let max_concurrent_captures = auto_concurrent_captures(env, &mut recs);

        // --- Pattern packs ---
        let pattern_packs = auto_pattern_packs(env, &mut recs);

        // --- Safety ---
        let (strict_safety, rate_limit_per_pane) = auto_safety(env, &mut recs);

        Self {
            poll_interval_ms,
            min_poll_interval_ms,
            max_concurrent_captures,
            pattern_packs,
            strict_safety,
            rate_limit_per_pane,
            recommendations: recs,
        }
    }
}

/// Choose poll interval based on system load, memory, and remote pane presence.
fn auto_poll_interval(env: &DetectedEnvironment, recs: &mut Vec<ConfigRecommendation>) -> u64 {
    let mut interval: u64 = 100; // Base: aggressive polling

    // Back off under high system load.
    if let Some(load) = env.system.load_average {
        #[allow(clippy::cast_precision_loss)]
        let per_core = load / env.system.cpu_count.max(1) as f64;
        if per_core > 2.0 {
            interval = interval.max(500);
            recs.push(ConfigRecommendation {
                key: "ingest.poll_interval_ms".into(),
                value: "500".into(),
                reason: format!(
                    "High per-core load ({per_core:.1}); throttling to reduce overhead"
                ),
                source: ConfigSource::AutoDetected,
            });
        } else if per_core > 1.0 {
            interval = interval.max(200);
            recs.push(ConfigRecommendation {
                key: "ingest.poll_interval_ms".into(),
                value: "200".into(),
                reason: format!(
                    "Moderate per-core load ({per_core:.1}); using conservative interval"
                ),
                source: ConfigSource::AutoDetected,
            });
        }
    }

    // Remote panes add network latency; poll slower to avoid timeout cascades.
    if !env.remotes.is_empty() {
        let prev = interval;
        interval = interval.max(200);
        if interval != prev {
            recs.push(ConfigRecommendation {
                key: "ingest.poll_interval_ms".into(),
                value: format!("{interval}"),
                reason: format!(
                    "Remote panes detected ({} host(s)); increased interval for network latency",
                    env.remotes.len()
                ),
                source: ConfigSource::AutoDetected,
            });
        }
    }

    // Low memory: be gentle.
    if let Some(mem) = env.system.memory_mb {
        if mem < 2048 {
            let prev = interval;
            interval = interval.max(300);
            if interval != prev {
                recs.push(ConfigRecommendation {
                    key: "ingest.poll_interval_ms".into(),
                    value: format!("{interval}"),
                    reason: format!("Low memory ({mem} MB); reduced polling frequency"),
                    source: ConfigSource::AutoDetected,
                });
            }
        }
    }

    interval
}

/// Minimum poll interval adapts to CPU cores.
fn auto_min_poll_interval(env: &DetectedEnvironment) -> u64 {
    if env.system.cpu_count >= 8 {
        25 // Fast CPUs can handle aggressive min interval
    } else if env.system.cpu_count >= 4 {
        50
    } else {
        100
    }
}

/// Scale concurrency with CPU count, capped by observed pane count.
fn auto_concurrent_captures(
    env: &DetectedEnvironment,
    recs: &mut Vec<ConfigRecommendation>,
) -> u32 {
    let cpu_based = (env.system.cpu_count * 2).min(32) as u32;
    let result = cpu_based.max(4); // At least 4

    if result != 10 {
        // 10 is the hard-coded default
        recs.push(ConfigRecommendation {
            key: "ingest.max_concurrent_captures".into(),
            value: format!("{result}"),
            reason: format!(
                "Scaled to {} CPUs (2× cores, min 4, max 32)",
                env.system.cpu_count
            ),
            source: ConfigSource::AutoDetected,
        });
    }

    result
}

/// Select pattern packs based on detected agent types.
fn auto_pattern_packs(
    env: &DetectedEnvironment,
    recs: &mut Vec<ConfigRecommendation>,
) -> Vec<String> {
    let mut packs = vec!["builtin:core".to_string()];

    let mut agent_packs = Vec::new();
    for agent in &env.agents {
        let pack = match agent.agent_type {
            AgentType::Codex => "builtin:codex",
            AgentType::ClaudeCode => "builtin:claude_code",
            AgentType::Gemini => "builtin:gemini",
            _ => continue,
        };
        if !packs.contains(&pack.to_string()) {
            packs.push(pack.to_string());
            agent_packs.push(pack);
        }
    }

    if !agent_packs.is_empty() {
        recs.push(ConfigRecommendation {
            key: "patterns.packs".into(),
            value: format!("{agent_packs:?}"),
            reason: format!(
                "Detected {} agent(s) in panes; enabled matching pattern packs",
                env.agents.len()
            ),
            source: ConfigSource::AutoDetected,
        });
    }

    packs.dedup();
    packs
}

/// Determine safety strictness from remote hosts and production indicators.
fn auto_safety(env: &DetectedEnvironment, recs: &mut Vec<ConfigRecommendation>) -> (bool, u32) {
    let mut strict = false;
    let mut rate = 30_u32; // Default per-pane rate

    // Remote panes need stricter safety.
    if !env.remotes.is_empty() {
        strict = true;
        rate = 15;
        recs.push(ConfigRecommendation {
            key: "safety.rate_limit_per_pane".into(),
            value: format!("{rate}"),
            reason: format!(
                "Remote panes detected ({} host(s)); reduced per-pane rate limit",
                env.remotes.len()
            ),
            source: ConfigSource::AutoDetected,
        });
    }

    // Production hostnames trigger maximum caution.
    for remote in &env.remotes {
        let host_lower = remote.hostname.to_lowercase();
        if host_lower.contains("prod")
            || host_lower.contains("live")
            || host_lower.contains("production")
        {
            strict = true;
            rate = rate.min(10);
            recs.push(ConfigRecommendation {
                key: "safety".into(),
                value: "strict".into(),
                reason: format!(
                    "Production host detected ({}); enabling strict safety mode",
                    remote.hostname
                ),
                source: ConfigSource::AutoDetected,
            });
            break;
        }
    }

    (strict, rate)
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

    // --- AutoConfig tests ---

    fn make_env(
        cpu_count: usize,
        memory_mb: Option<u64>,
        load_average: Option<f64>,
        agents: Vec<DetectedAgent>,
        remotes: Vec<RemoteHost>,
    ) -> DetectedEnvironment {
        DetectedEnvironment {
            wezterm: WeztermInfo {
                version: None,
                socket_path: None,
                is_running: false,
                capabilities: WeztermCapabilities::default(),
            },
            shell: ShellInfo {
                shell_path: Some("/bin/zsh".into()),
                shell_type: Some("zsh".into()),
                version: None,
                config_file: None,
                osc_133_enabled: false,
            },
            agents,
            remotes,
            system: SystemInfo {
                os: "linux".into(),
                arch: "x86_64".into(),
                cpu_count,
                memory_mb,
                load_average,
                detected_at_epoch_ms: 0,
            },
            detected_at: Utc::now(),
        }
    }

    #[test]
    fn auto_config_idle_system_uses_fast_polling() {
        let env = make_env(8, Some(16384), Some(0.5), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 100);
        assert_eq!(auto.min_poll_interval_ms, 25);
    }

    #[test]
    fn auto_config_high_load_throttles_polling() {
        // 4 cores, load 10.0 → per-core 2.5 → 500ms
        let env = make_env(4, Some(8192), Some(10.0), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 500);
    }

    #[test]
    fn auto_config_moderate_load_uses_200ms() {
        // 4 cores, load 6.0 → per-core 1.5 → 200ms
        let env = make_env(4, Some(8192), Some(6.0), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 200);
    }

    #[test]
    fn auto_config_low_memory_increases_interval() {
        let env = make_env(2, Some(1024), Some(0.1), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert!(auto.poll_interval_ms >= 300);
    }

    #[test]
    fn auto_config_remote_panes_increase_interval() {
        let remotes = vec![RemoteHost {
            hostname: "dev-server".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1, 2],
        }];
        let env = make_env(8, Some(16384), Some(0.5), vec![], remotes);
        let auto = AutoConfig::from_environment(&env);
        assert!(auto.poll_interval_ms >= 200);
        assert!(auto.strict_safety);
        assert!(auto.rate_limit_per_pane < 30);
    }

    #[test]
    fn auto_config_production_host_enables_strict() {
        let remotes = vec![RemoteHost {
            hostname: "web-prod-01".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1],
        }];
        let env = make_env(8, Some(16384), Some(0.5), vec![], remotes);
        let auto = AutoConfig::from_environment(&env);
        assert!(auto.strict_safety);
        assert!(auto.rate_limit_per_pane <= 10);
    }

    #[test]
    fn auto_config_agent_packs_selected() {
        let agents = vec![
            DetectedAgent {
                agent_type: AgentType::Codex,
                pane_id: 1,
                confidence: 0.9,
                indicators: vec!["title:codex".into()],
            },
            DetectedAgent {
                agent_type: AgentType::ClaudeCode,
                pane_id: 2,
                confidence: 0.8,
                indicators: vec!["title:claude".into()],
            },
        ];
        let env = make_env(4, Some(8192), None, agents, vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert!(auto.pattern_packs.contains(&"builtin:core".to_string()));
        assert!(auto.pattern_packs.contains(&"builtin:codex".to_string()));
        assert!(
            auto.pattern_packs
                .contains(&"builtin:claude_code".to_string())
        );
    }

    #[test]
    fn auto_config_no_agents_only_core_pack() {
        let env = make_env(4, Some(8192), None, vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.pattern_packs, vec!["builtin:core"]);
    }

    #[test]
    fn auto_config_concurrent_captures_scales_with_cpus() {
        let env_small = make_env(2, Some(4096), None, vec![], vec![]);
        let env_large = make_env(16, Some(32768), None, vec![], vec![]);
        let auto_small = AutoConfig::from_environment(&env_small);
        let auto_large = AutoConfig::from_environment(&env_large);
        assert_eq!(auto_small.max_concurrent_captures, 4);
        assert_eq!(auto_large.max_concurrent_captures, 32);
    }

    #[test]
    fn auto_config_min_poll_scales_with_cpus() {
        let env_2 = make_env(2, Some(4096), None, vec![], vec![]);
        let env_4 = make_env(4, Some(8192), None, vec![], vec![]);
        let env_8 = make_env(8, Some(16384), None, vec![], vec![]);
        assert_eq!(
            AutoConfig::from_environment(&env_2).min_poll_interval_ms,
            100
        );
        assert_eq!(
            AutoConfig::from_environment(&env_4).min_poll_interval_ms,
            50
        );
        assert_eq!(
            AutoConfig::from_environment(&env_8).min_poll_interval_ms,
            25
        );
    }

    #[test]
    fn auto_config_serializes() {
        let env = make_env(4, Some(8192), Some(1.0), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        let json = serde_json::to_string(&auto).unwrap();
        assert!(json.contains("poll_interval_ms"));
        assert!(json.contains("pattern_packs"));
    }

    #[test]
    fn auto_config_recommendations_populated_for_non_defaults() {
        let agents = vec![DetectedAgent {
            agent_type: AgentType::Gemini,
            pane_id: 3,
            confidence: 0.7,
            indicators: vec!["title:gemini".into()],
        }];
        let remotes = vec![RemoteHost {
            hostname: "staging".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![4],
        }];
        let env = make_env(2, Some(1024), Some(8.0), agents, remotes);
        let auto = AutoConfig::from_environment(&env);
        assert!(!auto.recommendations.is_empty());
        let keys: Vec<&str> = auto
            .recommendations
            .iter()
            .map(|r| r.key.as_str())
            .collect();
        assert!(keys.contains(&"ingest.poll_interval_ms"));
        assert!(keys.contains(&"patterns.packs"));
        assert!(keys.contains(&"safety.rate_limit_per_pane"));
    }

    #[test]
    fn auto_config_empty_env_uses_safe_defaults() {
        let env = make_env(1, None, None, vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 100);
        assert!(!auto.strict_safety);
        assert_eq!(auto.rate_limit_per_pane, 30);
        assert_eq!(auto.pattern_packs, vec!["builtin:core"]);
    }

    // -----------------------------------------------------------------------
    // Serde roundtrip tests for all public types
    // -----------------------------------------------------------------------

    #[test]
    fn wezterm_capabilities_serde_roundtrip() {
        let caps = WeztermCapabilities {
            cli_available: true,
            json_output: true,
            multiplexing: false,
            osc_133: true,
            osc_7: false,
            image_protocol: true,
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("\"cli_available\":true"));
        assert!(json.contains("\"osc_133\":true"));
        assert!(json.contains("\"multiplexing\":false"));
    }

    #[test]
    fn wezterm_info_serde_roundtrip() {
        let info = WeztermInfo {
            version: Some("20230712-072601-f4abf8fd".to_string()),
            socket_path: Some(PathBuf::from("/tmp/wezterm-sock")),
            is_running: true,
            capabilities: WeztermCapabilities::default(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("20230712"));
        assert!(json.contains("wezterm-sock"));
        assert!(json.contains("\"is_running\":true"));
    }

    #[test]
    fn shell_info_serde_roundtrip() {
        let info = ShellInfo {
            shell_path: Some("/usr/bin/fish".into()),
            shell_type: Some("fish".into()),
            version: Some("3.7.0".into()),
            config_file: Some(PathBuf::from("/home/user/.config/fish/config.fish")),
            osc_133_enabled: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("fish"));
        assert!(json.contains("\"osc_133_enabled\":true"));
        assert!(json.contains("3.7.0"));
    }

    #[test]
    fn system_info_serde_roundtrip() {
        let info = SystemInfo {
            os: "linux".into(),
            arch: "aarch64".into(),
            cpu_count: 12,
            memory_mb: Some(32768),
            load_average: Some(1.5),
            detected_at_epoch_ms: 1700000000000,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"aarch64\""));
        assert!(json.contains("32768"));
        assert!(json.contains("1.5"));
    }

    #[test]
    fn detected_agent_serde_roundtrip() {
        let agent = DetectedAgent {
            agent_type: AgentType::Codex,
            pane_id: 42,
            confidence: 0.85,
            indicators: vec!["title:codex".into(), "output:openai".into()],
        };
        let json = serde_json::to_string(&agent).unwrap();
        assert!(json.contains("42"));
        assert!(json.contains("0.85"));
        assert!(json.contains("output:openai"));
    }

    #[test]
    fn remote_host_serde_roundtrip() {
        let host = RemoteHost {
            hostname: "dev.example.com".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&host).unwrap();
        assert!(json.contains("dev.example.com"));
        assert!(json.contains("\"ssh\""));
        assert!(json.contains("[1,2,3]"));
    }

    #[test]
    fn connection_type_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ConnectionType::Ssh).unwrap(),
            "\"ssh\""
        );
        assert_eq!(
            serde_json::to_string(&ConnectionType::Wsl).unwrap(),
            "\"wsl\""
        );
        assert_eq!(
            serde_json::to_string(&ConnectionType::Docker).unwrap(),
            "\"docker\""
        );
        assert_eq!(
            serde_json::to_string(&ConnectionType::Unknown).unwrap(),
            "\"unknown\""
        );
    }

    #[test]
    fn config_source_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ConfigSource::Default).unwrap(),
            "\"default\""
        );
        assert_eq!(
            serde_json::to_string(&ConfigSource::AutoDetected).unwrap(),
            "\"auto_detected\""
        );
        assert_eq!(
            serde_json::to_string(&ConfigSource::ConfigFile).unwrap(),
            "\"config_file\""
        );
    }

    #[test]
    fn detected_environment_serde_roundtrip() {
        let env = make_env(4, Some(8192), Some(1.0), vec![], vec![]);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"wezterm\""));
        assert!(json.contains("\"shell\""));
        assert!(json.contains("\"agents\""));
        assert!(json.contains("\"remotes\""));
        assert!(json.contains("\"system\""));
        assert!(json.contains("\"detected_at\""));
    }

    #[test]
    fn config_recommendation_serde_roundtrip() {
        let rec = ConfigRecommendation {
            key: "ingest.poll_interval_ms".into(),
            value: "200".into(),
            reason: "test reason".into(),
            source: ConfigSource::AutoDetected,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("ingest.poll_interval_ms"));
        assert!(json.contains("\"auto_detected\""));
    }

    // -----------------------------------------------------------------------
    // Shell detection edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn shell_info_from_zsh_path() {
        let info = ShellInfo::from_shell_path(Some("/bin/zsh"));
        assert_eq!(info.shell_type.as_deref(), Some("zsh"));
        assert_eq!(info.shell_path.as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn shell_info_from_fish_path() {
        let info = ShellInfo::from_shell_path(Some("/usr/bin/fish"));
        assert_eq!(info.shell_type.as_deref(), Some("fish"));
    }

    #[test]
    fn shell_info_from_unknown_shell() {
        let info = ShellInfo::from_shell_path(Some("/usr/local/bin/nushell"));
        assert!(info.shell_type.is_none());
        assert_eq!(info.shell_path.as_deref(), Some("/usr/local/bin/nushell"));
    }

    #[test]
    fn shell_info_from_none_path() {
        let info = ShellInfo::from_shell_path(None);
        assert!(info.shell_type.is_none());
        assert!(info.shell_path.is_none());
        assert!(info.version.is_none());
        assert!(info.config_file.is_none());
        assert!(!info.osc_133_enabled);
    }

    #[test]
    fn shell_info_from_empty_path() {
        let info = ShellInfo::from_shell_path(Some(""));
        assert!(info.shell_type.is_none());
    }

    // -----------------------------------------------------------------------
    // Agent detection edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn detect_agents_empty_panes() {
        let detected = detect_agents_from_panes(&[]);
        assert!(detected.is_empty());
    }

    #[test]
    fn detect_agents_no_matching_titles() {
        let panes = vec![
            pane_with_title(1, "vim"),
            pane_with_title(2, "htop"),
            pane_with_title(3, "zsh"),
        ];
        let detected = detect_agents_from_panes(&panes);
        assert!(detected.is_empty());
    }

    #[test]
    fn detect_agents_case_insensitive() {
        let panes = vec![
            pane_with_title(1, "CODEX"),
            pane_with_title(2, "Claude Code"),
            pane_with_title(3, "GEMINI Pro"),
        ];
        let detected = detect_agents_from_panes(&panes);
        assert_eq!(detected.len(), 3);
    }

    #[test]
    fn detect_agents_partial_match() {
        let panes = vec![
            pane_with_title(1, "my-codex-session"),
            pane_with_title(2, "running claude code assistant"),
        ];
        let detected = detect_agents_from_panes(&panes);
        assert_eq!(detected.len(), 2);
        assert_eq!(detected[0].agent_type, AgentType::Codex);
        assert_eq!(detected[1].agent_type, AgentType::ClaudeCode);
    }

    #[test]
    fn detect_agents_openai_matches_codex() {
        let panes = vec![pane_with_title(1, "OpenAI CLI")];
        let detected = detect_agents_from_panes(&panes);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].agent_type, AgentType::Codex);
        assert_eq!(detected[0].indicators, vec!["title:codex"]);
    }

    #[test]
    fn detect_agents_empty_title_ignored() {
        let panes = vec![pane_with_title(1, "")];
        let detected = detect_agents_from_panes(&panes);
        assert!(detected.is_empty());
    }

    #[test]
    fn detect_agents_none_title_ignored() {
        let pane = PaneInfo {
            pane_id: 1,
            tab_id: 1,
            window_id: 1,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
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
        };
        let detected = detect_agents_from_panes(&[pane]);
        assert!(detected.is_empty());
    }

    #[test]
    fn detect_agents_confidence_and_pane_id() {
        let panes = vec![pane_with_title(99, "codex")];
        let detected = detect_agents_from_panes(&panes);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].pane_id, 99);
        assert!((detected[0].confidence - 0.7).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Remote detection edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn detect_remotes_wsl_domain() {
        let mut pane = pane_with_title(1, "bash");
        pane.domain_name = Some("wsl:Ubuntu-22.04".to_string());
        let remotes = detect_remotes_from_panes(&[pane]);
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].connection_type, ConnectionType::Wsl);
        assert_eq!(remotes[0].hostname, "Ubuntu-22.04");
    }

    #[test]
    fn detect_remotes_docker_domain() {
        let mut pane = pane_with_title(1, "bash");
        pane.domain_name = Some("docker:my-container".to_string());
        let remotes = detect_remotes_from_panes(&[pane]);
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].connection_type, ConnectionType::Docker);
    }

    #[test]
    fn detect_remotes_unknown_domain() {
        let mut pane = pane_with_title(1, "bash");
        pane.domain_name = Some("mux:server1".to_string());
        let remotes = detect_remotes_from_panes(&[pane]);
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].connection_type, ConnectionType::Unknown);
    }

    #[test]
    fn detect_remotes_local_domain_excluded() {
        let mut pane = pane_with_title(1, "bash");
        pane.domain_name = Some("local".to_string());
        let remotes = detect_remotes_from_panes(&[pane]);
        assert!(remotes.is_empty());
    }

    #[test]
    fn detect_remotes_local_case_insensitive() {
        let mut pane = pane_with_title(1, "bash");
        pane.domain_name = Some("Local".to_string());
        let remotes = detect_remotes_from_panes(&[pane]);
        assert!(remotes.is_empty());
    }

    #[test]
    fn detect_remotes_empty_panes() {
        let remotes = detect_remotes_from_panes(&[]);
        assert!(remotes.is_empty());
    }

    #[test]
    fn detect_remotes_groups_same_host() {
        let mut p1 = pane_with_title(1, "bash");
        p1.domain_name = Some("ssh:server1".to_string());
        let mut p2 = pane_with_title(2, "vim");
        p2.domain_name = Some("ssh:server1".to_string());
        let remotes = detect_remotes_from_panes(&[p1, p2]);
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].pane_ids.len(), 2);
        assert!(remotes[0].pane_ids.contains(&1));
        assert!(remotes[0].pane_ids.contains(&2));
    }

    #[test]
    fn detect_remotes_separates_different_hosts() {
        let mut p1 = pane_with_title(1, "bash");
        p1.domain_name = Some("ssh:server1".to_string());
        let mut p2 = pane_with_title(2, "bash");
        p2.domain_name = Some("ssh:server2".to_string());
        let remotes = detect_remotes_from_panes(&[p1, p2]);
        assert_eq!(remotes.len(), 2);
    }

    #[test]
    fn detect_remotes_from_cwd_uri() {
        // Pane without domain_name but with remote cwd
        let mut pane = pane_with_title(1, "bash");
        pane.domain_name = None;
        pane.cwd = Some("file://remote-host/home/user".to_string());
        let remotes = detect_remotes_from_panes(&[pane]);
        // inferred_domain returns "ssh:remote-host" when cwd is remote
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].connection_type, ConnectionType::Ssh);
    }

    // -----------------------------------------------------------------------
    // Compound AutoConfig scenarios
    // -----------------------------------------------------------------------

    #[test]
    fn auto_config_high_load_plus_low_memory_plus_remote() {
        let remotes = vec![RemoteHost {
            hostname: "staging.internal".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1],
        }];
        // 2 cores, load 6.0 (per-core 3.0) → 500ms, memory 1GB → ≥300ms, remote → ≥200ms
        let env = make_env(2, Some(1024), Some(6.0), vec![], remotes);
        let auto = AutoConfig::from_environment(&env);
        // All three factors push interval up; highest wins
        assert_eq!(auto.poll_interval_ms, 500);
        assert!(auto.strict_safety);
        assert!(auto.rate_limit_per_pane < 30);
    }

    #[test]
    fn auto_config_production_plus_agents() {
        let agents = vec![
            DetectedAgent {
                agent_type: AgentType::Codex,
                pane_id: 1,
                confidence: 0.9,
                indicators: vec!["title:codex".into()],
            },
            DetectedAgent {
                agent_type: AgentType::ClaudeCode,
                pane_id: 2,
                confidence: 0.8,
                indicators: vec!["title:claude".into()],
            },
            DetectedAgent {
                agent_type: AgentType::Gemini,
                pane_id: 3,
                confidence: 0.7,
                indicators: vec!["title:gemini".into()],
            },
        ];
        let remotes = vec![RemoteHost {
            hostname: "production-api".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![4],
        }];
        let env = make_env(8, Some(16384), Some(0.5), agents, remotes);
        let auto = AutoConfig::from_environment(&env);
        // Production host → strict safety, rate ≤ 10
        assert!(auto.strict_safety);
        assert!(auto.rate_limit_per_pane <= 10);
        // All three agent packs plus core
        assert!(auto.pattern_packs.contains(&"builtin:codex".to_string()));
        assert!(
            auto.pattern_packs
                .contains(&"builtin:claude_code".to_string())
        );
        assert!(auto.pattern_packs.contains(&"builtin:gemini".to_string()));
    }

    #[test]
    fn auto_config_boundary_load_at_exactly_1_0() {
        // per-core load 1.0 exactly → NOT moderate (condition is > 1.0)
        let env = make_env(4, Some(8192), Some(4.0), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 100);
    }

    #[test]
    fn auto_config_boundary_load_just_above_1_0() {
        // per-core load 1.01 → moderate → 200ms
        let env = make_env(4, Some(8192), Some(4.04), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 200);
    }

    #[test]
    fn auto_config_boundary_load_at_exactly_2_0() {
        // per-core load 2.0 exactly → moderate (condition for high is > 2.0)
        let env = make_env(4, Some(8192), Some(8.0), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 200);
    }

    #[test]
    fn auto_config_boundary_load_just_above_2_0() {
        // per-core load 2.01 → high → 500ms
        let env = make_env(4, Some(8192), Some(8.04), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 500);
    }

    #[test]
    fn auto_config_boundary_memory_at_2048() {
        // 2048 MB is NOT low (condition is < 2048)
        let env = make_env(4, Some(2048), Some(0.1), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.poll_interval_ms, 100);
    }

    #[test]
    fn auto_config_boundary_memory_at_2047() {
        // 2047 MB IS low → interval ≥ 300
        let env = make_env(4, Some(2047), Some(0.1), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert!(auto.poll_interval_ms >= 300);
    }

    #[test]
    fn auto_config_concurrent_captures_capped_at_32() {
        // 64 cores → 128 but capped at 32
        let env = make_env(64, Some(65536), None, vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.max_concurrent_captures, 32);
    }

    #[test]
    fn auto_config_concurrent_captures_min_4() {
        // 1 core → 2 but floor is 4
        let env = make_env(1, Some(4096), None, vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);
        assert_eq!(auto.max_concurrent_captures, 4);
    }

    #[test]
    fn auto_config_production_hostname_live() {
        let remotes = vec![RemoteHost {
            hostname: "api-live-01".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1],
        }];
        let env = make_env(8, Some(16384), Some(0.5), vec![], remotes);
        let auto = AutoConfig::from_environment(&env);
        assert!(auto.strict_safety);
        assert!(auto.rate_limit_per_pane <= 10);
    }

    #[test]
    fn auto_config_non_production_hostname_not_strict() {
        let remotes = vec![RemoteHost {
            hostname: "staging-01".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1],
        }];
        let env = make_env(8, Some(16384), Some(0.5), vec![], remotes);
        let auto = AutoConfig::from_environment(&env);
        // Remote → strict, but rate not capped to 10 (no production host)
        assert!(auto.strict_safety);
        assert_eq!(auto.rate_limit_per_pane, 15);
    }

    #[test]
    fn auto_config_duplicate_agent_types_deduped() {
        let agents = vec![
            DetectedAgent {
                agent_type: AgentType::Codex,
                pane_id: 1,
                confidence: 0.9,
                indicators: vec!["title:codex".into()],
            },
            DetectedAgent {
                agent_type: AgentType::Codex,
                pane_id: 2,
                confidence: 0.8,
                indicators: vec!["title:openai".into()],
            },
        ];
        let env = make_env(4, Some(8192), None, agents, vec![]);
        let auto = AutoConfig::from_environment(&env);
        let codex_count = auto
            .pattern_packs
            .iter()
            .filter(|p| *p == "builtin:codex")
            .count();
        assert_eq!(codex_count, 1, "codex pack should appear exactly once");
    }

    #[test]
    fn auto_config_recommendations_have_correct_sources() {
        let remotes = vec![RemoteHost {
            hostname: "web-prod-01".into(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1],
        }];
        let env = make_env(2, Some(1024), Some(8.0), vec![], remotes);
        let auto = AutoConfig::from_environment(&env);
        for rec in &auto.recommendations {
            assert_eq!(rec.source, ConfigSource::AutoDetected);
            assert!(!rec.key.is_empty());
            assert!(!rec.value.is_empty());
            assert!(!rec.reason.is_empty());
        }
    }

    // -----------------------------------------------------------------------
    // WeztermCapabilities defaults
    // -----------------------------------------------------------------------

    #[test]
    fn wezterm_capabilities_default_all_false() {
        let caps = WeztermCapabilities::default();
        assert!(!caps.cli_available);
        assert!(!caps.json_output);
        assert!(!caps.multiplexing);
        assert!(!caps.osc_133);
        assert!(!caps.osc_7);
        assert!(!caps.image_protocol);
    }

    // -----------------------------------------------------------------------
    // WeztermInfo with no WezTerm
    // -----------------------------------------------------------------------

    #[test]
    fn wezterm_info_missing_wezterm() {
        let info = WeztermInfo {
            version: None,
            socket_path: None,
            is_running: false,
            capabilities: WeztermCapabilities::default(),
        };
        assert!(!info.is_running);
        assert!(info.version.is_none());
        assert!(info.socket_path.is_none());
    }

    // -----------------------------------------------------------------------
    // ConnectionType equality and hash
    // -----------------------------------------------------------------------

    #[test]
    fn connection_type_equality() {
        assert_eq!(ConnectionType::Ssh, ConnectionType::Ssh);
        assert_ne!(ConnectionType::Ssh, ConnectionType::Wsl);
        assert_ne!(ConnectionType::Docker, ConnectionType::Unknown);
    }
}
