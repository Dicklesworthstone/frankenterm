//! WezTerm CLI client wrapper
//!
//! Provides a type-safe interface to WezTerm's CLI commands.
//!
//! ## JSON Model Design
//!
//! WezTerm's CLI output can vary between versions. We design for robustness:
//! - All non-ID fields are optional with sane defaults
//! - Unknown fields are ignored via `#[serde(flatten)]` with `Value`
//! - Domain inference falls back to `local` if not explicitly provided

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::error::WeztermError;

/// Pane size information
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaneSize {
    /// Number of rows (character cells)
    #[serde(default)]
    pub rows: u32,
    /// Number of columns (character cells)
    #[serde(default)]
    pub cols: u32,
    /// Pixel width (if available)
    #[serde(default)]
    pub pixel_width: Option<u32>,
    /// Pixel height (if available)
    #[serde(default)]
    pub pixel_height: Option<u32>,
    /// DPI (if available)
    #[serde(default)]
    pub dpi: Option<u32>,
}

/// Cursor visibility state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum CursorVisibility {
    /// Cursor is visible
    #[default]
    Visible,
    /// Cursor is hidden
    Hidden,
}

/// Parsed working directory URI with domain inference
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CwdInfo {
    /// Raw URI string from WezTerm (e.g., "file:///home/user" or "file://remote-host/path")
    pub raw_uri: String,
    /// Extracted path component
    pub path: String,
    /// Inferred host (empty string for local)
    pub host: String,
    /// Whether this is a remote cwd
    pub is_remote: bool,
}

impl CwdInfo {
    /// Parse a cwd URI string into components
    ///
    /// WezTerm uses file:// URIs:
    /// - Local: `file:///home/user` (host empty, 3 slashes)
    /// - Remote: `file://hostname/path` (host present, 2 slashes before host)
    #[must_use]
    #[allow(clippy::option_if_let_else)] // if-let-else is clearer for this multi-branch logic
    pub fn parse(uri: &str) -> Self {
        let uri = uri.trim();

        if uri.is_empty() {
            return Self::default();
        }

        // Handle file:// scheme
        if let Some(rest) = uri.strip_prefix("file://") {
            // file:///path -> local (empty host, path starts with /)
            // file://host/path -> remote
            if rest.starts_with('/') {
                // Local path
                Self {
                    raw_uri: uri.to_string(),
                    path: rest.to_string(),
                    host: String::new(),
                    is_remote: false,
                }
            } else if let Some(slash_pos) = rest.find('/') {
                // Remote path: host/path
                let host = &rest[..slash_pos];
                let path = &rest[slash_pos..];
                Self {
                    raw_uri: uri.to_string(),
                    path: path.to_string(),
                    host: host.to_string(),
                    is_remote: true,
                }
            } else {
                // Just host, no path
                Self {
                    raw_uri: uri.to_string(),
                    path: String::new(),
                    host: rest.to_string(),
                    is_remote: true,
                }
            }
        } else {
            // Not a file:// URI, treat as raw path
            Self {
                raw_uri: uri.to_string(),
                path: uri.to_string(),
                host: String::new(),
                is_remote: false,
            }
        }
    }
}

