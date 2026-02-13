//! Error types for frankenterm-core

use std::fmt::Write;
use thiserror::Error;

/// Remediation command for resolving an error
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemediationCommand {
    /// Short label describing the command purpose
    pub label: String,
    /// Command to run
    pub command: String,
    /// Optional platform hint (e.g., "macOS", "Linux")
    pub platform: Option<String>,
}

/// Actionable remediation guidance for an error
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Remediation {
    /// One-line summary of how to fix the issue
    pub summary: String,
    /// Suggested commands to resolve or diagnose the issue
    pub commands: Vec<RemediationCommand>,
    /// Additional alternative guidance
    pub alternatives: Vec<String>,
    /// Optional reference for more details
    pub learn_more: Option<String>,
}

impl Remediation {
    /// Create a new remediation with a summary
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            commands: Vec::new(),
            alternatives: Vec::new(),
            learn_more: None,
        }
    }

    /// Add a command without a platform hint
    #[must_use]
    pub fn command(mut self, label: impl Into<String>, command: impl Into<String>) -> Self {
        self.commands.push(RemediationCommand {
            label: label.into(),
            command: command.into(),
            platform: None,
        });
        self
    }

    /// Add a command with a platform hint
    #[must_use]
    pub fn platform_command(
        mut self,
        label: impl Into<String>,
        command: impl Into<String>,
        platform: impl Into<String>,
    ) -> Self {
        self.commands.push(RemediationCommand {
            label: label.into(),
            command: command.into(),
            platform: Some(platform.into()),
        });
        self
    }

    /// Add an alternative suggestion
    #[must_use]
    pub fn alternative(mut self, alternative: impl Into<String>) -> Self {
        self.alternatives.push(alternative.into());
        self
    }

    /// Add a learn-more reference
    #[must_use]
    pub fn learn_more(mut self, link: impl Into<String>) -> Self {
        self.learn_more = Some(link.into());
        self
    }

    /// Render remediation text for human-readable output
    #[must_use]
    pub fn render_plain(&self) -> String {
        let mut output = String::new();
        let _ = writeln!(output, "To fix:");
        let _ = writeln!(output, "  {}", self.summary);

        if !self.commands.is_empty() {
            let _ = writeln!(output, "  Commands:");
            for cmd in &self.commands {
                let label = cmd.platform.as_ref().map_or_else(
                    || cmd.label.clone(),
                    |platform| format!("{} ({platform})", cmd.label),
                );
                let _ = writeln!(output, "    - {label}: {}", cmd.command);
            }
        }

        if !self.alternatives.is_empty() {
            let _ = writeln!(output, "  Alternatives:");
            for alt in &self.alternatives {
                let _ = writeln!(output, "    - {alt}");
            }
        }

        if let Some(learn_more) = &self.learn_more {
            let _ = writeln!(output, "  Learn more: {learn_more}");
        }

        output
    }
}

