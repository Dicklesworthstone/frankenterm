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
use crate::circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerStatus, get_or_register_circuit,
};
use crate::error::WeztermError;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// Boxed future for WezTerm interface operations.
pub type WeztermFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Shared handle to a WezTerm interface implementation.
pub type WeztermHandle = Arc<dyn WeztermInterface>;

/// Abstraction layer over WezTerm interactions.
///
/// This allows swapping real CLI clients with mock implementations for
/// simulation/testing without changing call sites.
pub trait WeztermInterface: Send + Sync {
    /// List all panes across all windows and tabs.
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>>;
    /// Get a specific pane by ID.
    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo>;
    /// Get text content from a pane.
    fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String>;
    /// Send text using paste mode.
    fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()>;
    /// Send text without paste mode.
    fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()>;
    /// Send text with explicit options (paste/newline).
    fn send_text_with_options(
        &self,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> WeztermFuture<'_, ()>;
    /// Send a control character (no-paste).
    fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()>;
    /// Send Ctrl+C.
    fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()>;
    /// Send Ctrl+D.
    fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()>;
    /// Spawn a new pane.
    fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64>;
    /// Split an existing pane.
    fn split_pane(
        &self,
        pane_id: u64,
        direction: SplitDirection,
        cwd: Option<&str>,
        percent: Option<u8>,
    ) -> WeztermFuture<'_, u64>;
    /// Activate a pane.
    fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()>;
    /// Get a pane in a direction relative to another.
    fn get_pane_direction(
        &self,
        pane_id: u64,
        direction: MoveDirection,
    ) -> WeztermFuture<'_, Option<u64>>;
    /// Kill (close) a pane.
    fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()>;
    /// Zoom or unzoom a pane.
    fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()>;
    /// Get current circuit breaker status.
    fn circuit_status(&self) -> CircuitBreakerStatus;
}

/// Create a default WezTerm interface handle.
#[must_use]
pub fn default_wezterm_handle() -> WeztermHandle {
    Arc::new(WeztermClient::new())
}

/// Create a WezTerm handle with a custom timeout.
#[must_use]
pub fn wezterm_handle_with_timeout(timeout_secs: u64) -> WeztermHandle {
    Arc::new(WeztermClient::new().with_timeout(timeout_secs))
}

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
/// Environment variable to override the wezterm binary path.
const WEZTERM_CLI_ENV: &str = "WA_WEZTERM_CLI";

/// Resolve the wezterm binary path, respecting `WA_WEZTERM_CLI` env var.
fn wezterm_binary() -> String {
    std::env::var(WEZTERM_CLI_ENV).unwrap_or_else(|_| "wezterm".to_string())
}

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
#[derive(Clone)]
pub struct WeztermClient {
    /// Optional socket path override (WEZTERM_UNIX_SOCKET)
    socket_path: Option<String>,
    /// Command timeout in seconds
    timeout_secs: u64,
    /// Retry attempts for safe operations
    retry_attempts: u32,
    /// Delay between retries in milliseconds
    retry_delay_ms: u64,
    /// Circuit breaker for CLI reliability
    circuit_breaker: Arc<Mutex<CircuitBreaker>>,
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
            circuit_breaker: get_or_register_circuit(
                "wezterm_cli",
                CircuitBreakerConfig::default(),
            ),
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
            circuit_breaker: get_or_register_circuit(
                "wezterm_cli",
                CircuitBreakerConfig::default(),
            ),
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

    /// Configure circuit breaker settings.
    #[must_use]
    pub fn with_circuit_breaker_config(mut self, config: CircuitBreakerConfig) -> Self {
        self.circuit_breaker =
            Arc::new(Mutex::new(CircuitBreaker::with_name("wezterm_cli", config)));
        self
    }

    /// Get current circuit breaker status.
    #[must_use]
    pub fn circuit_status(&self) -> CircuitBreakerStatus {
        let guard = match self.circuit_breaker.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.status()
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
        self.send_text_impl(pane_id, text, false, false).await
    }

    /// Send text to a pane character by character (no paste mode)
    ///
    /// This is slower but necessary for some applications that don't handle
    /// paste mode well, or for simulating interactive typing.
    pub async fn send_text_no_paste(&self, pane_id: u64, text: &str) -> Result<()> {
        self.send_text_impl(pane_id, text, true, false).await
    }

