//! Crash recovery and health monitoring.
//!
//! This module provides structures for runtime health monitoring and
//! crash recovery.  The [`install_panic_hook`] function registers a custom
//! panic hook that writes a bounded, redacted crash bundle to disk when
//! the process panics.
//!
//! # Crash Bundle Layout
//!
//! ```text
//! .wa/crash/wa_crash_YYYYMMDD_HHMMSS/
//! ├── manifest.json        # Bundle metadata (version, timestamp, schema)
//! ├── crash_report.json    # Panic details (message, location, backtrace)
//! └── health_snapshot.json # Last known HealthSnapshot (if available)
//! ```

use std::backtrace::Backtrace;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::policy::Redactor;

/// Global health snapshot for crash reporting
static GLOBAL_HEALTH: OnceLock<RwLock<Option<HealthSnapshot>>> = OnceLock::new();

/// Maximum backtrace string length included in crash bundles (64 KiB).
const MAX_BACKTRACE_LEN: usize = 64 * 1024;

/// Maximum crash bundle size in bytes (1 MiB) — a privacy/size budget.
const MAX_BUNDLE_SIZE: usize = 1024 * 1024;

/// Runtime health snapshot for crash reporting.
///
/// This is periodically updated by the observation runtime and included
/// in crash reports to aid debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    /// Timestamp when snapshot was taken (epoch ms)
    pub timestamp: u64,
    /// Number of panes being observed
    pub observed_panes: usize,
    /// Current capture queue depth
    pub capture_queue_depth: usize,
    /// Current write queue depth
    pub write_queue_depth: usize,
    /// Last sequence number per pane
    pub last_seq_by_pane: Vec<(u64, i64)>,
    /// Any warnings detected
    pub warnings: Vec<String>,
    /// Average ingest lag in milliseconds
    pub ingest_lag_avg_ms: f64,
    /// Maximum ingest lag in milliseconds
    pub ingest_lag_max_ms: u64,
    /// Whether the database is writable
    pub db_writable: bool,
    /// Last database write timestamp (epoch ms)
    pub db_last_write_at: Option<u64>,

    /// Active runtime pane priority overrides (operator-set).
    #[serde(default)]
    pub pane_priority_overrides: Vec<PanePriorityOverrideSnapshot>,
}

/// Health snapshot view of a runtime pane priority override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanePriorityOverrideSnapshot {
    /// Pane ID
    pub pane_id: u64,
    /// Priority value (lower = higher priority)
    pub priority: u32,
    /// Expiration timestamp (epoch ms), if any
    pub expires_at: Option<u64>,
}

impl HealthSnapshot {
    /// Update the global health snapshot.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_HEALTH.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(snapshot);
        }
    }

    /// Get the current global health snapshot.
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_HEALTH.get_or_init(|| RwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }
}

/// Summary of a graceful shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownSummary {
    /// Total runtime in seconds
    pub elapsed_secs: u64,
    /// Final capture queue depth
    pub final_capture_queue: usize,
    /// Final write queue depth
    pub final_write_queue: usize,
    /// Total segments persisted
    pub segments_persisted: u64,
    /// Total events recorded
    pub events_recorded: u64,
    /// Last sequence number per pane
    pub last_seq_by_pane: Vec<(u64, i64)>,
    /// Whether shutdown was clean (no errors)
    pub clean: bool,
    /// Any warnings during shutdown
    pub warnings: Vec<String>,
}

/// Configuration for crash handling.
#[derive(Debug, Clone)]
pub struct CrashConfig {
    /// Path to write crash reports
    pub crash_dir: Option<PathBuf>,
    /// Whether to include stack traces
    pub include_backtrace: bool,
}

/// Crash report data written to crash_report.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashReport {
    /// Panic message (redacted)
    pub message: String,
    /// Source location if available (file:line:col)
    pub location: Option<String>,
    /// Backtrace (truncated to MAX_BACKTRACE_LEN)
    pub backtrace: Option<String>,
    /// Epoch seconds when the crash occurred
    pub timestamp: u64,
    /// Process ID
    pub pid: u32,
    /// Thread name if available
    pub thread_name: Option<String>,
}

/// Manifest written to manifest.json in each crash bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashManifest {
    /// wa version at crash time
    pub wa_version: String,
    /// ISO-8601 timestamp
    pub created_at: String,
    /// Files included in the bundle
    pub files: Vec<String>,
    /// Whether health snapshot was available
    pub has_health_snapshot: bool,
    /// Total bundle size in bytes
    pub bundle_size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Panic hook
// ---------------------------------------------------------------------------

/// Install the panic hook for crash reporting.
///
/// Replaces the default panic hook with one that writes a crash bundle
/// containing the panic message, backtrace, and last known health snapshot.
/// The bundle is written atomically (temp dir + rename) and all text
/// content is passed through the [`Redactor`] before being persisted.
///
/// If `crash_dir` is `None` the hook still prints the panic to stderr but
/// does not write any files.
pub fn install_panic_hook(config: &CrashConfig) {
    let include_backtrace = config.include_backtrace;
    let crash_dir = config.crash_dir.clone();

    std::panic::set_hook(Box::new(move |info| {
        // Capture backtrace early (before allocations that might fail)
        let bt = if include_backtrace {
            Some(Backtrace::force_capture())
        } else {
            None
        };

        // Extract panic message
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".to_string()
        };

        // Extract location
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()));

        // Always print to stderr (the original hook behavior)
        if let Some(ref loc) = location {
            eprintln!("wa: panic at {loc}: {message}");
        } else {
            eprintln!("wa: panic: {message}");
        }

        // Write crash bundle if crash_dir is configured
        if let Some(ref dir) = crash_dir {
            let report = CrashReport {
                message,
                location,
                backtrace: bt.map(|b| {
                    let s = b.to_string();
                    if s.len() > MAX_BACKTRACE_LEN {
                        let mut truncated = s[..MAX_BACKTRACE_LEN].to_string();
                        truncated.push_str("\n... [truncated]");
                        truncated
                    } else {
                        s
                    }
                }),
                timestamp: epoch_secs(),
                pid: std::process::id(),
                thread_name: std::thread::current().name().map(String::from),
            };

            let health = HealthSnapshot::get_global();

            match write_crash_bundle(dir, &report, health.as_ref()) {
                Ok(path) => eprintln!("wa: crash bundle written to {}", path.display()),
                Err(e) => eprintln!("wa: failed to write crash bundle: {e}"),
            }
        }
    }));
}

// ---------------------------------------------------------------------------
// Bundle writer
// ---------------------------------------------------------------------------

