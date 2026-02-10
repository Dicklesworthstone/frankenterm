//! Error code catalog for ft
//!
//! Defines structured error codes (WA-XXXX) with human-readable descriptions,
//! causes, and recovery steps. Used by `ft why <code>` for explainability.
//!
//! # Error Code Ranges
//!
//! | Range      | Category     | Description                          |
//! |------------|--------------|--------------------------------------|
//! | WA-1xxx    | WezTerm      | WezTerm CLI and pane errors          |
//! | WA-2xxx    | Storage      | Database and FTS errors              |
//! | WA-3xxx    | Pattern      | Pattern matching and pack errors     |
//! | WA-4xxx    | Policy       | Safety policy and send blocks        |
//! | WA-5xxx    | Workflow     | Workflow execution errors            |
//! | WA-6xxx    | Network      | Network and IPC errors               |
//! | WA-7xxx    | Config       | Configuration errors                 |
//! | WA-9xxx    | Internal     | Internal/unexpected errors           |

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// Error category corresponding to code ranges
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// WA-1xxx: WezTerm CLI and pane errors
    Wezterm,
    /// WA-2xxx: Database and FTS errors
    Storage,
    /// WA-3xxx: Pattern matching and pack errors
    Pattern,
    /// WA-4xxx: Safety policy and send blocks
    Policy,
    /// WA-5xxx: Workflow execution errors
    Workflow,
    /// WA-6xxx: Network and IPC errors
    Network,
    /// WA-7xxx: Configuration errors
    Config,
    /// WA-9xxx: Internal/unexpected errors
    Internal,
}

impl ErrorCategory {
    /// Return the numeric range for this category
    #[must_use]
    pub const fn range(&self) -> (u16, u16) {
        match self {
            Self::Wezterm => (1000, 1999),
            Self::Storage => (2000, 2999),
            Self::Pattern => (3000, 3999),
            Self::Policy => (4000, 4999),
            Self::Workflow => (5000, 5999),
            Self::Network => (6000, 6999),
            Self::Config => (7000, 7999),
            Self::Internal => (9000, 9999),
        }
    }

    /// Parse category from error code
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        let num: u16 = code.strip_prefix("FT-")?.parse().ok()?;
        match num {
            1000..=1999 => Some(Self::Wezterm),
            2000..=2999 => Some(Self::Storage),
            3000..=3999 => Some(Self::Pattern),
            4000..=4999 => Some(Self::Policy),
            5000..=5999 => Some(Self::Workflow),
            6000..=6999 => Some(Self::Network),
            7000..=7999 => Some(Self::Config),
            9000..=9999 => Some(Self::Internal),
            _ => None,
        }
    }
}

/// A single recovery step with optional command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryStep {
    /// Human-readable description of this step
    pub description: Cow<'static, str>,
    /// Optional command to run
    pub command: Option<Cow<'static, str>>,
}

impl RecoveryStep {
    /// Create a text-only recovery step
    #[must_use]
    pub const fn text(description: &'static str) -> Self {
        Self {
            description: Cow::Borrowed(description),
            command: None,
        }
    }

    /// Create a recovery step with an associated command
    #[must_use]
    pub const fn with_command(description: &'static str, command: &'static str) -> Self {
        Self {
            description: Cow::Borrowed(description),
            command: Some(Cow::Borrowed(command)),
        }
    }
}

/// A complete error code definition (static version for compile-time initialization)
#[derive(Debug, Clone)]
pub struct ErrorCodeDef {
    /// The error code (e.g., "FT-1001")
    pub code: &'static str,
    /// Error category
    pub category: ErrorCategory,
    /// Short title for the error
    pub title: &'static str,
    /// Full description of what this error means
    pub description: &'static str,
    /// Common causes for this error (static slice)
    pub causes: &'static [&'static str],
    /// Steps to recover from this error (static slice)
    pub recovery_steps: &'static [RecoveryStep],
    /// Optional documentation link
    pub doc_link: Option<&'static str>,
}

impl ErrorCodeDef {
    /// Format the error code for human-readable display
    #[must_use]
    pub fn format_plain(&self) -> String {
        let mut output = String::new();
        output.push_str(&format!("{}: {}\n\n", self.code, self.title));
        output.push_str(self.description);
        output.push_str("\n\n");

        if !self.causes.is_empty() {
            output.push_str("Common causes:\n");
            for cause in self.causes {
                output.push_str(&format!("  - {cause}\n"));
            }
            output.push('\n');
        }

        if !self.recovery_steps.is_empty() {
            output.push_str("Recovery steps:\n");
            for (i, step) in self.recovery_steps.iter().enumerate() {
                output.push_str(&format!("  {}. {}\n", i + 1, step.description));
                if let Some(cmd) = &step.command {
                    output.push_str(&format!("     $ {cmd}\n"));
                }
            }
            output.push('\n');
        }

        if let Some(link) = self.doc_link {
            output.push_str(&format!("Learn more: {link}\n"));
        }

        output
    }
}

