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
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::time::{Instant, sleep};

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

/// Direction for splitting a pane
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Split to the left
    Left,
    /// Split to the right
    Right,
    /// Split above
    Top,
    /// Split below
    Bottom,
}

/// Direction for pane navigation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    /// Navigate left
    Left,
    /// Navigate right
    Right,
    /// Navigate up
    Up,
    /// Navigate down
    Down,
}

/// Default command timeout in seconds
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default retry attempts for safe operations
const DEFAULT_RETRY_ATTEMPTS: u32 = 3;
/// Default delay between retries (ms)
const DEFAULT_RETRY_DELAY_MS: u64 = 200;

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
    /// Retry attempts for safe operations
    retry_attempts: u32,
    /// Delay between retries in milliseconds
    retry_delay_ms: u64,
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
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_delay_ms: DEFAULT_RETRY_DELAY_MS,
        }
    }

    /// Create a new client with a specific socket path
    #[must_use]
    pub fn with_socket(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: Some(socket_path.into()),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_delay_ms: DEFAULT_RETRY_DELAY_MS,
        }
    }

    /// Set the command timeout
    #[must_use]
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// Set retry attempts for safe operations
    #[must_use]
    pub fn with_retries(mut self, attempts: u32) -> Self {
        self.retry_attempts = attempts.max(1);
        self
    }

    /// Set retry delay in milliseconds
    #[must_use]
    pub fn with_retry_delay_ms(mut self, delay_ms: u64) -> Self {
        self.retry_delay_ms = delay_ms;
        self
    }

    /// List all panes across all windows and tabs
    ///
    /// Returns a vector of `PaneInfo` structs with full metadata about each pane.
    pub async fn list_panes(&self) -> Result<Vec<PaneInfo>> {
        let output = self
            .run_cli_with_retry(&["cli", "list", "--format", "json"])
            .await?;
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
        self.run_cli_with_pane_check_retry(&args, pane_id).await
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

    // =========================================================================
    // Pane lifecycle commands (wa-4vx.2.3)
    // =========================================================================

    /// Spawn a new pane in the current window
    ///
    /// # Arguments
    /// * `cwd` - Optional working directory for the new pane
    /// * `domain_name` - Optional domain to spawn in (defaults to local)
    ///
    /// # Returns
    /// The pane ID of the newly spawned pane
    pub async fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> Result<u64> {
        let mut args = vec!["cli", "spawn"];

        // Add domain if specified
        let domain_arg;
        if let Some(domain) = domain_name {
            domain_arg = format!("--domain-name={domain}");
            args.push(&domain_arg);
        }

        // Add cwd if specified
        let cwd_arg;
        if let Some(dir) = cwd {
            cwd_arg = format!("--cwd={dir}");
            args.push(&cwd_arg);
        }

        let output = self.run_cli(&args).await?;
        Self::parse_pane_id(&output)
    }

    /// Split an existing pane
    ///
    /// # Arguments
    /// * `pane_id` - The pane to split from
    /// * `direction` - Direction to split: "left", "right", "top", "bottom"
    /// * `cwd` - Optional working directory for the new pane
    /// * `percent` - Optional percentage of the split (10-90)
    ///
    /// # Returns
    /// The pane ID of the newly created pane
    pub async fn split_pane(
        &self,
        pane_id: u64,
        direction: SplitDirection,
        cwd: Option<&str>,
        percent: Option<u8>,
    ) -> Result<u64> {
        let pane_id_str = pane_id.to_string();
        let mut args = vec!["cli", "split-pane", "--pane-id", &pane_id_str];

        // Add direction
        let dir_flag = match direction {
            SplitDirection::Left => "--left",
            SplitDirection::Right => "--right",
            SplitDirection::Top => "--top",
            SplitDirection::Bottom => "--bottom",
        };
        args.push(dir_flag);

        // Add cwd if specified
        let cwd_arg;
        if let Some(dir) = cwd {
            cwd_arg = format!("--cwd={dir}");
            args.push(&cwd_arg);
        }

        // Add percent if specified
        let percent_arg;
        if let Some(pct) = percent {
            let clamped = pct.clamp(10, 90);
            percent_arg = format!("--percent={clamped}");
            args.push(&percent_arg);
        }

        let output = self.run_cli_with_pane_check(&args, pane_id).await?;
        Self::parse_pane_id(&output)
    }

    /// Activate (focus) a specific pane
    ///
    /// # Arguments
    /// * `pane_id` - The pane to activate
    pub async fn activate_pane(&self, pane_id: u64) -> Result<()> {
        let pane_id_str = pane_id.to_string();
        let args = ["cli", "activate-pane", "--pane-id", &pane_id_str];
        self.run_cli_with_pane_check(&args, pane_id).await?;
        Ok(())
    }

    /// Get the pane ID in a specific direction from the current pane
    ///
    /// # Arguments
    /// * `pane_id` - The reference pane
    /// * `direction` - Direction to look: "left", "right", "up", "down"
    ///
    /// # Returns
    /// The pane ID in the specified direction, or None if no pane exists there
    pub async fn get_pane_direction(
        &self,
        pane_id: u64,
        direction: MoveDirection,
    ) -> Result<Option<u64>> {
        // WezTerm doesn't have a direct get-pane-direction command.
        // We use activate-pane-direction to check, but that has side effects.
        // Instead, we list all panes and use geometry to determine neighbors.
        // For now, we return None and note this needs enhancement.
        //
        // A better implementation would:
        // 1. List all panes in the same tab
        // 2. Use cursor_x/cursor_y and size to determine spatial relationships
        // 3. Return the adjacent pane in the specified direction

        // Placeholder: Always returns None
        // TODO: Implement geometry-based pane direction lookup
        let _ = (pane_id, direction);
        Ok(None)
    }

    /// Kill (close) a pane
    ///
    /// # Arguments
    /// * `pane_id` - The pane to kill
    pub async fn kill_pane(&self, pane_id: u64) -> Result<()> {
        let pane_id_str = pane_id.to_string();
        let args = ["cli", "kill-pane", "--pane-id", &pane_id_str];
        self.run_cli_with_pane_check(&args, pane_id).await?;
        Ok(())
    }

    /// Zoom or unzoom a pane
    ///
    /// # Arguments
    /// * `pane_id` - The pane to zoom/unzoom
    /// * `zoom` - Whether to zoom (true) or unzoom (false)
    pub async fn zoom_pane(&self, pane_id: u64, zoom: bool) -> Result<()> {
        let pane_id_str = pane_id.to_string();
        let mut args = vec!["cli", "zoom-pane", "--pane-id", &pane_id_str];
        if !zoom {
            args.push("--unzoom");
        }
        self.run_cli_with_pane_check(&args, pane_id).await?;
        Ok(())
    }

    /// Parse a pane ID from CLI output
    ///
    /// WezTerm spawn/split-pane returns just the pane ID as a number.
    fn parse_pane_id(output: &str) -> Result<u64> {
        output.trim().parse::<u64>().map_err(|_| {
            WeztermError::ParseError(format!("Invalid pane ID: {}", output.trim())).into()
        })
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

        if let Some(ref socket) = self.socket_path {
            if !std::path::Path::new(socket).exists() {
                return Err(WeztermError::SocketNotFound(socket.clone()).into());
            }
        }

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
                || stderr_str.contains("No such file or directory") && stderr_str.contains("socket")
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

    async fn run_cli_with_pane_check_retry(&self, args: &[&str], pane_id: u64) -> Result<String> {
        self.retry_with(|| self.run_cli_with_pane_check(args, pane_id))
            .await
    }

    async fn run_cli_with_retry(&self, args: &[&str]) -> Result<String> {
        self.retry_with(|| self.run_cli(args)).await
    }

    async fn retry_with<F, Fut>(&self, mut runner: F) -> Result<String>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match runner().await {
                Ok(output) => return Ok(output),
                Err(err) => {
                    if attempt >= self.retry_attempts || !is_retryable_error(&err) {
                        return Err(err);
                    }
                    if self.retry_delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(self.retry_delay_ms)).await;
                    }
                }
            }
        }
    }
}