/// Write a crash bundle to `crash_dir`, returning the bundle directory path.
///
/// The bundle is written atomically: files go into a temporary directory
/// first, then the directory is renamed into place.  All text content is
/// redacted before writing.
pub fn write_crash_bundle(
    crash_dir: &Path,
    report: &CrashReport,
    health: Option<&HealthSnapshot>,
) -> std::io::Result<PathBuf> {
    let redactor = Redactor::new();

    // Build timestamped bundle directory name
    let ts_str = format_timestamp(report.timestamp);
    let bundle_name = format!("wa_crash_{ts_str}");
    let bundle_dir = crash_dir.join(&bundle_name);

    // Use a temp directory alongside the final location for atomic rename
    let tmp_name = format!(".{bundle_name}.tmp");
    let tmp_dir = crash_dir.join(&tmp_name);

    // Clean up any leftover temp directory
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }

    fs::create_dir_all(&tmp_dir)?;

    let mut files = Vec::new();
    let mut total_size: u64 = 0;

    // 1. Write crash_report.json (redacted)
    {
        let redacted_report = CrashReport {
            message: redactor.redact(&report.message),
            location: report.location.clone(),
            backtrace: report.backtrace.as_ref().map(|bt| redactor.redact(bt)),
            timestamp: report.timestamp,
            pid: report.pid,
            thread_name: report.thread_name.clone(),
        };
        let json = serde_json::to_string_pretty(&redacted_report).map_err(std::io::Error::other)?;
        let bytes = json.as_bytes();
        total_size += bytes.len() as u64;
        if total_size <= MAX_BUNDLE_SIZE as u64 {
            write_file_sync(&tmp_dir.join("crash_report.json"), bytes)?;
            files.push("crash_report.json".to_string());
        }
    }

    // 2. Write health_snapshot.json (if available)
    let has_health = if let Some(snap) = health {
        let json = serde_json::to_string_pretty(snap).map_err(std::io::Error::other)?;
        let bytes = json.as_bytes();
        total_size += bytes.len() as u64;
        if total_size <= MAX_BUNDLE_SIZE as u64 {
            write_file_sync(&tmp_dir.join("health_snapshot.json"), bytes)?;
            files.push("health_snapshot.json".to_string());
            true
        } else {
            false
        }
    } else {
        false
    };

    // 3. Write manifest.json
    {
        let manifest = CrashManifest {
            wa_version: crate::VERSION.to_string(),
            created_at: format_iso8601(report.timestamp),
            files: files.clone(),
            has_health_snapshot: has_health,
            bundle_size_bytes: total_size,
        };
        let json = serde_json::to_string_pretty(&manifest).map_err(std::io::Error::other)?;
        write_file_sync(&tmp_dir.join("manifest.json"), json.as_bytes())?;
        // manifest doesn't count toward the privacy budget
    }

    // Atomic rename: tmp → final
    // If bundle_dir already exists (rapid double-panic), append a counter
    let final_dir = if bundle_dir.exists() {
        let mut counter = 1u32;
        loop {
            let candidate = crash_dir.join(format!("{bundle_name}_{counter}"));
            if !candidate.exists() {
                break candidate;
            }
            counter += 1;
            if counter > 100 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "too many crash bundles with same timestamp",
                ));
            }
        }
    } else {
        bundle_dir
    };

    fs::rename(&tmp_dir, &final_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&final_dir, fs::Permissions::from_mode(0o700));
    }

    Ok(final_dir)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_file_sync(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Format epoch seconds as `YYYYMMDD_HHMMSS`.
fn format_timestamp(epoch_secs: u64) -> String {
    let secs = epoch_secs;
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}{month:02}{day:02}_{hours:02}{minutes:02}{seconds:02}")
}

/// Format epoch seconds as ISO-8601.
fn format_iso8601(epoch_secs: u64) -> String {
    let secs = epoch_secs;
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

// ---------------------------------------------------------------------------
// Crash bundle listing
// ---------------------------------------------------------------------------

/// Summary of a discovered crash bundle on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashBundleSummary {
    /// Path to the crash bundle directory
    pub path: PathBuf,
    /// Parsed manifest (if readable)
    pub manifest: Option<CrashManifest>,
    /// Parsed crash report (if readable)
    pub report: Option<CrashReport>,
}

/// List crash bundles in `crash_dir`, sorted newest first.
///
/// Scans for directories matching `wa_crash_*`, parses their manifests
/// and crash reports, and returns up to `limit` results.  Invalid or
/// unreadable bundles are silently skipped.
#[must_use]
pub fn list_crash_bundles(crash_dir: &Path, limit: usize) -> Vec<CrashBundleSummary> {
    let Ok(entries) = fs::read_dir(crash_dir) else {
        return Vec::new();
    };

    let mut bundles: Vec<CrashBundleSummary> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_ok_and(|ft| ft.is_dir())
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("wa_crash_"))
        })
        .filter_map(|e| {
            let path = e.path();
            let manifest = fs::read_to_string(path.join("manifest.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<CrashManifest>(&s).ok());
            let report = fs::read_to_string(path.join("crash_report.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<CrashReport>(&s).ok());

            // Skip bundles without at least a manifest or report
            if manifest.is_none() && report.is_none() {
                return None;
            }

            Some(CrashBundleSummary {
                path,
                manifest,
                report,
            })
        })
        .collect();

    // Sort newest first by timestamp (from report or manifest)
    bundles.sort_by(|a, b| {
        let ts_a = a.report.as_ref().map_or(0, |r| r.timestamp);
        let ts_b = b.report.as_ref().map_or(0, |r| r.timestamp);
        ts_b.cmp(&ts_a)
    });

    bundles.truncate(limit);
    bundles
}

/// Get the most recent crash bundle, if any.
#[must_use]
pub fn latest_crash_bundle(crash_dir: &Path) -> Option<CrashBundleSummary> {
    list_crash_bundles(crash_dir, 1).into_iter().next()
}

// ---------------------------------------------------------------------------
// Incident bundle export
// ---------------------------------------------------------------------------

/// Kind of incident to export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    Crash,
    Manual,
}

impl std::fmt::Display for IncidentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crash => write!(f, "crash"),
            Self::Manual => write!(f, "manual"),
        }
    }
}

/// Result of exporting an incident bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentBundleResult {
    /// Path to the produced bundle directory
    pub path: PathBuf,
    /// Kind of incident
    pub kind: IncidentKind,
    /// Files included in the bundle
    pub files: Vec<String>,
    /// Total size in bytes
    pub total_size_bytes: u64,
    /// wa version
    pub wa_version: String,
    /// Timestamp of export
    pub exported_at: String,
}