// ============================================================================
// Error Code Definitions
// ============================================================================

/// WA-1001: WezTerm CLI not found
pub static FT_1001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1001",
    category: ErrorCategory::Wezterm,
    title: "WezTerm CLI not found",
    description: "The `wezterm` command-line tool could not be found in your PATH. \
                  ft requires WezTerm to be installed and accessible.",
    causes: &[
        "WezTerm is not installed",
        "WezTerm is installed but not in PATH",
        "Using a portable WezTerm without CLI integration",
    ],
    recovery_steps: &[
        RecoveryStep::text("Install WezTerm from https://wezfurlong.org/wezterm/"),
        RecoveryStep::text("Add WezTerm to your PATH"),
        RecoveryStep::with_command("Verify installation", "wezterm --version"),
    ],
    doc_link: Some("https://wezfurlong.org/wezterm/install.html"),
};

/// WA-1002: WezTerm not running
pub static FT_1002: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1002",
    category: ErrorCategory::Wezterm,
    title: "WezTerm not running",
    description: "WezTerm is not currently running. ft requires an active WezTerm instance \
                  to observe and control terminal panes.",
    causes: &[
        "WezTerm application is not started",
        "WezTerm was recently closed",
        "Wrong socket path configured",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Start WezTerm", "wezterm start"),
        RecoveryStep::with_command("Check panes", "wezterm cli list --format json"),
    ],
    doc_link: None,
};

/// WA-1003: Socket not found
pub static FT_1003: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1003",
    category: ErrorCategory::Wezterm,
    title: "WezTerm socket not found",
    description: "The WezTerm IPC socket could not be found or accessed. \
                  This is used for communication between ft and WezTerm.",
    causes: &[
        "WezTerm is not running",
        "WEZTERM_UNIX_SOCKET environment variable points to wrong path",
        "Socket permissions prevent access",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check socket env", "echo $WEZTERM_UNIX_SOCKET"),
        RecoveryStep::with_command("Start WezTerm fresh", "wezterm start"),
        RecoveryStep::text("Unset WEZTERM_UNIX_SOCKET to use the default"),
    ],
    doc_link: None,
};

/// WA-1010: Pane not found
pub static FT_1010: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1010",
    category: ErrorCategory::Wezterm,
    title: "Pane not found",
    description: "The specified pane ID does not exist. The pane may have been closed \
                  or the ID may be incorrect.",
    causes: &[
        "Pane was closed after the command was issued",
        "Pane ID was typed incorrectly",
        "Using a stale pane ID from a previous session",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("List current panes", "ft list"),
        RecoveryStep::with_command("Get JSON pane list", "ft robot state"),
        RecoveryStep::text("Use a valid pane ID from the list"),
    ],
    doc_link: None,
};

/// WA-1020: Command execution failed
pub static FT_1020: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1020",
    category: ErrorCategory::Wezterm,
    title: "WezTerm command failed",
    description: "A WezTerm CLI command failed to execute. This could indicate \
                  a transient issue or a problem with WezTerm itself.",
    causes: &[
        "WezTerm is unresponsive",
        "System resource constraints",
        "WezTerm internal error",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check WezTerm status", "wezterm cli list"),
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
        RecoveryStep::text("Restart WezTerm if issues persist"),
    ],
    doc_link: None,
};

/// WA-1021: WezTerm parse error
pub static FT_1021: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1021",
    category: ErrorCategory::Wezterm,
    title: "WezTerm parse error",
    description: "Failed to parse output or response from WezTerm CLI. The data \
                  received was not in the expected format.",
    causes: &[
        "WezTerm returned unexpected output format",
        "Corrupted or truncated response",
        "Version mismatch between ft and WezTerm",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check WezTerm version", "wezterm --version"),
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
        RecoveryStep::text("Update WezTerm to the latest stable version"),
    ],
    doc_link: None,
};

/// WA-1022: WezTerm command timeout
pub static FT_1022: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1022",
    category: ErrorCategory::Wezterm,
    title: "WezTerm command timeout",
    description: "A WezTerm CLI command timed out before completing. The operation \
                  may still be running in the background.",
    causes: &[
        "WezTerm is under heavy load",
        "System resources are constrained",
        "Large amount of terminal output being processed",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check WezTerm responsiveness", "wezterm cli list"),
        RecoveryStep::text("Retry after a brief wait"),
        RecoveryStep::text("Restart WezTerm if timeouts persist"),
    ],
    doc_link: None,
};