    /// Send text with explicit options (paste/newline control).
    ///
    /// Use this when the caller needs to control paste mode and newline behavior
    /// (e.g., `wa send --no-paste --no-newline`).
    pub async fn send_text_with_options(
        &self,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> Result<()> {
        self.send_text_impl(pane_id, text, no_paste, no_newline)
            .await
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
        self.send_text_impl(pane_id, control_char, true, true).await
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
        // Get the source pane info
        let source_pane = self.get_pane(pane_id).await?;
        let tab_id = source_pane.tab_id;
        let window_id = source_pane.window_id;

        // List all panes to find neighbors
        let all_panes = self.list_panes().await?;

        // Filter for panes in the same tab/window
        let tab_panes: Vec<&PaneInfo> = all_panes
            .iter()
            .filter(|p| p.tab_id == tab_id && p.window_id == window_id && p.pane_id != pane_id)
            .collect();

        if tab_panes.is_empty() {
            return Ok(None);
        }

        // Geometry-based neighbor detection
        // WezTerm coordinates: (left_col, top_row) + (cols, rows)
        // Note: left_col/top_row might be viewport-relative or absolute depending on version
        // Assuming left_col/top_row are reliable spatial coordinates.
        // Fallback: use cursor_x/y if viewport coords are missing (less reliable)

        let src_left = i64::from(source_pane.left_col.unwrap_or(0));
        let src_top = source_pane.top_row.unwrap_or(0);
        let src_width = source_pane
            .size
            .as_ref()
            .map(|s| s.cols)
            .or(source_pane.cols)
            .unwrap_or(0);
        let src_width = i64::from(src_width);
        let src_height = source_pane
            .size
            .as_ref()
            .map(|s| s.rows)
            .or(source_pane.rows)
            .unwrap_or(0);
        let src_height = i64::from(src_height);

        let src_right = src_left + src_width;
        let src_bottom = src_top + src_height;

        let mut best_candidate: Option<u64> = None;
        let mut min_distance = i64::MAX;

        for candidate in tab_panes {
            let cand_left = i64::from(candidate.left_col.unwrap_or(0));
            let cand_top = candidate.top_row.unwrap_or(0);
            let cand_width = candidate
                .size
                .as_ref()
                .map(|s| s.cols)
                .or(candidate.cols)
                .unwrap_or(0);
            let cand_width = i64::from(cand_width);
            let cand_height = candidate
                .size
                .as_ref()
                .map(|s| s.rows)
                .or(candidate.rows)
                .unwrap_or(0);
            let cand_height = i64::from(cand_height);

            let cand_right = cand_left + cand_width;
            let cand_bottom = cand_top + cand_height;

            let is_candidate = match direction {
                MoveDirection::Left => {
                    // Candidate is to the left if its right edge aligns with source left edge
                    // and they overlap vertically
                    cand_right <= src_left && (cand_top < src_bottom && cand_bottom > src_top)
                }
                MoveDirection::Right => {
                    // Candidate is to the right if its left edge aligns with source right edge
                    // and they overlap vertically
                    cand_left >= src_right && (cand_top < src_bottom && cand_bottom > src_top)
                }
                MoveDirection::Up => {
                    // Candidate is above if its bottom edge aligns with source top edge
                    // and they overlap horizontally
                    cand_bottom <= src_top && (cand_left < src_right && cand_right > src_left)
                }
                MoveDirection::Down => {
                    // Candidate is below if its top edge aligns with source bottom edge
                    // and they overlap horizontally
                    cand_top >= src_bottom && (cand_left < src_right && cand_right > src_left)
                }
            };

            if is_candidate {
                // Calculate distance to edge (should be 0 or small for adjacent)
                let distance = match direction {
                    MoveDirection::Left => (src_left - cand_right).abs(),
                    MoveDirection::Right => (cand_left - src_right).abs(),
                    MoveDirection::Up => (src_top - cand_bottom).abs(),
                    MoveDirection::Down => (cand_top - src_bottom).abs(),
                };

                if distance < min_distance {
                    min_distance = distance;
                    best_candidate = Some(candidate.pane_id);
                }
            }
        }

        Ok(best_candidate)
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
    async fn send_text_impl(
        &self,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> Result<()> {
        let pane_id_str = pane_id.to_string();
        let mut args = vec!["cli", "send-text", "--pane-id", &pane_id_str];
        if no_paste {
            args.push("--no-paste");
        }
        if no_newline {
            args.push("--no-newline");
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

        let mut cmd = Command::new(wezterm_binary());
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

    fn circuit_guard(&self) -> Result<()> {
        let mut guard = match self.circuit_breaker.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if guard.allow() {
            Ok(())
        } else {
            let status = guard.status();
            drop(guard); // Release lock before returning
            let retry_after_ms = status.cooldown_remaining_ms.unwrap_or(0);
            Err(WeztermError::CircuitOpen { retry_after_ms }.into())
        }
    }

    fn circuit_record_result(&self, outcome: &Result<String>) {
        let mut guard = match self.circuit_breaker.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        match outcome {
            Ok(_) => guard.record_success(),
            Err(err) => {
                if let crate::Error::Wezterm(wez) = err {
                    if wez.is_circuit_breaker_trigger() {
                        guard.record_failure();
                    }
                }
            }
        }
    }

    async fn run_cli_with_pane_check_retry(&self, args: &[&str], pane_id: u64) -> Result<String> {
        self.circuit_guard()?;
        let result = self
            .retry_with(|| self.run_cli_with_pane_check(args, pane_id))
            .await;
        self.circuit_record_result(&result);
        result
    }

    async fn run_cli_with_retry(&self, args: &[&str]) -> Result<String> {
        self.circuit_guard()?;
        let result = self.retry_with(|| self.run_cli(args)).await;
        self.circuit_record_result(&result);
        result
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

impl WeztermInterface for WeztermClient {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        Box::pin(async move { WeztermClient::list_panes(self).await })
    }

    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        Box::pin(async move { WeztermClient::get_pane(self, pane_id).await })
    }

    fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
        Box::pin(async move { WeztermClient::get_text(self, pane_id, escapes).await })
    }

    fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move { WeztermClient::send_text(self, pane_id, &text).await })
    }

    fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move { WeztermClient::send_text_no_paste(self, pane_id, &text).await })
    }

    fn send_text_with_options(
        &self,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move {
            WeztermClient::send_text_with_options(self, pane_id, &text, no_paste, no_newline).await
        })
    }

    fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
        let control_char = control_char.to_string();
        Box::pin(async move { WeztermClient::send_control(self, pane_id, &control_char).await })
    }

    fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move { WeztermClient::send_ctrl_c(self, pane_id).await })
    }

    fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move { WeztermClient::send_ctrl_d(self, pane_id).await })
    }

    fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
        let cwd = cwd.map(str::to_string);
        let domain = domain_name.map(str::to_string);
        Box::pin(async move { WeztermClient::spawn(self, cwd.as_deref(), domain.as_deref()).await })
    }

    fn split_pane(
        &self,
        pane_id: u64,
        direction: SplitDirection,
        cwd: Option<&str>,
        percent: Option<u8>,
    ) -> WeztermFuture<'_, u64> {
        let cwd = cwd.map(str::to_string);
        Box::pin(async move {
            WeztermClient::split_pane(self, pane_id, direction, cwd.as_deref(), percent).await
        })
    }

    fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move { WeztermClient::activate_pane(self, pane_id).await })
    }

    fn get_pane_direction(
        &self,
        pane_id: u64,
        direction: MoveDirection,
    ) -> WeztermFuture<'_, Option<u64>> {
        Box::pin(async move { WeztermClient::get_pane_direction(self, pane_id, direction).await })
    }

    fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move { WeztermClient::kill_pane(self, pane_id).await })
    }

    fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async move { WeztermClient::zoom_pane(self, pane_id, zoom).await })
    }

    fn circuit_status(&self) -> CircuitBreakerStatus {
        WeztermClient::circuit_status(self)
    }
}

impl WeztermInterface for Arc<dyn WeztermInterface> {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        self.as_ref().list_panes()
    }

    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        self.as_ref().get_pane(pane_id)
    }

    fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
        self.as_ref().get_text(pane_id, escapes)
    }

    fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        self.as_ref().send_text(pane_id, text)
    }

    fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        self.as_ref().send_text_no_paste(pane_id, text)
    }

    fn send_text_with_options(
        &self,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> WeztermFuture<'_, ()> {
        self.as_ref()
            .send_text_with_options(pane_id, text, no_paste, no_newline)
    }

    fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
        self.as_ref().send_control(pane_id, control_char)
    }

    fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.as_ref().send_ctrl_c(pane_id)
    }

    fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.as_ref().send_ctrl_d(pane_id)
    }

    fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
        self.as_ref().spawn(cwd, domain_name)
    }

    fn split_pane(
        &self,
        pane_id: u64,
        direction: SplitDirection,
        cwd: Option<&str>,
        percent: Option<u8>,
    ) -> WeztermFuture<'_, u64> {
        self.as_ref().split_pane(pane_id, direction, cwd, percent)
    }

    fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.as_ref().activate_pane(pane_id)
    }

    fn get_pane_direction(
        &self,
        pane_id: u64,
        direction: MoveDirection,
    ) -> WeztermFuture<'_, Option<u64>> {
        self.as_ref().get_pane_direction(pane_id, direction)
    }

    fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.as_ref().kill_pane(pane_id)
    }

    fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
        self.as_ref().zoom_pane(pane_id, zoom)
    }

    fn circuit_status(&self) -> CircuitBreakerStatus {
        self.as_ref().circuit_status()
    }
}

/// Pane text source backed by a WezTerm handle.
#[derive(Clone)]
pub struct WeztermHandleSource {
    handle: WeztermHandle,
}

impl WeztermHandleSource {
    #[must_use]
    pub fn new(handle: WeztermHandle) -> Self {
        Self { handle }
    }
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

impl PaneTextSource for WeztermHandleSource {
    type Fut<'a> = WeztermFuture<'a, String>;

    fn get_text(&self, pane_id: u64, escapes: bool) -> Self::Fut<'_> {
        self.handle.get_text(pane_id, escapes)
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
            poll_max: Duration::from_secs(1),
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

/// Marker presence snapshot for Codex session summary detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexSummaryMarkers {
    /// Whether "Token usage:" marker is present.
    pub token_usage: bool,
    /// Whether "codex resume" marker is present.
    pub resume_hint: bool,
}

impl CodexSummaryMarkers {
    #[must_use]
    pub fn complete(self) -> bool {
        self.token_usage && self.resume_hint
    }
}

/// Outcome of waiting for Codex session summary markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSummaryWaitResult {
    /// Whether both markers were observed.
    pub matched: bool,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: u64,
    /// Number of polls performed.
    pub polls: usize,
    /// Hash of the last tail observed (for safe debugging).
    pub last_tail_hash: Option<u64>,
    /// Marker snapshot from the last poll.
    pub last_markers: CodexSummaryMarkers,
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

/// Wait for Codex session summary markers to appear in the pane tail.
///
/// This requires both:
/// - "Token usage:" (summary header)
/// - "codex resume" (resume hint)
///
/// It returns a bounded result with only hashes and marker booleans (no raw text).
pub async fn wait_for_codex_session_summary<S: PaneTextSource + Sync + ?Sized>(
    source: &S,
    pane_id: u64,
    timeout: Duration,
    options: WaitOptions,
) -> Result<CodexSummaryWaitResult> {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut polls = 0usize;
    let mut interval = options.poll_initial;

    tracing::info!(
        pane_id,
        timeout_ms = ms_u64(timeout),
        "codex_summary_wait start"
    );

    loop {
        polls += 1;
        let text = source.get_text(pane_id, options.escapes).await?;
        let tail = tail_text(&text, options.tail_lines);
        let last_tail_hash = Some(stable_hash(tail.as_bytes()));

        let last_markers = CodexSummaryMarkers {
            token_usage: tail.contains("Token usage:"),
            resume_hint: tail.contains("codex resume"),
        };

        if last_markers.complete() {
            let elapsed_ms = elapsed_ms(start);
            tracing::info!(pane_id, elapsed_ms, polls, "codex_summary_wait matched");
            return Ok(CodexSummaryWaitResult {
                matched: true,
                elapsed_ms,
                polls,
                last_tail_hash,
                last_markers,
            });
        }

        let now = Instant::now();
        if now >= deadline || polls >= options.max_polls {
            let elapsed_ms = elapsed_ms(start);
            tracing::info!(pane_id, elapsed_ms, polls, "codex_summary_wait timeout");
            return Ok(CodexSummaryWaitResult {
                matched: false,
                elapsed_ms,
                polls,
                last_tail_hash,
                last_markers,
            });
        }

        let remaining = deadline.saturating_duration_since(now);
        let sleep_duration = if interval > remaining {
            remaining
        } else {
            interval
        };
        if !sleep_duration.is_zero() {
            sleep(sleep_duration).await;
        }
        interval = interval.saturating_mul(2);
        if interval > options.poll_max {
            interval = options.poll_max;
        }
    }
}

pub(crate) fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64; // FNV-1a offset basis
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

pub(crate) fn tail_text(text: &str, tail_lines: usize) -> String {
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

// ---------------------------------------------------------------------------
// UnifiedClient: backend-agnostic WezTerm client (wa-nu4.4.1.3)
// ---------------------------------------------------------------------------

/// Which backend the UnifiedClient selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// WezTerm CLI subprocess (`wezterm cli ...`).
    Cli,
    /// Vendored direct mux socket connection.
    Vendored,
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cli => f.write_str("cli"),
            Self::Vendored => f.write_str("vendored"),
        }
    }
}