/// Export an incident bundle to `out_dir`.
///
/// Gathers the most recent crash bundle (if `kind` is `Crash`), configuration
/// summary, and a redacted manifest into a self-contained directory.
///
/// Returns the path and metadata for the exported bundle.
pub fn export_incident_bundle(
    crash_dir: &Path,
    config_path: Option<&Path>,
    out_dir: &Path,
    kind: IncidentKind,
) -> std::io::Result<IncidentBundleResult> {
    let ts = epoch_secs();
    let ts_str = format_timestamp(ts);
    let bundle_name = format!("wa_incident_{kind}_{ts_str}");
    let bundle_dir = out_dir.join(&bundle_name);

    fs::create_dir_all(&bundle_dir)?;

    let redactor = Redactor::new();
    let mut files = Vec::new();
    let mut total_size: u64 = 0;

    // 1. Include latest crash bundle contents (if crash kind)
    if kind == IncidentKind::Crash {
        if let Some(crash) = latest_crash_bundle(crash_dir) {
            // Copy crash report
            if let Some(ref report) = crash.report {
                let json = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
                let redacted = redactor.redact(&json);
                let bytes = redacted.as_bytes();
                total_size += bytes.len() as u64;
                write_file_sync(&bundle_dir.join("crash_report.json"), bytes)?;
                files.push("crash_report.json".to_string());
            }

            // Copy crash manifest
            if let Some(ref manifest) = crash.manifest {
                let json = serde_json::to_string_pretty(manifest).map_err(std::io::Error::other)?;
                let bytes = json.as_bytes();
                total_size += bytes.len() as u64;
                write_file_sync(&bundle_dir.join("crash_manifest.json"), bytes)?;
                files.push("crash_manifest.json".to_string());
            }

            // Copy health snapshot if present in crash bundle
            let health_path = crash.path.join("health_snapshot.json");
            if health_path.exists() {
                if let Ok(contents) = fs::read_to_string(&health_path) {
                    let redacted = redactor.redact(&contents);
                    let bytes = redacted.as_bytes();
                    total_size += bytes.len() as u64;
                    write_file_sync(&bundle_dir.join("health_snapshot.json"), bytes)?;
                    files.push("health_snapshot.json".to_string());
                }
            }
        }
    }

    // 2. Include config summary (redacted) if available
    if let Some(cfg_path) = config_path {
        if cfg_path.exists() {
            if let Ok(contents) = fs::read_to_string(cfg_path) {
                let redacted = redactor.redact(&contents);
                let bytes = redacted.as_bytes();
                // Limit config to 64 KiB
                if bytes.len() <= 64 * 1024 {
                    total_size += bytes.len() as u64;
                    write_file_sync(&bundle_dir.join("config_summary.toml"), bytes)?;
                    files.push("config_summary.toml".to_string());
                }
            }
        }
    }

    // 3. Write incident manifest
    let result = IncidentBundleResult {
        path: bundle_dir.clone(),
        kind,
        files: files.clone(),
        total_size_bytes: total_size,
        wa_version: crate::VERSION.to_string(),
        exported_at: format_iso8601(ts),
    };

    let manifest_json = serde_json::to_string_pretty(&result).map_err(std::io::Error::other)?;
    write_file_sync(
        &bundle_dir.join("incident_manifest.json"),
        manifest_json.as_bytes(),
    )?;

    Ok(result)
}

// ---------------------------------------------------------------------------
// Enhanced incident bundle collector
// ---------------------------------------------------------------------------

/// Summary of what was redacted during bundle collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionReport {
    /// Total number of redaction replacements across all files
    pub total_redactions: usize,
    /// Per-file redaction counts
    pub per_file: Vec<FileRedactionEntry>,
}

/// Redaction details for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRedactionEntry {
    /// File name within the bundle
    pub file: String,
    /// Number of secrets redacted in this file
    pub count: usize,
}

/// Database metadata collected for the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbMetadata {
    /// Schema version (from wa_meta table)
    pub schema_version: Option<i64>,
    /// Database file size in bytes
    pub db_size_bytes: Option<u64>,
    /// SQLite journal mode (e.g., "wal")
    pub journal_mode: Option<String>,
    /// Number of events in the database
    pub event_count: Option<i64>,
    /// Number of segments in the database
    pub segment_count: Option<i64>,
}

/// Options for the enhanced incident bundle collector.
pub struct IncidentBundleOptions<'a> {
    /// Crash directory path
    pub crash_dir: &'a Path,
    /// Optional config file path
    pub config_path: Option<&'a Path>,
    /// Output directory
    pub out_dir: &'a Path,
    /// Kind of incident
    pub kind: IncidentKind,
    /// Optional path to the database file
    pub db_path: Option<&'a Path>,
    /// Maximum number of recent events to include
    pub max_events: usize,
}