/// WA-1030: JSON parse error from WezTerm
pub static FT_1030: ErrorCodeDef = ErrorCodeDef {
    code: "FT-1030",
    category: ErrorCategory::Wezterm,
    title: "WezTerm output parse error",
    description: "Failed to parse JSON output from WezTerm CLI. This may indicate \
                  a version mismatch or unexpected output format.",
    causes: &[
        "WezTerm version is too old or too new",
        "WezTerm output format changed",
        "Corrupted or incomplete output",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check WezTerm version", "wezterm --version"),
        RecoveryStep::text("Update WezTerm to the latest stable version"),
        RecoveryStep::text("Report issue if the problem persists"),
    ],
    doc_link: None,
};

// --- Storage Errors (WA-2xxx) ---

/// WA-2001: Database initialization failed
pub static FT_2001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2001",
    category: ErrorCategory::Storage,
    title: "Database initialization failed",
    description: "Failed to initialize the SQLite database. This could be due to \
                  permissions, disk space, or a corrupted database file.",
    causes: &[
        "No write permission to the data directory",
        "Disk is full",
        "Database file is corrupted",
        "Database is locked by another process",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check disk space", "df -h ~/.local/share/ft"),
        RecoveryStep::with_command("Check permissions", "ls -la ~/.local/share/ft"),
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
    ],
    doc_link: None,
};

/// WA-2002: Migration failed
pub static FT_2002: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2002",
    category: ErrorCategory::Storage,
    title: "Database migration failed",
    description: "Failed to migrate the database to the new schema version. \
                  This may require manual intervention.",
    causes: &[
        "Database file is corrupted",
        "Incompatible schema changes",
        "Insufficient disk space during migration",
    ],
    recovery_steps: &[
        RecoveryStep::text("Back up the database file before retrying"),
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
        RecoveryStep::text(
            "Consider deleting the database and starting fresh if data is not critical",
        ),
    ],
    doc_link: None,
};

/// WA-2003: Database schema too new
pub static FT_2003: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2003",
    category: ErrorCategory::Storage,
    title: "Database schema too new",
    description: "The database schema version is newer than this ft build supports.",
    causes: &[
        "Downgraded ft while keeping a newer database",
        "Database was created by a newer ft version",
    ],
    recovery_steps: &[
        RecoveryStep::with_command(
            "Upgrade ft",
            "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft",
        ),
        RecoveryStep::text("Restore a database backup created by an older ft version"),
    ],
    doc_link: None,
};

/// WA-2004: ft version too old for database
pub static FT_2004: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2004",
    category: ErrorCategory::Storage,
    title: "ft version too old",
    description: "This database requires a newer ft version than the one currently running.",
    causes: &[
        "Downgraded ft while keeping a newer database",
        "Database migrations raised the minimum compatible ft version",
    ],
    recovery_steps: &[
        RecoveryStep::with_command(
            "Upgrade ft",
            "cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft",
        ),
        RecoveryStep::text("Restore an older database backup if upgrade is not possible"),
    ],
    doc_link: None,
};

/// WA-2010: Sequence discontinuity
pub static FT_2010: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2010",
    category: ErrorCategory::Storage,
    title: "Output sequence discontinuity",
    description: "A gap was detected in the captured output sequence. This means \
                  some terminal output may have been missed.",
    causes: &[
        "Watcher was restarted while a pane was active",
        "High system load caused missed polls",
        "Terminal scrollback buffer overflow",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check watcher status", "ft status"),
        RecoveryStep::with_command("View gap events", "ft events --type gap"),
        RecoveryStep::text(
            "Gaps are tracked and will not affect search accuracy for captured content",
        ),
    ],
    doc_link: None,
};

/// WA-2020: FTS query error
pub static FT_2020: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2020",
    category: ErrorCategory::Storage,
    title: "Full-text search query error",
    description: "The search query syntax is invalid. ft uses SQLite FTS5 \
                  for full-text search which has specific syntax requirements.",
    causes: &[
        "Invalid FTS5 query syntax",
        "Unbalanced quotes in search term",
        "Invalid operator usage",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Simple search", "ft search \"error message\""),
        RecoveryStep::text("Use double quotes for exact phrases"),
        RecoveryStep::text("Use AND/OR/NOT for boolean queries"),
    ],
    doc_link: Some("https://www.sqlite.org/fts5.html#full_text_query_syntax"),
};