/// Describes why a particular backend was selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendSelection {
    /// The selected backend.
    pub kind: BackendKind,
    /// Human-readable reason for the selection.
    pub reason: String,
    /// Vendored compatibility report serialized as JSON value.
    /// This avoids a hard dependency on the `vendored` feature for the type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<serde_json::Value>,
}

/// A WezTerm client that automatically selects the best available backend.
///
/// When the `vendored` feature is enabled, the binary is compiled with
/// vendored WezTerm dependencies, and the local WezTerm version is compatible,
/// the client will use the direct mux socket backend (faster for large
/// scrollback reads). Otherwise it falls back to the CLI subprocess backend.
///
/// The selection decision is captured in [`BackendSelection`] for observability
/// (`wa doctor`, `wa status`, logging).
pub struct UnifiedClient {
    inner: WeztermHandle,
    selection: BackendSelection,
}

impl UnifiedClient {
    /// Create a `UnifiedClient` with the CLI backend.
    #[must_use]
    pub fn cli() -> Self {
        Self {
            inner: Arc::new(WeztermClient::new()),
            selection: BackendSelection {
                kind: BackendKind::Cli,
                reason: "explicit CLI backend".to_string(),
                compatibility: None,
            },
        }
    }

    /// Create a `UnifiedClient` wrapping an existing handle.
    #[must_use]
    pub fn from_handle(handle: WeztermHandle, selection: BackendSelection) -> Self {
        Self {
            inner: handle,
            selection,
        }
    }

    /// Return the backend selection metadata (for `wa doctor` / logging).
    #[must_use]
    pub fn selection(&self) -> &BackendSelection {
        &self.selection
    }

    /// Return the inner handle.
    #[must_use]
    pub fn handle(&self) -> &WeztermHandle {
        &self.inner
    }
}

/// Inputs for backend selection logic, decoupled from feature-gated types.
#[derive(Debug, Clone)]
pub struct BackendSelectionInputs {
    /// Whether the `vendored` feature is enabled at compile time.
    pub vendored_feature_enabled: bool,
    /// Whether vendored backend is allowed by compatibility checks.
    pub allow_vendored: bool,
    /// Human-readable compatibility message.
    pub compat_message: String,
    /// Serialized compatibility report (for observability).
    pub compat_json: Option<serde_json::Value>,
    /// Whether a mux socket was discovered.
    pub socket_discovered: bool,
}

/// Evaluate backend selection rules and return a `BackendSelection` describing
/// the outcome. This is a pure function over the provided inputs, suitable for
/// unit testing without filesystem or network side effects.
#[must_use]
pub fn evaluate_backend_selection(inputs: &BackendSelectionInputs) -> BackendSelection {
    if !inputs.vendored_feature_enabled {
        return BackendSelection {
            kind: BackendKind::Cli,
            reason: "vendored feature not enabled at compile time".to_string(),
            compatibility: inputs.compat_json.clone(),
        };
    }

    if !inputs.allow_vendored {
        return BackendSelection {
            kind: BackendKind::Cli,
            reason: format!("vendored backend disallowed: {}", inputs.compat_message),
            compatibility: inputs.compat_json.clone(),
        };
    }

    if !inputs.socket_discovered {
        return BackendSelection {
            kind: BackendKind::Cli,
            reason: "mux socket not discovered; falling back to CLI".to_string(),
            compatibility: inputs.compat_json.clone(),
        };
    }

    BackendSelection {
        kind: BackendKind::Vendored,
        reason: format!("vendored backend selected: {}", inputs.compat_message),
        compatibility: inputs.compat_json.clone(),
    }
}