fn is_retryable_error(err: &crate::Error) -> bool {
    matches!(
        err,
        crate::Error::Wezterm(
            WeztermError::NotRunning | WeztermError::Timeout(_) | WeztermError::CommandFailed(_)
        )
    )
}

// =============================================================================
// PaneWaiter: shared wait-for logic (substring/regex) with timeout/backoff
// =============================================================================

/// Source of pane text for wait operations.
///
/// This abstraction allows PaneWaiter to be tested without invoking WezTerm.
pub trait PaneTextSource {
    /// Future returned by get_text.
    type Fut<'a>: Future<Output = Result<String>> + Send + 'a
    where
        Self: 'a;

    /// Fetch the pane text. Implementations may ignore tail_lines and return full text.
    fn get_text(&self, pane_id: u64, escapes: bool) -> Self::Fut<'_>;
}

impl PaneTextSource for WeztermClient {
    type Fut<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

    fn get_text(&self, pane_id: u64, escapes: bool) -> Self::Fut<'_> {
        Box::pin(async move { self.get_text(pane_id, escapes).await })
    }
}

/// Wait matcher kinds for pane text.
#[derive(Debug, Clone)]
pub enum WaitMatcher {
    /// Simple substring match (fast path).
    Substring(String),
    /// Regex match (explicit; use for structured patterns).
    Regex(fancy_regex::Regex),
}