/// WA-2030: Database corruption detected
pub static FT_2030: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2030",
    category: ErrorCategory::Storage,
    title: "Database corruption detected",
    description: "The database integrity check failed or corruption was detected \
                  during a read/write operation.",
    causes: &[
        "Unexpected system shutdown during write",
        "Disk hardware failure",
        "Database file modified by external process",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
        RecoveryStep::text("Back up the database file immediately"),
        RecoveryStep::text("Delete and recreate the database if data is not critical"),
    ],
    doc_link: None,
};

/// WA-2040: Storage record not found
pub static FT_2040: ErrorCodeDef = ErrorCodeDef {
    code: "FT-2040",
    category: ErrorCategory::Storage,
    title: "Storage record not found",
    description: "The requested record does not exist in the database. It may have \
                  been deleted by retention cleanup or never existed.",
    causes: &[
        "Record was deleted by retention policy",
        "ID refers to a non-existent record",
        "Database was recreated since the ID was issued",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("List recent records", "ft robot events --limit 20"),
        RecoveryStep::text("Verify the record ID is correct"),
        RecoveryStep::text("The record may have been removed by automatic cleanup"),
    ],
    doc_link: None,
};

// --- Pattern Errors (WA-3xxx) ---

/// WA-3001: Invalid regex
pub static FT_3001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-3001",
    category: ErrorCategory::Pattern,
    title: "Invalid regex pattern",
    description: "A regex pattern in the pattern pack is invalid and cannot be compiled.",
    causes: &[
        "Syntax error in regular expression",
        "Unsupported regex feature",
        "Corrupted pattern pack file",
    ],
    recovery_steps: &[
        RecoveryStep::text("Check the pattern pack file for syntax errors"),
        RecoveryStep::with_command(
            "Disable pattern detection temporarily",
            "ft watch --no-patterns",
        ),
        RecoveryStep::text("Test the regex at regex101.com (Rust flavor)"),
    ],
    doc_link: None,
};

/// WA-3002: Pattern pack not found
pub static FT_3002: ErrorCodeDef = ErrorCodeDef {
    code: "FT-3002",
    category: ErrorCategory::Pattern,
    title: "Pattern pack not found",
    description: "The specified pattern pack could not be found in the configured paths.",
    causes: &[
        "Pack name is misspelled in configuration",
        "Pack file was deleted or moved",
        "Custom pack path is incorrect",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("List available packs", "ft rules packs"),
        RecoveryStep::text("Check the pack name in ft.toml [patterns] section"),
        RecoveryStep::text("Use built-in packs: core.codex, core.claude, core.gemini"),
    ],
    doc_link: None,
};

/// WA-3010: Pattern match timeout
pub static FT_3010: ErrorCodeDef = ErrorCodeDef {
    code: "FT-3010",
    category: ErrorCategory::Pattern,
    title: "Pattern match timeout",
    description: "Pattern evaluation timed out. This usually indicates a complex \
                  regex that causes catastrophic backtracking.",
    causes: &[
        "Complex regex with nested quantifiers",
        "Very large input text",
        "Catastrophic backtracking in regex",
    ],
    recovery_steps: &[
        RecoveryStep::text("Simplify the pattern regex"),
        RecoveryStep::text("Add anchors or atomic groups to prevent backtracking"),
        RecoveryStep::with_command(
            "Disable problematic pack",
            "ft config set patterns.disabled '[\"pack-name\"]'",
        ),
    ],
    doc_link: None,
};

/// WA-3020: Pattern match timeout
pub static FT_3020: ErrorCodeDef = ErrorCodeDef {
    code: "FT-3020",
    category: ErrorCategory::Pattern,
    title: "Pattern match timeout",
    description: "A pattern match operation timed out. This usually indicates a \
                  regex with catastrophic backtracking on the given input.",
    causes: &[
        "Complex regex with nested quantifiers",
        "Very large input text exceeding match budget",
        "Catastrophic backtracking in pattern",
    ],
    recovery_steps: &[
        RecoveryStep::text("Simplify the pattern regex"),
        RecoveryStep::text("Add anchors or atomic groups to prevent backtracking"),
        RecoveryStep::with_command(
            "Disable the problematic pack",
            "ft config set patterns.disabled '[\"pack-name\"]'",
        ),
    ],
    doc_link: None,
};

// --- Policy Errors (WA-4xxx) ---