/// Collect a comprehensive incident bundle with DB metadata, recent events,
/// and a redaction report.
///
/// This is an enhanced version of [`export_incident_bundle`] that additionally
/// gathers storage metadata and recent event summaries.
pub fn collect_incident_bundle(
    opts: &IncidentBundleOptions<'_>,
) -> std::io::Result<IncidentBundleResult> {
    let ts = epoch_secs();
    let ts_str = format_timestamp(ts);
    let bundle_name = format!("wa_incident_{kind}_{ts_str}", kind = opts.kind);
    let bundle_dir = opts.out_dir.join(&bundle_name);

    fs::create_dir_all(&bundle_dir)?;

    let redactor = Redactor::with_debug_markers();
    let mut files = Vec::new();
    let mut total_size: u64 = 0;
    let mut redaction_entries: Vec<FileRedactionEntry> = Vec::new();

    // 1. Include latest crash bundle contents (if crash kind)
    if opts.kind == IncidentKind::Crash {
        if let Some(crash) = latest_crash_bundle(opts.crash_dir) {
            if let Some(ref report) = crash.report {
                let json = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
                write_redacted_file(
                    "crash_report.json",
                    &json,
                    &bundle_dir,
                    &redactor,
                    &mut files,
                    &mut total_size,
                    &mut redaction_entries,
                )?;
            }

            if let Some(ref manifest) = crash.manifest {
                let json = serde_json::to_string_pretty(manifest).map_err(std::io::Error::other)?;
                write_redacted_file(
                    "crash_manifest.json",
                    &json,
                    &bundle_dir,
                    &redactor,
                    &mut files,
                    &mut total_size,
                    &mut redaction_entries,
                )?;
            }

            let health_path = crash.path.join("health_snapshot.json");
            if health_path.exists() {
                if let Ok(contents) = fs::read_to_string(&health_path) {
                    write_redacted_file(
                        "health_snapshot.json",
                        &contents,
                        &bundle_dir,
                        &redactor,
                        &mut files,
                        &mut total_size,
                        &mut redaction_entries,
                    )?;
                }
            }
        }
    }

    // 2. Include config summary (redacted, max 64 KiB)
    if let Some(cfg_path) = opts.config_path {
        if cfg_path.exists() {
            if let Ok(contents) = fs::read_to_string(cfg_path) {
                let truncated = if contents.len() > 64 * 1024 {
                    format!("{}\n... [truncated at 64 KiB]", &contents[..64 * 1024])
                } else {
                    contents
                };
                write_redacted_file(
                    "config_summary.toml",
                    &truncated,
                    &bundle_dir,
                    &redactor,
                    &mut files,
                    &mut total_size,
                    &mut redaction_entries,
                )?;
            }
        }
    }

    // 3. Gather DB metadata + recent events
    if let Some(db_path) = opts.db_path {
        if db_path.exists() {
            let db_meta = collect_db_metadata(db_path);
            let meta_json =
                serde_json::to_string_pretty(&db_meta).map_err(std::io::Error::other)?;
            write_redacted_file(
                "db_metadata.json",
                &meta_json,
                &bundle_dir,
                &redactor,
                &mut files,
                &mut total_size,
                &mut redaction_entries,
            )?;

            // Recent events (sanitized summaries)
            if opts.max_events > 0 {
                if let Some(events_json) = collect_recent_events_summary(db_path, opts.max_events) {
                    write_redacted_file(
                        "recent_events.json",
                        &events_json,
                        &bundle_dir,
                        &redactor,
                        &mut files,
                        &mut total_size,
                        &mut redaction_entries,
                    )?;
                }
            }
        }
    }

    // 4. Write redaction report
    let total_redactions: usize = redaction_entries.iter().map(|e| e.count).sum();
    let redaction_report = RedactionReport {
        total_redactions,
        per_file: redaction_entries,
    };
    let report_json =
        serde_json::to_string_pretty(&redaction_report).map_err(std::io::Error::other)?;
    let report_bytes = report_json.as_bytes();
    total_size += report_bytes.len() as u64;
    write_file_sync(&bundle_dir.join("redaction_report.json"), report_bytes)?;
    files.push("redaction_report.json".to_string());

    // 5. Write incident manifest
    let result = IncidentBundleResult {
        path: bundle_dir.clone(),
        kind: opts.kind,
        files: files.clone(),
        total_size_bytes: total_size,
        wa_version: crate::VERSION.to_string(),
        exported_at: format_iso8601(ts),
    };

    let manifest_json = serde_json::to_string_pretty(&result).map_err(std::io::Error::other)?;
    write_file_sync(
        &bundle_dir.join("incident_manifest.json"),
        manifest_json.as_bytes(),
    )?;

    Ok(result)
}

/// Collect database metadata from a SQLite database file.
fn collect_db_metadata(db_path: &Path) -> DbMetadata {
    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => {
            return DbMetadata {
                schema_version: None,
                db_size_bytes: fs::metadata(db_path).ok().map(|m| m.len()),
                journal_mode: None,
                event_count: None,
                segment_count: None,
            };
        }
    };

    let schema_version = conn
        .query_row(
            "SELECT value FROM wa_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse::<i64>().ok());

    let journal_mode = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        .ok();

    let event_count = conn
        .query_row("SELECT count(*) FROM events", [], |row| {
            row.get::<_, i64>(0)
        })
        .ok();

    let segment_count = conn
        .query_row("SELECT count(*) FROM segments", [], |row| {
            row.get::<_, i64>(0)
        })
        .ok();

    DbMetadata {
        schema_version,
        db_size_bytes: fs::metadata(db_path).ok().map(|m| m.len()),
        journal_mode,
        event_count,
        segment_count,
    }
}

/// Collect summaries of recent events from the database (redacted by caller).
fn collect_recent_events_summary(db_path: &Path, max_events: usize) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;

    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, rule_id, event_type, severity, detected_at, \
             COALESCE(matched_text, '') as matched_text \
             FROM events ORDER BY detected_at DESC LIMIT ?1",
        )
        .ok()?;

    let rows = stmt
        .query_map([max_events as i64], |row| {
            let id: i64 = row.get(0)?;
            let pane_id: i64 = row.get(1)?;
            let rule_id: String = row.get(2)?;
            let event_type: String = row.get(3)?;
            let severity: String = row.get(4)?;
            let detected_at: i64 = row.get(5)?;
            let text: String = row.get(6)?;
            let preview: String = text.chars().take(200).collect();
            Ok(serde_json::json!({
                "id": id,
                "pane_id": pane_id,
                "rule_id": rule_id,
                "event_type": event_type,
                "severity": severity,
                "detected_at": detected_at,
                "matched_text_preview": preview,
            }))
        })
        .ok()?;

    let events: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
    serde_json::to_string_pretty(&events).ok()
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Mode for deterministic bundle replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    /// Re-run policy evaluation on recorded decision context.
    Policy,
    /// Re-run rule/pattern engine on recorded segments.
    Rules,
}

impl std::fmt::Display for ReplayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayMode::Policy => write!(f, "policy"),
            ReplayMode::Rules => write!(f, "rules"),
        }
    }
}

/// A single check result within a replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayCheck {
    /// Name of the check.
    pub name: String,
    /// Whether this check passed.
    pub passed: bool,
    /// Optional detail about the result.
    pub detail: Option<String>,
}

/// Result of replaying an incident bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayResult {
    /// The replay mode used.
    pub mode: ReplayMode,
    /// Overall status: "pass", "fail", or "incomplete".
    pub status: String,
    /// Individual check results.
    pub checks: Vec<ReplayCheck>,
    /// Warnings (non-fatal issues).
    pub warnings: Vec<String>,
}

