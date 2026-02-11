//! Per-pane terminal state snapshot for session persistence.
//!
//! Captures and serializes terminal state (cursor, alt-screen, scrollback ref,
//! process info, curated env vars) for each pane. Stored in
//! `mux_pane_state.terminal_state_json` and related columns.
//!
//! # Size budget
//!
//! Each pane snapshot targets ≤64KB serialized. If exceeded, env and argv are
//! truncated and a warning is logged.

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

/// Maximum serialized size per pane state (64KB).
pub const PANE_STATE_SIZE_BUDGET: usize = 65_536;

/// Current schema version for pane state snapshots.
pub const PANE_STATE_SCHEMA_VERSION: u32 = 1;

/// Environment variable names that are safe to capture.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "TERM",
    "LANG",
    "EDITOR",
    "FT_WORKSPACE",
    "FT_OUTPUT_FORMAT",
    "VISUAL",
    "USER",
    "HOSTNAME",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
];

/// Patterns that indicate a sensitive env var name.
const SENSITIVE_VAR_PATTERNS: &[&str] = &[
    "SECRET",
    "TOKEN",
    "KEY",
    "PASSWORD",
    "CREDENTIAL",
    "AUTH",
    "API_KEY",
    "PRIVATE",
    "PASSWD",
];

// =============================================================================
// Core types
// =============================================================================

/// Complete per-pane state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneStateSnapshot {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// WezTerm pane ID at capture time.
    pub pane_id: u64,
    /// When this snapshot was captured (epoch ms).
    pub captured_at: u64,

    /// Process info (best-effort).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_process: Option<ProcessInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,

    /// Terminal state.
    pub terminal: TerminalState,

    /// Scrollback linkage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_ref: Option<ScrollbackRef>,

    /// Agent context (populated by agent detection if active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentMetadata>,

    /// Curated environment variables (redacted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<CapturedEnv>,
}

/// Best-effort foreground process information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Process name (e.g., "claude-code", "bash").
    pub name: String,
    /// Process ID if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// First 5 arguments (if available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
}

/// Terminal display state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalState {
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub cursor_row: u16,
    #[serde(default)]
    pub cursor_col: u16,
    #[serde(default)]
    pub is_alt_screen: bool,
    #[serde(default)]
    pub title: String,
}

/// Reference to scrollback data in output_segments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScrollbackRef {
    /// Last captured sequence number in output_segments.
    pub output_segments_seq: i64,
    /// Total lines captured for this pane.
    pub total_lines_captured: u64,
    /// When the last output was captured (epoch ms).
    pub last_capture_at: u64,
}

/// Agent metadata for AI coding agent sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMetadata {
    /// Agent type (e.g., "claude_code", "codex", "gemini").
    pub agent_type: String,
    /// Agent session ID if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Current agent state (e.g., "idle", "working", "rate_limited").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Curated set of environment variables (with redaction).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturedEnv {
    /// Safe environment variables captured.
    pub vars: std::collections::HashMap<String, String>,
    /// Number of variables that were redacted.
    pub redacted_count: usize,
}

// =============================================================================
// Construction
// =============================================================================

impl PaneStateSnapshot {
    /// Create a new pane state snapshot from available data.
    #[must_use]
    pub fn new(pane_id: u64, captured_at: u64, terminal: TerminalState) -> Self {
        Self {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            pane_id,
            captured_at,
            cwd: None,
            foreground_process: None,
            shell: None,
            terminal,
            scrollback_ref: None,
            agent: None,
            env: None,
        }
    }