impl WaitMatcher {
    /// Create a substring matcher.
    #[must_use]
    pub fn substring(value: impl Into<String>) -> Self {
        Self::Substring(value.into())
    }

    /// Create a regex matcher from a compiled regex.
    #[must_use]
    pub fn regex(regex: fancy_regex::Regex) -> Self {
        Self::Regex(regex)
    }

    fn matches(&self, haystack: &str) -> Result<bool> {
        match self {
            Self::Substring(needle) => Ok(haystack.contains(needle)),
            Self::Regex(regex) => regex
                .is_match(haystack)
                .map_err(|e| crate::error::PatternError::InvalidRegex(e.to_string()).into()),
        }
    }

    fn description(&self) -> String {
        match self {
            Self::Substring(needle) => format!(
                "substring(len={}, hash={:016x})",
                needle.len(),
                stable_hash(needle.as_bytes())
            ),
            Self::Regex(regex) => {
                let pattern = regex.as_str();
                format!(
                    "regex(len={}, hash={:016x})",
                    pattern.len(),
                    stable_hash(pattern.as_bytes())
                )
            }
        }
    }
}

/// Options for wait-for polling behavior.
#[derive(Debug, Clone)]
pub struct WaitOptions {
    /// Number of tail lines to consider for matching (0 = empty).
    pub tail_lines: usize,
    /// Whether to include escape sequences.
    pub escapes: bool,
    /// Initial polling interval.
    pub poll_initial: Duration,
    /// Maximum polling interval.
    pub poll_max: Duration,
    /// Maximum number of polls before forcing timeout.
    pub max_polls: usize,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            tail_lines: 200,
            escapes: false,
            poll_initial: Duration::from_millis(50),
            poll_max: Duration::from_millis(1000),
            max_polls: 10_000,
        }
    }
}

/// Outcome of a wait-for operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitResult {
    /// Matcher satisfied within timeout.
    Matched { elapsed_ms: u64, polls: usize },
    /// Timeout elapsed (or max_polls reached) without a match.
    TimedOut {
        elapsed_ms: u64,
        polls: usize,
        last_tail_hash: Option<u64>,
    },
}

/// Shared waiter for polling pane text until a matcher succeeds.
pub struct PaneWaiter<'a, S: PaneTextSource + Sync + ?Sized> {
    source: &'a S,
    options: WaitOptions,
}

impl<'a, S: PaneTextSource + Sync + ?Sized> PaneWaiter<'a, S> {
    /// Create a new PaneWaiter with default options.
    #[must_use]
    pub fn new(source: &'a S) -> Self {
        Self {
            source,
            options: WaitOptions::default(),
        }
    }