/// WA-4001: Send blocked - alternate screen
pub static FT_4001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-4001",
    category: ErrorCategory::Policy,
    title: "Send blocked - alternate screen mode",
    description: "The send action was blocked because the pane is in alternate \
                  screen mode. This typically means a full-screen application \
                  like vim, less, or htop is running.",
    causes: &[
        "A text editor (vim, nano, emacs) is open in the pane",
        "A pager (less, more) is showing output",
        "A TUI application (htop, ncdu) is running",
    ],
    recovery_steps: &[
        RecoveryStep::text("Wait for the application to exit"),
        RecoveryStep::text("Close the application manually (e.g., :q in vim)"),
        RecoveryStep::with_command("Check pane status", "ft status --pane <id>"),
    ],
    doc_link: None,
};

/// WA-4002: Send blocked - command running
pub static FT_4002: ErrorCodeDef = ErrorCodeDef {
    code: "FT-4002",
    category: ErrorCategory::Policy,
    title: "Send blocked - command running",
    description: "The send action was blocked because a command is currently \
                  running in the pane. Sending input while a command runs \
                  could interfere with its execution.",
    causes: &[
        "A long-running command is executing",
        "The shell is not at a prompt",
        "OSC 133 markers indicate command in progress",
    ],
    recovery_steps: &[
        RecoveryStep::text("Wait for the current command to finish"),
        RecoveryStep::with_command("Send Ctrl-C to cancel", "ft send <id> --ctrl-c"),
        RecoveryStep::with_command(
            "Use --wait-for to wait for prompt",
            "ft send <id> --wait-for 'prompt'",
        ),
    ],
    doc_link: None,
};

/// WA-4003: Send blocked - rate limit
pub static FT_4003: ErrorCodeDef = ErrorCodeDef {
    code: "FT-4003",
    category: ErrorCategory::Policy,
    title: "Send blocked - rate limit protection",
    description: "The send action was blocked due to rate limiting. This protects \
                  against accidental input storms and runaway automation.",
    causes: &[
        "Too many sends in a short period",
        "Automation loop sending too frequently",
        "Rate limit configured lower than needed",
    ],
    recovery_steps: &[
        RecoveryStep::text("Wait a moment before retrying"),
        RecoveryStep::text("Reduce send frequency in your automation"),
        RecoveryStep::with_command("Check rate limit config", "ft config show | grep rate"),
    ],
    doc_link: None,
};

/// WA-4010: Approval required
pub static FT_4010: ErrorCodeDef = ErrorCodeDef {
    code: "FT-4010",
    category: ErrorCategory::Policy,
    title: "Approval required",
    description: "This action requires explicit approval before it can proceed. \
                  Use the provided allow-once code to approve the action.",
    causes: &[
        "Safety policy requires approval for this action type",
        "Pane is in an uncertain state",
        "Action could have significant side effects",
    ],
    recovery_steps: &[
        RecoveryStep::text("Review the action carefully"),
        RecoveryStep::with_command("Approve with code", "ft robot approve <CODE>"),
        RecoveryStep::with_command("See what was blocked", "ft why <CODE>"),
    ],
    doc_link: None,
};

/// WA-4020: Action blocked by safety policy
pub static FT_4020: ErrorCodeDef = ErrorCodeDef {
    code: "FT-4020",
    category: ErrorCategory::Policy,
    title: "Action blocked by safety policy",
    description: "The action was blocked by the safety policy. This is a \
                  protective measure to prevent accidental damage.",
    causes: &[
        "Action matches a blocked pattern",
        "Pane reservation conflict",
        "Insufficient capabilities for this action",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check policy details", "ft why deny.<reason>"),
        RecoveryStep::with_command("Check pane status", "ft status --pane <id>"),
        RecoveryStep::text("Review ft.toml [safety] section for policy rules"),
    ],
    doc_link: None,
};

// --- Workflow Errors (WA-5xxx) ---

/// WA-5001: Workflow not found
pub static FT_5001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-5001",
    category: ErrorCategory::Workflow,
    title: "Workflow not found",
    description: "The specified workflow name does not exist in the registered workflows.",
    causes: &[
        "Workflow name is misspelled",
        "Workflow is not enabled in configuration",
        "Custom workflow file is missing",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("List available workflows", "ft workflow list"),
        RecoveryStep::text("Check spelling of workflow name"),
        RecoveryStep::text("Ensure workflow is enabled in ft.toml"),
    ],
    doc_link: None,
};