/// Result type alias using the library's Error type
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for frankenterm-core
#[derive(Error, Debug)]
pub enum Error {
    /// WezTerm CLI errors
    #[error("WezTerm error: {0}")]
    Wezterm(#[from] WeztermError),

    /// Storage errors
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    /// Pattern matching errors
    #[error("Pattern error: {0}")]
    Pattern(#[from] PatternError),

    /// Workflow errors
    #[error("Workflow error: {0}")]
    Workflow(#[from] WorkflowError),

    /// Configuration errors
    #[error("Config error: {0}")]
    Config(#[from] ConfigError),

    /// Policy violation errors
    #[error("Policy violation: {0}")]
    Policy(String),

    /// I/O errors
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization errors
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Runtime errors (hot reload, channel failures, etc.)
    #[error("Runtime error: {0}")]
    Runtime(String),

    /// Setup/configuration automation errors
    #[error("Setup error: {0}")]
    SetupError(String),

    /// Operation was cancelled (structured cancellation from asupersync)
    #[error("Operation cancelled: {0}")]
    Cancelled(String),

    /// Task panicked (panic propagation from asupersync)
    #[error("Task panicked: {0}")]
    Panicked(String),
}

impl Error {
    /// Return remediation guidance when available.
    #[must_use]
    pub fn remediation(&self) -> Option<Remediation> {
        match self {
            Self::Wezterm(err) => Some(err.remediation()),
            Self::Storage(err) => Some(err.remediation()),
            Self::Pattern(err) => Some(err.remediation()),
            Self::Workflow(err) => Some(err.remediation()),
            Self::Config(err) => Some(err.remediation()),
            Self::Policy(_) => Some(
                Remediation::new("Review policy configuration or request approval if needed.")
                    .command("Status", "ft status")
                    .command("Diagnostics", "ft doctor")
                    .alternative("Check ft.toml policy settings for explicit allow rules."),
            ),
            Self::Io(_) => Some(
                Remediation::new("Check filesystem permissions and paths, then retry.")
                    .command("Diagnostics", "ft doctor")
                    .alternative("Verify the workspace directory exists and is writable."),
            ),
            Self::Json(_) => Some(
                Remediation::new("Validate the JSON input and retry.")
                    .command("Validate JSON", "python -m json.tool < input.json")
                    .alternative("Check for trailing commas or invalid UTF-8."),
            ),
            Self::Runtime(_) => Some(
                Remediation::new("Restart the watcher or retry the command.")
                    .command("Diagnostics", "ft doctor")
                    .alternative("If the issue persists, restart ft watch."),
            ),
            Self::SetupError(_) => Some(
                Remediation::new(
                    "Check terminal-backend bridge configuration (current: WezTerm) and filesystem permissions.",
                )
                    .command("Locate config", "ls -la ~/.config/wezterm/ ~/.wezterm.lua 2>/dev/null || echo 'No config found'")
                    .command("Diagnostics", "ft doctor")
                    .alternative("Create a wezterm.lua config file if it doesn't exist."),
            ),
            Self::Cancelled(_) => Some(
                Remediation::new("Operation was cancelled. Retry if the cancellation was unexpected.")
                    .command("Status", "ft status")
                    .alternative("Check whether a timeout or shutdown triggered the cancellation."),
            ),
            Self::Panicked(_) => Some(
                Remediation::new("A task panicked. Check logs for the panic backtrace and retry.")
                    .command("Diagnostics", "ft doctor")
                    .alternative("If the panic persists, report the issue with the backtrace."),
            ),
        }
    }
}

/// WezTerm-specific errors
#[derive(Error, Debug)]
pub enum WeztermError {
    /// WezTerm CLI binary not found in PATH
    #[error("WezTerm bridge CLI not found in PATH. Install/configure WezTerm or add it to PATH.")]
    CliNotFound,

    /// WezTerm bridge is not running (no GUI or socket available)
    #[error("WezTerm bridge is not running. Start the active backend first.")]
    NotRunning,

    /// Specified pane does not exist
    #[error("Pane not found: {0}")]
    PaneNotFound(u64),

    /// Socket path doesn't exist or is inaccessible
    #[error("Socket not found or inaccessible: {0}")]
    SocketNotFound(String),

    /// Command execution failed with stderr output
    #[error("Command failed: {0}")]
    CommandFailed(String),

    /// JSON parsing failed
    #[error("Failed to parse WezTerm output: {0}")]
    ParseError(String),

    /// Timeout waiting for command
    #[error("Command timed out after {0} seconds")]
    Timeout(u64),

    /// Circuit breaker open (temporary backoff)
    #[error("WezTerm circuit breaker is open; retry in {retry_after_ms} ms")]
    CircuitOpen { retry_after_ms: u64 },
}

impl WeztermError {
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::CliNotFound => {
                Remediation::new(
                    "Install/configure the active backend bridge (current: WezTerm) and ensure `wezterm` is on PATH.",
                )
                    .platform_command("Install", "brew install wezterm", "macOS")
                    .platform_command(
                        "Install",
                        "sudo apt install wezterm",
                        "Linux (Debian/Ubuntu)",
                    )
                    .platform_command("Install", "sudo pacman -S wezterm", "Linux (Arch)")
                    .command("Verify install", "wezterm --version")
                    .alternative("If WezTerm is installed elsewhere, add it to PATH.")
                    .learn_more("https://wezfurlong.org/wezterm/install.html")
            }
            Self::NotRunning => Remediation::new(
                "Start the active backend bridge (current: WezTerm) and retry the command.",
            )
            .command("Start backend bridge (WezTerm)", "wezterm start")
                .command("Check panes", "wezterm cli list --format json")
                .alternative("If using a remote socket, set WEZTERM_UNIX_SOCKET."),
            Self::PaneNotFound(_) => Remediation::new("List panes and use a valid pane id.")
                .command("List panes", "ft list")
                .command("List panes (raw)", "wezterm cli list --format json"),
            Self::SocketNotFound(path) => {
                Remediation::new(format!("Verify the WezTerm socket path exists: {path}"))
                    .command("Show socket env", "echo $WEZTERM_UNIX_SOCKET")
                    .command("List socket", format!("ls \"{path}\""))
                    .alternative("Unset WEZTERM_UNIX_SOCKET to use the default socket.")
            }
            Self::CommandFailed(_) => {
                Remediation::new(
                    "Backend bridge CLI command failed. Check WezTerm/backend logs and retry.",
                )
                    .command("Check panes", "wezterm cli list --format json")
                    .command("Diagnostics", "ft doctor")
                    .alternative(
                        "Ensure the active backend bridge (current: WezTerm) is running and responsive.",
                    )
            }
            Self::ParseError(_) => {
                Remediation::new("WezTerm returned unexpected output; verify the version.")
                    .command("Check version", "wezterm --version")
                    .alternative("Upgrade WezTerm if the output format changed.")
            }
            Self::Timeout(timeout) => Remediation::new(format!(
                "Backend bridge CLI timed out after {timeout} seconds. Try again when the system is idle."
            ))
            .command("Diagnostics", "ft doctor")
            .alternative("Reduce load or retry with a longer timeout."),
            Self::CircuitOpen { retry_after_ms } => Remediation::new(format!(
                "Backend bridge circuit breaker is open. Retry after {retry_after_ms} ms."
            ))
            .command("Check status", "ft status")
            .command("Diagnostics", "ft doctor")
            .alternative(
                "Ensure the active backend bridge (current: WezTerm) is running and responsive.",
            ),
        }
    }

    /// Returns true if this error should contribute to circuit breaker failure counts.
    #[must_use]
    pub fn is_circuit_breaker_trigger(&self) -> bool {
        matches!(
            self,
            Self::CliNotFound
                | Self::NotRunning
                | Self::SocketNotFound(_)
                | Self::Timeout(_)
                | Self::CommandFailed(_)
        )
    }
}

/// Storage-specific errors
#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Database error: {0}")]
    Database(String),

    #[error("Sequence discontinuity: expected {expected}, got {actual}")]
    SequenceDiscontinuity { expected: u64, actual: u64 },

    #[error("Migration failed: {0}")]
    MigrationFailed(String),

    #[error("Database schema version ({current}) is newer than supported ({supported})")]
    SchemaTooNew { current: i32, supported: i32 },

    #[error("Database requires wa >= {min_compatible} (current {current})")]
    WaTooOld {
        current: String,
        min_compatible: String,
    },

    #[error("FTS query error: {0}")]
    FtsQueryError(String),

    #[error("Database corruption detected: {details}")]
    Corruption { details: String },

    #[error("Not found: {0}")]
    NotFound(String),
}

impl StorageError {
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::Database(_) => Remediation::new(
                "Database operation failed. Check workspace permissions and retry.",
            )
            .command("Diagnostics", "ft doctor")
            .alternative("Ensure the workspace directory is writable."),
            Self::SequenceDiscontinuity { expected, actual } => Remediation::new(format!(
                "Output sequence gap detected (expected {expected}, got {actual})."
            ))
            .command("Status", "ft status")
            .alternative("Restart the watcher: ft watch --foreground"),
            Self::MigrationFailed(_) => {
                Remediation::new("Database migration failed. Check logs and retry after backup.")
                    .command("Diagnostics", "ft doctor")
                    .alternative("Backup the database file before retrying.")
            }
            Self::SchemaTooNew { current, supported } => Remediation::new(format!(
                "Database schema version {current} is newer than supported ({supported}). Upgrade ft."
            ))
            .command(
                "Upgrade ft",
                "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft",
            )
            .alternative("If you must stay on this version, restore an older database backup."),
            Self::WaTooOld { min_compatible, .. } => Remediation::new(format!(
                "This database requires ft {min_compatible} or newer."
            ))
            .command(
                "Upgrade ft",
                "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft",
            )
            .alternative("Restore a database created by an older ft version."),
            Self::FtsQueryError(_) => Remediation::new("Invalid FTS query syntax.")
                .command("Example search", "ft search \"term\"")
                .alternative("Review FTS5 query syntax and try again."),
            Self::Corruption { .. } => Remediation::new(
                "Database corruption detected. Automatic recovery is not possible.",
            )
            .command("Run diagnostics", "ft doctor")
            .alternative("Delete the database file and restart with fresh data."),
            Self::NotFound(_) => Remediation::new("The requested resource was not found.")
                .command("List resources", "ft status")
                .alternative("Verify the resource exists before accessing it."),
        }
    }
}