/// Build a `UnifiedClient` by probing the runtime environment.
///
/// 1. Check if the `vendored` feature is enabled (compile time).
/// 2. Run vendored compatibility checks (when feature available).
/// 3. Attempt mux socket discovery.
/// 4. If all pass, use vendored backend; else fall back to CLI.
pub fn build_unified_client(config: &crate::config::Config) -> UnifiedClient {
    let vendored_enabled = cfg!(feature = "vendored");

    // Build compatibility inputs depending on feature availability.
    let (allow_vendored, compat_message, compat_json) = if vendored_enabled {
        #[cfg(feature = "vendored")]
        {
            let local_version = crate::vendored::read_local_wezterm_version();
            let report = crate::vendored::compatibility_report(local_version.as_ref());
            let json = serde_json::to_value(&report).ok();
            (report.allow_vendored, report.message.clone(), json)
        }
        #[cfg(not(feature = "vendored"))]
        {
            (false, "vendored module unavailable".to_string(), None)
        }
    } else {
        (false, "vendored feature not enabled".to_string(), None)
    };

    // Socket discovery: check if a socket path is configured or discoverable.
    let socket_found = config
        .vendored
        .mux_socket_path
        .as_ref()
        .is_some_and(|p| !p.trim().is_empty() && std::path::Path::new(p).exists())
        || std::env::var_os("WEZTERM_UNIX_SOCKET")
            .is_some_and(|p| !p.is_empty() && std::path::Path::new(&p).exists());

    let inputs = BackendSelectionInputs {
        vendored_feature_enabled: vendored_enabled,
        allow_vendored,
        compat_message,
        compat_json,
        socket_discovered: socket_found,
    };

    let selection = evaluate_backend_selection(&inputs);

    tracing::info!(
        backend = %selection.kind,
        reason = %selection.reason,
        "UnifiedClient backend selection"
    );

    let inner: WeztermHandle = match selection.kind {
        BackendKind::Cli => Arc::new(WeztermClient::new()),
        BackendKind::Vendored => {
            // Even though selection says Vendored, we still wrap the CLI client
            // because DirectMuxClient doesn't implement WeztermInterface (it has a
            // different async API with &mut self). A future bead can bridge the gap
            // with a proper adapter; for now, vendored selection is recorded for
            // observability while we use CLI as the transport.
            //
            // TODO(wa-nu4.4.1.5): Implement VendoredAdapter that wraps
            // DirectMuxClient behind WeztermInterface.
            Arc::new(WeztermClient::new())
        }
    };

    UnifiedClient { inner, selection }
}

impl WeztermInterface for UnifiedClient {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        self.inner.list_panes()
    }

    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        self.inner.get_pane(pane_id)
    }

    fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
        self.inner.get_text(pane_id, escapes)
    }

    fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        self.inner.send_text(pane_id, text)
    }

    fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        self.inner.send_text_no_paste(pane_id, text)
    }

    fn send_text_with_options(
        &self,
        pane_id: u64,
        text: &str,
        no_paste: bool,
        no_newline: bool,
    ) -> WeztermFuture<'_, ()> {
        self.inner
            .send_text_with_options(pane_id, text, no_paste, no_newline)
    }

    fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
        self.inner.send_control(pane_id, control_char)
    }

    fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.inner.send_ctrl_c(pane_id)
    }

    fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.inner.send_ctrl_d(pane_id)
    }

    fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
        self.inner.spawn(cwd, domain_name)
    }

    fn split_pane(
        &self,
        pane_id: u64,
        direction: SplitDirection,
        cwd: Option<&str>,
        percent: Option<u8>,
    ) -> WeztermFuture<'_, u64> {
        self.inner.split_pane(pane_id, direction, cwd, percent)
    }

    fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.inner.activate_pane(pane_id)
    }

    fn get_pane_direction(
        &self,
        pane_id: u64,
        direction: MoveDirection,
    ) -> WeztermFuture<'_, Option<u64>> {
        self.inner.get_pane_direction(pane_id, direction)
    }

    fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.inner.kill_pane(pane_id)
    }

    fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
        self.inner.zoom_pane(pane_id, zoom)
    }

    fn circuit_status(&self) -> CircuitBreakerStatus {
        self.inner.circuit_status()
    }
}

// ---------------------------------------------------------------------------
// MockWezterm: in-memory pane state for testing and simulation
// ---------------------------------------------------------------------------

/// In-memory mock of WezTerm for testing, simulation, and demo scenarios.
///
/// Maintains pane state (content, titles, dimensions) and supports
/// event injection (append output, resize, clear) without a running
/// WezTerm instance.
pub struct MockWezterm {
    panes: tokio::sync::RwLock<std::collections::HashMap<u64, MockPane>>,
    next_pane_id: std::sync::atomic::AtomicU64,
}

/// State of a single mock pane.
#[derive(Debug, Clone)]
pub struct MockPane {
    pub pane_id: u64,
    pub window_id: u64,
    pub tab_id: u64,
    pub title: String,
    pub domain: String,
    pub cwd: String,
    pub is_active: bool,
    pub is_zoomed: bool,
    pub cols: u32,
    pub rows: u32,
    /// Accumulated text content (scrollback).
    pub content: String,
}

impl MockPane {
    fn to_pane_info(&self) -> PaneInfo {
        PaneInfo {
            pane_id: self.pane_id,
            window_id: self.window_id,
            tab_id: self.tab_id,
            domain_id: None,
            domain_name: Some(self.domain.clone()),
            workspace: None,
            size: None,
            rows: Some(self.rows),
            cols: Some(self.cols),
            title: Some(self.title.clone()),
            cwd: Some(self.cwd.clone()),
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: self.is_active,
            is_zoomed: self.is_zoomed,
            extra: std::collections::HashMap::new(),
        }
    }
}

/// Injection events for the mock.
#[derive(Debug, Clone)]
pub enum MockEvent {
    /// Append text to a pane's content buffer.
    AppendOutput(String),
    /// Clear a pane's content buffer.
    ClearScreen,
    /// Resize a pane.
    Resize(u32, u32),
    /// Set a pane's title.
    SetTitle(String),
}