    /// Set the current working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: String) -> Self {
        self.cwd = Some(cwd);
        self
    }

    /// Set foreground process info.
    #[must_use]
    pub fn with_process(mut self, process: ProcessInfo) -> Self {
        self.foreground_process = Some(process);
        self
    }

    /// Set shell name.
    #[must_use]
    pub fn with_shell(mut self, shell: String) -> Self {
        self.shell = Some(shell);
        self
    }

    /// Set scrollback reference.
    #[must_use]
    pub fn with_scrollback(mut self, scrollback: ScrollbackRef) -> Self {
        debug!(
            pane_id = self.pane_id,
            seq = scrollback.output_segments_seq,
            lines = scrollback.total_lines_captured,
            "Scrollback ref for pane"
        );
        self.scrollback_ref = Some(scrollback);
        self
    }

    /// Set agent metadata.
    #[must_use]
    pub fn with_agent(mut self, agent: AgentMetadata) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Capture and set environment variables from the current process environment.
    ///
    /// Only captures variables from the safe-list and redacts sensitive ones.
    #[must_use]
    pub fn with_env_from_current(mut self) -> Self {
        let env = capture_env_from_iter(std::env::vars());
        trace!(
            pane_id = self.pane_id,
            var_count = env.vars.len(),
            redacted_count = env.redacted_count,
            "Environment capture for pane"
        );
        self.env = Some(env);
        self
    }

    /// Capture environment from an explicit iterator (for testing).
    #[must_use]
    pub fn with_env_from_iter(mut self, vars: impl Iterator<Item = (String, String)>) -> Self {
        let env = capture_env_from_iter(vars);
        trace!(
            pane_id = self.pane_id,
            var_count = env.vars.len(),
            redacted_count = env.redacted_count,
            "Environment capture for pane"
        );
        self.env = Some(env);
        self
    }

    /// Serialize to JSON.
    ///
    /// # Errors
    /// Returns error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Serialize to JSON, enforcing the size budget.
    ///
    /// If the serialized form exceeds `PANE_STATE_SIZE_BUDGET`, env and argv
    /// are progressively truncated. Returns the JSON and whether truncation
    /// occurred.
    pub fn to_json_budgeted(&self) -> Result<(String, bool), serde_json::Error> {
        let json = serde_json::to_string(self)?;
        if json.len() <= PANE_STATE_SIZE_BUDGET {
            return Ok((json, false));
        }

        // Truncate: remove env first, then argv
        tracing::warn!(
            pane_id = self.pane_id,
            actual_bytes = json.len(),
            budget = PANE_STATE_SIZE_BUDGET,
            "Pane state exceeds size budget, truncating"
        );

        let mut truncated = self.clone();
        truncated.env = None;

        let json = serde_json::to_string(&truncated)?;
        if json.len() <= PANE_STATE_SIZE_BUDGET {
            return Ok((json, true));
        }

        // Also truncate argv
        if let Some(ref mut proc) = truncated.foreground_process {
            proc.argv = None;
        }

        let json = serde_json::to_string(&truncated)?;
        Ok((json, true))
    }

    /// Deserialize from JSON.
    ///
    /// # Errors
    /// Returns error if the JSON is invalid.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

// =============================================================================
// Environment capture
// =============================================================================

/// Capture environment variables from an iterator, applying the safe-list
/// and redacting sensitive names.
fn capture_env_from_iter(vars: impl Iterator<Item = (String, String)>) -> CapturedEnv {
    let mut captured = std::collections::HashMap::new();
    let mut redacted_count = 0usize;

    for (key, value) in vars {
        if is_sensitive_var(&key) {
            redacted_count += 1;
            continue;
        }

        if SAFE_ENV_VARS.iter().any(|&safe| safe == key) {
            captured.insert(key, value);
        }
    }

    CapturedEnv {
        vars: captured,
        redacted_count,
    }
}

/// Check if a variable name matches sensitive patterns.
fn is_sensitive_var(name: &str) -> bool {
    let upper = name.to_uppercase();
    SENSITIVE_VAR_PATTERNS.iter().any(|pat| upper.contains(pat))
}

// =============================================================================
// PaneInfo integration
// =============================================================================

impl PaneStateSnapshot {
    /// Build a snapshot from a `PaneInfo` (from wezterm cli list).
    ///
    /// Extracts terminal size, cursor position, title, cwd, and alt-screen
    /// status (from the provided tracker state).
    #[must_use]
    pub fn from_pane_info(
        pane: &crate::wezterm::PaneInfo,
        captured_at: u64,
        is_alt_screen: bool,
    ) -> Self {
        let terminal = TerminalState {
            rows: pane.effective_rows() as u16,
            cols: pane.effective_cols() as u16,
            cursor_row: pane.cursor_y.unwrap_or(0) as u16,
            cursor_col: pane.cursor_x.unwrap_or(0) as u16,
            is_alt_screen,
            title: pane.title.clone().unwrap_or_default(),
        };

        let mut snapshot = Self::new(pane.pane_id, captured_at, terminal);
        if let Some(ref cwd) = pane.cwd {
            let parsed = crate::wezterm::CwdInfo::parse(cwd);
            snapshot.cwd = Some(parsed.path);
        }

        debug!(
            pane_id = pane.pane_id,
            cwd = ?snapshot.cwd,
            alt_screen = is_alt_screen,
            "Captured state for pane"
        );

        snapshot
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_terminal() -> TerminalState {
        TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: 10,
            cursor_col: 5,
            is_alt_screen: false,
            title: "bash".to_string(),
        }
    }

    // ---- Roundtrip ----

    #[test]
    fn pane_state_roundtrip_minimal() {
        let snapshot = PaneStateSnapshot::new(0, 1000, make_terminal());
        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    #[test]
    fn pane_state_roundtrip_full() {
        let snapshot = PaneStateSnapshot::new(5, 2000, make_terminal())
            .with_cwd("/home/user/project".to_string())
            .with_process(ProcessInfo {
                name: "claude-code".to_string(),
                pid: Some(12345),
                argv: Some(vec![
                    "claude-code".to_string(),
                    "--model".to_string(),
                    "opus".to_string(),
                ]),
            })
            .with_shell("zsh".to_string())
            .with_scrollback(ScrollbackRef {
                output_segments_seq: 42,
                total_lines_captured: 1000,
                last_capture_at: 1999,
            })
            .with_agent(AgentMetadata {
                agent_type: "claude_code".to_string(),
                session_id: Some("sess-123".to_string()),
                state: Some("working".to_string()),
            });

        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
    }

    // ---- Alt-screen ----

    #[test]
    fn alt_screen_detection_captured() {
        let terminal = TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: 0,
            cursor_col: 0,
            is_alt_screen: true,
            title: "vim".to_string(),
        };
        let snapshot = PaneStateSnapshot::new(0, 1000, terminal);
        assert!(snapshot.terminal.is_alt_screen);

        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert!(restored.terminal.is_alt_screen);
    }