/// WA-5002: Workflow aborted
pub static FT_5002: ErrorCodeDef = ErrorCodeDef {
    code: "FT-5002",
    category: ErrorCategory::Workflow,
    title: "Workflow aborted",
    description: "The workflow was aborted before completion. This may be due to \
                  a guard failure, user cancellation, or an unrecoverable error.",
    causes: &[
        "Guard condition failed",
        "User requested abort",
        "Step encountered unrecoverable error",
        "Pane closed during workflow",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check workflow status", "ft workflow status <id>"),
        RecoveryStep::text("Review the abort reason in the workflow logs"),
        RecoveryStep::text("Retry the workflow when conditions are met"),
    ],
    doc_link: None,
};

/// WA-5010: Guard condition failed
pub static FT_5010: ErrorCodeDef = ErrorCodeDef {
    code: "FT-5010",
    category: ErrorCategory::Workflow,
    title: "Workflow guard condition failed",
    description: "The workflow's guard condition was not satisfied. Guards ensure \
                  the pane is in the correct state before a workflow runs.",
    causes: &[
        "Pane is not in the expected state",
        "Required pattern not detected",
        "Prerequisites not met",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check pane status", "ft status --pane <id>"),
        RecoveryStep::text("Ensure the triggering condition is still present"),
        RecoveryStep::text("Manually put the pane in the required state"),
    ],
    doc_link: None,
};

/// WA-5020: Pane locked by another workflow
pub static FT_5020: ErrorCodeDef = ErrorCodeDef {
    code: "FT-5020",
    category: ErrorCategory::Workflow,
    title: "Pane locked by another workflow",
    description: "Another workflow is currently running on this pane. Only one \
                  workflow can control a pane at a time to prevent conflicts.",
    causes: &[
        "Previous workflow is still running",
        "Workflow is waiting for a condition",
        "Stale lock from crashed workflow",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check running workflows", "ft workflow status"),
        RecoveryStep::text("Wait for the current workflow to complete"),
        RecoveryStep::with_command("Abort stuck workflow", "ft workflow abort <id>"),
    ],
    doc_link: None,
};

/// WA-5030: Pane locked by another workflow
pub static FT_5030: ErrorCodeDef = ErrorCodeDef {
    code: "FT-5030",
    category: ErrorCategory::Workflow,
    title: "Pane locked",
    description: "The target pane is locked by an active workflow. Only one workflow \
                  can control a pane at a time to prevent conflicts.",
    causes: &[
        "Another workflow is currently executing on this pane",
        "A workflow is waiting for a pattern or condition",
        "Stale lock from a crashed workflow",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Check running workflows", "ft workflow status"),
        RecoveryStep::text("Wait for the current workflow to complete"),
        RecoveryStep::with_command("Abort stuck workflow", "ft workflow abort <id>"),
    ],
    doc_link: None,
};

// --- Network Errors (WA-6xxx) ---

/// WA-6001: IPC connection failed
pub static FT_6001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-6001",
    category: ErrorCategory::Network,
    title: "IPC connection failed",
    description: "Failed to connect to the ft watcher daemon via IPC socket.",
    causes: &[
        "Watcher daemon is not running",
        "Socket file does not exist",
        "Permission denied on socket",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Start the watcher", "ft watch"),
        RecoveryStep::with_command("Check watcher status", "ft daemon status"),
        RecoveryStep::text("Ensure you have permission to access the socket"),
    ],
    doc_link: None,
};

// --- Config Errors (WA-7xxx) ---

/// WA-7001: Config file not found
pub static FT_7001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-7001",
    category: ErrorCategory::Config,
    title: "Config file not found",
    description: "The configuration file could not be found at the expected location.",
    causes: &[
        "ft.toml does not exist",
        "Wrong config path specified",
        "First time running ft (no config created yet)",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Initialize config", "ft config init"),
        RecoveryStep::with_command("Show config path", "ft config show --path"),
        RecoveryStep::text("ft will use defaults if no config file exists"),
    ],
    doc_link: None,
};

/// WA-7002: Config parse error
pub static FT_7002: ErrorCodeDef = ErrorCodeDef {
    code: "FT-7002",
    category: ErrorCategory::Config,
    title: "Config parse error",
    description: "The configuration file contains invalid TOML syntax.",
    causes: &[
        "Invalid TOML syntax",
        "Incorrect value types",
        "Missing required fields",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Validate config", "ft config validate"),
        RecoveryStep::text("Check for syntax errors in ft.toml"),
        RecoveryStep::text("Ensure all values have correct types"),
    ],
    doc_link: None,
};