/// Replay an incident bundle for deterministic analysis.
///
/// Loads the bundle manifest and runs checks based on the selected mode:
/// - `Policy`: validates that crash/incident data is internally consistent
///   and that redaction was applied correctly.
/// - `Rules`: validates that event data in the bundle matches expected patterns
///   and that no secrets leaked through redaction.
pub fn replay_incident_bundle(
    bundle_path: &Path,
    mode: ReplayMode,
) -> std::io::Result<ReplayResult> {
    if !bundle_path.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Bundle directory not found: {}", bundle_path.display()),
        ));
    }

    let mut checks = Vec::new();
    let mut warnings = Vec::new();

    // Check 1: manifest exists and is valid JSON
    let manifest_path = bundle_path.join("incident_manifest.json");
    let manifest_ok = if manifest_path.exists() {
        match fs::read_to_string(&manifest_path) {
            Ok(content) => match serde_json::from_str::<IncidentBundleResult>(&content) {
                Ok(_) => {
                    checks.push(ReplayCheck {
                        name: "manifest_valid".to_string(),
                        passed: true,
                        detail: Some("incident_manifest.json is valid".to_string()),
                    });
                    true
                }
                Err(e) => {
                    checks.push(ReplayCheck {
                        name: "manifest_valid".to_string(),
                        passed: false,
                        detail: Some(format!("Invalid manifest JSON: {e}")),
                    });
                    false
                }
            },
            Err(e) => {
                checks.push(ReplayCheck {
                    name: "manifest_valid".to_string(),
                    passed: false,
                    detail: Some(format!("Cannot read manifest: {e}")),
                });
                false
            }
        }
    } else {
        checks.push(ReplayCheck {
            name: "manifest_valid".to_string(),
            passed: false,
            detail: Some("incident_manifest.json not found".to_string()),
        });
        false
    };

    // Check 2: redaction report exists and shows no leaks
    let redaction_path = bundle_path.join("redaction_report.json");
    if redaction_path.exists() {
        if let Ok(content) = fs::read_to_string(&redaction_path) {
            match serde_json::from_str::<RedactionReport>(&content) {
                Ok(report) => {
                    checks.push(ReplayCheck {
                        name: "redaction_report_valid".to_string(),
                        passed: true,
                        detail: Some(format!(
                            "{} total redactions across {} files",
                            report.total_redactions,
                            report.per_file.len()
                        )),
                    });
                }
                Err(e) => {
                    checks.push(ReplayCheck {
                        name: "redaction_report_valid".to_string(),
                        passed: false,
                        detail: Some(format!("Invalid redaction report: {e}")),
                    });
                }
            }
        }
    } else {
        warnings.push("No redaction_report.json found".to_string());
    }

    // Check 3: verify no secrets remain in any bundle file
    let redactor = Redactor::new();
    let mut leak_found = false;
    if let Ok(entries) = fs::read_dir(bundle_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|ext| ext == "json" || ext == "toml")
            {
                if let Ok(content) = fs::read_to_string(&path) {
                    let detections = redactor.detect(&content);
                    if !detections.is_empty() {
                        leak_found = true;
                        let fname = path.file_name().unwrap_or_default().to_string_lossy();
                        checks.push(ReplayCheck {
                            name: format!("no_secrets_{fname}"),
                            passed: false,
                            detail: Some(format!(
                                "{} potential secret(s) detected in {fname}",
                                detections.len()
                            )),
                        });
                    }
                }
            }
        }
    }
    if !leak_found {
        checks.push(ReplayCheck {
            name: "no_secrets_leaked".to_string(),
            passed: true,
            detail: Some("No secrets detected in bundle files".to_string()),
        });
    }

    // Mode-specific checks
    match mode {
        ReplayMode::Policy => {
            // Check 4: if crash_report exists, validate structure
            let crash_report_path = bundle_path.join("crash_report.json");
            if crash_report_path.exists() {
                if let Ok(content) = fs::read_to_string(&crash_report_path) {
                    match serde_json::from_str::<CrashReport>(&content) {
                        Ok(report) => {
                            checks.push(ReplayCheck {
                                name: "crash_report_valid".to_string(),
                                passed: true,
                                detail: Some(format!(
                                    "Crash at {} (pid {})",
                                    report.timestamp, report.pid
                                )),
                            });
                        }
                        Err(e) => {
                            checks.push(ReplayCheck {
                                name: "crash_report_valid".to_string(),
                                passed: false,
                                detail: Some(format!("Invalid crash report: {e}")),
                            });
                        }
                    }
                }
            }

            // Check 5: if db_metadata exists, validate schema version
            let db_meta_path = bundle_path.join("db_metadata.json");
            if db_meta_path.exists() {
                if let Ok(content) = fs::read_to_string(&db_meta_path) {
                    match serde_json::from_str::<DbMetadata>(&content) {
                        Ok(meta) => {
                            let sv = meta
                                .schema_version
                                .map_or_else(|| "unknown".to_string(), |v| v.to_string());
                            let ec = meta
                                .event_count
                                .map_or_else(|| "unknown".to_string(), |v| v.to_string());
                            let sc = meta
                                .segment_count
                                .map_or_else(|| "unknown".to_string(), |v| v.to_string());
                            let detail =
                                format!("schema_version={sv}, events={ec}, segments={sc}",);
                            checks.push(ReplayCheck {
                                name: "db_metadata_valid".to_string(),
                                passed: true,
                                detail: Some(detail),
                            });
                        }
                        Err(e) => {
                            checks.push(ReplayCheck {
                                name: "db_metadata_valid".to_string(),
                                passed: false,
                                detail: Some(format!("Invalid db metadata: {e}")),
                            });
                        }
                    }
                }
            }
        }

        ReplayMode::Rules => {
            // Check 4: if recent_events exists, validate event structure
            let events_path = bundle_path.join("recent_events.json");
            if events_path.exists() {
                if let Ok(content) = fs::read_to_string(&events_path) {
                    match serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                        Ok(events) => {
                            let valid_count = events
                                .iter()
                                .filter(|e| {
                                    e.get("rule_id").is_some()
                                        && e.get("event_type").is_some()
                                        && e.get("severity").is_some()
                                })
                                .count();
                            checks.push(ReplayCheck {
                                name: "events_structure_valid".to_string(),
                                passed: valid_count == events.len(),
                                detail: Some(format!(
                                    "{valid_count}/{} events have required fields",
                                    events.len()
                                )),
                            });

                            // Check that matched_text_preview is bounded
                            let oversized = events
                                .iter()
                                .filter(|e| {
                                    e.get("matched_text_preview")
                                        .and_then(|v| v.as_str())
                                        .is_some_and(|s| s.len() > 200)
                                })
                                .count();
                            checks.push(ReplayCheck {
                                name: "events_text_bounded".to_string(),
                                passed: oversized == 0,
                                detail: Some(if oversized == 0 {
                                    "All matched_text_preview values are bounded".to_string()
                                } else {
                                    format!("{oversized} events have oversized text previews")
                                }),
                            });
                        }
                        Err(e) => {
                            checks.push(ReplayCheck {
                                name: "events_structure_valid".to_string(),
                                passed: false,
                                detail: Some(format!("Invalid events JSON: {e}")),
                            });
                        }
                    }
                }
            } else {
                warnings.push("No recent_events.json in bundle".to_string());
            }
        }
    }

    // File completeness check (if manifest is valid)
    if manifest_ok {
        if let Ok(content) = fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<IncidentBundleResult>(&content) {
                let missing: Vec<&str> = manifest
                    .files
                    .iter()
                    .filter(|f| !bundle_path.join(f).exists())
                    .map(|f| f.as_str())
                    .collect();
                checks.push(ReplayCheck {
                    name: "files_complete".to_string(),
                    passed: missing.is_empty(),
                    detail: Some(if missing.is_empty() {
                        format!("All {} listed files present", manifest.files.len())
                    } else {
                        format!("Missing files: {}", missing.join(", "))
                    }),
                });
            }
        }
    }

    let all_passed = checks.iter().all(|c| c.passed);
    let status = if all_passed {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    Ok(ReplayResult {
        mode,
        status,
        checks,
        warnings,
    })
}