impl MockWezterm {
    /// Create a new MockWezterm with no panes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            panes: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            next_pane_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Add a pre-configured pane.
    pub async fn add_pane(&self, pane: MockPane) {
        let mut panes = self.panes.write().await;
        let id = pane.pane_id;
        panes.insert(id, pane);
        // Ensure next_pane_id stays above any manually inserted pane
        let _ = self
            .next_pane_id
            .fetch_max(id + 1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Create a simple mock pane with defaults.
    pub async fn add_default_pane(&self, pane_id: u64) -> MockPane {
        let pane = MockPane {
            pane_id,
            window_id: 0,
            tab_id: 0,
            title: format!("pane-{pane_id}"),
            domain: "local".to_string(),
            cwd: "/home/user".to_string(),
            is_active: pane_id == 0,
            is_zoomed: false,
            cols: 80,
            rows: 24,
            content: String::new(),
        };
        self.add_pane(pane.clone()).await;
        pane
    }

    /// Inject an event into a specific pane.
    pub async fn inject(&self, pane_id: u64, event: MockEvent) -> crate::Result<()> {
        let mut panes = self.panes.write().await;
        let pane = panes.get_mut(&pane_id).ok_or_else(|| {
            crate::Error::Runtime(format!("MockWezterm: pane {pane_id} not found"))
        })?;
        match event {
            MockEvent::AppendOutput(text) => pane.content.push_str(&text),
            MockEvent::ClearScreen => pane.content.clear(),
            MockEvent::Resize(cols, rows) => {
                pane.cols = cols;
                pane.rows = rows;
            }
            MockEvent::SetTitle(title) => pane.title = title,
        }
        Ok(())
    }

    /// Inject output text into a pane (convenience wrapper).
    pub async fn inject_output(&self, pane_id: u64, text: &str) -> crate::Result<()> {
        self.inject(pane_id, MockEvent::AppendOutput(text.to_string()))
            .await
    }

    /// Get a snapshot of a pane's state.
    pub async fn pane_state(&self, pane_id: u64) -> Option<MockPane> {
        let panes = self.panes.read().await;
        panes.get(&pane_id).cloned()
    }

    /// Get the number of panes.
    pub async fn pane_count(&self) -> usize {
        self.panes.read().await.len()
    }
}

impl Default for MockWezterm {
    fn default() -> Self {
        Self::new()
    }
}

impl WeztermInterface for MockWezterm {
    fn list_panes(&self) -> WeztermFuture<'_, Vec<PaneInfo>> {
        Box::pin(async move {
            let panes = self.panes.read().await;
            Ok(panes.values().map(MockPane::to_pane_info).collect())
        })
    }

    fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, PaneInfo> {
        Box::pin(async move {
            let panes = self.panes.read().await;
            panes
                .get(&pane_id)
                .map(MockPane::to_pane_info)
                .ok_or(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)))
        })
    }

    fn get_text(&self, pane_id: u64, _escapes: bool) -> WeztermFuture<'_, String> {
        Box::pin(async move {
            let panes = self.panes.read().await;
            panes
                .get(&pane_id)
                .map(|p| p.content.clone())
                .ok_or(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)))
        })
    }

    fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        let text = text.to_string();
        Box::pin(async move {
            let mut panes = self.panes.write().await;
            let pane = panes
                .get_mut(&pane_id)
                .ok_or(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)))?;
            // Echo sent text to content (simulating terminal echo)
            pane.content.push_str(&text);
            Ok(())
        })
    }

    fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
        self.send_text(pane_id, text)
    }

    fn send_text_with_options(
        &self,
        pane_id: u64,
        text: &str,
        _no_paste: bool,
        _no_newline: bool,
    ) -> WeztermFuture<'_, ()> {
        self.send_text(pane_id, text)
    }

    fn send_control(&self, pane_id: u64, _control_char: &str) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let panes = self.panes.read().await;
            if !panes.contains_key(&pane_id) {
                return Err(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)));
            }
            Ok(())
        })
    }

    fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.send_control(pane_id, "\x03")
    }

    fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        self.send_control(pane_id, "\x04")
    }

    fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
        let cwd = cwd.unwrap_or("/home/user").to_string();
        let domain = domain_name.unwrap_or("local").to_string();
        Box::pin(async move {
            let pane_id = self
                .next_pane_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let pane = MockPane {
                pane_id,
                window_id: 0,
                tab_id: 0,
                title: format!("pane-{pane_id}"),
                domain,
                cwd,
                is_active: false,
                is_zoomed: false,
                cols: 80,
                rows: 24,
                content: String::new(),
            };
            self.panes.write().await.insert(pane_id, pane);
            Ok(pane_id)
        })
    }

    fn split_pane(
        &self,
        _pane_id: u64,
        _direction: SplitDirection,
        cwd: Option<&str>,
        _percent: Option<u8>,
    ) -> WeztermFuture<'_, u64> {
        self.spawn(cwd, None)
    }

    fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let mut panes = self.panes.write().await;
            // Deactivate all, then activate target
            for pane in panes.values_mut() {
                pane.is_active = false;
            }
            let pane = panes
                .get_mut(&pane_id)
                .ok_or(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)))?;
            pane.is_active = true;
            Ok(())
        })
    }

    fn get_pane_direction(
        &self,
        _pane_id: u64,
        _direction: MoveDirection,
    ) -> WeztermFuture<'_, Option<u64>> {
        Box::pin(async move { Ok(None) })
    }

    fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let mut panes = self.panes.write().await;
            panes.remove(&pane_id);
            Ok(())
        })
    }

    fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
        Box::pin(async move {
            let mut panes = self.panes.write().await;
            let pane = panes
                .get_mut(&pane_id)
                .ok_or(crate::Error::Wezterm(WeztermError::PaneNotFound(pane_id)))?;
            pane.is_zoomed = zoom;
            Ok(())
        })
    }

    fn circuit_status(&self) -> CircuitBreakerStatus {
        CircuitBreakerStatus::default()
    }
}

#[cfg(test)]
mod mock_tests {
    use super::*;