/// WA-7003: Config parse error (TOML)
pub static FT_7003: ErrorCodeDef = ErrorCodeDef {
    code: "FT-7003",
    category: ErrorCategory::Config,
    title: "Config parse error",
    description: "The configuration file could not be parsed. It may contain \
                  invalid TOML syntax or unexpected value types.",
    causes: &[
        "Invalid TOML syntax in ft.toml",
        "Incorrect value types for configuration keys",
        "Encoding issues in the configuration file",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Validate config syntax", "ft config validate"),
        RecoveryStep::text("Check for unmatched quotes, brackets, or invalid characters"),
        RecoveryStep::text("Compare against a known-good configuration"),
    ],
    doc_link: None,
};

/// WA-7004: Config serialization failed
pub static FT_7004: ErrorCodeDef = ErrorCodeDef {
    code: "FT-7004",
    category: ErrorCategory::Config,
    title: "Config serialization failed",
    description: "Failed to serialize configuration to TOML format. This is an \
                  internal error that should not normally occur.",
    causes: &[
        "Internal data structure contains unsupported types",
        "Bug in configuration serialization code",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
        RecoveryStep::text(
            "Report the issue at https://github.com/Dicklesworthstone/frankenterm/issues",
        ),
    ],
    doc_link: None,
};

/// WA-7010: Config validation error
pub static FT_7010: ErrorCodeDef = ErrorCodeDef {
    code: "FT-7010",
    category: ErrorCategory::Config,
    title: "Config validation error",
    description: "The configuration values are syntactically correct but semantically invalid.",
    causes: &[
        "Invalid poll interval (too fast or too slow)",
        "Invalid regex in filter rules",
        "Conflicting configuration options",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Validate config", "ft config validate"),
        RecoveryStep::with_command("Show effective config", "ft config show --effective"),
        RecoveryStep::text("Review the validation errors and fix the values"),
    ],
    doc_link: None,
};

// --- Internal Errors (WA-9xxx) ---

/// WA-9001: Internal error
pub static FT_9001: ErrorCodeDef = ErrorCodeDef {
    code: "FT-9001",
    category: ErrorCategory::Internal,
    title: "Internal error",
    description: "An unexpected internal error occurred. This is likely a bug in ft.",
    causes: &["Bug in ft code", "Unexpected state", "Unhandled edge case"],
    recovery_steps: &[
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
        RecoveryStep::text("Try restarting the watcher"),
        RecoveryStep::text(
            "Report the issue at https://github.com/Dicklesworthstone/frankenterm/issues",
        ),
    ],
    doc_link: Some("https://github.com/Dicklesworthstone/frankenterm/issues"),
};

/// WA-9002: I/O error
pub static FT_9002: ErrorCodeDef = ErrorCodeDef {
    code: "FT-9002",
    category: ErrorCategory::Internal,
    title: "I/O error",
    description: "A filesystem or OS I/O error occurred while ft was running.",
    causes: &[
        "Missing files or directories",
        "Permission denied when reading or writing",
        "Disk full or unavailable filesystem",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Run diagnostics", "ft doctor"),
        RecoveryStep::text("Check filesystem permissions and available space"),
        RecoveryStep::text("Retry after resolving the underlying I/O issue"),
    ],
    doc_link: None,
};

/// WA-9003: JSON serialization error
pub static FT_9003: ErrorCodeDef = ErrorCodeDef {
    code: "FT-9003",
    category: ErrorCategory::Internal,
    title: "JSON serialization error",
    description: "JSON input or output could not be parsed or serialized.",
    causes: &[
        "Malformed JSON input",
        "Unexpected data types in JSON payloads",
        "Invalid UTF-8 in JSON streams",
    ],
    recovery_steps: &[
        RecoveryStep::with_command("Validate JSON", "python -m json.tool < input.json"),
        RecoveryStep::text("Check for trailing commas or invalid characters"),
        RecoveryStep::text("Retry with a well-formed JSON payload"),
    ],
    doc_link: None,
};

// ============================================================================
// Error Code Registry
// ============================================================================