/// Pattern-specific errors
#[derive(Error, Debug)]
pub enum PatternError {
    #[error("Invalid rule: {0}")]
    InvalidRule(String),

    #[error("Invalid regex: {0}")]
    InvalidRegex(String),

    #[error("Pack not found: {0}")]
    PackNotFound(String),

    #[error("Match timeout")]
    MatchTimeout,
}

impl PatternError {
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::InvalidRule(_) => {
                Remediation::new("Pattern rule invalid. Fix the rule or disable pattern detection.")
                    .command("Disable patterns", "ft watch --no-patterns")
                    .alternative("Fix the rule definition in ft.toml.")
            }
            Self::InvalidRegex(_) => Remediation::new(
                "Regex pattern invalid. Fix the regex or disable pattern detection.",
            )
            .command("Disable patterns", "ft watch --no-patterns")
            .alternative("Validate the regex syntax."),
            Self::PackNotFound(_) => Remediation::new(
                "Pattern pack not found. Enable the pack or disable pattern detection.",
            )
            .command("Disable patterns", "ft watch --no-patterns")
            .alternative("Enable the pack in ft.toml."),
            Self::MatchTimeout => Remediation::new(
                "Pattern evaluation timed out. Simplify patterns or reduce input size.",
            )
            .command("Disable patterns", "ft watch --no-patterns")
            .alternative("Tighten regex or reduce scan scope."),
        }
    }
}