/// Information about a WezTerm pane from `wezterm cli list --format json`
///
/// This struct is designed to tolerate unknown fields and missing optional fields.
/// Required fields (pane_id, tab_id, window_id) will cause parse failure if missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Unique pane ID (required)
    pub pane_id: u64,
    /// Tab ID containing this pane (required)
    pub tab_id: u64,
    /// Window ID containing this pane (required)
    pub window_id: u64,

    // --- Domain identification ---
    /// Domain ID (if provided)
    #[serde(default)]
    pub domain_id: Option<u64>,
    /// Domain name (prefer this for identification)
    #[serde(default)]
    pub domain_name: Option<String>,
    /// Workspace name
    #[serde(default)]
    pub workspace: Option<String>,

    // --- Size information ---
    /// Pane size (may be nested or flat depending on version)
    #[serde(default)]
    pub size: Option<PaneSize>,
    /// Legacy/flat rows field (fallback if size not present)
    #[serde(default)]
    pub rows: Option<u32>,
    /// Legacy/flat cols field (fallback if size not present)
    #[serde(default)]
    pub cols: Option<u32>,

    // --- Pane content/state ---
    /// Pane title (from shell or application)
    #[serde(default)]
    pub title: Option<String>,
    /// Current working directory as URI
    #[serde(default)]
    pub cwd: Option<String>,
    /// TTY device name (e.g., "/dev/pts/0")
    #[serde(default)]
    pub tty_name: Option<String>,

    // --- Cursor state ---
    /// Cursor column position
    #[serde(default)]
    pub cursor_x: Option<u32>,
    /// Cursor row position
    #[serde(default)]
    pub cursor_y: Option<u32>,
    /// Cursor visibility
    #[serde(default)]
    pub cursor_visibility: Option<CursorVisibility>,

    // --- Viewport state ---
    /// Left column of viewport (for scrollback)
    #[serde(default)]
    pub left_col: Option<u32>,
    /// Top row of viewport (for scrollback)
    #[serde(default)]
    pub top_row: Option<i64>,

    // --- Boolean flags ---
    /// Whether this is the active pane in its tab
    #[serde(default)]
    pub is_active: bool,
    /// Whether this pane is zoomed
    #[serde(default)]
    pub is_zoomed: bool,

    // --- Unknown fields (for forward compatibility) ---
    /// Any additional fields we don't recognize
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, Value>,
}

impl PaneInfo {
    /// Get the effective domain name, falling back to "local" if not specified
    #[must_use]
    pub fn effective_domain(&self) -> &str {
        self.domain_name.as_deref().unwrap_or("local")
    }

    /// Get the effective number of rows
    #[must_use]
    pub fn effective_rows(&self) -> u32 {
        self.size
            .as_ref()
            .map(|s| s.rows)
            .or(self.rows)
            .unwrap_or(24)
    }

    /// Get the effective number of columns
    #[must_use]
    pub fn effective_cols(&self) -> u32 {
        self.size
            .as_ref()
            .map(|s| s.cols)
            .or(self.cols)
            .unwrap_or(80)
    }

    /// Parse the cwd field into structured components
    #[must_use]
    pub fn parsed_cwd(&self) -> CwdInfo {
        self.cwd.as_deref().map(CwdInfo::parse).unwrap_or_default()
    }

    /// Infer the domain from available information
    ///
    /// Priority:
    /// 1. Explicit `domain_name` field
    /// 2. Remote host from `cwd` URI
    /// 3. Default to "local"
    #[must_use]
    pub fn inferred_domain(&self) -> String {
        // First try explicit domain_name
        if let Some(ref name) = self.domain_name {
            if !name.is_empty() {
                return name.clone();
            }
        }

        // Try to infer from cwd URI
        let cwd_info = self.parsed_cwd();
        if cwd_info.is_remote && !cwd_info.host.is_empty() {
            return format!("ssh:{}", cwd_info.host);
        }

        // Default to local
        "local".to_string()
    }

    /// Get the title, with a default fallback
    #[must_use]
    pub fn effective_title(&self) -> &str {
        self.title.as_deref().unwrap_or("")
    }
}

/// Control characters that can be sent to panes
pub mod control {
    /// Ctrl+C (SIGINT / interrupt)
    pub const CTRL_C: &str = "\x03";
    /// Ctrl+D (EOF)
    pub const CTRL_D: &str = "\x04";
    /// Ctrl+Z (SIGTSTP / suspend)
    pub const CTRL_Z: &str = "\x1a";
    /// Ctrl+\\ (SIGQUIT)
    pub const CTRL_BACKSLASH: &str = "\x1c";
    /// Enter/Return
    pub const ENTER: &str = "\r";
    /// Escape
    pub const ESCAPE: &str = "\x1b";
}

/// Default command timeout in seconds
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// WezTerm CLI client for interacting with WezTerm instances
///
/// This client wraps the `wezterm cli` commands and provides a type-safe
/// async interface for:
/// - Listing panes
/// - Reading pane content
/// - Sending text (including control characters)
///
/// # Error Handling
///
/// The client provides stable error variants to help callers distinguish
/// between different failure modes:
/// - `CliNotFound`: wezterm binary not in PATH
/// - `NotRunning`: wezterm process not running
/// - `PaneNotFound`: specified pane ID doesn't exist
/// - `Timeout`: command took too long
pub struct WeztermClient {
    /// Optional socket path override (WEZTERM_UNIX_SOCKET)
    socket_path: Option<String>,
    /// Command timeout in seconds
    timeout_secs: u64,
}

