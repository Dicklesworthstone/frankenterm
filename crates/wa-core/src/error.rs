//! Error types for wa-core

use thiserror::Error;

/// Result type alias using the library's Error type
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for wa-core
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
}

/// WezTerm-specific errors
#[derive(Error, Debug)]
pub enum WeztermError {
    /// WezTerm CLI binary not found in PATH
    #[error("WezTerm CLI not found in PATH. Install WezTerm or add it to PATH.")]
    CliNotFound,

    /// WezTerm is not running (no GUI or socket available)
    #[error("WezTerm is not running. Start WezTerm first.")]
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

    #[error("FTS query error: {0}")]
    FtsQueryError(String),
}

/// Pattern-specific errors
#[derive(Error, Debug)]
pub enum PatternError {
    #[error("Invalid regex: {0}")]
    InvalidRegex(String),

    #[error("Pack not found: {0}")]
    PackNotFound(String),

    #[error("Match timeout")]
    MatchTimeout,
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

/// Configuration-specific errors
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Config file not found: {0}")]
    FileNotFound(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}
