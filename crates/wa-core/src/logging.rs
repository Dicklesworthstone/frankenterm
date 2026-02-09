//! Structured logging for wa
//!
//! This module provides the logging infrastructure for wa, using `tracing`
//! with configurable output formats and destinations.
//!
//! # Features
//!
//! - **Pretty format**: Human-friendly colored output for interactive use
//! - **JSON format**: Machine-parseable JSON lines for CI/E2E/ops
//! - **File output**: Optional log file for diagnostic bundles
//! - **Correlation fields**: Consistent context propagation (pane_id, workflow_name, etc.)
//!
//! # Usage
//!
//! Initialize logging once at startup:
//!
//! ```ignore
//! use wa_core::logging::{init_logging, LogConfig};
//! use wa_core::config::LogFormat;
//!
//! let config = LogConfig {
//!     level: "info".to_string(),
//!     format: LogFormat::Pretty,
//!     file: None,
//! };
//! init_logging(&config)?;
//! ```
//!
//! # Correlation Fields
//!
//! Use these field names consistently in spans and events:
//! - `workspace`: Workspace identifier
//! - `domain`: WezTerm domain (local, ssh, etc.)
//! - `pane_id`: Pane identifier
//! - `window_id`, `tab_id`: Window/tab context
//! - `rule_id`, `event_id`: Pattern/event identifiers
//! - `workflow_name`, `execution_id`: Workflow context
//! - `action_id`: Audit action identifier
//!
//! # Safety
//!
//! **Never log raw pane contents.** Any user-provided text that could contain
//! secrets must be logged only via the redaction layer.

pub use crate::config::LogFormat;
use serde::{Deserialize, Serialize};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::time::SystemTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Global flag to track if logging has been initialized
static LOGGING_INITIALIZED: OnceLock<bool> = OnceLock::new();

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Log level filter (trace, debug, info, warn, error)
    /// Can be overridden by RUST_LOG environment variable
    pub level: String,

    /// Output format (pretty or json)
    pub format: LogFormat,

    /// Optional path to log file
    /// When set, logs are written to this file (useful for E2E/diagnostic bundles)
    pub file: Option<PathBuf>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Pretty,
            file: None,
        }
    }
}