    #[tokio::test]
    async fn mock_add_and_list_panes() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        let panes = mock.list_panes().await.unwrap();
        assert_eq!(panes.len(), 2);
    }

    #[tokio::test]
    async fn mock_get_text_returns_content() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "hello world\n").await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "hello world\n");
    }

    #[tokio::test]
    async fn mock_send_text_echoes() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.send_text(0, "ls -la\n").await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "ls -la\n");
    }

    #[tokio::test]
    async fn mock_inject_events() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        mock.inject(0, MockEvent::AppendOutput("line 1\n".to_string()))
            .await
            .unwrap();
        mock.inject(0, MockEvent::SetTitle("New Title".to_string()))
            .await
            .unwrap();
        mock.inject(0, MockEvent::Resize(120, 40)).await.unwrap();

        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.content, "line 1\n");
        assert_eq!(state.title, "New Title");
        assert_eq!(state.cols, 120);
        assert_eq!(state.rows, 40);
    }

    #[tokio::test]
    async fn mock_spawn_creates_pane() {
        let mock = MockWezterm::new();
        let id = mock.spawn(Some("/tmp"), None).await.unwrap();
        assert_eq!(mock.pane_count().await, 1);

        let pane = mock.get_pane(id).await.unwrap();
        assert_eq!(pane.cwd.as_deref(), Some("/tmp"));
    }

    #[tokio::test]
    async fn mock_kill_pane_removes() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        assert_eq!(mock.pane_count().await, 1);

        mock.kill_pane(0).await.unwrap();
        assert_eq!(mock.pane_count().await, 0);
    }

    #[tokio::test]
    async fn mock_activate_pane() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        mock.activate_pane(1).await.unwrap();

        let p0 = mock.pane_state(0).await.unwrap();
        let p1 = mock.pane_state(1).await.unwrap();
        assert!(!p0.is_active);
        assert!(p1.is_active);
    }

    #[tokio::test]
    async fn mock_zoom_pane() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        mock.zoom_pane(0, true).await.unwrap();
        let state = mock.pane_state(0).await.unwrap();
        assert!(state.is_zoomed);

        mock.zoom_pane(0, false).await.unwrap();
        let state = mock.pane_state(0).await.unwrap();
        assert!(!state.is_zoomed);
    }

    #[tokio::test]
    async fn mock_clear_screen() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "some text").await.unwrap();
        mock.inject(0, MockEvent::ClearScreen).await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.is_empty());
    }

    #[tokio::test]
    async fn mock_pane_not_found() {
        let mock = MockWezterm::new();
        assert!(mock.get_text(99, false).await.is_err());
        assert!(mock.send_text(99, "x").await.is_err());
        assert!(mock.inject_output(99, "x").await.is_err());
    }

    #[tokio::test]
    async fn mock_split_pane_creates_new() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        let new_id = mock
            .split_pane(0, SplitDirection::Right, None, None)
            .await
            .unwrap();
        assert_eq!(mock.pane_count().await, 2);
        assert_ne!(new_id, 0);
    }

    #[tokio::test]
    async fn mock_as_wezterm_handle() {
        // Verify MockWezterm works as a WeztermHandle (Arc<dyn WeztermInterface>)
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "test").await.unwrap();

        let handle: WeztermHandle = std::sync::Arc::new(mock);
        let text = handle.get_text(0, false).await.unwrap();
        assert_eq!(text, "test");
    }

    #[tokio::test]
    async fn mock_pane_content_isolation() {
        // Content in one pane doesn't leak to another
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        mock.inject_output(0, "pane-zero-only").await.unwrap();
        mock.inject_output(1, "pane-one-only").await.unwrap();

        let t0 = mock.get_text(0, false).await.unwrap();
        let t1 = mock.get_text(1, false).await.unwrap();
        assert!(t0.contains("pane-zero-only"));
        assert!(!t0.contains("pane-one-only"));
        assert!(t1.contains("pane-one-only"));
        assert!(!t1.contains("pane-zero-only"));
    }

    #[tokio::test]
    async fn mock_pane_size_via_state() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.cols, 80);
        assert_eq!(state.rows, 24);

        // After resize
        mock.inject(0, MockEvent::Resize(200, 50)).await.unwrap();
        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.cols, 200);
        assert_eq!(state.rows, 50);
    }

    #[tokio::test]
    async fn mock_multiple_appends_accumulate() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        mock.inject_output(0, "a").await.unwrap();
        mock.inject_output(0, "b").await.unwrap();
        mock.inject_output(0, "c").await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "abc");
    }

    #[tokio::test]
    async fn mock_spawn_multiple_gets_unique_ids() {
        let mock = MockWezterm::new();
        let id1 = mock.spawn(None, None).await.unwrap();
        let id2 = mock.spawn(None, None).await.unwrap();
        let id3 = mock.spawn(None, None).await.unwrap();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_eq!(mock.pane_count().await, 3);
    }

    #[tokio::test]
    async fn mock_kill_nonexistent_pane_is_noop() {
        let mock = MockWezterm::new();
        // kill_pane on nonexistent pane succeeds silently (HashMap::remove returns None)
        assert!(mock.kill_pane(99).await.is_ok());
    }

    #[tokio::test]
    async fn mock_split_ignores_parent_creates_new() {
        let mock = MockWezterm::new();
        // split_pane delegates to spawn, ignoring parent pane ID
        let new_id = mock
            .split_pane(99, SplitDirection::Right, None, None)
            .await
            .unwrap();
        assert_eq!(mock.pane_count().await, 1);
        assert_eq!(new_id, 0);
    }
}