    // ---- Env redaction ----

    #[test]
    fn env_redaction_secret_vars_removed() {
        let vars = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/user".to_string()),
            ("AWS_SECRET_KEY".to_string(), "super-secret".to_string()),
            ("API_TOKEN".to_string(), "tok-12345".to_string()),
            ("SHELL".to_string(), "/bin/bash".to_string()),
            ("MY_PASSWORD".to_string(), "hunter2".to_string()),
        ];

        let env = capture_env_from_iter(vars.into_iter());

        assert_eq!(env.vars.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(env.vars.get("HOME"), Some(&"/home/user".to_string()));
        assert_eq!(env.vars.get("SHELL"), Some(&"/bin/bash".to_string()));
        assert!(!env.vars.contains_key("AWS_SECRET_KEY"));
        assert!(!env.vars.contains_key("API_TOKEN"));
        assert!(!env.vars.contains_key("MY_PASSWORD"));
        assert_eq!(env.redacted_count, 3);
    }

    #[test]
    fn env_only_captures_safe_vars() {
        let vars = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("RANDOM_VAR".to_string(), "foo".to_string()),
            ("CUSTOM_THING".to_string(), "bar".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ];

        let env = capture_env_from_iter(vars.into_iter());

        assert!(env.vars.contains_key("PATH"));
        assert!(env.vars.contains_key("TERM"));
        assert!(!env.vars.contains_key("RANDOM_VAR"));
        assert!(!env.vars.contains_key("CUSTOM_THING"));
        assert_eq!(env.redacted_count, 0);
    }

    // ---- Size budget ----

    #[test]
    fn size_budget_small_snapshot_not_truncated() {
        let snapshot = PaneStateSnapshot::new(0, 1000, make_terminal());
        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(!truncated);
        assert!(json.len() < PANE_STATE_SIZE_BUDGET);
    }

    #[test]
    fn size_budget_large_env_truncated() {
        let mut vars = std::collections::HashMap::new();
        // Create a large env that will push us over budget
        for i in 0..1000 {
            vars.insert(format!("VAR_{i}"), "x".repeat(100));
        }

        let mut snapshot = PaneStateSnapshot::new(0, 1000, make_terminal());
        snapshot.env = Some(CapturedEnv {
            vars,
            redacted_count: 0,
        });

        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(truncated);
        assert!(json.len() <= PANE_STATE_SIZE_BUDGET);
    }

    // ---- Schema version forward compat ----

    #[test]
    fn schema_version_forward_compat() {
        let json = r#"{
            "schema_version": 2,
            "pane_id": 0,
            "captured_at": 1000,
            "terminal": {"rows": 24, "cols": 80, "cursor_row": 0, "cursor_col": 0, "is_alt_screen": false, "title": ""},
            "future_field": "ignored"
        }"#;
        let snapshot: PaneStateSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(snapshot.pane_id, 0);
    }

    // ---- PaneInfo integration ----

    #[test]
    fn from_pane_info_extracts_state() {
        let pane = crate::wezterm::PaneInfo {
            pane_id: 7,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: Some(crate::wezterm::PaneSize {
                rows: 30,
                cols: 120,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some("claude-code".to_string()),
            cwd: Some("file:///home/user/project".to_string()),
            tty_name: None,
            cursor_x: Some(15),
            cursor_y: Some(20),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };

        let snapshot = PaneStateSnapshot::from_pane_info(&pane, 5000, false);

        assert_eq!(snapshot.pane_id, 7);
        assert_eq!(snapshot.terminal.rows, 30);
        assert_eq!(snapshot.terminal.cols, 120);
        assert_eq!(snapshot.terminal.cursor_row, 20);
        assert_eq!(snapshot.terminal.cursor_col, 15);
        assert!(!snapshot.terminal.is_alt_screen);
        assert_eq!(snapshot.terminal.title, "claude-code");
        assert_eq!(snapshot.cwd, Some("/home/user/project".to_string()));
    }

    // ---- Sensitive var detection ----

    #[test]
    fn is_sensitive_detects_patterns() {
        assert!(is_sensitive_var("AWS_SECRET_KEY"));
        assert!(is_sensitive_var("my_api_token"));
        assert!(is_sensitive_var("DB_PASSWORD"));
        assert!(is_sensitive_var("GITHUB_AUTH"));
        assert!(is_sensitive_var("Private_key_path"));

        assert!(!is_sensitive_var("PATH"));
        assert!(!is_sensitive_var("HOME"));
        assert!(!is_sensitive_var("SHELL"));
        assert!(!is_sensitive_var("TERM"));
    }
}