impl Default for WeztermClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WeztermClient {
    /// Create a new client with default socket detection
    #[must_use]
    pub fn new() -> Self {
        Self {
            socket_path: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// Create a new client with a specific socket path
    #[must_use]
    pub fn with_socket(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: Some(socket_path.into()),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// Set the command timeout
    #[must_use]
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// List all panes across all windows and tabs
    ///
    /// Returns a vector of `PaneInfo` structs with full metadata about each pane.
    pub async fn list_panes(&self) -> Result<Vec<PaneInfo>> {
        let output = self.run_cli(&["cli", "list", "--format", "json"]).await?;
        let panes: Vec<PaneInfo> =
            serde_json::from_str(&output).map_err(|e| WeztermError::ParseError(e.to_string()))?;
        Ok(panes)
    }

    /// Get a specific pane by ID
    ///
    /// Returns the pane info if found, or `WeztermError::PaneNotFound` if not.
    pub async fn get_pane(&self, pane_id: u64) -> Result<PaneInfo> {
        let panes = self.list_panes().await?;
        panes
            .into_iter()
            .find(|p| p.pane_id == pane_id)
            .ok_or_else(|| WeztermError::PaneNotFound(pane_id).into())
    }

    /// Get text content from a pane
    ///
    /// # Arguments
    /// * `pane_id` - The pane to read from
    /// * `escapes` - Whether to include escape sequences (useful for capturing color info)
    pub async fn get_text(&self, pane_id: u64, escapes: bool) -> Result<String> {
        let pane_id_str = pane_id.to_string();
        let mut args = vec!["cli", "get-text", "--pane-id", &pane_id_str];
        if escapes {
            args.push("--escapes");
        }
        self.run_cli_with_pane_check(&args, pane_id).await
    }

    /// Send text to a pane using paste mode (default, faster for multi-char input)
    ///
    /// This uses WezTerm's paste mode which is efficient for sending multiple
    /// characters at once. For control characters, use `send_control` instead.
    pub async fn send_text(&self, pane_id: u64, text: &str) -> Result<()> {
        self.send_text_impl(pane_id, text, false).await
    }

    /// Send text to a pane character by character (no paste mode)
    ///
    /// This is slower but necessary for some applications that don't handle
    /// paste mode well, or for simulating interactive typing.
    pub async fn send_text_no_paste(&self, pane_id: u64, text: &str) -> Result<()> {
        self.send_text_impl(pane_id, text, true).await
    }

    /// Send a control character to a pane
    ///
    /// Control characters must be sent with `--no-paste` to work correctly.
    /// Use the constants in the `control` module for common control characters.
    ///
    /// # Example
    /// ```ignore
    /// use wa_core::wezterm::{WeztermClient, control};
    ///
    /// let client = WeztermClient::new();
    /// client.send_control(0, control::CTRL_C).await?; // Send interrupt
    /// ```
    pub async fn send_control(&self, pane_id: u64, control_char: &str) -> Result<()> {
        // Control characters MUST use no-paste mode
        self.send_text_impl(pane_id, control_char, true).await
    }

    /// Send Ctrl+C (interrupt) to a pane
    ///
    /// Convenience method for `send_control(pane_id, control::CTRL_C)`.
    pub async fn send_ctrl_c(&self, pane_id: u64) -> Result<()> {
        self.send_control(pane_id, control::CTRL_C).await
    }

    /// Send Ctrl+D (EOF) to a pane
    ///
    /// Convenience method for `send_control(pane_id, control::CTRL_D)`.
    pub async fn send_ctrl_d(&self, pane_id: u64) -> Result<()> {
        self.send_control(pane_id, control::CTRL_D).await
    }

    /// Internal implementation for send_text with paste mode option
    async fn send_text_impl(&self, pane_id: u64, text: &str, no_paste: bool) -> Result<()> {
        let pane_id_str = pane_id.to_string();
        let mut args = vec!["cli", "send-text", "--pane-id", &pane_id_str];
        if no_paste {
            args.push("--no-paste");
        }
        args.push("--");
        args.push(text);
        self.run_cli_with_pane_check(&args, pane_id).await?;
        Ok(())
    }

    /// Run a CLI command with pane-specific error handling
    async fn run_cli_with_pane_check(&self, args: &[&str], pane_id: u64) -> Result<String> {
        match self.run_cli(args).await {
            Ok(output) => Ok(output),
            Err(crate::Error::Wezterm(WeztermError::CommandFailed(ref stderr)))
                if stderr.contains("pane")
                    && (stderr.contains("not found")
                        || stderr.contains("does not exist")
                        || stderr.contains("no such")) =>
            {
                Err(WeztermError::PaneNotFound(pane_id).into())
            }
            Err(e) => Err(e),
        }
    }

    /// Run a WezTerm CLI command with timeout
    async fn run_cli(&self, args: &[&str]) -> Result<String> {
        use tokio::process::Command;
        use tokio::time::{Duration, timeout};

        let mut cmd = Command::new("wezterm");
        cmd.args(args);

        // Add socket path if specified
        if let Some(ref socket) = self.socket_path {
            cmd.env("WEZTERM_UNIX_SOCKET", socket);
        }

        // Execute with timeout
        let timeout_duration = Duration::from_secs(self.timeout_secs);
        let output = match timeout(timeout_duration, cmd.output()).await {
            Ok(result) => result.map_err(|e| Self::categorize_io_error(&e))?,
            Err(_) => return Err(WeztermError::Timeout(self.timeout_secs).into()),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr_str = stderr.to_string();

            // Categorize common error patterns
            if stderr_str.contains("Connection refused")
                || stderr_str.contains("No such file or directory")
                    && stderr_str.contains("socket")
            {
                return Err(WeztermError::NotRunning.into());
            }

            return Err(WeztermError::CommandFailed(stderr_str).into());
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Categorize I/O errors into specific WeztermError variants
    fn categorize_io_error(e: &std::io::Error) -> WeztermError {
        match e.kind() {
            std::io::ErrorKind::NotFound => WeztermError::CliNotFound,
            std::io::ErrorKind::PermissionDenied => {
                WeztermError::CommandFailed("Permission denied".to_string())
            }
            _ => WeztermError::CommandFailed(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_info_deserializes_minimal() {
        let json = r#"{
            "pane_id": 1,
            "tab_id": 2,
            "window_id": 3
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(pane.pane_id, 1);
        assert_eq!(pane.tab_id, 2);
        assert_eq!(pane.window_id, 3);
        assert_eq!(pane.effective_domain(), "local");
        assert_eq!(pane.effective_rows(), 24);
        assert_eq!(pane.effective_cols(), 80);
    }

    #[test]
    fn pane_info_deserializes_full() {
        let json = r#"{
            "pane_id": 1,
            "tab_id": 2,
            "window_id": 3,
            "domain_name": "local",
            "domain_id": 0,
            "workspace": "default",
            "title": "zsh",
            "cwd": "file:///home/user",
            "size": {
                "rows": 48,
                "cols": 120,
                "pixel_width": 960,
                "pixel_height": 720,
                "dpi": 96
            },
            "cursor_x": 10,
            "cursor_y": 5,
            "cursor_visibility": "Visible",
            "is_active": true,
            "is_zoomed": false,
            "tty_name": "/dev/pts/0"
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(pane.pane_id, 1);
        assert_eq!(pane.effective_domain(), "local");
        assert_eq!(pane.effective_rows(), 48);
        assert_eq!(pane.effective_cols(), 120);
        assert_eq!(pane.effective_title(), "zsh");
        assert!(pane.is_active);
        assert!(!pane.is_zoomed);

        let size = pane.size.as_ref().unwrap();
        assert_eq!(size.pixel_width, Some(960));
        assert_eq!(size.dpi, Some(96));
    }

    #[test]
    fn pane_info_tolerates_unknown_fields() {
        let json = r#"{
            "pane_id": 1,
            "tab_id": 2,
            "window_id": 3,
            "some_future_field": "value",
            "another_new_thing": 42
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(pane.pane_id, 1);
        assert_eq!(pane.extra.len(), 2);
        assert_eq!(pane.extra.get("some_future_field").unwrap(), "value");
    }

    #[test]
    fn pane_info_flat_rows_cols_fallback() {
        let json = r#"{
            "pane_id": 1,
            "tab_id": 2,
            "window_id": 3,
            "rows": 30,
            "cols": 100
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(pane.effective_rows(), 30);
        assert_eq!(pane.effective_cols(), 100);
    }

    #[test]
    fn cwd_info_parses_local() {
        let cwd = CwdInfo::parse("file:///home/user/projects");
        assert!(!cwd.is_remote);
        assert_eq!(cwd.path, "/home/user/projects");
        assert_eq!(cwd.host, "");
    }

    #[test]
    fn cwd_info_parses_remote() {
        let cwd = CwdInfo::parse("file://remote-server/home/user");
        assert!(cwd.is_remote);
        assert_eq!(cwd.path, "/home/user");
        assert_eq!(cwd.host, "remote-server");
    }

    #[test]
    fn cwd_info_parses_empty() {
        let cwd = CwdInfo::parse("");
        assert!(!cwd.is_remote);
        assert_eq!(cwd.path, "");
        assert_eq!(cwd.host, "");
    }

    #[test]
    fn cwd_info_parses_raw_path() {
        let cwd = CwdInfo::parse("/home/user");
        assert!(!cwd.is_remote);
        assert_eq!(cwd.path, "/home/user");
        assert_eq!(cwd.host, "");
    }

    #[test]
    fn pane_info_infers_domain_from_cwd() {
        let json = r#"{
            "pane_id": 1,
            "tab_id": 2,
            "window_id": 3,
            "cwd": "file://prod-server/home/deploy"
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(pane.inferred_domain(), "ssh:prod-server");
    }

    #[test]
    fn pane_info_explicit_domain_takes_priority() {
        let json = r#"{
            "pane_id": 1,
            "tab_id": 2,
            "window_id": 3,
            "domain_name": "my-ssh-domain",
            "cwd": "file://other-server/home/user"
        }"#;

        let pane: PaneInfo = serde_json::from_str(json).unwrap();
        // Explicit domain_name takes precedence over cwd inference
        assert_eq!(pane.inferred_domain(), "my-ssh-domain");
    }

    #[test]
    fn client_can_be_created() {
        let client = WeztermClient::new();
        assert_eq!(client.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn client_with_socket() {
        let client = WeztermClient::with_socket("/tmp/test.sock");
        assert_eq!(client.socket_path.as_deref(), Some("/tmp/test.sock"));
    }

    #[test]
    fn client_with_timeout() {
        let client = WeztermClient::new().with_timeout(60);
        assert_eq!(client.timeout_secs, 60);
    }

    #[test]
    fn control_characters_are_correct() {
        // Verify control character byte values
        assert_eq!(control::CTRL_C.as_bytes(), &[0x03]);
        assert_eq!(control::CTRL_D.as_bytes(), &[0x04]);
        assert_eq!(control::CTRL_Z.as_bytes(), &[0x1a]);
        assert_eq!(control::CTRL_BACKSLASH.as_bytes(), &[0x1c]);
        assert_eq!(control::ENTER.as_bytes(), &[0x0d]);
        assert_eq!(control::ESCAPE.as_bytes(), &[0x1b]);
    }

    #[test]
    fn cursor_visibility_deserializes() {
        let visible: CursorVisibility = serde_json::from_str(r#""Visible""#).unwrap();
        assert_eq!(visible, CursorVisibility::Visible);

        let hidden: CursorVisibility = serde_json::from_str(r#""Hidden""#).unwrap();
        assert_eq!(hidden, CursorVisibility::Hidden);
    }

    #[test]
    fn pane_list_deserializes() {
        let json = r#"[
            {"pane_id": 0, "tab_id": 0, "window_id": 0, "title": "shell1"},
            {"pane_id": 1, "tab_id": 0, "window_id": 0, "title": "shell2"},
            {"pane_id": 2, "tab_id": 1, "window_id": 0, "title": "editor"}
        ]"#;

        let panes: Vec<PaneInfo> = serde_json::from_str(json).unwrap();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].effective_title(), "shell1");
        assert_eq!(panes[2].tab_id, 1);
    }

    #[test]
    fn categorize_io_error_not_found() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let wez_err = WeztermClient::categorize_io_error(&e);
        assert!(matches!(wez_err, WeztermError::CliNotFound));
    }

    #[test]
    fn categorize_io_error_permission_denied() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let wez_err = WeztermClient::categorize_io_error(&e);
        assert!(matches!(wez_err, WeztermError::CommandFailed(_)));
    }
}