// ---------------------------------------------------------------------------
// UnifiedClient tests (wa-nu4.4.1.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod unified_tests {
    use super::*;

    fn inputs(
        feature_enabled: bool,
        allow: bool,
        message: &str,
        socket: bool,
    ) -> BackendSelectionInputs {
        BackendSelectionInputs {
            vendored_feature_enabled: feature_enabled,
            allow_vendored: allow,
            compat_message: message.to_string(),
            compat_json: Some(serde_json::json!({
                "status": if allow { "matched" } else { "incompatible" },
                "message": message,
            })),
            socket_discovered: socket,
        }
    }

    #[test]
    fn select_vendored_when_all_conditions_met() {
        let inp = inputs(true, true, "commit matches vendored build", true);
        let sel = evaluate_backend_selection(&inp);
        assert_eq!(sel.kind, BackendKind::Vendored);
        assert!(sel.reason.contains("vendored backend selected"));
        assert!(sel.compatibility.is_some());
    }

    #[test]
    fn select_cli_when_feature_disabled() {
        let inp = inputs(false, false, "vendored feature not enabled", true);
        let sel = evaluate_backend_selection(&inp);
        assert_eq!(sel.kind, BackendKind::Cli);
        assert!(sel.reason.contains("not enabled"));
    }

    #[test]
    fn select_cli_when_incompatible() {
        let inp = inputs(
            true,
            false,
            "local commit deadbeef does not match vendored abcdef12",
            true,
        );
        let sel = evaluate_backend_selection(&inp);
        assert_eq!(sel.kind, BackendKind::Cli);
        assert!(sel.reason.contains("disallowed"));
    }

    #[test]
    fn select_cli_when_socket_not_found() {
        let inp = inputs(true, true, "commit matches vendored build", false);
        let sel = evaluate_backend_selection(&inp);
        assert_eq!(sel.kind, BackendKind::Cli);
        assert!(sel.reason.contains("socket not discovered"));
    }

    #[test]
    fn select_vendored_compatible_with_socket() {
        let inp = inputs(
            true,
            true,
            "local version unavailable; assuming compatible",
            true,
        );
        let sel = evaluate_backend_selection(&inp);
        assert_eq!(sel.kind, BackendKind::Vendored);
    }

    #[test]
    fn backend_kind_display() {
        assert_eq!(format!("{}", BackendKind::Cli), "cli");
        assert_eq!(format!("{}", BackendKind::Vendored), "vendored");
    }

    #[test]
    fn backend_kind_serde_roundtrip() {
        let json = serde_json::to_string(&BackendKind::Vendored).unwrap();
        assert_eq!(json, r#""vendored""#);
        let back: BackendKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, BackendKind::Vendored);
    }

    #[test]
    fn backend_selection_serializes() {
        let inp = inputs(true, true, "matched", true);
        let sel = evaluate_backend_selection(&inp);
        let json = serde_json::to_value(&sel).expect("should serialize");
        assert_eq!(json["kind"], "vendored");
        assert!(
            json["reason"]
                .as_str()
                .unwrap()
                .contains("vendored backend selected")
        );
        assert!(json["compatibility"].is_object());
    }

    #[test]
    fn backend_selection_without_compat_json() {
        let inp = BackendSelectionInputs {
            vendored_feature_enabled: false,
            allow_vendored: false,
            compat_message: "no vendored".to_string(),
            compat_json: None,
            socket_discovered: false,
        };
        let sel = evaluate_backend_selection(&inp);
        assert_eq!(sel.kind, BackendKind::Cli);
        // compatibility should be omitted in JSON (skip_serializing_if)
        let json = serde_json::to_value(&sel).unwrap();
        assert!(json.get("compatibility").is_none());
    }

    #[test]
    fn unified_client_cli_delegates_to_mock() {
        let mock = MockWezterm::new();
        let handle: WeztermHandle = Arc::new(mock);
        let sel = BackendSelection {
            kind: BackendKind::Cli,
            reason: "test".to_string(),
            compatibility: None,
        };
        let unified = UnifiedClient::from_handle(handle, sel);
        assert_eq!(unified.selection().kind, BackendKind::Cli);
        assert_eq!(unified.selection().reason, "test");
    }

    #[tokio::test]
    async fn unified_client_get_text_delegates() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.inject_output(0, "hello from unified").await.unwrap();

        let handle: WeztermHandle = Arc::new(mock);
        let sel = BackendSelection {
            kind: BackendKind::Cli,
            reason: "test".to_string(),
            compatibility: None,
        };
        let unified = UnifiedClient::from_handle(handle, sel);
        let text = unified.get_text(0, false).await.unwrap();
        assert_eq!(text, "hello from unified");
    }

    #[tokio::test]
    async fn unified_client_send_text_delegates() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;

        let handle: WeztermHandle = Arc::new(mock);
        let sel = BackendSelection {
            kind: BackendKind::Cli,
            reason: "test".to_string(),
            compatibility: None,
        };
        let unified = UnifiedClient::from_handle(handle, sel);
        unified.send_text(0, "cmd\n").await.unwrap();
        let text = unified.get_text(0, false).await.unwrap();
        assert_eq!(text, "cmd\n");
    }

    #[tokio::test]
    async fn unified_client_list_panes_delegates() {
        let mock = MockWezterm::new();
        mock.add_default_pane(0).await;
        mock.add_default_pane(1).await;

        let handle: WeztermHandle = Arc::new(mock);
        let sel = BackendSelection {
            kind: BackendKind::Vendored,
            reason: "test".to_string(),
            compatibility: None,
        };
        let unified = UnifiedClient::from_handle(handle, sel);
        let panes = unified.list_panes().await.unwrap();
        assert_eq!(panes.len(), 2);
    }

    #[test]
    fn build_unified_client_returns_cli_without_vendored_feature() {
        let config = crate::config::Config::default();
        let client = build_unified_client(&config);
        if !cfg!(feature = "vendored") {
            assert_eq!(client.selection().kind, BackendKind::Cli);
        }
    }

    #[test]
    fn unified_client_cli_constructor() {
        let unified = UnifiedClient::cli();
        assert_eq!(unified.selection().kind, BackendKind::Cli);
        assert!(unified.selection().reason.contains("explicit CLI"));
    }
}