/// Write a file to the bundle directory, redacting secrets and tracking metadata.
fn write_redacted_file(
    name: &str,
    content: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    let before_count = redactor.detect(content).len();
    let redacted = redactor.redact(content);
    let bytes = redacted.as_bytes();
    *total_size += bytes.len() as u64;
    write_file_sync(&bundle_dir.join(name), bytes)?;
    files.push(name.to_string());
    if before_count > 0 {
        redaction_entries.push(FileRedactionEntry {
            file: name.to_string(),
            count: before_count,
        });
    }
    Ok(())
}

/// Convert days since epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar conversion (Euclidean affine)
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_snapshot() -> HealthSnapshot {
        HealthSnapshot {
            timestamp: 1_234_567_890,
            observed_panes: 5,
            capture_queue_depth: 10,
            write_queue_depth: 5,
            last_seq_by_pane: vec![(1, 100), (2, 200)],
            warnings: vec!["test warning".to_string()],
            ingest_lag_avg_ms: 15.5,
            ingest_lag_max_ms: 50,
            db_writable: true,
            db_last_write_at: Some(1_234_567_800),
            pane_priority_overrides: vec![],
        }
    }

    fn test_report() -> CrashReport {
        CrashReport {
            message: "assertion failed".to_string(),
            location: Some("src/main.rs:42:5".to_string()),
            backtrace: Some("   0: std::backtrace\n   1: my_func".to_string()),
            timestamp: 1_700_000_000,
            pid: 12345,
            thread_name: Some("main".to_string()),
        }
    }

    #[test]
    fn health_snapshot_serialization() {
        let snapshot = test_snapshot();

        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: HealthSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.timestamp, snapshot.timestamp);
        assert_eq!(parsed.observed_panes, snapshot.observed_panes);
        assert!((parsed.ingest_lag_avg_ms - snapshot.ingest_lag_avg_ms).abs() < f64::EPSILON);
    }

    #[test]
    fn shutdown_summary_serialization() {
        let summary = ShutdownSummary {
            elapsed_secs: 3600,
            final_capture_queue: 0,
            final_write_queue: 0,
            segments_persisted: 1000,
            events_recorded: 50,
            last_seq_by_pane: vec![(1, 500)],
            clean: true,
            warnings: vec![],
        };

        let json = serde_json::to_string(&summary).unwrap();
        let parsed: ShutdownSummary = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.elapsed_secs, summary.elapsed_secs);
        assert_eq!(parsed.segments_persisted, summary.segments_persisted);
        assert!(parsed.clean);
    }

    #[test]
    fn global_health_snapshot_update_and_get() {
        let snapshot = HealthSnapshot {
            timestamp: 1000,
            observed_panes: 3,
            capture_queue_depth: 0,
            write_queue_depth: 0,
            last_seq_by_pane: vec![],
            warnings: vec![],
            ingest_lag_avg_ms: 0.0,
            ingest_lag_max_ms: 0,
            db_writable: true,
            db_last_write_at: None,
            pane_priority_overrides: vec![],
        };

        HealthSnapshot::update_global(snapshot);

        let retrieved = HealthSnapshot::get_global();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().timestamp, 1000);
    }

    // -- CrashReport tests --

    #[test]
    fn crash_report_serialization() {
        let report = CrashReport {
            message: "assertion failed".to_string(),
            location: Some("src/main.rs:42:5".to_string()),
            backtrace: Some("   0: std::backtrace\n   1: my_func".to_string()),
            timestamp: 1_700_000_000,
            pid: 12345,
            thread_name: Some("main".to_string()),
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: CrashReport = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.message, "assertion failed");
        assert_eq!(parsed.location.as_deref(), Some("src/main.rs:42:5"));
        assert_eq!(parsed.pid, 12345);
        assert_eq!(parsed.thread_name.as_deref(), Some("main"));
    }

    #[test]
    fn crash_report_without_optional_fields() {
        let report = CrashReport {
            message: "panic".to_string(),
            location: None,
            backtrace: None,
            timestamp: 0,
            pid: 1,
            thread_name: None,
        };

        let json = serde_json::to_string(&report).unwrap();
        let parsed: CrashReport = serde_json::from_str(&json).unwrap();
        assert!(parsed.location.is_none());
        assert!(parsed.backtrace.is_none());
        assert!(parsed.thread_name.is_none());
    }

    // -- CrashManifest tests --

    #[test]
    fn crash_manifest_serialization() {
        let manifest = CrashManifest {
            wa_version: "0.1.0".to_string(),
            created_at: "2026-01-28T12:00:00Z".to_string(),
            files: vec!["crash_report.json".to_string()],
            has_health_snapshot: false,
            bundle_size_bytes: 1024,
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: CrashManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.wa_version, "0.1.0");
        assert_eq!(parsed.files.len(), 1);
        assert!(!parsed.has_health_snapshot);
    }

    // -- write_crash_bundle tests --

    #[test]
    fn write_crash_bundle_creates_directory_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "test panic".to_string(),
            location: Some("test.rs:1:1".to_string()),
            backtrace: Some("frame 0\nframe 1".to_string()),
            timestamp: 1_700_000_000,
            pid: 999,
            thread_name: Some("test".to_string()),
        };

        let health = test_snapshot();
        let bundle_path = write_crash_bundle(&crash_dir, &report, Some(&health)).unwrap();

        assert!(bundle_path.exists());
        assert!(bundle_path.join("manifest.json").exists());
        assert!(bundle_path.join("crash_report.json").exists());
        assert!(bundle_path.join("health_snapshot.json").exists());
    }

    #[test]
    fn write_crash_bundle_without_health_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "no health".to_string(),
            location: None,
            backtrace: None,
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None).unwrap();

        assert!(bundle_path.join("manifest.json").exists());
        assert!(bundle_path.join("crash_report.json").exists());
        assert!(!bundle_path.join("health_snapshot.json").exists());

        // Verify manifest records no health snapshot
        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();
        assert!(!manifest.has_health_snapshot);
        assert_eq!(manifest.files.len(), 1);
    }

    #[test]
    fn write_crash_bundle_manifest_contains_version() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "version check".to_string(),
            location: None,
            backtrace: None,
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None).unwrap();

        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();

        assert_eq!(manifest.wa_version, crate::VERSION);
        assert!(!manifest.created_at.is_empty());
    }

    #[test]
    fn write_crash_bundle_redacts_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "failed with key sk-ant-api03-secret123456789012345678901234567890ABCDEF"
                .to_string(),
            location: None,
            backtrace: Some("token=my_secret_token_1234567890 in frame".to_string()),
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None).unwrap();

        let report_json = fs::read_to_string(bundle_path.join("crash_report.json")).unwrap();
        let parsed: CrashReport = serde_json::from_str(&report_json).unwrap();

        // Secrets should be redacted
        assert!(
            !parsed.message.contains("sk-ant-api03"),
            "API key should be redacted: {}",
            parsed.message
        );
        assert!(
            parsed.message.contains("[REDACTED]"),
            "Should contain REDACTED marker: {}",
            parsed.message
        );
    }

    #[test]
    fn write_crash_bundle_handles_duplicate_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "first".to_string(),
            location: None,
            backtrace: None,
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let path1 = write_crash_bundle(&crash_dir, &report, None).unwrap();

        let report2 = CrashReport {
            message: "second".to_string(),
            ..report.clone()
        };

        let path2 = write_crash_bundle(&crash_dir, &report2, None).unwrap();

        assert_ne!(path1, path2);
        assert!(path1.exists());
        assert!(path2.exists());
    }

    #[test]
    fn write_crash_bundle_directory_name_format() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "test".to_string(),
            location: None,
            backtrace: None,
            // 2023-11-14 22:13:20 UTC
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None).unwrap();
        let dir_name = bundle_path.file_name().unwrap().to_str().unwrap();

        assert!(
            dir_name.starts_with("wa_crash_"),
            "should start with wa_crash_: {dir_name}"
        );
        // Should contain a timestamp-like string
        assert!(dir_name.len() > "wa_crash_".len());
    }

    #[test]
    fn crash_report_files_have_restricted_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "perm check".to_string(),
            location: None,
            backtrace: None,
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let crash_file = bundle_path.join("crash_report.json");
            let perms = fs::metadata(&crash_file).unwrap().permissions();
            let mode = perms.mode() & 0o777;
            assert_eq!(mode, 0o600, "crash report should be owner-only: {mode:o}");
        }
    }

    // -- Helper tests --

    #[test]
    fn format_timestamp_produces_valid_string() {
        // 2023-11-14 22:13:20 UTC
        let ts = format_timestamp(1_700_000_000);
        assert_eq!(ts, "20231114_221320");
    }

    #[test]
    fn format_iso8601_produces_valid_string() {
        let s = format_iso8601(0);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_iso8601_known_date() {
        let s = format_iso8601(1_700_000_000);
        assert_eq!(s, "2023-11-14T22:13:20Z");
    }

    #[test]
    fn days_to_ymd_epoch() {
        let (y, m, d) = days_to_ymd(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-02-29 (leap day)
        let (y, m, d) = days_to_ymd(19_782);
        assert_eq!(y, 2024);
        assert_eq!(m, 2);
        assert_eq!(d, 29);
    }

    #[test]
    fn max_backtrace_len_is_bounded() {
        assert!(MAX_BACKTRACE_LEN <= MAX_BUNDLE_SIZE);
    }

    #[test]
    fn max_bundle_size_is_reasonable() {
        assert!(MAX_BUNDLE_SIZE >= 1024, "bundle size too small");
        assert!(MAX_BUNDLE_SIZE <= 10 * 1024 * 1024, "bundle size too large");
    }

    #[test]
    fn crash_config_accepts_none_dir() {
        let config = CrashConfig {
            crash_dir: None,
            include_backtrace: true,
        };
        // install_panic_hook should accept this without crash_dir
        // (it just won't write files)
        assert!(config.crash_dir.is_none());
        assert!(config.include_backtrace);
    }

    #[test]
    fn write_crash_bundle_health_snapshot_is_valid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let health = test_snapshot();

        let report = CrashReport {
            message: "health json check".to_string(),
            location: None,
            backtrace: None,
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, Some(&health)).unwrap();

        let health_json = fs::read_to_string(bundle_path.join("health_snapshot.json")).unwrap();
        let parsed: HealthSnapshot = serde_json::from_str(&health_json).unwrap();

        assert_eq!(parsed.timestamp, health.timestamp);
        assert_eq!(parsed.observed_panes, health.observed_panes);
        assert_eq!(parsed.capture_queue_depth, health.capture_queue_depth);
    }

    #[test]
    fn write_crash_bundle_size_budget_skips_oversized_files() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        // Create a report with a backtrace that exceeds MAX_BUNDLE_SIZE.
        // The bundle writer should skip writing crash_report.json when the
        // serialized content exceeds the privacy budget.
        let huge_bt = "x".repeat(MAX_BUNDLE_SIZE + 1000);
        let report = CrashReport {
            message: "big backtrace".to_string(),
            location: None,
            backtrace: Some(huge_bt),
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None).unwrap();

        // Manifest should always exist regardless of budget
        assert!(bundle_path.join("manifest.json").exists());

        // The oversized crash_report.json should be skipped
        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();

        // Since the report exceeds budget, it should not be in the file list
        assert!(
            !manifest.files.contains(&"crash_report.json".to_string()),
            "oversized report should be skipped, files: {:?}",
            manifest.files
        );
    }

    #[test]
    fn write_crash_bundle_within_budget_includes_all_files() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        // Small report that fits within budget
        let report = CrashReport {
            message: "small panic".to_string(),
            location: Some("test.rs:1:1".to_string()),
            backtrace: Some("frame 0".to_string()),
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let health = test_snapshot();
        let bundle_path = write_crash_bundle(&crash_dir, &report, Some(&health)).unwrap();

        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();

        assert_eq!(manifest.files.len(), 2);
        assert!(manifest.files.contains(&"crash_report.json".to_string()));
        assert!(manifest.files.contains(&"health_snapshot.json".to_string()));
        assert!(manifest.has_health_snapshot);
        assert!(manifest.bundle_size_bytes > 0);
        assert!(manifest.bundle_size_bytes < MAX_BUNDLE_SIZE as u64);
    }

    #[test]
    fn manifest_is_deterministic_for_same_input() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let crash_dir1 = tmp1.path().join("crash");
        let crash_dir2 = tmp2.path().join("crash");

        let report = CrashReport {
            message: "deterministic".to_string(),
            location: Some("test.rs:1:1".to_string()),
            backtrace: None,
            timestamp: 1_700_000_000,
            pid: 42,
            thread_name: Some("main".to_string()),
        };

        let health = test_snapshot();

        let path1 = write_crash_bundle(&crash_dir1, &report, Some(&health)).unwrap();
        let path2 = write_crash_bundle(&crash_dir2, &report, Some(&health)).unwrap();

        // Manifests should have the same structural content
        let m1: CrashManifest =
            serde_json::from_str(&fs::read_to_string(path1.join("manifest.json")).unwrap())
                .unwrap();
        let m2: CrashManifest =
            serde_json::from_str(&fs::read_to_string(path2.join("manifest.json")).unwrap())
                .unwrap();

        assert_eq!(m1.wa_version, m2.wa_version);
        assert_eq!(m1.created_at, m2.created_at);
        assert_eq!(m1.files, m2.files);
        assert_eq!(m1.has_health_snapshot, m2.has_health_snapshot);
        assert_eq!(m1.bundle_size_bytes, m2.bundle_size_bytes);

        // Crash reports should also be identical
        let r1: CrashReport =
            serde_json::from_str(&fs::read_to_string(path1.join("crash_report.json")).unwrap())
                .unwrap();
        let r2: CrashReport =
            serde_json::from_str(&fs::read_to_string(path2.join("crash_report.json")).unwrap())
                .unwrap();

        assert_eq!(r1.message, r2.message);
        assert_eq!(r1.location, r2.location);
        assert_eq!(r1.timestamp, r2.timestamp);
        assert_eq!(r1.pid, r2.pid);
    }

    #[test]
    fn backtrace_truncation_at_max_len() {
        // Simulate what the panic hook does with a very long backtrace
        let long_bt = "a".repeat(MAX_BACKTRACE_LEN + 500);
        let truncated = if long_bt.len() > MAX_BACKTRACE_LEN {
            let mut s = long_bt[..MAX_BACKTRACE_LEN].to_string();
            s.push_str("\n... [truncated]");
            s
        } else {
            long_bt.clone()
        };

        assert!(truncated.len() < long_bt.len());
        assert!(truncated.ends_with("\n... [truncated]"));
        assert!(truncated.len() <= MAX_BACKTRACE_LEN + 20);
    }

    // -----------------------------------------------------------------------
    // Crash bundle listing tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_crash_bundles_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let result = list_crash_bundles(tmp.path(), 10);
        assert!(result.is_empty());
    }

    #[test]
    fn list_crash_bundles_nonexistent_dir() {
        let result = list_crash_bundles(Path::new("/nonexistent/crash/dir"), 10);
        assert!(result.is_empty());
    }

    #[test]
    fn list_crash_bundles_finds_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        let report = test_report();
        write_crash_bundle(crash_dir, &report, None).unwrap();

        let bundles = list_crash_bundles(crash_dir, 10);
        assert_eq!(bundles.len(), 1);
        assert!(bundles[0].manifest.is_some());
        assert!(bundles[0].report.is_some());
    }

    #[test]
    fn list_crash_bundles_sorted_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        let mut r1 = test_report();
        r1.timestamp = 1000;
        r1.message = "first".to_string();
        write_crash_bundle(crash_dir, &r1, None).unwrap();

        let mut r2 = test_report();
        r2.timestamp = 2000;
        r2.message = "second".to_string();
        write_crash_bundle(crash_dir, &r2, None).unwrap();

        let bundles = list_crash_bundles(crash_dir, 10);
        assert_eq!(bundles.len(), 2);
        assert_eq!(bundles[0].report.as_ref().unwrap().message, "second");
        assert_eq!(bundles[1].report.as_ref().unwrap().message, "first");
    }

    #[test]
    fn list_crash_bundles_respects_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        for i in 0..5 {
            let mut r = test_report();
            r.timestamp = 1000 + i;
            write_crash_bundle(crash_dir, &r, None).unwrap();
        }

        let bundles = list_crash_bundles(crash_dir, 3);
        assert_eq!(bundles.len(), 3);
    }

    #[test]
    fn list_crash_bundles_skips_non_crash_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        // Create a non-crash directory
        fs::create_dir(crash_dir.join("some_other_dir")).unwrap();
        // Create a crash bundle
        let report = test_report();
        write_crash_bundle(crash_dir, &report, None).unwrap();

        let bundles = list_crash_bundles(crash_dir, 10);
        assert_eq!(bundles.len(), 1);
    }

    #[test]
    fn list_crash_bundles_skips_empty_crash_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        // Create an empty wa_crash_ directory (no manifest or report)
        fs::create_dir(crash_dir.join("wa_crash_empty")).unwrap();
        // Create a valid crash bundle
        let report = test_report();
        write_crash_bundle(crash_dir, &report, None).unwrap();

        let bundles = list_crash_bundles(crash_dir, 10);
        assert_eq!(bundles.len(), 1);
    }

    #[test]
    fn latest_crash_bundle_returns_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        let mut r1 = test_report();
        r1.timestamp = 1000;
        r1.message = "older".to_string();
        write_crash_bundle(crash_dir, &r1, None).unwrap();

        let mut r2 = test_report();
        r2.timestamp = 2000;
        r2.message = "newer".to_string();
        write_crash_bundle(crash_dir, &r2, None).unwrap();

        let latest = latest_crash_bundle(crash_dir).unwrap();
        assert_eq!(latest.report.as_ref().unwrap().message, "newer");
    }

    // -----------------------------------------------------------------------
    // Incident bundle export tests
    // -----------------------------------------------------------------------

    #[test]
    fn export_incident_bundle_crash_with_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");

        let report = test_report();
        write_crash_bundle(&crash_dir, &report, Some(&test_snapshot())).unwrap();

        let result =
            export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Crash).unwrap();

        assert_eq!(result.kind, IncidentKind::Crash);
        assert!(result.path.exists());
        assert!(result.files.contains(&"crash_report.json".to_string()));
        assert!(result.files.contains(&"crash_manifest.json".to_string()));
        assert!(result.files.contains(&"health_snapshot.json".to_string()));
        assert!(result.total_size_bytes > 0);

        let manifest_path = result.path.join("incident_manifest.json");
        assert!(manifest_path.exists());
    }

    #[test]
    fn export_incident_bundle_crash_without_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");

        let result =
            export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Crash).unwrap();

        assert_eq!(result.kind, IncidentKind::Crash);
        assert!(result.path.exists());
        assert!(result.files.is_empty());
    }

    #[test]
    fn export_incident_bundle_manual_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");

        let result =
            export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Manual).unwrap();

        assert_eq!(result.kind, IncidentKind::Manual);
        assert!(
            result
                .path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("wa_incident_manual_")
        );
    }

    #[test]
    fn export_incident_bundle_includes_config() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");
        let config_path = tmp.path().join("config.toml");

        fs::write(&config_path, "[ingest]\nbuffer_size = 1024\n").unwrap();

        let result = export_incident_bundle(
            &crash_dir,
            Some(&config_path),
            &out_dir,
            IncidentKind::Manual,
        )
        .unwrap();

        assert!(result.files.contains(&"config_summary.toml".to_string()));
        let config_content = fs::read_to_string(result.path.join("config_summary.toml")).unwrap();
        assert!(config_content.contains("buffer_size"));
    }

    #[test]
    fn incident_kind_display() {
        assert_eq!(format!("{}", IncidentKind::Crash), "crash");
        assert_eq!(format!("{}", IncidentKind::Manual), "manual");
    }
}