    /// Override default wait options.
    #[must_use]
    pub fn with_options(mut self, options: WaitOptions) -> Self {
        self.options = options;
        self
    }

    /// Wait for a matcher to appear in the pane within the given timeout.
    pub async fn wait_for(
        &self,
        pane_id: u64,
        matcher: &WaitMatcher,
        timeout: Duration,
    ) -> Result<WaitResult> {
        let matcher_desc = matcher.description();
        let start = Instant::now();
        let deadline = start + timeout;
        let mut polls = 0usize;
        let mut interval = self.options.poll_initial;
        tracing::info!(
            pane_id,
            timeout_ms = ms_u64(timeout),
            matcher = %matcher_desc,
            "wait_for start"
        );

        loop {
            polls += 1;
            let text = self.source.get_text(pane_id, self.options.escapes).await?;
            let tail = tail_text(&text, self.options.tail_lines);
            let tail_hash = stable_hash(tail.as_bytes());

            if matcher.matches(&tail)? {
                let elapsed_ms = elapsed_ms(start);
                tracing::info!(
                    pane_id,
                    elapsed_ms,
                    polls,
                    matcher = %matcher_desc,
                    "wait_for matched"
                );
                return Ok(WaitResult::Matched { elapsed_ms, polls });
            }

            let now = Instant::now();
            if now >= deadline || polls >= self.options.max_polls {
                let elapsed_ms = elapsed_ms(start);
                tracing::info!(
                    pane_id,
                    elapsed_ms,
                    polls,
                    matcher = %matcher_desc,
                    "wait_for timeout"
                );
                return Ok(WaitResult::TimedOut {
                    elapsed_ms,
                    polls,
                    last_tail_hash: Some(tail_hash),
                });
            }

            let remaining = deadline.saturating_duration_since(now);
            let sleep_duration = if interval > remaining {
                remaining
            } else {
                interval
            };

            sleep(sleep_duration).await;
            interval = interval.saturating_mul(2);
            if interval > self.options.poll_max {
                interval = self.options.poll_max;
            }
        }
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64; // FNV-1a offset basis
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn tail_text(text: &str, tail_lines: usize) -> String {
    if tail_lines == 0 {
        return String::new();
    }

    let bytes = text.as_bytes();
    let mut iter = memchr::memrchr_iter(b'\n', bytes);
    let mut cutoff = None;

    // If text ends with \n, that trailing newline is part of the last line,
    // not a separator. We need to skip one extra newline to get the right count.
    let count = if bytes.last() == Some(&b'\n') {
        tail_lines + 1
    } else {
        tail_lines
    };

    for _ in 0..count {
        if let Some(pos) = iter.next() {
            cutoff = Some(pos);
        } else {
            // Not enough lines, return everything
            return text.to_string();
        }
    }

    // cutoff points to the newline BEFORE our desired output
    match cutoff {
        Some(pos) if pos + 1 < bytes.len() => text[pos + 1..].to_string(),
        _ => text.to_string(),
    }
}

fn ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        assert_eq!(client.retry_attempts, DEFAULT_RETRY_ATTEMPTS);
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
    fn client_with_retries() {
        let client = WeztermClient::new().with_retries(5).with_retry_delay_ms(10);
        assert_eq!(client.retry_attempts, 5);
        assert_eq!(client.retry_delay_ms, 10);
    }

    #[tokio::test]
    async fn retry_with_retries_transient_errors() {
        let client = WeztermClient::new().with_retries(3).with_retry_delay_ms(0);
        let attempts = Cell::new(0);

        let result = client
            .retry_with(|| {
                attempts.set(attempts.get() + 1);
                async {
                    if attempts.get() < 2 {
                        Err(WeztermError::NotRunning.into())
                    } else {
                        Ok("ok".to_string())
                    }
                }
            })
            .await;

        assert_eq!(attempts.get(), 2);
        assert_eq!(result.unwrap(), "ok");
    }

    #[tokio::test]
    async fn retry_with_stops_on_non_retryable_error() {
        let client = WeztermClient::new().with_retries(3).with_retry_delay_ms(0);
        let attempts = Cell::new(0);

        let result = client
            .retry_with(|| {
                attempts.set(attempts.get() + 1);
                async { Err(WeztermError::PaneNotFound(42).into()) }
            })
            .await;

        assert_eq!(attempts.get(), 1);
        assert!(matches!(
            result,
            Err(crate::Error::Wezterm(WeztermError::PaneNotFound(42)))
        ));
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

    #[derive(Clone)]
    struct TestTextSource {
        sequence: Arc<Vec<String>>,
        index: Arc<AtomicUsize>,
    }

    impl TestTextSource {
        fn new(sequence: Vec<&str>) -> Self {
            Self {
                sequence: Arc::new(sequence.into_iter().map(str::to_string).collect()),
                index: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl PaneTextSource for TestTextSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let idx = self.index.fetch_add(1, Ordering::SeqCst);
            let text = self
                .sequence
                .get(idx)
                .cloned()
                .or_else(|| self.sequence.last().cloned())
                .unwrap_or_default();
            Box::pin(async move { Ok(text) })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waiter_matches_substring() {
        let source = TestTextSource::new(vec!["booting...", "ready: prompt"]);
        let waiter = PaneWaiter::new(&source).with_options(WaitOptions {
            tail_lines: 50,
            escapes: false,
            poll_initial: Duration::from_secs(1),
            poll_max: Duration::from_secs(1),
            max_polls: 10,
        });

        let matcher = WaitMatcher::substring("ready");
        let mut fut = Box::pin(waiter.wait_for(1, &matcher, Duration::from_secs(5)));

        for _ in 0..3 {
            tokio::select! {
                result = &mut fut => {
                    let result = result.expect("wait_for");
                    match result {
                        WaitResult::Matched { polls, .. } => {
                            assert!(polls >= 2, "expected at least two polls");
                        }
                        WaitResult::TimedOut { .. } => panic!("unexpected timeout"),
                    }
                    return;
                }
                () = tokio::time::advance(Duration::from_secs(1)) => {}
            }
            tokio::task::yield_now().await;
        }

        let result = fut.await.expect("wait_for");
        match result {
            WaitResult::Matched { polls, .. } => {
                assert!(polls >= 2, "expected at least two polls");
            }
            WaitResult::TimedOut { .. } => panic!("unexpected timeout"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waiter_times_out() {
        let source = TestTextSource::new(vec!["still waiting"]);
        let waiter = PaneWaiter::new(&source).with_options(WaitOptions {
            tail_lines: 10,
            escapes: false,
            poll_initial: Duration::from_secs(1),
            poll_max: Duration::from_secs(1),
            max_polls: 100,
        });

        let matcher = WaitMatcher::substring("never");
        let mut fut = Box::pin(waiter.wait_for(1, &matcher, Duration::from_secs(2)));

        for _ in 0..4 {
            tokio::select! {
                result = &mut fut => {
                    let result = result.expect("wait_for");
                    match result {
                        WaitResult::TimedOut {
                            polls,
                            last_tail_hash,
                            ..
                        } => {
                            assert!(polls >= 1);
                            assert!(last_tail_hash.is_some());
                        }
                        WaitResult::Matched { .. } => panic!("unexpected match"),
                    }
                    return;
                }
                () = tokio::time::advance(Duration::from_secs(1)) => {}
            }
            tokio::task::yield_now().await;
        }

        let result = fut.await.expect("wait_for");
        match result {
            WaitResult::TimedOut {
                polls,
                last_tail_hash,
                ..
            } => {
                assert!(polls >= 1);
                assert!(last_tail_hash.is_some());
            }
            WaitResult::Matched { .. } => panic!("unexpected match"),
        }
    }

    #[test]
    fn tail_text_limits_lines() {
        let text = "one\ntwo\nthree\nfour\n";
        let tail = tail_text(text, 2);
        assert_eq!(tail, "three\nfour\n");
    }
}