/// Workflow-specific errors
#[derive(Error, Debug)]
pub enum WorkflowError {
    #[error("Workflow not found: {0}")]
    NotFound(String),

    #[error("Workflow aborted: {0}")]
    Aborted(String),

    #[error("Guard failed: {0}")]
    GuardFailed(String),

    #[error("Pane locked by another workflow")]
    PaneLocked,
}

impl WorkflowError {
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::NotFound(_) => Remediation::new("Workflow not found. Use a valid workflow name.")
                .command("List workflows", "ft workflow list")
                .alternative("Use 'ft watch --auto-handle' for event-driven workflows."),
            Self::Aborted(_) => {
                Remediation::new("Workflow aborted. Check logs and retry when the pane is stable.")
                    .command("Status", "ft status")
                    .alternative("Retry the workflow once the pane is ready.")
            }
            Self::GuardFailed(_) => {
                Remediation::new("Workflow guard failed. Resolve the guard condition and retry.")
                    .command("Status", "ft status")
                    .alternative("Verify the pane state and policy settings.")
            }
            Self::PaneLocked => {
                Remediation::new("Pane is locked by another workflow. Wait for it to complete.")
                    .command("Status", "ft status")
                    .alternative("Avoid running multiple workflows on the same pane.")
            }
        }
    }
}