/// Static registry of all error codes
pub static ERROR_CATALOG: LazyLock<HashMap<&'static str, &'static ErrorCodeDef>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        // WezTerm errors
        m.insert("FT-1001", &FT_1001);
        m.insert("FT-1002", &FT_1002);
        m.insert("FT-1003", &FT_1003);
        m.insert("FT-1010", &FT_1010);
        m.insert("FT-1020", &FT_1020);
        m.insert("FT-1021", &FT_1021);
        m.insert("FT-1022", &FT_1022);
        m.insert("FT-1030", &FT_1030);
        // Storage errors
        m.insert("FT-2001", &FT_2001);
        m.insert("FT-2002", &FT_2002);
        m.insert("FT-2003", &FT_2003);
        m.insert("FT-2004", &FT_2004);
        m.insert("FT-2010", &FT_2010);
        m.insert("FT-2020", &FT_2020);
        m.insert("FT-2030", &FT_2030);
        m.insert("FT-2040", &FT_2040);
        // Pattern errors
        m.insert("FT-3001", &FT_3001);
        m.insert("FT-3002", &FT_3002);
        m.insert("FT-3010", &FT_3010);
        m.insert("FT-3020", &FT_3020);
        // Policy errors
        m.insert("FT-4001", &FT_4001);
        m.insert("FT-4002", &FT_4002);
        m.insert("FT-4003", &FT_4003);
        m.insert("FT-4010", &FT_4010);
        m.insert("FT-4020", &FT_4020);
        // Workflow errors
        m.insert("FT-5001", &FT_5001);
        m.insert("FT-5002", &FT_5002);
        m.insert("FT-5010", &FT_5010);
        m.insert("FT-5020", &FT_5020);
        m.insert("FT-5030", &FT_5030);
        // Network errors
        m.insert("FT-6001", &FT_6001);
        // Config errors
        m.insert("FT-7001", &FT_7001);
        m.insert("FT-7002", &FT_7002);
        m.insert("FT-7003", &FT_7003);
        m.insert("FT-7004", &FT_7004);
        m.insert("FT-7010", &FT_7010);
        // Internal errors
        m.insert("FT-9001", &FT_9001);
        m.insert("FT-9002", &FT_9002);
        m.insert("FT-9003", &FT_9003);
        m
    });

/// Look up an error code definition
#[must_use]
pub fn get_error_code(code: &str) -> Option<&'static ErrorCodeDef> {
    ERROR_CATALOG.get(code).copied()
}

/// List all error codes in sorted order
#[must_use]
pub fn list_error_codes() -> Vec<&'static str> {
    let mut codes: Vec<&str> = ERROR_CATALOG.keys().copied().collect();
    codes.sort_unstable();
    codes
}

/// List error codes by category
#[must_use]
pub fn list_codes_by_category(category: ErrorCategory) -> Vec<&'static ErrorCodeDef> {
    ERROR_CATALOG
        .values()
        .filter(|def| def.category == category)
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_codes_are_registered() {
        // Verify we have codes for each category
        assert!(!list_codes_by_category(ErrorCategory::Wezterm).is_empty());
        assert!(!list_codes_by_category(ErrorCategory::Storage).is_empty());
        assert!(!list_codes_by_category(ErrorCategory::Pattern).is_empty());
        assert!(!list_codes_by_category(ErrorCategory::Policy).is_empty());
        assert!(!list_codes_by_category(ErrorCategory::Workflow).is_empty());
        assert!(!list_codes_by_category(ErrorCategory::Config).is_empty());
        assert!(!list_codes_by_category(ErrorCategory::Internal).is_empty());
    }

    #[test]
    fn code_lookup_works() {
        assert!(get_error_code("FT-1001").is_some());
        assert!(get_error_code("FT-4001").is_some());
        assert!(get_error_code("FT-9999").is_none());
    }

    #[test]
    fn category_from_code_works() {
        assert_eq!(
            ErrorCategory::from_code("FT-1001"),
            Some(ErrorCategory::Wezterm)
        );
        assert_eq!(
            ErrorCategory::from_code("FT-4001"),
            Some(ErrorCategory::Policy)
        );
        assert_eq!(
            ErrorCategory::from_code("FT-9001"),
            Some(ErrorCategory::Internal)
        );
        assert_eq!(ErrorCategory::from_code("INVALID"), None);
    }

    #[test]
    fn all_codes_have_recovery_steps() {
        for (code, def) in ERROR_CATALOG.iter() {
            assert!(
                !def.recovery_steps.is_empty(),
                "Error code {code} has no recovery steps"
            );
        }
    }

    #[test]
    fn all_codes_have_titles_and_descriptions() {
        for (code, def) in ERROR_CATALOG.iter() {
            assert!(
                !def.title.trim().is_empty(),
                "Error code {code} has empty title"
            );
            assert!(
                !def.description.trim().is_empty(),
                "Error code {code} has empty description"
            );
        }
    }

    #[test]
    fn list_error_codes_is_sorted() {
        let codes = list_error_codes();
        for window in codes.windows(2) {
            assert!(window[0] <= window[1], "Codes not sorted: {:?}", codes);
        }
    }

    #[test]
    fn format_plain_is_nonempty() {
        for def in ERROR_CATALOG.values() {
            let formatted = def.format_plain();
            assert!(!formatted.is_empty());
            assert!(formatted.contains(def.code));
            assert!(formatted.contains(def.title));
        }
    }
}