/// Error type for logging initialization
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("logging already initialized")]
    AlreadyInitialized,

    #[error("invalid log level: {0}")]
    InvalidLevel(String),

    #[error("failed to create log file: {0}")]
    FileCreate(#[from] io::Error),

    #[error("failed to set global subscriber: {0}")]
    SetSubscriber(#[from] tracing::subscriber::SetGlobalDefaultError),
}

fn ensure_parent_dir(path: &std::path::Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let existed = parent.exists();
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            if !existed {
                let permissions = std::fs::Permissions::from_mode(0o700);
                std::fs::set_permissions(parent, permissions)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &std::path::Path, mode: u32) -> io::Result<()> {
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions)
}

/// Initialize the global logging subscriber
///
/// This function should be called once at application startup.
/// Subsequent calls will return `Err(LogError::AlreadyInitialized)`.
///
/// # Environment Override
///
/// The `RUST_LOG` environment variable overrides the configured log level.
/// Example: `RUST_LOG=wa_core=debug,wa=trace`
///
/// # Arguments
///
/// * `config` - Logging configuration
///
/// # Returns
///
/// Returns `Ok(())` on success, or an error if initialization fails.
pub fn init_logging(config: &LogConfig) -> Result<(), LogError> {
    // Check if already initialized
    if LOGGING_INITIALIZED.get().is_some() {
        return Err(LogError::AlreadyInitialized);
    }

    // Build environment filter with fallback to config level
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    // Handle optional file output
    let file_writer = if let Some(path) = &config.file {
        ensure_parent_dir(path)?;
        let existed = path.exists();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        #[cfg(unix)]
        if !existed {
            set_file_permissions(path, 0o600)?;
        }
        Some(file)
    } else {
        None
    };

    // Build the stderr writer.  When the TUI module is compiled, use a
    // TUI-aware writer that suppresses output while the rendering pipeline
    // owns the terminal (one-writer rule).  Without a TUI feature the plain
    // stderr writer is used — zero overhead.
    #[cfg(any(feature = "tui", feature = "ftui"))]
    let stderr_writer = crate::tui::output_gate::TuiAwareWriter;

    // Configure and install subscriber based on format
    match config.format {
        LogFormat::Pretty => {
            let subscriber = tracing_subscriber::registry().with(env_filter).with(
                fmt::layer()
                    .with_writer({
                        #[cfg(any(feature = "tui", feature = "ftui"))]
                        {
                            stderr_writer
                        }
                        #[cfg(not(any(feature = "tui", feature = "ftui")))]
                        {
                            std::io::stderr
                        }
                    })
                    .with_target(true)
                    .with_thread_ids(false)
                    .with_thread_names(false)
                    .with_file(false)
                    .with_line_number(false)
                    .with_span_events(FmtSpan::NONE)
                    .with_ansi(true),
            );

            if let Some(file) = file_writer {
                // Add file layer for pretty format
                let file_layer = fmt::layer()
                    .with_writer(file)
                    .with_target(true)
                    .with_ansi(false);
                tracing::subscriber::set_global_default(subscriber.with(file_layer))?;
            } else {
                tracing::subscriber::set_global_default(subscriber)?;
            }
        }
        LogFormat::Json => {
            let subscriber = tracing_subscriber::registry().with(env_filter).with(
                fmt::layer()
                    .json()
                    .with_timer(SystemTime)
                    .with_writer({
                        #[cfg(any(feature = "tui", feature = "ftui"))]
                        {
                            stderr_writer
                        }
                        #[cfg(not(any(feature = "tui", feature = "ftui")))]
                        {
                            std::io::stderr
                        }
                    })
                    .with_target(true)
                    .with_current_span(true)
                    .with_span_list(false)
                    .flatten_event(true),
            );

            if let Some(file) = file_writer {
                // Add file layer for JSON format
                let file_layer = fmt::layer()
                    .json()
                    .with_writer(file)
                    .with_timer(SystemTime)
                    .with_target(true)
                    .with_current_span(true)
                    .flatten_event(true);
                tracing::subscriber::set_global_default(subscriber.with(file_layer))?;
            } else {
                tracing::subscriber::set_global_default(subscriber)?;
            }
        }
    }

    // Mark as initialized
    let _ = LOGGING_INITIALIZED.set(true);

    tracing::info!(
        log_level = %config.level,
        log_format = %config.format,
        log_file = ?config.file,
        "Logging initialized"
    );

    Ok(())
}

/// Check if logging has been initialized
pub fn is_logging_initialized() -> bool {
    LOGGING_INITIALIZED.get().is_some()
}

/// Create a span with standard correlation fields
///
/// This macro creates a tracing span with the common correlation fields
/// used throughout wa. Use this for consistent context propagation.
///
/// # Example
///
/// ```ignore
/// let span = wa_core::wa_span!(
///     "ingest_pane",
///     pane_id = 42,
///     domain = "local"
/// );
/// let _guard = span.enter();
/// ```
#[macro_export]
macro_rules! wa_span {
    ($name:expr $(, $field:ident = $value:expr)* $(,)?) => {
        tracing::info_span!($name $(, $field = $value)*)
    };
}

/// Log levels that can be used for filtering
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => Self::TRACE,
            LogLevel::Debug => Self::DEBUG,
            LogLevel::Info => Self::INFO,
            LogLevel::Warn => Self::WARN,
            LogLevel::Error => Self::ERROR,
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" | "warning" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            _ => Err(format!(
                "unknown log level: {s}. Expected one of: trace, debug, info, warn, error"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Redactor;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// A mock writer that captures output to a shared buffer for testing
    #[derive(Clone)]
    struct MockLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl MockLogWriter {
        fn new() -> Self {
            Self {
                buffer: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn contents(&self) -> String {
            String::from_utf8(self.buffer.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for MockLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for MockLogWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn log_format_from_str() {
        assert_eq!("pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("Pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("PRETTY".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("JSON".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert!("invalid".parse::<LogFormat>().is_err());
    }

    #[test]
    fn log_format_display() {
        assert_eq!(LogFormat::Pretty.to_string(), "pretty");
        assert_eq!(LogFormat::Json.to_string(), "json");
    }

    #[test]
    fn log_level_from_str() {
        assert_eq!("trace".parse::<LogLevel>().unwrap(), LogLevel::Trace);
        assert_eq!("debug".parse::<LogLevel>().unwrap(), LogLevel::Debug);
        assert_eq!("info".parse::<LogLevel>().unwrap(), LogLevel::Info);
        assert_eq!("warn".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("warning".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("error".parse::<LogLevel>().unwrap(), LogLevel::Error);
        assert!("invalid".parse::<LogLevel>().is_err());
    }

    #[test]
    fn log_config_default() {
        let config = LogConfig::default();
        assert_eq!(config.level, "info");
        assert_eq!(config.format, LogFormat::Pretty);
        assert!(config.file.is_none());
    }

    #[test]
    fn log_config_serde_roundtrip() {
        let config = LogConfig {
            level: "debug".to_string(),
            format: LogFormat::Json,
            file: Some(PathBuf::from("/tmp/test.log")),
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: LogConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.level, config.level);
        assert_eq!(parsed.format, config.format);
        assert_eq!(parsed.file, config.file);
    }

    #[test]
    fn json_logs_are_parseable() {
        let writer = MockLogWriter::new();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(
                fmt::layer()
                    .json()
                    .with_timer(SystemTime)
                    .with_target(true)
                    .with_current_span(true)
                    .flatten_event(true)
                    .with_writer(writer.clone()),
            );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(workspace = "test", pane_id = 42u64, "hello");
        });

        let output = writer.contents();
        let line = output.lines().find(|line| !line.trim().is_empty()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();

        assert!(parsed.get("timestamp").is_some());
        assert_eq!(
            parsed.get("workspace").and_then(|v| v.as_str()),
            Some("test")
        );
        assert_eq!(
            parsed.get("pane_id").and_then(serde_json::Value::as_u64),
            Some(42)
        );
    }

    #[test]
    fn redacted_fields_do_not_include_secrets() {
        let writer = MockLogWriter::new();
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(
                fmt::layer()
                    .json()
                    .with_timer(SystemTime)
                    .with_target(true)
                    .flatten_event(true)
                    .with_writer(writer.clone()),
            );

        let redactor = Redactor::new();
        let secret = "API key sk-abc123456789012345678901234567890123456789012345678901";
        let redacted = redactor.redact(secret);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(payload = %redacted, "redacted log");
        });

        let output = writer.contents();
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("sk-abc"));
    }

    // Note: We can't easily test init_logging in unit tests because:
    // 1. It sets a global subscriber
    // 2. Tests run in parallel
    // 3. Once set, it can't be changed
    //
    // Integration tests should verify:
    // - Logger initialization doesn't panic
    // - JSON log lines are valid JSON
    // - Required correlation fields appear on key spans
}