/// Configuration-specific errors
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Config file not found: {0}")]
    FileNotFound(String),

    #[error("Failed to read config file {0}: {1}")]
    ReadFailed(String, String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Failed to parse config: {0}")]
    ParseFailed(String),

    #[error("Failed to serialize config: {0}")]
    SerializeFailed(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}

impl ConfigError {
    #[must_use]
    pub fn remediation(&self) -> Remediation {
        match self {
            Self::FileNotFound(path) => Remediation::new(format!(
                "Config file not found: {path}. Verify the path and retry."
            ))
            .command("Check path", format!("ls -l \"{path}\""))
            .alternative("Pass --config with the correct path."),
            Self::ReadFailed(path, _) => Remediation::new(format!(
                "Failed to read config file: {path}. Check permissions."
            ))
            .command("Check permissions", format!("ls -l \"{path}\""))
            .alternative("Ensure the file is readable by the current user."),
            Self::ParseError(_) | Self::ParseFailed(_) => {
                Remediation::new("Config parse failed. Fix the syntax and retry.")
                    .command("Diagnostics", "ft doctor")
                    .alternative("Validate the config file format.")
            }
            Self::SerializeFailed(_) => {
                Remediation::new("Failed to serialize configuration. Check config values.")
                    .command("Diagnostics", "ft doctor")
                    .alternative("Recreate the config from known-good defaults.")
            }
            Self::ValidationError(_) => {
                Remediation::new("Config validation failed. Fix the invalid fields and retry.")
                    .command("Diagnostics", "ft doctor")
                    .alternative("Review validation errors and adjust ft.toml.")
            }
        }
    }
}

/// Format an error with remediation guidance for display.
#[must_use]
pub fn format_error_with_remediation(error: &Error) -> String {
    let mut output = format!("Error: {error}");
    if let Some(remediation) = error.remediation() {
        output.push('\n');
        output.push('\n');
        output.push_str(&remediation.render_plain());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remediation_available_for_error_variants() {
        let json_err = serde_json::from_str::<serde_json::Value>("").unwrap_err();
        let errors = vec![
            Error::Wezterm(WeztermError::CliNotFound),
            Error::Wezterm(WeztermError::NotRunning),
            Error::Wezterm(WeztermError::PaneNotFound(1)),
            Error::Wezterm(WeztermError::SocketNotFound("/tmp/wez.sock".to_string())),
            Error::Wezterm(WeztermError::CommandFailed("boom".to_string())),
            Error::Wezterm(WeztermError::ParseError("bad json".to_string())),
            Error::Wezterm(WeztermError::Timeout(5)),
            Error::Wezterm(WeztermError::CircuitOpen {
                retry_after_ms: 500,
            }),
            Error::Storage(StorageError::Database("db error".to_string())),
            Error::Storage(StorageError::SequenceDiscontinuity {
                expected: 1,
                actual: 2,
            }),
            Error::Storage(StorageError::MigrationFailed("migrate".to_string())),
            Error::Storage(StorageError::SchemaTooNew {
                current: 9,
                supported: 6,
            }),
            Error::Storage(StorageError::WaTooOld {
                current: "0.1.0".to_string(),
                min_compatible: "1.0.0".to_string(),
            }),
            Error::Storage(StorageError::FtsQueryError("fts".to_string())),
            Error::Pattern(PatternError::InvalidRule("rule".to_string())),
            Error::Pattern(PatternError::InvalidRegex("regex".to_string())),
            Error::Pattern(PatternError::PackNotFound("pack".to_string())),
            Error::Pattern(PatternError::MatchTimeout),
            Error::Workflow(WorkflowError::NotFound("name".to_string())),
            Error::Workflow(WorkflowError::Aborted("abort".to_string())),
            Error::Workflow(WorkflowError::GuardFailed("guard".to_string())),
            Error::Workflow(WorkflowError::PaneLocked),
            Error::Config(ConfigError::FileNotFound("ft.toml".to_string())),
            Error::Config(ConfigError::ReadFailed(
                "ft.toml".to_string(),
                "io".to_string(),
            )),
            Error::Config(ConfigError::ParseError("parse".to_string())),
            Error::Config(ConfigError::ParseFailed("parse".to_string())),
            Error::Config(ConfigError::SerializeFailed("serialize".to_string())),
            Error::Config(ConfigError::ValidationError("invalid".to_string())),
            Error::Policy("denied".to_string()),
            Error::Io(std::io::Error::other("io")),
            Error::Json(json_err),
            Error::Runtime("runtime".to_string()),
            Error::SetupError("setup".to_string()),
            Error::Cancelled("user cancelled".to_string()),
            Error::Panicked("task panicked".to_string()),
        ];

        for error in errors {
            let remediation = error.remediation().expect("missing remediation");
            assert!(
                !remediation.summary.is_empty(),
                "remediation summary empty for {error:?}"
            );
            assert!(
                !remediation.commands.is_empty(),
                "remediation commands empty for {error:?}"
            );
        }
    }

    // --- Remediation builder tests ---

    #[test]
    fn remediation_new_has_empty_fields() {
        let r = Remediation::new("Fix the thing");
        assert_eq!(r.summary, "Fix the thing");
        assert!(r.commands.is_empty());
        assert!(r.alternatives.is_empty());
        assert!(r.learn_more.is_none());
    }

    #[test]
    fn remediation_builder_chain() {
        let r = Remediation::new("summary")
            .command("Run", "ft doctor")
            .platform_command("Install", "brew install wezterm", "macOS")
            .alternative("Try something else")
            .learn_more("https://example.com");

        assert_eq!(r.summary, "summary");
        assert_eq!(r.commands.len(), 2);
        assert_eq!(r.commands[0].label, "Run");
        assert_eq!(r.commands[0].command, "ft doctor");
        assert!(r.commands[0].platform.is_none());
        assert_eq!(r.commands[1].label, "Install");
        assert_eq!(r.commands[1].platform.as_deref(), Some("macOS"));
        assert_eq!(r.alternatives, vec!["Try something else"]);
        assert_eq!(r.learn_more.as_deref(), Some("https://example.com"));
    }

    // --- Remediation render_plain tests ---

    #[test]
    fn render_plain_includes_summary() {
        let r = Remediation::new("Check your PATH");
        let output = r.render_plain();
        assert!(output.contains("Check your PATH"));
        assert!(output.contains("To fix:"));
    }

    #[test]
    fn render_plain_includes_commands() {
        let r = Remediation::new("Fix it").command("Diagnose", "ft doctor");
        let output = r.render_plain();
        assert!(output.contains("Commands:"));
        assert!(output.contains("Diagnose: ft doctor"));
    }

    #[test]
    fn render_plain_includes_platform_hint() {
        let r =
            Remediation::new("Fix it").platform_command("Install", "brew install wezterm", "macOS");
        let output = r.render_plain();
        assert!(output.contains("Install (macOS): brew install wezterm"));
    }

    #[test]
    fn render_plain_includes_alternatives() {
        let r = Remediation::new("Fix it").alternative("Try plan B");
        let output = r.render_plain();
        assert!(output.contains("Alternatives:"));
        assert!(output.contains("Try plan B"));
    }

    #[test]
    fn render_plain_includes_learn_more() {
        let r = Remediation::new("Fix it").learn_more("https://docs.example.com");
        let output = r.render_plain();
        assert!(output.contains("Learn more: https://docs.example.com"));
    }

    #[test]
    fn render_plain_omits_empty_sections() {
        let r = Remediation::new("Fix it");
        let output = r.render_plain();
        assert!(!output.contains("Commands:"));
        assert!(!output.contains("Alternatives:"));
        assert!(!output.contains("Learn more:"));
    }

    // --- RemediationCommand serde roundtrip ---

    #[test]
    fn remediation_command_serde_roundtrip() {
        let cmd = RemediationCommand {
            label: "Diagnose".to_string(),
            command: "ft doctor".to_string(),
            platform: Some("macOS".to_string()),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: RemediationCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back.label, "Diagnose");
        assert_eq!(back.command, "ft doctor");
        assert_eq!(back.platform.as_deref(), Some("macOS"));
    }

    #[test]
    fn remediation_serde_roundtrip() {
        let r = Remediation::new("Fix it")
            .command("Run", "ft doctor")
            .alternative("Manual fix")
            .learn_more("https://example.com");
        let json = serde_json::to_string(&r).unwrap();
        let back: Remediation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.summary, "Fix it");
        assert_eq!(back.commands.len(), 1);
        assert_eq!(back.alternatives, vec!["Manual fix"]);
        assert_eq!(back.learn_more.as_deref(), Some("https://example.com"));
    }

    // --- Error Display tests ---

    #[test]
    fn error_display_includes_context() {
        let err = Error::Wezterm(WeztermError::PaneNotFound(42));
        assert!(err.to_string().contains("42"));

        let err = Error::Policy("rate limit exceeded".to_string());
        assert!(err.to_string().contains("rate limit exceeded"));

        let err = Error::Runtime("channel closed".to_string());
        assert!(err.to_string().contains("channel closed"));
    }

    #[test]
    fn wezterm_error_display() {
        assert!(WeztermError::CliNotFound.to_string().contains("not found"));
        assert!(WeztermError::NotRunning.to_string().contains("not running"));
        assert!(WeztermError::Timeout(10).to_string().contains("10"));
        assert!(
            WeztermError::CircuitOpen {
                retry_after_ms: 500
            }
            .to_string()
            .contains("500")
        );
    }

    #[test]
    fn storage_error_display() {
        assert!(
            StorageError::Database("corruption".to_string())
                .to_string()
                .contains("corruption")
        );
        let seq = StorageError::SequenceDiscontinuity {
            expected: 5,
            actual: 8,
        };
        let msg = seq.to_string();
        assert!(msg.contains("5") && msg.contains("8"));
    }

    // --- From conversions ---

    #[test]
    fn from_wezterm_error() {
        let inner = WeztermError::CliNotFound;
        let err: Error = inner.into();
        assert!(matches!(err, Error::Wezterm(WeztermError::CliNotFound)));
    }

    #[test]
    fn from_storage_error() {
        let inner = StorageError::Database("test".to_string());
        let err: Error = inner.into();
        assert!(matches!(err, Error::Storage(StorageError::Database(_))));
    }

    #[test]
    fn from_io_error() {
        let inner = std::io::Error::other("test");
        let err: Error = inner.into();
        assert!(matches!(err, Error::Io(_)));
    }

    // --- Sub-error remediation tests ---

    #[test]
    fn wezterm_cli_not_found_remediation_has_install_commands() {
        let r = WeztermError::CliNotFound.remediation();
        assert!(r.commands.len() >= 3); // multiple platform installs
        assert!(r.learn_more.is_some());
    }

    #[test]
    fn storage_schema_too_new_remediation_mentions_upgrade() {
        let r = StorageError::SchemaTooNew {
            current: 9,
            supported: 6,
        }
        .remediation();
        let text = r.render_plain();
        assert!(
            text.contains("upgrade") || text.contains("install") || text.contains("Update"),
            "Should mention upgrading: {}",
            text
        );
    }

    #[test]
    fn workflow_pane_locked_remediation() {
        let r = WorkflowError::PaneLocked.remediation();
        assert!(!r.summary.is_empty());
        assert!(!r.commands.is_empty());
    }
}
