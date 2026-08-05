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
//! .ft/crash/ft_crash_YYYYMMDD_HHMMSS_pPID_SEQUENCE/
//! ├── manifest.json        # Bundle metadata (version, timestamp, schema)
//! ├── crash_report.json    # Generic fatal summary, line/column, bounded backtrace
//! ├── environment_markers.json # Terminal/session crash triage markers
//! └── health_snapshot.json # Last known HealthSnapshot (if available)
//! ```

use std::backtrace::Backtrace;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::RwLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_std::fs::{Dir as CapDir, OpenOptions as CapOpenOptions};
use serde::{Deserialize, Serialize};

use crate::policy::Redactor;

/// Global health snapshot for crash reporting
static GLOBAL_HEALTH: OnceLock<RwLock<Option<HealthSnapshot>>> = OnceLock::new();

/// Latest robot-state shaped pane inventory for incident bundles.
static GLOBAL_INCIDENT_ROBOT_STATE: OnceLock<RwLock<Option<IncidentRobotStateSnapshot>>> =
    OnceLock::new();

/// Latest privacy-bounded pane text summaries for incident bundles.
static GLOBAL_INCIDENT_PANE_TEXT_SUMMARIES: OnceLock<
    RwLock<Option<IncidentPaneTextSummariesSnapshot>>,
> = OnceLock::new();

/// Latest retained RCH/proof evidence supplied to incident bundles.
static GLOBAL_INCIDENT_PROOF_RCH_EVIDENCE: OnceLock<
    RwLock<Option<IncidentProofRchEvidenceSnapshot>>,
> = OnceLock::new();

/// Latest read-only Agent Mail evidence supplied to incident bundles.
static GLOBAL_INCIDENT_AGENT_MAIL: OnceLock<RwLock<Option<IncidentAgentMailSnapshot>>> =
    OnceLock::new();

/// Latest TUI lifecycle markers published by terminal-session code.
static GLOBAL_CRASH_TERMINAL_SESSION_MARKERS: OnceLock<
    RwLock<Option<CrashTerminalSessionMarkers>>,
> = OnceLock::new();

// ============================================================================
// br-ft-94cdu: crash-bundle parse-drop observability
// ============================================================================

/// br-ft-94cdu: cumulative count of crash-bundle file parse failures
/// observed during bundle enumeration. Pre-fix the `.ok()` chain
/// silently dropped corrupted bundles, so operators chasing a
/// crash report saw "no bundles" rather than "bundle present but
/// corrupted, here's why".
///
/// File-not-found is NOT counted (the file just doesn't exist);
/// only file-present-but-fails-to-{read,parse} bumps the counter. The first
/// [`CRASH_BUNDLE_PARSE_DROP_LOG_LIMIT`] failures include a finite phase tag;
/// later failures remain visible in this counter without producing an
/// unbounded warning storm. Neither paths nor backend error strings are
/// reflected into the log.
///
/// Same observability shape as ft-bn6qi epoch_clock_anomaly_count,
/// ft-yygus policy_decision_context_serde_drop_count, ft-zkthg
/// workflows_serde_drop_count.
static CRASH_BUNDLE_PARSE_DROP_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Cap warning emission independently of the total drop counter. Historical
/// crash directories are untrusted input; thousands of malformed files must
/// not turn one TUI refresh into a log storm.
const CRASH_BUNDLE_PARSE_DROP_LOG_LIMIT: u64 = 16;
static CRASH_BUNDLE_PARSE_DROP_LOG_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Total fail-closed compatibility requests whose bounded search could not
/// produce an authoritative answer. Typed `discover_*` callers receive the
/// incompleteness directly and do not increment this wrapper-level counter.
/// Keep it distinct from payload parse drops: a clean directory can still
/// exceed a candidate or result window.
static CRASH_BUNDLE_DISCOVERY_INCOMPLETE_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Cap repeated refresh warnings while retaining the total counter above.
const CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_LIMIT: u64 = 16;
static CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Process-local nonce for disjoint crash-bundle staging and final paths.
///
/// Fatal hooks can run concurrently on multiple threads. A shared
/// `.{timestamp}.tmp` path lets one hook remove or rename another hook's
/// in-flight evidence, so every attempt gets its own process/sequence identity.
static CRASH_BUNDLE_WRITE_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Clone an optional diagnostic snapshot without ever waiting for its writer.
///
/// Panic hooks can run on the thread that currently owns a global snapshot's
/// write lock. A blocking read from that hook would self-deadlock and prevent
/// both the crash report and process termination. Poison means the writer is
/// already gone, so its last value remains safe to clone; active contention is
/// omitted from best-effort evidence.
fn try_clone_diagnostic_snapshot<T: Clone>(lock: &RwLock<Option<T>>) -> Option<T> {
    match lock.try_read() {
        Ok(guard) => guard.clone(),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            lock.clear_poison();
            poisoned.into_inner().clone()
        }
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

fn clone_diagnostic_snapshot<T: Clone>(lock: &RwLock<Option<T>>) -> Option<T> {
    match lock.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => {
            lock.clear_poison();
            poisoned.into_inner().clone()
        }
    }
}

fn replace_diagnostic_snapshot<T>(lock: &RwLock<Option<T>>, value: Option<T>) {
    let mut guard = match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => {
            lock.clear_poison();
            poisoned.into_inner()
        }
    };
    *guard = value;
}

/// br-ft-94cdu: cumulative count of crash-bundle parse failures.
#[must_use]
pub fn crash_bundle_parse_drop_count() -> u64 {
    CRASH_BUNDLE_PARSE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Cumulative fail-closed `list`/`latest` compatibility results withheld
/// because bounded discovery could not prove their answer authoritative.
#[must_use]
pub fn crash_bundle_discovery_incomplete_count() -> u64 {
    CRASH_BUNDLE_DISCOVERY_INCOMPLETE_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// br-ft-94cdu: test helper to reset the parse-drop counter.
#[cfg(test)]
pub(crate) fn reset_crash_bundle_parse_drop_count_for_test() {
    CRASH_BUNDLE_PARSE_DROP_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    CRASH_BUNDLE_PARSE_DROP_LOG_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
fn reset_crash_bundle_discovery_incomplete_count_for_test() {
    CRASH_BUNDLE_DISCOVERY_INCOMPLETE_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
    CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[inline]
fn record_crash_bundle_parse_drop(
    _bundle_path: &Path,
    phase: &'static str,
    _error: &dyn std::fmt::Display,
) {
    let dropped = CRASH_BUNDLE_PARSE_DROP_COUNT
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    let log_index = claim_bounded_crash_bundle_log_slot(
        &CRASH_BUNDLE_PARSE_DROP_LOG_COUNT,
        CRASH_BUNDLE_PARSE_DROP_LOG_LIMIT,
    );
    if log_index.is_some_and(|index| index <= CRASH_BUNDLE_PARSE_DROP_LOG_LIMIT) {
        tracing::warn!(
            target: "ft.crash.bundle",
            event = "crash_bundle_parse_drop",
            phase,
            dropped,
            "crash bundle payload was rejected (br-ft-94cdu)"
        );
    } else if log_index == Some(CRASH_BUNDLE_PARSE_DROP_LOG_LIMIT.saturating_add(1)) {
        tracing::warn!(
            target: "ft.crash.bundle",
            event = "crash_bundle_parse_drop_suppressed",
            dropped,
            log_limit = CRASH_BUNDLE_PARSE_DROP_LOG_LIMIT,
            "further crash bundle parse-drop warnings are suppressed"
        );
    }
}

/// Claim one of a finite number of process-lifetime warning slots.
///
/// The counter stops at `warning_limit + 1`: the first `warning_limit` slots
/// carry individual warnings and the last slot carries one suppression notice.
/// A plain `fetch_add` would keep wrapping forever after warning emission had
/// stopped, contradict the counter's finite-emission meaning, and made the
/// suppression-bound regression test fail after more than one suppressed
/// event.
fn claim_bounded_crash_bundle_log_slot(
    counter: &std::sync::atomic::AtomicU64,
    warning_limit: u64,
) -> Option<u64> {
    let terminal = warning_limit.saturating_add(1);
    counter
        .try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| (current < terminal).then(|| current.saturating_add(1)),
        )
        .ok()
        .map(|previous| previous.saturating_add(1))
}

#[inline]
fn record_crash_bundle_discovery_incomplete(
    surface: &'static str,
    completeness: &CrashBundleDiscoveryCompleteness,
    directory_entries_examined: usize,
    ranked_candidates: usize,
    unranked_candidates: usize,
    payload_files_opened: usize,
    payload_bytes_read: u64,
) {
    let incomplete = CRASH_BUNDLE_DISCOVERY_INCOMPLETE_COUNT
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    let log_index = claim_bounded_crash_bundle_log_slot(
        &CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_COUNT,
        CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_LIMIT,
    );
    if log_index.is_some_and(|index| index <= CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_LIMIT) {
        tracing::warn!(
            target: "ft.crash.bundle",
            event = "crash_bundle_discovery_incomplete",
            surface,
            completeness = ?completeness,
            incomplete,
            directory_entries_examined,
            ranked_candidates,
            unranked_candidates,
            payload_files_opened,
            payload_bytes_read,
            "crash-bundle result is withheld because bounded discovery was incomplete"
        );
    } else if log_index
        == Some(CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_LIMIT.saturating_add(1))
    {
        tracing::warn!(
            target: "ft.crash.bundle",
            event = "crash_bundle_discovery_incomplete_suppressed",
            incomplete,
            log_limit = CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_LIMIT,
            "further crash-bundle discovery-incomplete warnings are suppressed"
        );
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CrashBundlePayloadReadStats {
    files_opened: usize,
    bytes_read: u64,
    authority_unreadable: bool,
}

fn crash_bundle_io_error_withholds_authority(error: &std::io::Error) -> bool {
    !matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::IsADirectory
    )
}

#[cfg(test)]
fn open_crash_bundle_dir_nofollow(bundle_path: &Path) -> std::io::Result<CapDir> {
    let parent = bundle_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "crash bundle directory has no parent",
        )
    })?;
    let name = bundle_path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "crash bundle directory has no leaf name",
        )
    })?;
    let parent_dir = CapDir::open_ambient_dir(parent, cap_std::ambient_authority())?;
    parent_dir.open_dir_nofollow(name)
}

fn open_regular_crash_bundle_payload(
    bundle_dir: &CapDir,
    relative: &Path,
) -> std::io::Result<cap_std::fs::File> {
    if relative.components().count() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "crash bundle payload must be one regular-file leaf",
        ));
    }
    // Type-check without following before open. This rejects a FIFO or
    // symlink deterministically and avoids relying on platform-specific open
    // errors to distinguish hostile leaves from genuinely unreadable files.
    if !bundle_dir.symlink_metadata(relative)?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "crash bundle payload is not a regular file",
        ));
    }
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.nonblock(true);
    let file = bundle_dir.open_with(relative, &options)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "crash bundle payload is not a regular file",
        ));
    }
    Ok(file)
}

fn read_optional_json_from_bundle_dir_bounded<T: serde::de::DeserializeOwned>(
    bundle_dir: &CapDir,
    bundle_path: &Path,
    file_path: &Path,
    read_fail_phase: &'static str,
    parse_fail_phase: &'static str,
    max_bytes: u64,
    stats: &mut CrashBundlePayloadReadStats,
) -> Option<T> {
    let relative = match file_path.strip_prefix(bundle_path) {
        Ok(relative) => relative,
        Err(error) => {
            record_crash_bundle_parse_drop(bundle_path, read_fail_phase, &error);
            return None;
        }
    };
    let mut file = match open_regular_crash_bundle_payload(bundle_dir, relative) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            stats.authority_unreadable |= crash_bundle_io_error_withholds_authority(&error);
            record_crash_bundle_parse_drop(bundle_path, read_fail_phase, &error);
            return None;
        }
    };
    stats.files_opened = stats.files_opened.saturating_add(1);
    let mut raw = Vec::new();
    let read_result = (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut raw);
    stats.bytes_read = stats
        .bytes_read
        .saturating_add(u64::try_from(raw.len()).unwrap_or(u64::MAX));
    if let Err(error) = read_result {
        stats.authority_unreadable = true;
        record_crash_bundle_parse_drop(bundle_path, read_fail_phase, &error);
        return None;
    }
    if u64::try_from(raw.len()).unwrap_or(u64::MAX) > max_bytes {
        let error = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "crash bundle payload exceeds its byte limit",
        );
        record_crash_bundle_parse_drop(bundle_path, read_fail_phase, &error);
        return None;
    }
    match serde_json::from_slice::<T>(&raw) {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            record_crash_bundle_parse_drop(bundle_path, parse_fail_phase, &error);
            None
        }
    }
}

#[cfg(test)]
fn read_optional_json_bundle_file_bounded<T: serde::de::DeserializeOwned>(
    bundle_path: &Path,
    file_path: &Path,
    read_fail_phase: &'static str,
    parse_fail_phase: &'static str,
    max_bytes: u64,
    stats: &mut CrashBundlePayloadReadStats,
) -> Option<T> {
    let bundle_dir = match open_crash_bundle_dir_nofollow(bundle_path) {
        Ok(bundle_dir) => bundle_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            record_crash_bundle_parse_drop(bundle_path, read_fail_phase, &error);
            return None;
        }
    };
    read_optional_json_from_bundle_dir_bounded(
        &bundle_dir,
        bundle_path,
        file_path,
        read_fail_phase,
        parse_fail_phase,
        max_bytes,
        stats,
    )
}

/// br-ft-94cdu: read+parse a JSON file, treating file-not-found as
/// `Ok(None)` (legitimate absence) and read/parse failures as
/// counter bumps with discriminating phase tags. Returns
/// `Ok(Some(T))` on success, `Ok(None)` if the file is missing,
/// and `Ok(None)` on read/parse error after recording the drop.
#[cfg(test)]
fn read_optional_json_bundle_file<T: serde::de::DeserializeOwned>(
    bundle_path: &Path,
    file_path: &Path,
    read_fail_phase: &'static str,
    parse_fail_phase: &'static str,
) -> Option<T> {
    let max_bytes = if file_path.file_name().and_then(|name| name.to_str()) == Some("manifest.json")
    {
        MAX_CRASH_MANIFEST_JSON_READ_BYTES
    } else {
        MAX_CRASH_REPORT_JSON_READ_BYTES
    };
    read_optional_json_bundle_file_bounded(
        bundle_path,
        file_path,
        read_fail_phase,
        parse_fail_phase,
        max_bytes,
        &mut CrashBundlePayloadReadStats::default(),
    )
}

/// Maximum backtrace string length included in crash bundles (64 KiB).
const MAX_BACKTRACE_LEN: usize = 64 * 1024;

/// Maximum panic-message bytes retained in a redacted crash bundle.
const MAX_CRASH_MESSAGE_LEN: usize = 8 * 1024;
/// Maximum terminal cells retained when a crash message is rendered.
const MAX_CRASH_MESSAGE_WIDTH: usize = 2 * 1024;

/// Maximum source-location bytes retained in a redacted crash bundle.
const MAX_CRASH_LOCATION_LEN: usize = 1024;
/// Maximum terminal cells retained when a crash location is rendered.
const MAX_CRASH_LOCATION_WIDTH: usize = 256;

/// Maximum thread-name bytes retained in a redacted crash bundle.
const MAX_CRASH_THREAD_NAME_LEN: usize = 256;
/// Maximum terminal cells retained when a crash thread name is rendered.
const MAX_CRASH_THREAD_NAME_WIDTH: usize = 64;

/// Maximum number of warning lines retained from the last health snapshot.
const MAX_CRASH_HEALTH_WARNINGS: usize = 64;
/// Maximum bytes retained for one health warning or environment marker.
const MAX_CRASH_DIAGNOSTIC_FIELD_LEN: usize = 1024;
/// Maximum terminal cells retained for one health warning or environment marker.
const MAX_CRASH_DIAGNOSTIC_FIELD_WIDTH: usize = 256;

/// Maximum crash bundle size in bytes (1 MiB) — a privacy/size budget.
const MAX_BUNDLE_SIZE: usize = 1024 * 1024;

/// Generated manifests contain only a small fixed file inventory. Historical
/// or attacker-created manifests above 64 KiB are never valid for discovery.
const MAX_CRASH_MANIFEST_JSON_READ_BYTES: u64 = 64 * 1024;
/// A generated report is bounded well below this by its 64 KiB backtrace plus
/// fixed diagnostic fields. Keep historical reads finite with room for JSON.
const MAX_CRASH_REPORT_JSON_READ_BYTES: u64 = 128 * 1024;

/// Cheap lifecycle inventory for leak and retention triage.
///
/// The runtime updates this together with [`HealthSnapshot`] so later
/// remediation and soak work can consume the same substrate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LeakRiskInventorySnapshot {
    /// Total panes still tracked in the runtime registry.
    #[serde(default)]
    pub tracked_pane_entries: usize,
    /// Panes currently marked observed by policy.
    #[serde(default)]
    pub observed_pane_count: usize,
    /// Distinct windows represented by tracked panes.
    #[serde(default)]
    pub window_count: usize,
    /// Distinct tabs represented by tracked panes.
    #[serde(default)]
    pub tab_count: usize,
    /// Distinct non-empty workspaces represented by tracked panes.
    #[serde(default)]
    pub workspace_count: usize,
    /// Live pane arena reservations.
    #[serde(default)]
    pub pane_arena_count: usize,
    /// Sum of currently tracked pane-arena bytes.
    #[serde(default)]
    pub pane_arena_tracked_bytes: u64,
    /// Sum of pane-arena peak tracked bytes.
    #[serde(default)]
    pub pane_arena_peak_tracked_bytes: u64,
    /// Last cursor snapshot memory sample in bytes.
    #[serde(default)]
    pub cursor_snapshot_bytes: u64,
    /// Peak cursor snapshot memory sample in bytes.
    #[serde(default)]
    pub cursor_snapshot_peak_bytes: u64,
    /// Storage lock contention events recorded by the runtime.
    #[serde(default)]
    pub storage_lock_contention_events: u64,
    /// Maximum storage lock wait observed in milliseconds.
    #[serde(default)]
    pub storage_lock_wait_max_ms: f64,
    /// Maximum storage lock hold observed in milliseconds.
    #[serde(default)]
    pub storage_lock_hold_max_ms: f64,
    /// Watchdog snapshot for runtime heartbeat health.
    #[serde(default)]
    pub watchdog: LeakRiskWatchdogSnapshot,
}

impl LeakRiskInventorySnapshot {
    /// Whether the snapshot contains any non-default inventory data.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracked_pane_entries == 0
            && self.observed_pane_count == 0
            && self.window_count == 0
            && self.tab_count == 0
            && self.workspace_count == 0
            && self.pane_arena_count == 0
            && self.pane_arena_tracked_bytes == 0
            && self.pane_arena_peak_tracked_bytes == 0
            && self.cursor_snapshot_bytes == 0
            && self.cursor_snapshot_peak_bytes == 0
            && self.storage_lock_contention_events == 0
            && self.storage_lock_wait_max_ms.abs() < f64::EPSILON
            && self.storage_lock_hold_max_ms.abs() < f64::EPSILON
            && self.watchdog.is_empty()
    }
}

/// Watchdog detail embedded in [`LeakRiskInventorySnapshot`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeakRiskWatchdogSnapshot {
    /// Overall runtime heartbeat health.
    #[serde(default)]
    pub overall: Option<crate::watchdog::HealthStatus>,
    /// Components currently outside the healthy band.
    #[serde(default)]
    pub unhealthy_components: Vec<LeakRiskWatchdogComponentSnapshot>,
    /// Raw heartbeat counters collected by the registry.
    #[serde(default)]
    pub telemetry: Option<crate::watchdog::WatchdogTelemetrySnapshot>,
}

impl LeakRiskWatchdogSnapshot {
    /// Whether the watchdog snapshot carries any data.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.overall.is_none() && self.unhealthy_components.is_empty() && self.telemetry.is_none()
    }
}

/// Leak-risk projection of a single unhealthy watchdog component.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeakRiskWatchdogComponentSnapshot {
    /// Runtime component name.
    pub component: crate::watchdog::Component,
    /// Current watchdog status for that component.
    pub status: crate::watchdog::HealthStatus,
    /// Age of the most recent heartbeat in milliseconds.
    #[serde(default)]
    pub age_ms: Option<u64>,
    /// Configured stale threshold in milliseconds.
    pub threshold_ms: u64,
}

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

    /// Capture scheduler state (budget enforcement + throttling).
    #[serde(default)]
    pub scheduler: Option<crate::tailer::SchedulerSnapshot>,

    /// Current backpressure tier (Green/Yellow/Red/Black).
    #[serde(default)]
    pub backpressure_tier: Option<String>,

    /// Best-effort per-pane output activity timestamp (epoch ms) for stuck pane
    /// detection. Each entry is `(pane_id, last_output_progress_epoch_ms)`.
    #[serde(default)]
    pub last_activity_by_pane: Vec<(u64, u64)>,

    /// Total number of watcher restarts since process start.
    #[serde(default)]
    pub restart_count: u32,

    /// Timestamp of the most recent crash (epoch ms), if any.
    #[serde(default)]
    pub last_crash_at: Option<u64>,

    /// Number of consecutive crashes without a stable run.
    #[serde(default)]
    pub consecutive_crashes: u32,

    /// Current backoff delay in milliseconds (0 if healthy).
    #[serde(default)]
    pub current_backoff_ms: u64,

    /// Whether the watcher is currently in a detected crash loop.
    #[serde(default)]
    pub in_crash_loop: bool,

    /// Fleet scrollback coordinator compound pressure tier (ft-dwjtm).
    /// Values: Normal, Elevated, Critical, Emergency.
    #[serde(default)]
    pub fleet_pressure_tier: Option<String>,

    /// Redacted swarm capacity certificate/controller summary for robot and doctor surfaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_capacity: Option<crate::runtime_telemetry::SwarmCapacityOperatorSummary>,

    /// Leak-risk lifecycle inventory for retention debugging.
    #[serde(default)]
    pub leak_risk_inventory: LeakRiskInventorySnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CrashTerminalSessionMarkers {
    session_phase: String,
    screen_mode: String,
}

/// Environment markers written to `environment_markers.json` in crash bundles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CrashEnvironmentMarkers {
    /// Current output-gate phase at crash-bundle write time.
    pub gate_phase: String,
    /// Last published TUI terminal-session phase.
    pub session_phase: String,
    /// Last published screen mode.
    pub screen_mode: String,
    /// Compile-time frontend/runtime flags relevant to TUI crash triage.
    pub feature_flags: Vec<String>,
    /// `$TERM` value, redacted before persistence.
    pub terminal_type: String,
    /// `$TERM_PROGRAM` value, redacted before persistence.
    pub terminal_program: String,
    /// Last known runtime backpressure tier.
    pub backpressure_tier: String,
}

impl CrashEnvironmentMarkers {
    fn capture(health: Option<&HealthSnapshot>) -> Self {
        let session = crash_terminal_session_markers_global();
        Self {
            gate_phase: crash_output_gate_phase(),
            session_phase: session
                .as_ref()
                .map(|markers| markers.session_phase.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            screen_mode: session
                .as_ref()
                .map(|markers| markers.screen_mode.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            feature_flags: crash_feature_flags(),
            terminal_type: std::env::var("TERM").unwrap_or_else(|_| "unknown".to_string()),
            terminal_program: std::env::var("TERM_PROGRAM")
                .unwrap_or_else(|_| "unknown".to_string()),
            backpressure_tier: health
                .and_then(|snapshot| snapshot.backpressure_tier.clone())
                .unwrap_or_else(|| "unknown".to_string()),
        }
    }

    fn redacted(&self, redactor: &Redactor) -> Self {
        let sanitize = |value: &str| {
            crate::output::truncate_bounded(
                &redactor.redact(value),
                MAX_CRASH_DIAGNOSTIC_FIELD_WIDTH,
                MAX_CRASH_DIAGNOSTIC_FIELD_LEN,
            )
        };
        Self {
            gate_phase: sanitize(&self.gate_phase),
            session_phase: sanitize(&self.session_phase),
            screen_mode: sanitize(&self.screen_mode),
            feature_flags: self.feature_flags.clone(),
            terminal_type: sanitize(&self.terminal_type),
            terminal_program: sanitize(&self.terminal_program),
            backpressure_tier: sanitize(&self.backpressure_tier),
        }
    }
}

#[derive(Serialize)]
struct CrashHealthSnapshot<'a> {
    timestamp: u64,
    observed_panes: usize,
    capture_queue_depth: usize,
    write_queue_depth: usize,
    last_seq_by_pane: &'a [(u64, i64)],
    warnings: Vec<String>,
    ingest_lag_avg_ms: f64,
    ingest_lag_max_ms: u64,
    db_writable: bool,
    db_last_write_at: Option<u64>,
    pane_priority_overrides: &'a [PanePriorityOverrideSnapshot],
    scheduler: Option<&'a crate::tailer::SchedulerSnapshot>,
    backpressure_tier: Option<String>,
    last_activity_by_pane: &'a [(u64, u64)],
    restart_count: u32,
    last_crash_at: Option<u64>,
    consecutive_crashes: u32,
    current_backoff_ms: u64,
    in_crash_loop: bool,
    fleet_pressure_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    swarm_capacity: Option<&'a crate::runtime_telemetry::SwarmCapacityOperatorSummary>,
    leak_risk_inventory: &'a LeakRiskInventorySnapshot,
}

fn redacted_health_snapshot<'a>(
    snapshot: &'a HealthSnapshot,
    redactor: &Redactor,
) -> CrashHealthSnapshot<'a> {
    let warning_limit = if snapshot.warnings.len() > MAX_CRASH_HEALTH_WARNINGS {
        MAX_CRASH_HEALTH_WARNINGS.saturating_sub(1)
    } else {
        MAX_CRASH_HEALTH_WARNINGS
    };
    let mut warnings: Vec<String> = snapshot
        .warnings
        .iter()
        .take(warning_limit)
        .map(|warning| {
            crate::output::truncate_bounded(
                &redactor.redact(warning),
                MAX_CRASH_DIAGNOSTIC_FIELD_WIDTH,
                MAX_CRASH_DIAGNOSTIC_FIELD_LEN,
            )
        })
        .collect();
    if snapshot.warnings.len() > MAX_CRASH_HEALTH_WARNINGS {
        warnings.push(format!(
            "{} additional health warnings omitted",
            snapshot.warnings.len().saturating_sub(warning_limit)
        ));
    }
    let backpressure_tier = snapshot.backpressure_tier.as_deref().map(|tier| {
        crate::output::truncate_bounded(
            &redactor.redact(tier),
            MAX_CRASH_DIAGNOSTIC_FIELD_WIDTH,
            MAX_CRASH_DIAGNOSTIC_FIELD_LEN,
        )
    });
    let fleet_pressure_tier = snapshot.fleet_pressure_tier.as_deref().map(|tier| {
        crate::output::truncate_bounded(
            &redactor.redact(tier),
            MAX_CRASH_DIAGNOSTIC_FIELD_WIDTH,
            MAX_CRASH_DIAGNOSTIC_FIELD_LEN,
        )
    });
    CrashHealthSnapshot {
        timestamp: snapshot.timestamp,
        observed_panes: snapshot.observed_panes,
        capture_queue_depth: snapshot.capture_queue_depth,
        write_queue_depth: snapshot.write_queue_depth,
        last_seq_by_pane: &snapshot.last_seq_by_pane,
        warnings,
        ingest_lag_avg_ms: snapshot.ingest_lag_avg_ms,
        ingest_lag_max_ms: snapshot.ingest_lag_max_ms,
        db_writable: snapshot.db_writable,
        db_last_write_at: snapshot.db_last_write_at,
        pane_priority_overrides: &snapshot.pane_priority_overrides,
        scheduler: snapshot.scheduler.as_ref(),
        backpressure_tier,
        last_activity_by_pane: &snapshot.last_activity_by_pane,
        restart_count: snapshot.restart_count,
        last_crash_at: snapshot.last_crash_at,
        consecutive_crashes: snapshot.consecutive_crashes,
        current_backoff_ms: snapshot.current_backoff_ms,
        in_crash_loop: snapshot.in_crash_loop,
        fleet_pressure_tier,
        swarm_capacity: snapshot.swarm_capacity.as_ref(),
        leak_risk_inventory: &snapshot.leak_risk_inventory,
    }
}

fn crash_terminal_session_markers_global() -> Option<CrashTerminalSessionMarkers> {
    GLOBAL_CRASH_TERMINAL_SESSION_MARKERS
        .get()
        .and_then(try_clone_diagnostic_snapshot)
}

/// Publish the current TUI terminal-session markers for future crash bundles.
pub fn update_crash_terminal_session_markers(
    session_phase: impl Into<String>,
    screen_mode: impl Into<String>,
) {
    let lock = GLOBAL_CRASH_TERMINAL_SESSION_MARKERS.get_or_init(|| RwLock::new(None));
    replace_diagnostic_snapshot(
        lock,
        Some(CrashTerminalSessionMarkers {
            session_phase: session_phase.into(),
            screen_mode: screen_mode.into(),
        }),
    );
}

#[cfg(test)]
pub(crate) fn clear_crash_terminal_session_markers_for_test() {
    let lock = GLOBAL_CRASH_TERMINAL_SESSION_MARKERS.get_or_init(|| RwLock::new(None));
    replace_diagnostic_snapshot(lock, None);
}

fn crash_feature_flags() -> Vec<String> {
    let mut flags = Vec::new();
    if cfg!(feature = "tui") {
        flags.push("tui".to_string());
    }
    if cfg!(feature = "ftui") {
        flags.push("ftui".to_string());
    }
    if flags.is_empty() {
        flags.push("headless".to_string());
    }
    flags
}

fn crash_output_gate_phase() -> String {
    #[cfg(any(feature = "tui", feature = "ftui"))]
    {
        format!("{:?}", crate::tui::output_gate::phase())
    }
    #[cfg(not(any(feature = "tui", feature = "ftui")))]
    {
        "unavailable".to_string()
    }
}

/// Text-free pane inventory supplied to the incident-bundle robot_state source.
///
/// This mirrors the privacy-bounded `ft robot state` shape and intentionally
/// omits pane text. Runtime or CLI layers can publish it before collecting an
/// incident bundle; the collector degrades to a typed unavailable source when
/// no publisher has supplied a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentRobotStateSnapshot {
    /// Epoch milliseconds when this robot-state view was captured.
    pub captured_at_ms: u64,
    /// Read-only surface that produced the snapshot.
    pub source_surface: String,
    /// Pane metadata rows.
    #[serde(default)]
    pub panes: Vec<IncidentRobotPaneState>,
}

impl IncidentRobotStateSnapshot {
    /// Build a snapshot from incident-specific pane rows.
    #[must_use]
    pub fn new(
        captured_at_ms: u64,
        source_surface: impl Into<String>,
        panes: Vec<IncidentRobotPaneState>,
    ) -> Self {
        Self {
            captured_at_ms,
            source_surface: source_surface.into(),
            panes,
        }
    }

    /// Build a snapshot from the public robot-state DTO.
    #[must_use]
    pub fn from_robot_panes(
        captured_at_ms: u64,
        source_surface: impl Into<String>,
        panes: Vec<crate::robot_types::PaneStateData>,
    ) -> Self {
        Self::new(
            captured_at_ms,
            source_surface,
            panes.into_iter().map(Into::into).collect(),
        )
    }

    /// Publish the current robot-state snapshot for future incident bundles.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_INCIDENT_ROBOT_STATE.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(snapshot);
        }
    }

    /// Return the latest published incident robot-state snapshot.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_INCIDENT_ROBOT_STATE.get_or_init(|| RwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }

    #[cfg(test)]
    pub(crate) fn clear_global_for_test() {
        let lock = GLOBAL_INCIDENT_ROBOT_STATE.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    }
}

/// Text-free pane metadata row included in incident robot_state payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentRobotPaneState {
    /// Numeric pane id.
    pub pane_id: u64,
    /// Stable pane UUID if the daemon has assigned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_uuid: Option<String>,
    /// Tab id reported by the mux, or 0 for distributed persisted panes.
    pub tab_id: u64,
    /// Window id reported by the mux, or 0 for distributed persisted panes.
    pub window_id: u64,
    /// Domain label.
    pub domain: String,
    /// Pane title when already exposed by robot state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Current working directory when already exposed by robot state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether the pane is observed by the configured pane filter.
    #[serde(default)]
    pub observed: bool,
    /// Human-readable visibility state derived from robot-state metadata.
    pub state: String,
    /// Reason the pane is ignored, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_reason: Option<String>,
    /// Epoch milliseconds for this row when the publisher has per-pane timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<u64>,
    /// Epoch milliseconds of recent output activity when already known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<u64>,
}

impl IncidentRobotPaneState {
    /// Construct a text-free incident pane row.
    #[must_use]
    pub fn new(
        pane_id: u64,
        tab_id: u64,
        window_id: u64,
        domain: impl Into<String>,
        observed: bool,
    ) -> Self {
        Self {
            pane_id,
            pane_uuid: None,
            tab_id,
            window_id,
            domain: domain.into(),
            title: None,
            cwd: None,
            observed,
            state: pane_visibility_state(observed, None).to_string(),
            ignore_reason: None,
            observed_at_ms: None,
            last_activity_at_ms: None,
        }
    }

    /// Override title metadata for builder-style construction.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Override cwd metadata for builder-style construction.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Override ignore metadata and recompute the derived state.
    #[must_use]
    pub fn with_ignore_reason(mut self, reason: impl Into<String>) -> Self {
        self.ignore_reason = Some(reason.into());
        self.state =
            pane_visibility_state(self.observed, self.ignore_reason.as_deref()).to_string();
        self
    }

    /// Override per-row timing metadata.
    #[must_use]
    pub fn with_timestamps(
        mut self,
        observed_at_ms: Option<u64>,
        last_activity_at_ms: Option<u64>,
    ) -> Self {
        self.observed_at_ms = observed_at_ms;
        self.last_activity_at_ms = last_activity_at_ms;
        self
    }
}

impl From<crate::robot_types::PaneStateData> for IncidentRobotPaneState {
    fn from(pane: crate::robot_types::PaneStateData) -> Self {
        let state = pane_visibility_state(pane.observed, pane.ignore_reason.as_deref()).to_string();
        Self {
            pane_id: pane.pane_id,
            pane_uuid: pane.pane_uuid,
            tab_id: pane.tab_id,
            window_id: pane.window_id,
            domain: pane.domain,
            title: pane.title,
            cwd: pane.cwd,
            observed: pane.observed,
            state,
            ignore_reason: pane.ignore_reason,
            observed_at_ms: None,
            last_activity_at_ms: None,
        }
    }
}

fn pane_visibility_state(observed: bool, ignore_reason: Option<&str>) -> &'static str {
    if observed {
        "observed"
    } else if ignore_reason.is_some_and(|reason| !reason.trim().is_empty()) {
        "ignored"
    } else {
        "unobserved"
    }
}

/// Privacy-bounded pane text summaries supplied to incident bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentPaneTextSummariesSnapshot {
    /// Epoch milliseconds when these summaries were captured.
    pub captured_at_ms: u64,
    /// Read-only surface that produced the summaries.
    pub source_surface: String,
    /// Tail-line budget used by the producer.
    pub tail_lines: usize,
    /// Maximum bytes allowed per pane summary after redaction and truncation.
    pub max_summary_bytes: usize,
    /// Whether the privacy budget allowed pane text summaries in this snapshot.
    pub privacy_allowed: bool,
    /// Reason summaries were excluded when privacy did not allow text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy_reason: Option<String>,
    /// Per-pane bounded summaries or explicit placeholders.
    #[serde(default)]
    pub panes: Vec<IncidentPaneTextSummary>,
}

impl IncidentPaneTextSummariesSnapshot {
    /// Build a snapshot with already bounded summary rows.
    #[must_use]
    pub fn new(
        captured_at_ms: u64,
        source_surface: impl Into<String>,
        tail_lines: usize,
        max_summary_bytes: usize,
        privacy_allowed: bool,
        panes: Vec<IncidentPaneTextSummary>,
    ) -> Self {
        Self {
            captured_at_ms,
            source_surface: source_surface.into(),
            tail_lines,
            max_summary_bytes,
            privacy_allowed,
            privacy_reason: None,
            panes,
        }
    }

    /// Attach the reason pane text summaries were withheld.
    #[must_use]
    pub fn with_privacy_reason(mut self, reason: impl Into<String>) -> Self {
        self.privacy_reason = Some(reason.into());
        self
    }

    /// Publish current pane text summaries for future incident bundles.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_INCIDENT_PANE_TEXT_SUMMARIES.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(snapshot);
        }
    }

    /// Return the latest published incident pane text summaries.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_INCIDENT_PANE_TEXT_SUMMARIES.get_or_init(|| RwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }

    #[cfg(test)]
    pub(crate) fn clear_global_for_test() {
        let lock = GLOBAL_INCIDENT_PANE_TEXT_SUMMARIES.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    }
}

/// One pane's incident-bundle text summary or explicit placeholder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentPaneTextSummary {
    /// Pane id summarized by this row.
    pub pane_id: u64,
    /// Row status: `summary`, `excluded`, or `error`.
    pub status: String,
    /// Tail-line budget used for the row.
    pub tail_lines: usize,
    /// Redacted, bounded summary text or a placeholder.
    pub summary: String,
    /// Number of redactions applied before this summary is written.
    #[serde(default)]
    pub redactions: usize,
    /// Whether the row was truncated to the summary byte budget.
    #[serde(default)]
    pub truncated: bool,
    /// Truncation metadata when the row was clipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_info: Option<crate::robot_types::TruncationInfo>,
    /// Error or exclusion code for non-summary rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Human-readable error or exclusion reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl IncidentPaneTextSummary {
    /// Build a redacted, bounded summary row from pane tail text.
    #[must_use]
    pub fn from_text(
        pane_id: u64,
        text: &str,
        tail_lines: usize,
        max_summary_bytes: usize,
        redactor: &Redactor,
    ) -> Self {
        let redactions = redactor.detect(text).len();
        let redacted = redactor.redact(text);
        let summary =
            truncate_utf8_with_marker(&redacted, max_summary_bytes, "\n[PANE_TEXT_TRUNCATED]");
        let truncated = summary.len() < redacted.len();
        let truncation_info = truncated.then(|| crate::robot_types::TruncationInfo {
            original_bytes: redacted.len(),
            returned_bytes: summary.len(),
            original_lines: redacted.lines().count(),
            returned_lines: summary.lines().count(),
        });
        Self {
            pane_id,
            status: "summary".to_string(),
            tail_lines,
            summary,
            redactions,
            truncated,
            truncation_info,
            code: None,
            message: None,
        }
    }

    /// Build an explicit placeholder for privacy-excluded pane text.
    #[must_use]
    pub fn excluded(pane_id: u64, tail_lines: usize, reason: impl Into<String>) -> Self {
        Self {
            pane_id,
            status: "excluded".to_string(),
            tail_lines,
            summary: "[PANE_TEXT_EXCLUDED]".to_string(),
            redactions: 0,
            truncated: false,
            truncation_info: None,
            code: Some("pane_text.privacy_disabled".to_string()),
            message: Some(reason.into()),
        }
    }

    /// Build an explicit placeholder for a per-pane read failure.
    #[must_use]
    pub fn error(
        pane_id: u64,
        tail_lines: usize,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            pane_id,
            status: "error".to_string(),
            tail_lines,
            summary: "[PANE_TEXT_UNAVAILABLE]".to_string(),
            redactions: 0,
            truncated: false,
            truncation_info: None,
            code: Some(code.into()),
            message: Some(message.into()),
        }
    }
}

fn sanitize_pane_text_summary_for_payload(
    mut pane: IncidentPaneTextSummary,
    max_summary_bytes: usize,
    redactor: &Redactor,
) -> IncidentPaneTextSummary {
    let additional_redactions = redactor.detect(&pane.summary).len();
    if additional_redactions > 0 {
        pane.summary = redactor.redact(&pane.summary);
        pane.redactions = pane.redactions.saturating_add(additional_redactions);
    }
    if let Some(message) = pane.message.as_mut() {
        let message_redactions = redactor.detect(message).len();
        if message_redactions > 0 {
            *message = redactor.redact(message);
            pane.redactions = pane.redactions.saturating_add(message_redactions);
        }
    }

    if pane.summary.len() > max_summary_bytes {
        let original_bytes = pane.summary.len();
        let original_lines = pane.summary.lines().count();
        pane.summary =
            truncate_utf8_with_marker(&pane.summary, max_summary_bytes, "\n[PANE_TEXT_TRUNCATED]");
        pane.truncated = true;
        pane.truncation_info = Some(crate::robot_types::TruncationInfo {
            original_bytes,
            returned_bytes: pane.summary.len(),
            original_lines,
            returned_lines: pane.summary.lines().count(),
        });
    }

    pane
}

/// Retained proof/RCH evidence supplied to incident bundles.
///
/// This is an attachment point for already-existing proof logs and verdict
/// metadata. The incident collector serializes this snapshot read-only; it must
/// not launch Cargo, RCH, or any other proof command while collecting a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentProofRchEvidenceSnapshot {
    /// Epoch milliseconds when the evidence was captured or assembled.
    pub captured_at_ms: u64,
    /// Read-only surface that supplied this evidence.
    pub source_surface: String,
    /// Overall proof verdict: `passed`, `failed`, `blocked`, or `no_verdict`.
    pub verdict: String,
    /// Stable reason code explaining the verdict.
    pub reason_code: String,
    /// Paths to retained proof/RCH artifacts, relative when possible.
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    /// Per-attempt evidence rows.
    #[serde(default)]
    pub attempts: Vec<IncidentProofRchAttempt>,
    /// Whether local fallback was explicitly rejected by the proof lane.
    #[serde(default)]
    pub local_fallback_rejected: bool,
    /// Whether the available artifacts are only setup/sync/queue chatter.
    #[serde(default)]
    pub setup_chatter_only: bool,
}

impl IncidentProofRchEvidenceSnapshot {
    /// Build a retained proof-evidence snapshot.
    #[must_use]
    pub fn new(
        captured_at_ms: u64,
        source_surface: impl Into<String>,
        verdict: impl Into<String>,
        reason_code: impl Into<String>,
    ) -> Self {
        Self {
            captured_at_ms,
            source_surface: source_surface.into(),
            verdict: verdict.into(),
            reason_code: reason_code.into(),
            artifact_paths: Vec::new(),
            attempts: Vec::new(),
            local_fallback_rejected: false,
            setup_chatter_only: false,
        }
    }

    /// Attach retained proof/RCH artifact paths.
    #[must_use]
    pub fn with_artifact_paths(mut self, artifact_paths: Vec<String>) -> Self {
        self.artifact_paths = artifact_paths;
        self
    }

    /// Attach per-attempt evidence rows.
    #[must_use]
    pub fn with_attempts(mut self, attempts: Vec<IncidentProofRchAttempt>) -> Self {
        self.attempts = attempts;
        self
    }

    /// Record that local fallback was refused rather than counted as proof.
    #[must_use]
    pub fn with_local_fallback_rejected(mut self, rejected: bool) -> Self {
        self.local_fallback_rejected = rejected;
        self
    }

    /// Record that retained artifacts are setup/sync chatter only.
    #[must_use]
    pub fn with_setup_chatter_only(mut self, setup_chatter_only: bool) -> Self {
        self.setup_chatter_only = setup_chatter_only;
        self
    }

    /// Publish retained proof/RCH evidence for future incident bundles.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_INCIDENT_PROOF_RCH_EVIDENCE.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(snapshot);
        }
    }

    /// Return the latest retained proof/RCH evidence snapshot.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_INCIDENT_PROOF_RCH_EVIDENCE.get_or_init(|| RwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }

    #[cfg(test)]
    pub(crate) fn clear_global_for_test() {
        let lock = GLOBAL_INCIDENT_PROOF_RCH_EVIDENCE.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    }
}

/// One retained proof/RCH attempt summarized for incident bundles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentProofRchAttempt {
    /// Command or proof lane label.
    pub command: String,
    /// Attempt status: `passed`, `failed`, `blocked`, or `no_verdict`.
    pub status: String,
    /// Stable reason code for the attempt.
    pub reason_code: String,
    /// Retained artifact path for this attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    /// Whether this attempt reached remote Cargo/rustc/test execution.
    #[serde(default)]
    pub remote_execution_confirmed: bool,
    /// Whether local fallback was rejected for this attempt.
    #[serde(default)]
    pub local_fallback_rejected: bool,
    /// Whether this row is setup/sync chatter only and not a proof verdict.
    #[serde(default)]
    pub setup_chatter_only: bool,
}

impl IncidentProofRchAttempt {
    /// Build a retained proof/RCH attempt row.
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        status: impl Into<String>,
        reason_code: impl Into<String>,
    ) -> Self {
        Self {
            command: command.into(),
            status: status.into(),
            reason_code: reason_code.into(),
            artifact_path: None,
            remote_execution_confirmed: false,
            local_fallback_rejected: false,
            setup_chatter_only: false,
        }
    }

    /// Attach a retained artifact path.
    #[must_use]
    pub fn with_artifact_path(mut self, artifact_path: impl Into<String>) -> Self {
        self.artifact_path = Some(artifact_path.into());
        self
    }

    /// Record whether the attempt reached remote execution.
    #[must_use]
    pub fn with_remote_execution_confirmed(mut self, confirmed: bool) -> Self {
        self.remote_execution_confirmed = confirmed;
        self
    }

    /// Record that local fallback was rejected.
    #[must_use]
    pub fn with_local_fallback_rejected(mut self, rejected: bool) -> Self {
        self.local_fallback_rejected = rejected;
        self
    }

    /// Record that this row is setup/sync chatter only.
    #[must_use]
    pub fn with_setup_chatter_only(mut self, setup_chatter_only: bool) -> Self {
        self.setup_chatter_only = setup_chatter_only;
        self
    }
}

/// Read-only Agent Mail evidence supplied to incident bundles.
///
/// This records already-collected health or unavailable-after-retry metadata.
/// Incident collection serializes the snapshot and must not repair, restart,
/// kill, register, acknowledge, or fetch message bodies from Agent Mail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentAgentMailSnapshot {
    /// Epoch milliseconds when the evidence was captured or assembled.
    pub captured_at_ms: u64,
    /// Read-only surface that supplied this evidence.
    pub source_surface: String,
    /// Service state such as `ok`, `degraded`, or `unavailable`.
    pub status: String,
    /// Health level reported by Agent Mail, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_level: Option<String>,
    /// Stable reason code for non-ok state or `agent_mail.ok`.
    pub reason_code: String,
    /// Number of retries already consumed by the producer.
    #[serde(default)]
    pub retry_count: u8,
    /// Archive project inventory count, when already returned by health/list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_count: Option<u64>,
    /// Agent inventory count, when already returned by health/list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_count: Option<u64>,
    /// Message inventory count, when already returned by health/list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u64>,
    /// Names of active agents, when a read-only list was already supplied.
    #[serde(default)]
    pub active_agents: Vec<String>,
    /// Per-attempt health/list evidence rows.
    #[serde(default)]
    pub attempts: Vec<IncidentAgentMailAttempt>,
    /// Whether any forbidden repair/restart/kill action was attempted.
    #[serde(default)]
    pub repair_restart_kill_attempted: bool,
}

impl IncidentAgentMailSnapshot {
    /// Build a read-only Agent Mail evidence snapshot.
    #[must_use]
    pub fn new(
        captured_at_ms: u64,
        source_surface: impl Into<String>,
        status: impl Into<String>,
        reason_code: impl Into<String>,
    ) -> Self {
        Self {
            captured_at_ms,
            source_surface: source_surface.into(),
            status: status.into(),
            health_level: None,
            reason_code: reason_code.into(),
            retry_count: 0,
            project_count: None,
            agent_count: None,
            message_count: None,
            active_agents: Vec::new(),
            attempts: Vec::new(),
            repair_restart_kill_attempted: false,
        }
    }

    /// Attach a health level returned by Agent Mail health_check.
    #[must_use]
    pub fn with_health_level(mut self, health_level: impl Into<String>) -> Self {
        self.health_level = Some(health_level.into());
        self
    }

    /// Record how many allowed retries were consumed.
    #[must_use]
    pub fn with_retry_count(mut self, retry_count: u8) -> Self {
        self.retry_count = retry_count;
        self
    }

    /// Attach already-returned inventory counts without message bodies.
    #[must_use]
    pub fn with_inventory_counts(
        mut self,
        project_count: Option<u64>,
        agent_count: Option<u64>,
        message_count: Option<u64>,
    ) -> Self {
        self.project_count = project_count;
        self.agent_count = agent_count;
        self.message_count = message_count;
        self
    }

    /// Attach active agent names from an already-returned read-only listing.
    #[must_use]
    pub fn with_active_agents(mut self, active_agents: Vec<String>) -> Self {
        self.active_agents = active_agents;
        self
    }

    /// Attach per-attempt Agent Mail evidence rows.
    #[must_use]
    pub fn with_attempts(mut self, attempts: Vec<IncidentAgentMailAttempt>) -> Self {
        self.attempts = attempts;
        self
    }

    /// Record whether a forbidden repair/restart/kill action was attempted.
    #[must_use]
    pub fn with_repair_restart_kill_attempted(mut self, attempted: bool) -> Self {
        self.repair_restart_kill_attempted = attempted;
        self
    }

    /// Publish read-only Agent Mail evidence for future incident bundles.
    pub fn update_global(snapshot: Self) {
        let lock = GLOBAL_INCIDENT_AGENT_MAIL.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(snapshot);
        }
    }

    /// Return the latest read-only Agent Mail evidence snapshot.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_INCIDENT_AGENT_MAIL.get_or_init(|| RwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }

    #[cfg(test)]
    pub(crate) fn clear_global_for_test() {
        let lock = GLOBAL_INCIDENT_AGENT_MAIL.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    }
}

/// One read-only Agent Mail health/list attempt summarized for incident bundles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncidentAgentMailAttempt {
    /// Read-only operation label, for example `health_check`.
    pub operation: String,
    /// Attempt status such as `ok`, `error`, or `timeout`.
    pub status: String,
    /// Stable reason code for this attempt.
    pub reason_code: String,
    /// Bounded diagnostic message without mailbox bodies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Attempt elapsed milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl IncidentAgentMailAttempt {
    /// Build a read-only Agent Mail attempt row.
    #[must_use]
    pub fn new(
        operation: impl Into<String>,
        status: impl Into<String>,
        reason_code: impl Into<String>,
    ) -> Self {
        Self {
            operation: operation.into(),
            status: status.into(),
            reason_code: reason_code.into(),
            message: None,
            elapsed_ms: None,
        }
    }

    /// Attach a bounded diagnostic message.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Attach attempt elapsed milliseconds.
    #[must_use]
    pub fn with_elapsed_ms(mut self, elapsed_ms: u64) -> Self {
        self.elapsed_ms = Some(elapsed_ms);
        self
    }
}

fn sanitize_agent_mail_attempt_for_payload(
    mut attempt: IncidentAgentMailAttempt,
    redactor: &Redactor,
) -> (IncidentAgentMailAttempt, usize) {
    let mut redactions = 0_usize;
    if let Some(message) = attempt.message.as_mut() {
        redactions = redactor.detect(message).len();
        if redactions > 0 {
            *message = redactor.redact(message);
        }
    }
    (attempt, redactions)
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
        replace_diagnostic_snapshot(lock, Some(snapshot));
    }

    /// Get the current global health snapshot.
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_HEALTH.get_or_init(|| RwLock::new(None));
        clone_diagnostic_snapshot(lock)
    }

    /// Get the latest health snapshot without waiting for an active writer.
    ///
    /// This is reserved for panic-hook collection, where blocking on a lock
    /// owned by the panicking thread would deadlock the process.
    fn try_get_global_for_panic() -> Option<Self> {
        GLOBAL_HEALTH
            .get()
            .and_then(try_clone_diagnostic_snapshot)
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
    /// ft version at crash time
    pub wa_version: String,
    /// ISO-8601 timestamp
    pub created_at: String,
    /// Files included in the bundle
    pub files: Vec<String>,
    /// Whether health snapshot was available
    pub has_health_snapshot: bool,
    /// Whether resize/reflow crash forensics were available
    #[serde(default)]
    pub has_resize_forensics: bool,
    /// Whether environment markers were written.
    #[serde(default)]
    pub has_environment_markers: bool,
    /// Total bundle size in bytes
    pub bundle_size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Panic hook
// ---------------------------------------------------------------------------

/// Install the panic hook for crash reporting.
///
/// Wraps the current project panic hook with one that writes a privacy-bounded
/// crash bundle containing a generic message, bounded backtrace, and last known
/// health snapshot. The bundle is written atomically (temp dir + rename);
/// caller-controlled diagnostic fields are suppressed or passed through the
/// [`Redactor`], terminal sanitizer, and explicit bounds before persistence.
///
/// This layer never treats artifact persistence as user-visible reporting. It
/// always delegates fatal panics so the next GUI/base hook emits exactly one
/// generic notification; recoverable panics and EPIPE retain their shared
/// process-policy semantics.
pub fn install_panic_hook(config: &CrashConfig) {
    let include_backtrace = config.include_backtrace;
    let crash_dir = config.crash_dir.clone();
    let previous_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        // A hook runs before catch_unwind returns. The project-owned marker is
        // the only authority that this panic belongs to an audited recovery
        // boundary; never create fatal artifacts for that path. This check
        // must precede payload classification because payload text is
        // caller-controlled and can spoof an EPIPE-looking message.
        if frankenterm_sigpipe::is_recoverable_panic() {
            return;
        }
        // Unmarked EPIPE must retain the shared hook's deterministic quiet
        // exit(141), including after watch mode installs bundle reporting.
        if frankenterm_sigpipe::panic_is_broken_pipe(info) {
            previous_hook(info);
            return;
        }

        // Write a silent, privacy-bounded crash bundle if configured. Artifact
        // persistence is not an operator-visible report: regardless of write
        // success, delegation below lets the next GUI/base hook surface
        // exactly one generic notification.
        if let Some(ref dir) = crash_dir {
            // Capture the backtrace only when there is an artifact sink. A
            // hook configured without a crash directory must delegate without
            // paying this substantial panic-path allocation cost.
            let backtrace = include_backtrace.then(Backtrace::force_capture);

            // Never retain caller- or plugin-controlled panic payload text.
            // The backtrace and privacy-bounded line/column retain diagnostic
            // value without reflecting the panic message or source path.
            let report = CrashReport {
                message: frankenterm_sigpipe::GENERIC_FATAL_REPORT.to_string(),
                location: info
                    .location()
                    .map(|loc| format!("line:{}:column:{}", loc.line(), loc.column())),
                backtrace: backtrace.map(|backtrace| {
                    truncate_utf8_with_marker(
                        &backtrace.to_string(),
                        MAX_BACKTRACE_LEN,
                        "\n... [truncated]",
                    )
                }),
                timestamp: epoch_secs(),
                pid: std::process::id(),
                // Thread names are application-controlled and may carry pane,
                // account, or plugin text. The backtrace already identifies the
                // failing execution context without reflecting that content.
                thread_name: None,
            };

            let health = HealthSnapshot::try_get_global_for_panic();
            let resize_ctx =
                crate::resize_crash_forensics::ResizeCrashContext::try_get_global_for_panic();
            let _ = write_crash_bundle(dir, &report, health.as_ref(), resize_ctx.as_ref());
        }

        // Never claim operator-visible reporting here. In GUI binaries the
        // next hook owns a generic toast and suppresses the base stderr line;
        // in headless binaries the base hook owns one generic stderr line.
        previous_hook(info);
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
    resize_forensics: Option<&crate::resize_crash_forensics::ResizeCrashContext>,
) -> std::io::Result<PathBuf> {
    let redactor = Redactor::new();

    // Build timestamped bundle directory name
    let ts_str = format_timestamp(report.timestamp);
    let bundle_prefix = format!("ft_crash_{ts_str}");
    fs::create_dir_all(crash_dir)?;

    // Use a unique, private staging directory alongside the final location.
    // The process id separates simultaneously running FrankenTerm processes;
    // the monotonic nonce separates hooks racing inside one process. Never
    // reuse or remove a pre-existing staging path: it may contain evidence
    // from an interrupted or still-running writer.
    let process_id = std::process::id();
    let mut staging_attempts = 0_u16;
    let (tmp_dir, final_dir) = loop {
        if staging_attempts >= 256 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a unique crash-bundle staging directory",
            ));
        }
        staging_attempts += 1;
        let sequence = CRASH_BUNDLE_WRITE_SEQUENCE.fetch_add(
            1,
            std::sync::atomic::Ordering::Relaxed,
        );
        let unique_name = format!("{bundle_prefix}_p{process_id}_{sequence}");
        let candidate_final = crash_dir.join(&unique_name);
        let candidate_tmp = crash_dir.join(format!(".{unique_name}.tmp"));
        if candidate_final.exists() {
            continue;
        }

        #[cfg(unix)]
        let create_result = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&candidate_tmp)
        };
        #[cfg(not(unix))]
        let create_result = fs::DirBuilder::new().create(&candidate_tmp);

        match create_result {
            Ok(()) => break (candidate_tmp, candidate_final),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let mut files = Vec::new();
    let mut total_size: u64 = 0;
    let mut maybe_write_bounded = |file_name: &str, bytes: &[u8]| -> std::io::Result<bool> {
        let prospective_total = total_size.saturating_add(bytes.len() as u64);
        if prospective_total > MAX_BUNDLE_SIZE as u64 {
            return Ok(false);
        }

        write_file_sync(&tmp_dir.join(file_name), bytes)?;
        total_size = prospective_total;
        files.push(file_name.to_string());
        Ok(true)
    };

    // 1. Write crash_report.json (redacted)
    {
        let redacted_report = CrashReport {
            message: crate::output::truncate_bounded(
                &redactor.redact(&report.message),
                MAX_CRASH_MESSAGE_WIDTH,
                MAX_CRASH_MESSAGE_LEN,
            ),
            location: report.location.as_ref().map(|location| {
                crate::output::truncate_bounded(
                    &redactor.redact(location),
                    MAX_CRASH_LOCATION_WIDTH,
                    MAX_CRASH_LOCATION_LEN,
                )
            }),
            backtrace: report.backtrace.as_ref().map(|backtrace| {
                truncate_utf8_with_marker(
                    &redactor.redact(backtrace),
                    MAX_BACKTRACE_LEN,
                    "\n... [truncated]",
                )
            }),
            timestamp: report.timestamp,
            pid: report.pid,
            thread_name: report.thread_name.as_ref().map(|thread_name| {
                crate::output::truncate_bounded(
                    &redactor.redact(thread_name),
                    MAX_CRASH_THREAD_NAME_WIDTH,
                    MAX_CRASH_THREAD_NAME_LEN,
                )
            }),
        };
        let json = serde_json::to_string_pretty(&redacted_report).map_err(std::io::Error::other)?;
        let bytes = json.as_bytes();
        let _ = maybe_write_bounded("crash_report.json", bytes)?;
    }

    // 2. Write health_snapshot.json (if available)
    let has_health = if let Some(snap) = health {
        let redacted_health = redacted_health_snapshot(snap, &redactor);
        let json =
            serde_json::to_string_pretty(&redacted_health).map_err(std::io::Error::other)?;
        let bytes = json.as_bytes();
        maybe_write_bounded("health_snapshot.json", bytes)?
    } else {
        false
    };

    // 2b. Write resize_forensics.json (if available)
    let has_resize_forensics = if let Some(ctx) = resize_forensics {
        let json = serde_json::to_string_pretty(ctx).map_err(std::io::Error::other)?;
        let bytes = json.as_bytes();
        maybe_write_bounded("resize_forensics.json", bytes)?
    } else {
        false
    };

    // 2c. Write environment_markers.json.
    let has_environment_markers = {
        let markers = CrashEnvironmentMarkers::capture(health).redacted(&redactor);
        let json = serde_json::to_string_pretty(&markers).map_err(std::io::Error::other)?;
        maybe_write_bounded("environment_markers.json", json.as_bytes())?
    };

    // 3. Write manifest.json
    {
        let manifest = CrashManifest {
            wa_version: crate::VERSION.to_string(),
            created_at: format_iso8601(report.timestamp),
            files: files.clone(),
            has_health_snapshot: has_health,
            has_resize_forensics,
            has_environment_markers,
            bundle_size_bytes: total_size,
        };
        let json = serde_json::to_string_pretty(&manifest).map_err(std::io::Error::other)?;
        write_file_sync(&tmp_dir.join("manifest.json"), json.as_bytes())?;
        // manifest doesn't count toward the privacy budget
    }

    // Atomic rename: this writer's private staging directory → its private
    // final directory. No existence check/rename race is shared with another
    // writer in this process.
    fs::rename(&tmp_dir, &final_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&final_dir, fs::Permissions::from_mode(0o700))?;
    }

    Ok(final_dir)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_file_sync(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;

    // `mode(0o600)` applies at creation. Reassert it before writing as well so
    // overwriting an older file cannot expose newly written crash evidence
    // through permissive pre-existing mode bits.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    f.write_all(data)?;
    f.sync_all()?;

    Ok(())
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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

/// Maximum number of name-rankable candidates whose payloads may be opened by
/// one latest-bundle discovery. At the per-file caps above, the hard aggregate
/// read ceiling is below 8 MiB (40 × (64 KiB + 128 KiB), plus one byte per
/// file used for oversize detection, including the unranked fallback window).
const LATEST_CRASH_BUNDLE_CANDIDATE_WINDOW: usize = 32;
/// Public list requests retain at most this many results. Without a hard cap,
/// an adversarial `usize::MAX` request could turn the heap itself into the
/// unbounded operation this path is designed to avoid. At the maximum request,
/// the ranked result-plus-invalid slack and the legacy fallback can inspect at
/// most 104 candidates (208 payload opens and less than 20 MiB of payload
/// bytes, including oversize sentinels).
const MAX_CRASH_BUNDLE_LIST_RESULTS: usize = 64;
/// Extra ranked candidates inspected when malformed payloads occupy the newest
/// names. Discovery reports incompleteness instead of scanning indefinitely if
/// this reserve is exhausted before the requested result count is satisfied.
const CRASH_BUNDLE_LIST_INVALID_CANDIDATE_SLACK: usize = 32;
/// Malformed historical names cannot be ranked by their encoded timestamp.
/// Inspect only a small modification-time window and report incompleteness if
/// more exist rather than claiming that a bounded fallback is authoritative.
const LATEST_CRASH_BUNDLE_UNRANKED_WINDOW: usize = 8;

/// Finite reason why a bounded discovery result is not authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashBundleDiscoveryIncompleteReason {
    /// At least one directory entry or its file type could not be inspected.
    DirectoryEntryUnreadable,
    /// A matching directory's modification time could not be read, so an
    /// mtime-dependent tie-break or legacy-name rank could not be proven.
    CandidateMetadataUnreadable,
    /// A retained candidate or one of its payloads could not be opened or read;
    /// unlike malformed bytes, this does not prove that the candidate is invalid.
    CandidatePayloadUnreadable,
    /// Every retained ranked candidate was invalid and older candidates were
    /// excluded by the ranked payload-work window.
    RankedCandidateWindowExhausted,
    /// More legacy or malformed names existed than the bounded fallback window
    /// can inspect authoritatively.
    UnrankedCandidateWindowExceeded,
    /// The caller requested more results than the hard bounded result cap.
    RequestedLimitExceeded,
}

/// Whether bounded latest-bundle discovery produced an authoritative answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashBundleDiscoveryCompleteness {
    /// All candidates capable of changing the answer were inspected.
    Complete,
    /// The bounded search could not prove that its candidate was newest.
    Incomplete {
        /// Finite reasons why the result is not authoritative.
        reasons: Vec<CrashBundleDiscoveryIncompleteReason>,
    },
}

impl CrashBundleDiscoveryCompleteness {
    /// Return true only when bounded discovery proved its answer authoritative.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Bounded latest-bundle discovery result. Callers that need to label a bundle
/// as authoritative must require [`CrashBundleDiscoveryCompleteness::Complete`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestCrashBundleDiscovery {
    /// Best candidate found within the bounded search, if any.
    pub bundle: Option<CrashBundleSummary>,
    /// Whether callers may treat `bundle` or its absence as authoritative.
    pub completeness: CrashBundleDiscoveryCompleteness,
    /// Directory entries consumed from the streaming enumeration.
    pub directory_entries_examined: usize,
    /// Generated names whose timestamp and optional process suffix were valid.
    pub ranked_candidates: usize,
    /// Legacy or malformed names requiring payload/modification-time fallback.
    pub unranked_candidates: usize,
    /// Payload files successfully opened during bounded discovery.
    pub payload_files_opened: usize,
    /// Payload bytes read, including the one-byte oversize sentinel.
    pub payload_bytes_read: u64,
}

/// Typed result for a bounded multi-bundle discovery request.
///
/// `bundles` is a best candidate prefix even when `completeness` is
/// `Incomplete`; only callers that explicitly want best-effort diagnostics
/// should consume it in that state. Authority-bearing callers must require
/// [`CrashBundleDiscoveryCompleteness::Complete`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashBundleListDiscovery {
    /// Best candidate prefix retained by the bounded search.
    pub bundles: Vec<CrashBundleSummary>,
    /// Whether the returned prefix and its length are authoritative.
    pub completeness: CrashBundleDiscoveryCompleteness,
    /// Result count requested by the caller.
    pub requested_limit: usize,
    /// Result count actually permitted by the hard discovery cap.
    pub effective_limit: usize,
    /// Directory entries consumed from the streaming enumeration.
    pub directory_entries_examined: usize,
    /// Generated names whose timestamp and optional process suffix were valid.
    pub ranked_candidates: usize,
    /// Legacy or malformed names requiring payload/modification-time fallback.
    pub unranked_candidates: usize,
    /// Payload files successfully opened during bounded discovery.
    pub payload_files_opened: usize,
    /// Payload bytes read, including the one-byte oversize sentinel.
    pub payload_bytes_read: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RankedCrashBundleCandidate {
    timestamp: String,
    modified: Option<SystemTime>,
    process_sequence: Option<(u32, u64)>,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct UnrankedCrashBundleCandidate {
    modified: Option<SystemTime>,
    path: PathBuf,
}

#[derive(Debug)]
struct LoadedCrashBundleCandidate {
    summary: CrashBundleSummary,
    authority_timestamp: Option<String>,
    modified: Option<SystemTime>,
    process_sequence: Option<(u32, u64)>,
}

fn retain_newest_candidate<T: Ord>(heap: &mut BinaryHeap<Reverse<T>>, candidate: T, limit: usize) {
    if limit == 0 {
        return;
    }
    if heap.len() < limit {
        heap.push(Reverse(candidate));
        return;
    }
    if heap.peek().is_some_and(|oldest_retained| {
        candidate.cmp(&oldest_retained.0).is_gt()
    }) {
        heap.pop();
        heap.push(Reverse(candidate));
    }
}

fn crash_bundle_candidate_modified(
    modified: std::io::Result<SystemTime>,
    metadata_unreadable: &mut bool,
) -> Option<SystemTime> {
    match modified {
        Ok(modified) => Some(modified),
        Err(_) => {
            *metadata_unreadable = true;
            None
        }
    }
}

fn crash_bundle_name_key(path: &Path) -> Option<(String, Option<(u32, u64)>)> {
    let name = path.file_name()?.to_str()?;
    let suffix = name.strip_prefix("ft_crash_")?;
    let timestamp = suffix.get(..15)?;
    let bytes = timestamp.as_bytes();
    if bytes.get(8) != Some(&b'_')
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| index != 8 && !byte.is_ascii_digit())
    {
        return None;
    }
    let year: u16 = timestamp.get(0..4)?.parse().ok()?;
    let month: u8 = timestamp.get(4..6)?.parse().ok()?;
    let day: u8 = timestamp.get(6..8)?.parse().ok()?;
    let hour: u8 = timestamp.get(9..11)?.parse().ok()?;
    let minute: u8 = timestamp.get(11..13)?.parse().ok()?;
    let second: u8 = timestamp.get(13..15)?.parse().ok()?;
    if year == 0 {
        return None;
    }
    let leap_year =
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let process_suffix = suffix.get(15..)?;
    let process_sequence = if process_suffix.is_empty() {
        None
    } else {
        let (process_id, sequence) = process_suffix.strip_prefix("_p")?.rsplit_once('_')?;
        Some((process_id.parse().ok()?, sequence.parse().ok()?))
    };
    Some((timestamp.to_string(), process_sequence))
}

fn crash_bundle_timestamp_iso8601(timestamp: &str) -> Option<String> {
    Some(format!(
        "{}-{}-{}T{}:{}:{}Z",
        timestamp.get(0..4)?,
        timestamp.get(4..6)?,
        timestamp.get(6..8)?,
        timestamp.get(9..11)?,
        timestamp.get(11..13)?,
        timestamp.get(13..15)?,
    ))
}

fn read_crash_bundle_summary_from_dir_bounded(
    bundle_dir: &CapDir,
    path: PathBuf,
    expected_timestamp: Option<&str>,
    stats: &mut CrashBundlePayloadReadStats,
) -> Option<CrashBundleSummary> {
    let manifest_path = path.join("manifest.json");
    let mut manifest = read_optional_json_from_bundle_dir_bounded::<CrashManifest>(
        bundle_dir,
        &path,
        &manifest_path,
        "manifest_read_fail",
        "manifest_parse_fail",
        MAX_CRASH_MANIFEST_JSON_READ_BYTES,
        stats,
    );
    let report_path = path.join("crash_report.json");
    let mut report = read_optional_json_from_bundle_dir_bounded::<CrashReport>(
        bundle_dir,
        &path,
        &report_path,
        "report_read_fail",
        "report_parse_fail",
        MAX_CRASH_REPORT_JSON_READ_BYTES,
        stats,
    );

    if let Some(timestamp) = expected_timestamp {
        if manifest.as_ref().is_some_and(|manifest| {
            crash_bundle_timestamp_iso8601(timestamp).as_deref()
                != Some(manifest.created_at.as_str())
        }) {
            let error = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "crash manifest timestamp does not match its generated directory name",
            );
            record_crash_bundle_parse_drop(&path, "manifest_name_mismatch", &error);
            manifest = None;
        }
        if report
            .as_ref()
            .is_some_and(|report| format_timestamp(report.timestamp) != timestamp)
        {
            let error = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "crash report timestamp does not match its generated directory name",
            );
            record_crash_bundle_parse_drop(&path, "report_name_mismatch", &error);
            report = None;
        }
    }

    if manifest.is_none() && report.is_none() {
        None
    } else {
        Some(CrashBundleSummary {
            path,
            manifest,
            report,
        })
    }
}

fn read_crash_bundle_summary_from_root_bounded(
    crash_root: &CapDir,
    path: PathBuf,
    expected_timestamp: Option<&str>,
    stats: &mut CrashBundlePayloadReadStats,
) -> Option<CrashBundleSummary> {
    let name = path.file_name()?;
    let bundle_dir = match crash_root.open_dir_nofollow(name) {
        Ok(bundle_dir) => bundle_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            stats.authority_unreadable |= crash_bundle_io_error_withholds_authority(&error);
            record_crash_bundle_parse_drop(&path, "bundle_dir_open_fail", &error);
            return None;
        }
    };
    read_crash_bundle_summary_from_dir_bounded(
        &bundle_dir,
        path,
        expected_timestamp,
        stats,
    )
}

fn crash_bundle_process_sequence(path: &Path) -> Option<(u32, u64)> {
    let name = path.file_name()?.to_str()?;
    let (process_prefix, sequence) = name.rsplit_once('_')?;
    let (_, process_id) = process_prefix.rsplit_once("_p")?;
    Some((process_id.parse().ok()?, sequence.parse().ok()?))
}

fn compare_loaded_crash_bundle_order(
    a: &LoadedCrashBundleCandidate,
    b: &LoadedCrashBundleCandidate,
) -> std::cmp::Ordering {
    b.authority_timestamp
        .cmp(&a.authority_timestamp)
        .then_with(|| b.modified.cmp(&a.modified))
        .then_with(|| b.process_sequence.cmp(&a.process_sequence))
        .then_with(|| b.summary.path.cmp(&a.summary.path))
}

fn unranked_crash_bundle_authority_timestamp(summary: &CrashBundleSummary) -> Option<String> {
    summary
        .report
        .as_ref()
        .map(|report| format_timestamp(report.timestamp))
}

fn discover_crash_bundles_bounded(
    crash_dir: &Path,
    requested_limit: usize,
    maximum_results: usize,
    ranked_candidate_window: usize,
) -> CrashBundleListDiscovery {
    let effective_limit = requested_limit.min(maximum_results);
    let mut result = CrashBundleListDiscovery {
        bundles: Vec::new(),
        completeness: CrashBundleDiscoveryCompleteness::Complete,
        requested_limit,
        effective_limit,
        directory_entries_examined: 0,
        ranked_candidates: 0,
        unranked_candidates: 0,
        payload_files_opened: 0,
        payload_bytes_read: 0,
    };
    if requested_limit == 0 {
        return result;
    }

    let mut incomplete_reasons = Vec::new();
    if requested_limit > maximum_results {
        incomplete_reasons.push(CrashBundleDiscoveryIncompleteReason::RequestedLimitExceeded);
    }
    let crash_root = match CapDir::open_ambient_dir(crash_dir, cap_std::ambient_authority()) {
        Ok(crash_root) => crash_root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !incomplete_reasons.is_empty() {
                result.completeness = CrashBundleDiscoveryCompleteness::Incomplete {
                    reasons: incomplete_reasons,
                };
            }
            return result;
        }
        Err(_) => {
            incomplete_reasons
                .push(CrashBundleDiscoveryIncompleteReason::DirectoryEntryUnreadable);
            result.completeness =
                CrashBundleDiscoveryCompleteness::Incomplete { reasons: incomplete_reasons };
            return result;
        }
    };
    let entries = match crash_root.entries() {
        Ok(entries) => entries,
        Err(_) => {
            incomplete_reasons
                .push(CrashBundleDiscoveryIncompleteReason::DirectoryEntryUnreadable);
            result.completeness =
                CrashBundleDiscoveryCompleteness::Incomplete { reasons: incomplete_reasons };
            return result;
        }
    };

    let mut ranked = BinaryHeap::new();
    let mut unranked = BinaryHeap::new();
    let mut entry_unreadable = false;
    let mut metadata_unreadable = false;
    for entry in entries {
        result.directory_entries_examined = result.directory_entries_examined.saturating_add(1);
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                entry_unreadable = true;
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                entry_unreadable = true;
                continue;
            }
        };
        if !file_type.is_dir()
            || !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("ft_crash_"))
        {
            continue;
        }

        let path = crash_dir.join(entry.file_name());
        let modified = crash_bundle_candidate_modified(
            entry.metadata().and_then(|metadata| {
                metadata
                    .modified()
                    .map(cap_std::time::SystemTime::into_std)
            }),
            &mut metadata_unreadable,
        );
        if let Some((timestamp, process_sequence)) = crash_bundle_name_key(&path) {
            result.ranked_candidates = result.ranked_candidates.saturating_add(1);
            retain_newest_candidate(
                &mut ranked,
                RankedCrashBundleCandidate {
                    timestamp,
                    modified,
                    process_sequence,
                    path,
                },
                ranked_candidate_window,
            );
        } else {
            result.unranked_candidates = result.unranked_candidates.saturating_add(1);
            retain_newest_candidate(
                &mut unranked,
                UnrankedCrashBundleCandidate { modified, path },
                LATEST_CRASH_BUNDLE_UNRANKED_WINDOW,
            );
        }
    }

    let mut payload_stats = CrashBundlePayloadReadStats::default();
    let mut loaded = Vec::with_capacity(
        effective_limit.saturating_add(LATEST_CRASH_BUNDLE_UNRANKED_WINDOW),
    );
    let mut ranked_candidates = ranked
        .into_iter()
        .map(|candidate| candidate.0)
        .collect::<Vec<_>>();
    ranked_candidates.sort_unstable_by(|a, b| b.cmp(a));
    let mut ranked_loaded = 0_usize;
    for candidate in ranked_candidates {
        if ranked_loaded >= effective_limit {
            break;
        }
        let authority_timestamp = candidate.timestamp.clone();
        if let Some(summary) = read_crash_bundle_summary_from_root_bounded(
            &crash_root,
            candidate.path,
            Some(&candidate.timestamp),
            &mut payload_stats,
        ) {
            loaded.push(LoadedCrashBundleCandidate {
                summary,
                authority_timestamp: Some(authority_timestamp),
                modified: candidate.modified,
                process_sequence: candidate.process_sequence,
            });
            ranked_loaded = ranked_loaded.saturating_add(1);
        }
    }

    let mut unranked_candidates = unranked
        .into_iter()
        .map(|candidate| candidate.0)
        .collect::<Vec<_>>();
    unranked_candidates.sort_unstable_by(|a, b| b.cmp(a));
    for candidate in unranked_candidates {
        if let Some(summary) = read_crash_bundle_summary_from_root_bounded(
            &crash_root,
            candidate.path,
            None,
            &mut payload_stats,
        )
        {
            let process_sequence = crash_bundle_process_sequence(&summary.path);
            loaded.push(LoadedCrashBundleCandidate {
                authority_timestamp: unranked_crash_bundle_authority_timestamp(&summary),
                summary,
                modified: candidate.modified,
                process_sequence,
            });
        }
    }

    loaded.sort_by(compare_loaded_crash_bundle_order);
    loaded.truncate(effective_limit);
    result.bundles = loaded
        .into_iter()
        .map(|candidate| candidate.summary)
        .collect();
    result.payload_files_opened = payload_stats.files_opened;
    result.payload_bytes_read = payload_stats.bytes_read;

    if entry_unreadable {
        incomplete_reasons.push(CrashBundleDiscoveryIncompleteReason::DirectoryEntryUnreadable);
    }
    if metadata_unreadable {
        incomplete_reasons
            .push(CrashBundleDiscoveryIncompleteReason::CandidateMetadataUnreadable);
    }
    if payload_stats.authority_unreadable {
        incomplete_reasons
            .push(CrashBundleDiscoveryIncompleteReason::CandidatePayloadUnreadable);
    }
    if ranked_loaded < effective_limit
        && result.ranked_candidates > ranked_candidate_window
    {
        incomplete_reasons
            .push(CrashBundleDiscoveryIncompleteReason::RankedCandidateWindowExhausted);
    }
    if result.unranked_candidates > LATEST_CRASH_BUNDLE_UNRANKED_WINDOW {
        incomplete_reasons
            .push(CrashBundleDiscoveryIncompleteReason::UnrankedCandidateWindowExceeded);
    }
    if !incomplete_reasons.is_empty() {
        result.completeness = CrashBundleDiscoveryCompleteness::Incomplete {
            reasons: incomplete_reasons,
        };
    }
    result
}

/// Discover up to `limit` crash bundles with bounded payload work.
///
/// Directory enumeration remains O(entry count), because no correct filesystem
/// query can identify the newest names without examining every name. Selection
/// memory, payload-file count, and payload bytes are hard bounded. A request
/// above 64 results or a candidate set that exhausts a fallback window returns
/// a typed incomplete result rather than silently claiming an authoritative
/// prefix.
#[must_use]
pub fn discover_crash_bundles(crash_dir: &Path, limit: usize) -> CrashBundleListDiscovery {
    let effective_limit = limit.min(MAX_CRASH_BUNDLE_LIST_RESULTS);
    discover_crash_bundles_bounded(
        crash_dir,
        limit,
        MAX_CRASH_BUNDLE_LIST_RESULTS,
        effective_limit.saturating_add(CRASH_BUNDLE_LIST_INVALID_CANDIDATE_SLACK),
    )
}

/// List crash bundles in `crash_dir`, sorted newest first.
///
/// This compatibility wrapper is fail closed: it returns results only when the
/// typed bounded search proves the whole requested prefix authoritative. Use
/// [`discover_crash_bundles`] when a diagnostic surface can explicitly display
/// and consume an incomplete best-effort prefix.
#[must_use]
pub fn list_crash_bundles(crash_dir: &Path, limit: usize) -> Vec<CrashBundleSummary> {
    let discovery = discover_crash_bundles(crash_dir, limit);
    if discovery.completeness.is_complete() {
        discovery.bundles
    } else {
        record_crash_bundle_discovery_incomplete(
            "list",
            &discovery.completeness,
            discovery.directory_entries_examined,
            discovery.ranked_candidates,
            discovery.unranked_candidates,
            discovery.payload_files_opened,
            discovery.payload_bytes_read,
        );
        Vec::new()
    }
}

/// Discover the most recent crash bundle with bounded payload work.
///
/// Generated directory names are the timestamp authority and payload
/// timestamps must agree. The returned candidate is best effort when
/// `completeness` is incomplete; authority-bearing callers must check it.
#[must_use]
pub fn discover_latest_crash_bundle(crash_dir: &Path) -> LatestCrashBundleDiscovery {
    let mut discovery = discover_crash_bundles_bounded(
        crash_dir,
        1,
        1,
        LATEST_CRASH_BUNDLE_CANDIDATE_WINDOW,
    );
    LatestCrashBundleDiscovery {
        bundle: discovery.bundles.pop(),
        completeness: discovery.completeness,
        directory_entries_examined: discovery.directory_entries_examined,
        ranked_candidates: discovery.ranked_candidates,
        unranked_candidates: discovery.unranked_candidates,
        payload_files_opened: discovery.payload_files_opened,
        payload_bytes_read: discovery.payload_bytes_read,
    }
}

/// Get the most recent crash bundle, if exact bounded discovery can prove it.
#[must_use]
pub fn latest_crash_bundle(crash_dir: &Path) -> Option<CrashBundleSummary> {
    let discovery = discover_latest_crash_bundle(crash_dir);
    if discovery.completeness.is_complete() {
        discovery.bundle
    } else {
        record_crash_bundle_discovery_incomplete(
            "latest",
            &discovery.completeness,
            discovery.directory_entries_examined,
            discovery.ranked_candidates,
            discovery.unranked_candidates,
            discovery.payload_files_opened,
            discovery.payload_bytes_read,
        );
        None
    }
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
    /// ft version
    pub wa_version: String,
    /// Timestamp of export
    pub exported_at: String,
    /// Optional swarm-triage extension manifest for read-only source provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm: Option<SwarmIncidentBundleManifest>,
}

/// Read-only swarm-triage extension metadata for incident bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmIncidentBundleManifest {
    /// Stable extension contract id.
    pub contract_id: String,
    /// Extension schema version retained for machine consumers.
    pub schema_version: u32,
    /// Human-readable extension format version.
    pub format_version: String,
    /// Unique bundle identifier derived from the bundle directory name.
    pub bundle_id: String,
    /// Incident kind represented by this bundle.
    pub kind: IncidentKind,
    /// UTC timestamp for bundle creation.
    pub created_at: String,
    /// Generator metadata for the collector that produced the bundle.
    pub generator: IncidentBundleGenerator,
    /// Privacy budget applied by the collector.
    pub privacy_budget: IncidentPrivacyBudget,
    /// Read-only collection policy that governed this bundle.
    pub collection_policy: IncidentCollectionPolicy,
    /// Host/runtime summary that does not contain secrets.
    pub environment: IncidentEnvironmentSummary,
    /// Per-source collection status and provenance.
    pub sources: Vec<IncidentSourceEntry>,
    /// Structured warnings emitted during partial collection.
    pub warnings: Vec<IncidentBundleWarning>,
    /// Redaction counts for the bundle.
    pub redaction_summary: IncidentRedactionSummary,
    /// Total bytes written for files tracked by the manifest.
    pub total_size_bytes: u64,
}

/// Metadata about the collector that produced a swarm incident bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentBundleGenerator {
    /// `ft` version embedded in the binary.
    pub ft_version: String,
    /// Git commit if it was embedded at build time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// Coarse host class that avoids leaking hostnames.
    pub hostname_class: String,
    /// Operating system family reported by Rust.
    pub os: String,
    /// CPU architecture reported by Rust.
    pub arch: String,
    /// API surface used to build the bundle.
    pub source_surface: String,
}

impl IncidentBundleGenerator {
    fn current() -> Self {
        Self {
            ft_version: crate::VERSION.to_string(),
            git_commit: option_env!("VERGEN_GIT_SHA")
                .or(option_env!("GIT_COMMIT"))
                .map(str::to_string),
            hostname_class: "local".to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            source_surface: "frankenterm_core::crash::collect_incident_bundle".to_string(),
        }
    }
}

/// Privacy limits applied during bundle collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentPrivacyBudget {
    /// Sharing tier applied to the default collector.
    pub tier: String,
    /// Maximum bytes read from a config source before truncation.
    pub config_summary_max_bytes: u64,
    /// Maximum recent event summaries requested.
    pub max_events: usize,
    /// Whether pane text collection was permitted.
    pub pane_text_allowed: bool,
    /// Whether process sampling was permitted.
    pub process_sample_allowed: bool,
}

impl IncidentPrivacyBudget {
    fn default_for_process_sampling(max_events: usize, process_sample_allowed: bool) -> Self {
        Self {
            tier: "default".to_string(),
            config_summary_max_bytes: 64 * 1024,
            max_events,
            pane_text_allowed: false,
            process_sample_allowed,
        }
    }
}

/// Counts describing redaction work performed for the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentRedactionSummary {
    /// Total number of redactions across all files.
    pub total_redactions: usize,
    /// Number of files that had at least one redaction.
    pub redacted_files: usize,
}

/// Non-mutating policy applied by the read-only incident collector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentCollectionPolicy {
    /// Whether the collector is allowed to mutate external state.
    pub mutating_actions_allowed: bool,
    /// Pane text collection mode.
    pub pane_text_allowed: String,
    /// Process sampler mode.
    pub process_sampler: String,
    /// Whether Agent Mail repair/restart actions are allowed.
    pub agent_mail_repair_allowed: bool,
    /// Maximum time any future external source should spend collecting.
    pub source_timeout_ms: u64,
}

impl Default for IncidentCollectionPolicy {
    fn default() -> Self {
        Self::with_process_sampler("disabled")
    }
}

impl IncidentCollectionPolicy {
    fn with_process_sampler(process_sampler: &str) -> Self {
        Self {
            mutating_actions_allowed: false,
            pane_text_allowed: "disabled".to_string(),
            process_sampler: process_sampler.to_string(),
            agent_mail_repair_allowed: false,
            source_timeout_ms: 5_000,
        }
    }
}

/// Secret-safe host/runtime summary for a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentEnvironmentSummary {
    /// Operating system family reported by Rust.
    pub os: String,
    /// CPU architecture reported by Rust.
    pub arch: String,
    /// Whether the collector could observe a current directory without recording it.
    pub current_dir_available: bool,
}

impl IncidentEnvironmentSummary {
    fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            current_dir_available: std::env::current_dir().is_ok(),
        }
    }
}

/// Status of one source in the read-only incident collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentSourceStatus {
    /// Source payload was collected and written.
    Collected,
    /// Source was intentionally skipped by policy or options.
    Skipped,
    /// Source was not available in the current environment.
    Unavailable,
    /// Source collection was attempted and failed.
    Failed,
    /// Source was available but outside the freshness budget.
    Stale,
}

/// Evidence quality for one incident-bundle source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentEvidenceState {
    /// Evidence came from a measured live or persisted source.
    Measured,
    /// Evidence is synthetic or simulated.
    Simulated,
    /// No evidence was available.
    Unavailable,
    /// Evidence existed but was too old for the freshness budget.
    Stale,
    /// Evidence combines measured and unavailable fields.
    Mixed,
}

/// Redaction state for a source payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentRedactionState {
    /// No redaction was required.
    None,
    /// Some content was redacted.
    Partial,
    /// The whole source was withheld or fully redacted.
    Full,
    /// Redaction does not apply because no payload was written.
    NotApplicable,
}

/// Per-source provenance and degradation state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentSourceEntry {
    /// Stable source identifier.
    pub name: String,
    /// Bundle-relative payload path when a payload was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Collection result for this source.
    pub status: IncidentSourceStatus,
    /// Evidence quality for this source.
    pub evidence_state: IncidentEvidenceState,
    /// Command or API surface used for collection.
    pub source_surface: String,
    /// Whether collection mutated external state.
    pub mutates_state: bool,
    /// Source generation timestamp when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    /// Age of the source in milliseconds when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_ms: Option<u64>,
    /// Freshness budget in milliseconds when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_age_ms: Option<u64>,
    /// Redaction state for the source payload.
    pub redaction: IncidentRedactionState,
    /// Privacy tier applied to this source.
    pub privacy_tier: String,
    /// Bytes written for the source payload.
    pub size_bytes: u64,
    /// Collection elapsed time in milliseconds.
    pub elapsed_ms: u64,
    /// Warning ids that explain partial or degraded collection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warning_ids: Vec<String>,
}

/// Structured warning emitted when a source degrades instead of aborting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentBundleWarning {
    /// Stable warning identifier.
    pub id: String,
    /// Warning severity.
    pub severity: String,
    /// Source that emitted this warning, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Human-readable warning message.
    pub message: String,
}

/// Bounded process-sampler command used by opt-in incident bundles.
#[derive(Debug, Clone)]
pub struct IncidentProcessSamplerConfig {
    /// Maximum wall-clock time spent waiting for the sampler command.
    pub timeout_ms: u64,
    /// Privacy tier recorded on the process-sample source.
    pub privacy_tier: String,
    program: IncidentProcessSamplerProgram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncidentProcessSamplerProgram {
    Ps,
    MissingToolForTest,
}

impl IncidentProcessSamplerConfig {
    /// Build the default read-only `ps` snapshot sampler.
    #[must_use]
    pub fn ps_snapshot(timeout_ms: u64) -> Self {
        Self {
            timeout_ms,
            privacy_tier: "default".to_string(),
            program: IncidentProcessSamplerProgram::Ps,
        }
    }

    /// Build a deterministic missing-tool sampler for tests.
    #[doc(hidden)]
    #[must_use]
    pub fn missing_tool_for_test(timeout_ms: u64) -> Self {
        Self {
            timeout_ms,
            privacy_tier: "default".to_string(),
            program: IncidentProcessSamplerProgram::MissingToolForTest,
        }
    }

    fn source_surface(&self) -> String {
        format!("bounded process sampler command {}", self.program.label())
    }

    fn command_args(&self) -> &'static [&'static str] {
        self.program.args()
    }
}

impl IncidentProcessSamplerProgram {
    fn label(self) -> &'static str {
        match self {
            Self::Ps => "ps",
            Self::MissingToolForTest => "__ft_missing_process_sampler__",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Ps => &["-axo", "pid=,ppid=,rss=,vsz=,comm="],
            Self::MissingToolForTest => &[],
        }
    }
}

/// Captured process-sampler payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentProcessSample {
    /// Operating-system family reported by Rust.
    pub platform: String,
    /// CPU architecture reported by Rust.
    pub arch: String,
    /// ISO-8601 UTC timestamp for this sample.
    pub sampled_at: String,
    /// Sampler timeout budget.
    pub timeout_ms: u64,
    /// Command label used to collect the sample.
    pub collector: String,
    /// Parsed process rows from the snapshot.
    pub processes: Vec<IncidentProcessRow>,
    /// Memory categories distinguished by this platform/sample.
    pub memory_categories: Vec<IncidentProcessMemoryCategory>,
}

/// One row in a process sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentProcessRow {
    /// Process id.
    pub pid: u32,
    /// Parent process id, when reported.
    pub parent_pid: Option<u32>,
    /// Resident memory in bytes, when reported.
    pub resident_bytes: Option<u64>,
    /// Virtual memory in bytes, when reported.
    pub virtual_bytes: Option<u64>,
    /// Sanitized executable label.
    pub command: String,
}

/// Memory category summary for process-sampler output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentProcessMemoryCategory {
    /// Stable category id.
    pub category: String,
    /// Byte total for measured categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Collection status for this category.
    pub status: IncidentSourceStatus,
    /// Evidence state for this category.
    pub evidence_state: IncidentEvidenceState,
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

    // 3. Write incident manifest. The manifest is part of the bundle and lists
    // itself, so compute total_size_bytes to a fixed point before writing it.
    let mut manifest_files = files.clone();
    manifest_files.push("incident_manifest.json".to_string());
    let mut result = IncidentBundleResult {
        path: bundle_dir.clone(),
        kind,
        files: manifest_files,
        total_size_bytes: total_size,
        wa_version: crate::VERSION.to_string(),
        exported_at: format_iso8601(ts),
        swarm: None,
    };

    let mut manifest_result = result.clone();
    manifest_result.path = PathBuf::from(&bundle_name);
    let manifest_json = loop {
        let manifest_json =
            serde_json::to_string_pretty(&manifest_result).map_err(std::io::Error::other)?;
        let next_total = total_size.saturating_add(manifest_json.len() as u64);
        if next_total == manifest_result.total_size_bytes {
            break manifest_json;
        }
        manifest_result.total_size_bytes = next_total;
    };
    result.total_size_bytes = manifest_result.total_size_bytes;
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
    /// Schema version (from ft_meta/wa_meta table)
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

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn bundle_file_size(bundle_dir: &Path, name: &str) -> u64 {
    fs::metadata(bundle_dir.join(name)).map_or(0, |metadata| metadata.len())
}

fn redaction_state_for_file(
    redaction_entries: &[FileRedactionEntry],
    name: &str,
) -> IncidentRedactionState {
    if redaction_entries.iter().any(|entry| entry.file == name) {
        IncidentRedactionState::Partial
    } else {
        IncidentRedactionState::None
    }
}

fn record_file_redactions(
    redaction_entries: &mut Vec<FileRedactionEntry>,
    name: &str,
    count: usize,
) {
    if count == 0 {
        return;
    }
    if let Some(entry) = redaction_entries
        .iter_mut()
        .find(|entry| entry.file == name)
    {
        entry.count = entry.count.saturating_add(count);
    } else {
        redaction_entries.push(FileRedactionEntry {
            file: name.to_string(),
            count,
        });
    }
}

fn sanitize_manifest_source_text(
    text: &str,
    source_file: &str,
    redactor: &Redactor,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> String {
    let redaction_count = redactor.detect(text).len();
    if redaction_count == 0 {
        return text.to_string();
    }
    record_file_redactions(redaction_entries, source_file, redaction_count);
    record_file_redactions(redaction_entries, "incident_manifest.json", redaction_count);
    redactor.redact(text)
}

fn sanitize_incident_manifest_fields_for_payload(
    sources: &mut [IncidentSourceEntry],
    warnings: &mut [IncidentBundleWarning],
    redactor: &Redactor,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) {
    let mut manifest_redactions = 0_usize;
    for source in sources {
        let redaction_count = redactor.detect(&source.source_surface).len();
        if redaction_count > 0 {
            source.source_surface = redactor.redact(&source.source_surface);
            manifest_redactions = manifest_redactions.saturating_add(redaction_count);
        }
    }

    let mut warning_redactions = 0_usize;
    for warning in warnings {
        let redaction_count = redactor.detect(&warning.message).len();
        if redaction_count > 0 {
            warning.message = redactor.redact(&warning.message);
            manifest_redactions = manifest_redactions.saturating_add(redaction_count);
            warning_redactions = warning_redactions.saturating_add(redaction_count);
        }
    }

    record_file_redactions(
        redaction_entries,
        "incident_manifest.json",
        manifest_redactions,
    );
    record_file_redactions(redaction_entries, "warnings.jsonl", warning_redactions);
}

fn incident_warning(id: &str, source: &str, message: String) -> IncidentBundleWarning {
    IncidentBundleWarning {
        id: id.to_string(),
        severity: "warning".to_string(),
        source: Some(source.to_string()),
        message,
    }
}

fn skipped_source(
    name: &str,
    source_surface: impl Into<String>,
    max_age_ms: Option<u64>,
    elapsed_ms: u64,
    warning_id: &str,
    message: String,
    warnings: &mut Vec<IncidentBundleWarning>,
) -> IncidentSourceEntry {
    warnings.push(incident_warning(warning_id, name, message));
    IncidentSourceEntry {
        name: name.to_string(),
        file: None,
        status: IncidentSourceStatus::Skipped,
        evidence_state: IncidentEvidenceState::Unavailable,
        source_surface: source_surface.into(),
        mutates_state: false,
        generated_at: None,
        freshness_ms: None,
        max_age_ms,
        redaction: IncidentRedactionState::NotApplicable,
        privacy_tier: "default".to_string(),
        size_bytes: 0,
        elapsed_ms,
        warning_ids: vec![warning_id.to_string()],
    }
}

// Degraded incident sources preserve the failed source metadata as an audit row.
#[allow(clippy::too_many_arguments)]
fn degraded_source(
    name: &str,
    status: IncidentSourceStatus,
    source_surface: impl Into<String>,
    max_age_ms: Option<u64>,
    elapsed_ms: u64,
    warning_id: &str,
    message: String,
    warnings: &mut Vec<IncidentBundleWarning>,
) -> IncidentSourceEntry {
    warnings.push(incident_warning(warning_id, name, message));
    IncidentSourceEntry {
        name: name.to_string(),
        file: None,
        status,
        evidence_state: IncidentEvidenceState::Unavailable,
        source_surface: source_surface.into(),
        mutates_state: false,
        generated_at: None,
        freshness_ms: None,
        max_age_ms,
        redaction: IncidentRedactionState::NotApplicable,
        privacy_tier: "default".to_string(),
        size_bytes: 0,
        elapsed_ms,
        warning_ids: vec![warning_id.to_string()],
    }
}

struct IncidentJsonSourceMeta<'a> {
    name: &'a str,
    file: &'a str,
    source_surface: &'a str,
    evidence_state: IncidentEvidenceState,
    max_age_ms: Option<u64>,
    started: Instant,
}

// Incident JSON sources keep payload, path, timing, and status fields explicit.
#[allow(clippy::too_many_arguments)]
fn write_incident_json_source(
    meta: IncidentJsonSourceMeta<'_>,
    payload: &serde_json::Value,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<IncidentSourceEntry> {
    if let Some(parent) = Path::new(meta.file).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(bundle_dir.join(parent))?;
    }
    let json = serde_json::to_string_pretty(payload).map_err(std::io::Error::other)?;
    write_redacted_file(
        meta.file,
        &json,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    Ok(IncidentSourceEntry {
        name: meta.name.to_string(),
        file: Some(meta.file.to_string()),
        status: IncidentSourceStatus::Collected,
        evidence_state: meta.evidence_state,
        source_surface: meta.source_surface.to_string(),
        mutates_state: false,
        generated_at: Some(exported_at.to_string()),
        freshness_ms: Some(0),
        max_age_ms: meta.max_age_ms,
        redaction: redaction_state_for_file(redaction_entries, meta.file),
        privacy_tier: "default".to_string(),
        size_bytes: bundle_file_size(bundle_dir, meta.file),
        elapsed_ms: elapsed_ms(meta.started),
        warning_ids: Vec::new(),
    })
}

#[derive(Debug)]
struct StoredIncidentPaneRow {
    pane_id: i64,
    pane_uuid: Option<String>,
    domain: String,
    window_id: Option<i64>,
    tab_id: Option<i64>,
    title: Option<String>,
    cwd: Option<String>,
    first_seen_at: i64,
    last_seen_at: i64,
    observed: i64,
    ignore_reason: Option<String>,
}

fn incident_db_panes_source_surface(db_path: &Path) -> String {
    format!("rusqlite read-only panes table {}", db_path.display())
}

fn nonnegative_i64_to_u64(value: i64, column: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("{column} contained negative value {value}"))
}

fn non_empty_string(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

fn stored_incident_pane_to_robot_state(
    row: StoredIncidentPaneRow,
) -> Result<IncidentRobotPaneState, String> {
    let pane_id = nonnegative_i64_to_u64(row.pane_id, "panes.pane_id")?;
    let window_id = row
        .window_id
        .map(|value| nonnegative_i64_to_u64(value, "panes.window_id"))
        .transpose()?
        .unwrap_or(0);
    let tab_id = row
        .tab_id
        .map(|value| nonnegative_i64_to_u64(value, "panes.tab_id"))
        .transpose()?
        .unwrap_or(0);
    let first_seen_at = nonnegative_i64_to_u64(row.first_seen_at, "panes.first_seen_at")?;
    let last_seen_at = nonnegative_i64_to_u64(row.last_seen_at, "panes.last_seen_at")?;
    let observed = row.observed != 0;
    let mut pane = IncidentRobotPaneState::new(pane_id, tab_id, window_id, row.domain, observed)
        .with_timestamps(Some(first_seen_at), Some(last_seen_at));

    pane.pane_uuid = non_empty_string(row.pane_uuid);
    if let Some(title) = non_empty_string(row.title) {
        pane = pane.with_title(title);
    }
    if let Some(cwd) = non_empty_string(row.cwd) {
        pane = pane.with_cwd(cwd);
    }
    if let Some(ignore_reason) = non_empty_string(row.ignore_reason) {
        pane = pane.with_ignore_reason(ignore_reason);
    }

    Ok(pane)
}

fn load_incident_robot_panes_from_db(
    db_path: &Path,
) -> Result<Vec<IncidentRobotPaneState>, String> {
    if !db_path.exists() {
        return Err(format!(
            "incident DB path does not exist: {}",
            db_path.display()
        ));
    }
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("failed to open incident DB read-only: {error}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT pane_id, pane_uuid, domain, window_id, tab_id, title, cwd,
             first_seen_at, last_seen_at, observed, ignore_reason
             FROM panes
             ORDER BY last_seen_at DESC, pane_id ASC
             LIMIT 500",
        )
        .map_err(|error| format!("failed to prepare panes query: {error}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(StoredIncidentPaneRow {
                pane_id: row.get(0)?,
                pane_uuid: row.get(1)?,
                domain: row.get(2)?,
                window_id: row.get(3)?,
                tab_id: row.get(4)?,
                title: row.get(5)?,
                cwd: row.get(6)?,
                first_seen_at: row.get(7)?,
                last_seen_at: row.get(8)?,
                observed: row.get(9)?,
                ignore_reason: row.get(10)?,
            })
        })
        .map_err(|error| format!("failed to query panes: {error}"))?;

    let rows: Result<Vec<_>, _> = rows.collect();
    rows.map_err(|error| format!("failed to decode pane row: {error}"))?
        .into_iter()
        .map(stored_incident_pane_to_robot_state)
        .collect()
}

fn incident_robot_state_snapshot_from_db(
    db_path: &Path,
) -> Result<IncidentRobotStateSnapshot, String> {
    Ok(IncidentRobotStateSnapshot::new(
        epoch_millis(),
        incident_db_panes_source_surface(db_path),
        load_incident_robot_panes_from_db(db_path)?,
    ))
}

fn incident_pane_text_summaries_snapshot_from_db(
    db_path: &Path,
) -> Result<IncidentPaneTextSummariesSnapshot, String> {
    let reason = "incident DB fallback exports pane ids only because the default incident privacy policy forbids pane text collection";
    let panes = load_incident_robot_panes_from_db(db_path)?
        .into_iter()
        .map(|pane| IncidentPaneTextSummary::excluded(pane.pane_id, 0, reason))
        .collect();
    Ok(IncidentPaneTextSummariesSnapshot::new(
        epoch_millis(),
        format!(
            "{} + incident privacy policy pane_text_allowed=false",
            incident_db_panes_source_surface(db_path)
        ),
        0,
        0,
        false,
        panes,
    )
    .with_privacy_reason(reason))
}

// Swarm incident capture assembles each evidence source from shared bundle context.
#[allow(clippy::too_many_arguments)]
fn add_swarm_incident_sources(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
    db_path: Option<&Path>,
) -> std::io::Result<()> {
    add_robot_state_source_with_db(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
        db_path,
    )?;
    add_pane_text_summaries_source_with_db(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
        db_path,
    )?;
    add_tailer_capture_health_source(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    add_resource_pressure_source(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    add_proof_rch_evidence_source(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    add_beads_coordination_source(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    add_git_dirty_tree_source(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    add_agent_mail_source(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    Ok(())
}

// Robot state evidence needs bundle context, policy, and runtime metadata together.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn add_robot_state_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    add_robot_state_source_with_db(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
        None,
    )
}

// Robot state evidence can fall back to the read-only persisted pane inventory.
#[allow(clippy::too_many_arguments)]
fn add_robot_state_source_with_db(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
    db_path: Option<&Path>,
) -> std::io::Result<()> {
    let started = Instant::now();
    let snapshot = IncidentRobotStateSnapshot::get_global()
        .map(Ok)
        .or_else(|| db_path.map(incident_robot_state_snapshot_from_db));
    let Some(snapshot) = snapshot else {
        sources.push(degraded_source(
            "robot_state",
            IncidentSourceStatus::Unavailable,
            "IncidentRobotStateSnapshot::get_global",
            Some(30_000),
            elapsed_ms(started),
            "robot_state.snapshot_unavailable",
            "no text-free robot-state snapshot has been published in this process".to_string(),
            warnings,
        ));
        return Ok(());
    };
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(error) => {
            sources.push(degraded_source(
                "robot_state",
                IncidentSourceStatus::Failed,
                db_path
                    .map(incident_db_panes_source_surface)
                    .unwrap_or_else(|| "IncidentRobotStateSnapshot::get_global".to_string()),
                Some(30_000),
                elapsed_ms(started),
                "robot_state.db_read_failed",
                error,
                warnings,
            ));
            return Ok(());
        }
    };
    let source_surface_raw = if snapshot.source_surface.trim().is_empty() {
        "IncidentRobotStateSnapshot::get_global".to_string()
    } else {
        snapshot.source_surface.clone()
    };
    let source_surface = sanitize_manifest_source_text(
        &source_surface_raw,
        "sources/robot_state.json",
        redactor,
        redaction_entries,
    );
    let freshness_ms = epoch_millis().saturating_sub(snapshot.captured_at_ms);
    let pane_count = snapshot.panes.len();
    let observed_count = snapshot.panes.iter().filter(|pane| pane.observed).count();
    let ignored_count = snapshot
        .panes
        .iter()
        .filter(|pane| pane.state == "ignored")
        .count();
    let unobserved_count = pane_count.saturating_sub(observed_count + ignored_count);
    let payload = serde_json::json!({
        "captured_at_ms": snapshot.captured_at_ms,
        "collected_at": exported_at,
        "freshness_ms": freshness_ms,
        "source_surface": source_surface.clone(),
        "pane_count": pane_count,
        "observed_count": observed_count,
        "ignored_count": ignored_count,
        "unobserved_count": unobserved_count,
        "full_text_included": false,
        "redaction_policy": "bundle_redactor",
        "panes": &snapshot.panes,
    });
    let mut entry = write_incident_json_source(
        IncidentJsonSourceMeta {
            name: "robot_state",
            file: "sources/robot_state.json",
            source_surface: &source_surface,
            evidence_state: IncidentEvidenceState::Measured,
            max_age_ms: Some(30_000),
            started,
        },
        &payload,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
    )?;
    entry.freshness_ms = Some(freshness_ms);
    sources.push(entry);
    Ok(())
}

// Pane text summaries keep redaction, limits, and bundle metadata explicit.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn add_pane_text_summaries_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    add_pane_text_summaries_source_with_db(
        sources,
        warnings,
        exported_at,
        bundle_dir,
        redactor,
        files,
        total_size,
        redaction_entries,
        None,
    )
}

// Pane summary evidence can fall back to text-free DB pane ids under privacy policy.
#[allow(clippy::too_many_arguments)]
fn add_pane_text_summaries_source_with_db(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
    db_path: Option<&Path>,
) -> std::io::Result<()> {
    let started = Instant::now();
    let snapshot = IncidentPaneTextSummariesSnapshot::get_global()
        .map(Ok)
        .or_else(|| db_path.map(incident_pane_text_summaries_snapshot_from_db));
    if let Some(snapshot) = snapshot {
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                sources.push(degraded_source(
                    "pane_text_summaries",
                    IncidentSourceStatus::Failed,
                    db_path.map_or_else(
                        || "incident privacy policy pane_text_allowed=false".to_string(),
                        |path| {
                            format!(
                                "{} + incident privacy policy pane_text_allowed=false",
                                incident_db_panes_source_surface(path)
                            )
                        },
                    ),
                    Some(30_000),
                    elapsed_ms(started),
                    "pane_text_summaries.db_read_failed",
                    error,
                    warnings,
                ));
                return Ok(());
            }
        };
        let source_surface_raw = if snapshot.source_surface.trim().is_empty() {
            "IncidentPaneTextSummariesSnapshot::get_global".to_string()
        } else {
            snapshot.source_surface.clone()
        };
        let source_surface = sanitize_manifest_source_text(
            &source_surface_raw,
            "sources/pane_text_summaries.json",
            redactor,
            redaction_entries,
        );
        let freshness_ms = epoch_millis().saturating_sub(snapshot.captured_at_ms);
        let privacy_reason = snapshot.privacy_reason.clone();
        let privacy_message = privacy_reason.clone().unwrap_or_else(|| {
            "pane text summaries were withheld by incident privacy policy".to_string()
        });
        let privacy_warning_redactions = redactor.detect(&privacy_message).len();
        let privacy_warning_message = if privacy_warning_redactions > 0 {
            redactor.redact(&privacy_message)
        } else {
            privacy_message.clone()
        };
        let privacy_reason_for_payload = privacy_reason
            .as_ref()
            .map(|_| privacy_warning_message.clone());
        let privacy_reason_redactions = if privacy_reason.is_some() {
            privacy_warning_redactions
        } else {
            0
        };
        let panes_for_payload: Vec<_> = if snapshot.privacy_allowed {
            snapshot
                .panes
                .iter()
                .cloned()
                .map(|pane| {
                    sanitize_pane_text_summary_for_payload(
                        pane,
                        snapshot.max_summary_bytes,
                        redactor,
                    )
                })
                .collect()
        } else {
            snapshot
                .panes
                .iter()
                .map(|pane| {
                    let mut excluded = IncidentPaneTextSummary::excluded(
                        pane.pane_id,
                        snapshot.tail_lines,
                        privacy_warning_message.clone(),
                    );
                    excluded.redactions = privacy_warning_redactions;
                    excluded
                })
                .collect()
        };
        let summary_count = panes_for_payload.len();
        let redaction_count: usize = panes_for_payload
            .iter()
            .map(|pane| pane.redactions)
            .sum::<usize>()
            .saturating_add(privacy_reason_redactions);
        let excluded_count = panes_for_payload
            .iter()
            .filter(|pane| pane.status == "excluded")
            .count();
        let error_count = panes_for_payload
            .iter()
            .filter(|pane| pane.status == "error")
            .count();
        let truncated_count = panes_for_payload
            .iter()
            .filter(|pane| pane.truncated)
            .count();
        let payload = serde_json::json!({
            "generated_at": exported_at,
            "captured_at_ms": snapshot.captured_at_ms,
            "freshness_ms": freshness_ms,
            "source_surface": source_surface.clone(),
            "tail_lines": snapshot.tail_lines,
            "max_summary_bytes": snapshot.max_summary_bytes,
            "privacy_allowed": snapshot.privacy_allowed,
            "privacy_reason": privacy_reason_for_payload,
            "summary_count": summary_count,
            "excluded_count": excluded_count,
            "error_count": error_count,
            "truncated_count": truncated_count,
            "redaction_count": redaction_count,
            "panes": &panes_for_payload,
            "provenance": {
                "mutates_state": false,
                "source_surface": source_surface.clone(),
            },
        });
        let mut entry = write_incident_json_source(
            IncidentJsonSourceMeta {
                name: "pane_text_summaries",
                file: "sources/pane_text_summaries.json",
                source_surface: &source_surface,
                evidence_state: if snapshot.privacy_allowed {
                    IncidentEvidenceState::Measured
                } else {
                    IncidentEvidenceState::Unavailable
                },
                max_age_ms: Some(30_000),
                started,
            },
            &payload,
            exported_at,
            bundle_dir,
            redactor,
            files,
            total_size,
            redaction_entries,
        )?;
        entry.freshness_ms = Some(freshness_ms);
        if redaction_count > 0 {
            record_file_redactions(
                redaction_entries,
                "sources/pane_text_summaries.json",
                redaction_count,
            );
            entry.redaction = IncidentRedactionState::Partial;
        }
        if snapshot.privacy_allowed {
            sources.push(entry);
        } else {
            let warning_id = "pane_text_summaries.privacy_disabled";
            record_file_redactions(
                redaction_entries,
                "warnings.jsonl",
                privacy_warning_redactions,
            );
            record_file_redactions(
                redaction_entries,
                "incident_manifest.json",
                privacy_warning_redactions,
            );
            warnings.push(incident_warning(
                warning_id,
                "pane_text_summaries",
                privacy_warning_message,
            ));
            entry.status = IncidentSourceStatus::Skipped;
            entry.warning_ids = vec![warning_id.to_string()];
            sources.push(entry);
        }
    } else {
        sources.push(degraded_source(
            "pane_text_summaries",
            IncidentSourceStatus::Skipped,
            "incident privacy policy pane_text_allowed=false",
            Some(30_000),
            elapsed_ms(started),
            "pane_text_summaries.privacy_disabled",
            "pane text summaries were skipped because the default incident privacy budget forbids pane text collection".to_string(),
            warnings,
        ));
    }
    Ok(())
}

// Tailer health evidence carries source, policy, and capture-window context.
#[allow(clippy::too_many_arguments)]
fn add_tailer_capture_health_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    let started = Instant::now();
    let streaming = crate::tailer::StreamingHealth::get_global();
    let scheduler = HealthSnapshot::get_global().and_then(|snapshot| snapshot.scheduler);
    if streaming.is_some() || scheduler.is_some() {
        let payload = serde_json::json!({
            "streaming_health": streaming,
            "scheduler": scheduler,
        });
        sources.push(write_incident_json_source(
            IncidentJsonSourceMeta {
                name: "tailer_capture_health",
                file: "sources/tailer_capture_health.json",
                source_surface: "StreamingHealth::get_global + HealthSnapshot.scheduler",
                evidence_state: if payload["streaming_health"].is_null()
                    || payload["scheduler"].is_null()
                {
                    IncidentEvidenceState::Mixed
                } else {
                    IncidentEvidenceState::Measured
                },
                max_age_ms: Some(30_000),
                started,
            },
            &payload,
            exported_at,
            bundle_dir,
            redactor,
            files,
            total_size,
            redaction_entries,
        )?);
    } else {
        sources.push(degraded_source(
            "tailer_capture_health",
            IncidentSourceStatus::Unavailable,
            "StreamingHealth::get_global + HealthSnapshot.scheduler",
            Some(30_000),
            elapsed_ms(started),
            "tailer_capture_health.snapshot_unavailable",
            "no streaming tailer or scheduler health snapshot has been published in this process"
                .to_string(),
            warnings,
        ));
    }
    Ok(())
}

// Resource pressure evidence preserves the telemetry snapshot and bundle context.
#[allow(clippy::too_many_arguments)]
fn add_resource_pressure_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    let started = Instant::now();
    if let Some(snapshot) = HealthSnapshot::get_global() {
        let payload = serde_json::json!({
            "pressure": {
                "backpressure_tier": snapshot.backpressure_tier.unwrap_or_else(|| "unknown".to_string()),
                "fleet_pressure_tier": snapshot.fleet_pressure_tier.unwrap_or_else(|| "unknown".to_string()),
                "capture_queue_depth": snapshot.capture_queue_depth,
                "write_queue_depth": snapshot.write_queue_depth,
                "observed_panes": snapshot.observed_panes,
            },
            "swarm_capacity": snapshot.swarm_capacity,
            "leak_risk_inventory": snapshot.leak_risk_inventory,
        });
        sources.push(write_incident_json_source(
            IncidentJsonSourceMeta {
                name: "resource_pressure_cockpit",
                file: "sources/resource_pressure_cockpit.json",
                source_surface: "HealthSnapshot pressure fields",
                evidence_state: IncidentEvidenceState::Measured,
                max_age_ms: Some(30_000),
                started,
            },
            &payload,
            exported_at,
            bundle_dir,
            redactor,
            files,
            total_size,
            redaction_entries,
        )?);
    } else {
        sources.push(degraded_source(
            "resource_pressure_cockpit",
            IncidentSourceStatus::Unavailable,
            "HealthSnapshot pressure fields",
            Some(30_000),
            elapsed_ms(started),
            "resource_pressure_cockpit.snapshot_unavailable",
            "no runtime health snapshot was available for resource pressure collection".to_string(),
            warnings,
        ));
    }
    Ok(())
}

// RCH proof evidence keeps command, policy, and bundle metadata in one source builder.
#[allow(clippy::too_many_arguments)]
fn add_proof_rch_evidence_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    let started = Instant::now();
    if let Some(snapshot) = IncidentProofRchEvidenceSnapshot::get_global() {
        let source_surface_raw = if snapshot.source_surface.trim().is_empty() {
            "IncidentProofRchEvidenceSnapshot::get_global".to_string()
        } else {
            snapshot.source_surface.clone()
        };
        let source_surface = sanitize_manifest_source_text(
            &source_surface_raw,
            "sources/proof_rch_evidence.json",
            redactor,
            redaction_entries,
        );
        let freshness_ms = epoch_millis().saturating_sub(snapshot.captured_at_ms);
        let attempt_count = snapshot.attempts.len();
        let artifact_count = snapshot.artifact_paths.len();
        let setup_chatter_attempt_count = snapshot
            .attempts
            .iter()
            .filter(|attempt| attempt.setup_chatter_only)
            .count();
        let remote_execution_confirmed_count = snapshot
            .attempts
            .iter()
            .filter(|attempt| attempt.remote_execution_confirmed)
            .count();
        let attempts = snapshot
            .attempts
            .iter()
            .map(|attempt| {
                serde_json::json!({
                    "command": &attempt.command,
                    "status": &attempt.status,
                    "reason_code": &attempt.reason_code,
                    "reason_category": proof_rch_reason_category(&attempt.reason_code),
                    "artifact_path": &attempt.artifact_path,
                    "remote_execution_confirmed": attempt.remote_execution_confirmed,
                    "local_fallback_rejected": attempt.local_fallback_rejected,
                    "setup_chatter_only": attempt.setup_chatter_only,
                })
            })
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "generated_at": exported_at,
            "captured_at_ms": snapshot.captured_at_ms,
            "freshness_ms": freshness_ms,
            "source_surface": source_surface.clone(),
            "verdict": &snapshot.verdict,
            "reason_code": &snapshot.reason_code,
            "reason_category": proof_rch_reason_category(&snapshot.reason_code),
            "artifact_paths": &snapshot.artifact_paths,
            "artifact_count": artifact_count,
            "attempts": attempts,
            "attempt_count": attempt_count,
            "setup_chatter_attempt_count": setup_chatter_attempt_count,
            "remote_execution_confirmed_count": remote_execution_confirmed_count,
            "local_fallback_rejected": snapshot.local_fallback_rejected,
            "setup_chatter_only": snapshot.setup_chatter_only,
            "collector_launched_proof_commands": false,
            "collector_mutated_state": false,
            "local_cargo_counted_as_proof": false,
            "sync_chatter_counted_as_proof": false,
            "provenance": {
                "collector_launched_proof_commands": false,
                "collector_mutated_state": false,
                "local_cargo_counted_as_proof": false,
                "sync_chatter_counted_as_proof": false,
                "local_fallback_rejected": snapshot.local_fallback_rejected,
                "setup_chatter_only": snapshot.setup_chatter_only,
                "source_surface": source_surface.clone(),
            },
        });
        let mut entry = write_incident_json_source(
            IncidentJsonSourceMeta {
                name: "proof_rch_evidence",
                file: "sources/proof_rch_evidence.json",
                source_surface: &source_surface,
                evidence_state: proof_rch_evidence_state(&snapshot),
                max_age_ms: Some(300_000),
                started,
            },
            &payload,
            exported_at,
            bundle_dir,
            redactor,
            files,
            total_size,
            redaction_entries,
        )?;
        entry.freshness_ms = Some(freshness_ms);
        if snapshot.setup_chatter_only || setup_chatter_attempt_count > 0 {
            let warning_id = "proof_rch_evidence.setup_chatter_only";
            warnings.push(incident_warning(
                warning_id,
                "proof_rch_evidence",
                "retained RCH artifacts were setup/sync/queue chatter only and were not counted as proof".to_string(),
            ));
            entry.warning_ids.push(warning_id.to_string());
        }
        sources.push(entry);
    } else {
        sources.push(degraded_source(
            "proof_rch_evidence",
            IncidentSourceStatus::Unavailable,
            "IncidentProofRchEvidenceSnapshot::get_global",
            Some(300_000),
            elapsed_ms(started),
            "proof_rch_evidence.not_attached",
            "no retained RCH proof ledger was attached; incident collection never runs proof commands and does not count sync or setup chatter as proof".to_string(),
            warnings,
        ));
    }
    Ok(())
}

fn proof_rch_evidence_state(snapshot: &IncidentProofRchEvidenceSnapshot) -> IncidentEvidenceState {
    if snapshot.setup_chatter_only
        || snapshot.verdict == "no_verdict"
        || snapshot
            .attempts
            .iter()
            .any(|attempt| attempt.setup_chatter_only)
    {
        IncidentEvidenceState::Mixed
    } else {
        IncidentEvidenceState::Measured
    }
}

fn proof_rch_reason_category(reason_code: &str) -> &'static str {
    let reason = reason_code.to_ascii_lowercase();
    if reason.contains("no_worker")
        || reason.contains("no-workers")
        || reason.contains("no workers")
        || reason.contains("no_workers")
    {
        "no_worker"
    } else if reason.contains("topology") {
        "topology"
    } else if reason.contains("local_fallback")
        || reason.contains("local-fallback")
        || reason.contains("local fallback")
        || reason.contains("local_cargo")
        || reason.contains("local-cargo")
    {
        "local_fallback"
    } else if reason.contains("materialization")
        || reason.contains("materialize")
        || reason.contains("package")
        || reason.contains("manifest")
    {
        "package_materialization"
    } else if reason.contains("sync")
        || reason.contains("queue")
        || reason.contains("transfer")
        || reason.contains("setup_chatter")
    {
        "setup_sync"
    } else if reason.contains("transport")
        || reason.contains("ssh")
        || reason.contains("connection")
        || reason.contains("network")
    {
        "transport"
    } else if reason.contains("result")
        || reason.contains("verifier")
        || reason.contains("test")
        || reason.contains("exit_status")
        || reason.contains("exit-status")
    {
        "result"
    } else {
        "unknown"
    }
}

// Beads coordination evidence keeps repository, command, and bundle context explicit.
#[allow(clippy::too_many_arguments)]
fn add_beads_coordination_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    let started = Instant::now();
    let path = Path::new(".beads").join("issues.jsonl");
    if path.is_file() {
        match fs::read_to_string(&path) {
            Ok(raw) => {
                let payload = beads_coordination_payload(&raw);
                sources.push(write_incident_json_source(
                    IncidentJsonSourceMeta {
                        name: "beads_coordination_snapshot",
                        file: "sources/beads_coordination_snapshot.json",
                        source_surface: "read-only .beads/issues.jsonl snapshot",
                        evidence_state: if payload["parse_error_count"].as_u64().unwrap_or(0) > 0 {
                            IncidentEvidenceState::Mixed
                        } else {
                            IncidentEvidenceState::Measured
                        },
                        max_age_ms: Some(30_000),
                        started,
                    },
                    &payload,
                    exported_at,
                    bundle_dir,
                    redactor,
                    files,
                    total_size,
                    redaction_entries,
                )?);
            }
            Err(error) => {
                sources.push(degraded_source(
                    "beads_coordination_snapshot",
                    IncidentSourceStatus::Failed,
                    "read-only .beads/issues.jsonl snapshot",
                    Some(30_000),
                    elapsed_ms(started),
                    "beads_coordination_snapshot.read_failed",
                    format!("failed to read Beads JSONL snapshot: {error}"),
                    warnings,
                ));
            }
        }
    } else {
        sources.push(degraded_source(
            "beads_coordination_snapshot",
            IncidentSourceStatus::Unavailable,
            "read-only .beads/issues.jsonl snapshot",
            Some(30_000),
            elapsed_ms(started),
            "beads_coordination_snapshot.unavailable",
            "no .beads/issues.jsonl coordination snapshot was available; incident collection does not claim, reopen, sync, or mutate Beads".to_string(),
            warnings,
        ));
    }
    Ok(())
}

fn beads_coordination_payload(raw: &str) -> serde_json::Value {
    let mut total = 0_usize;
    let mut parse_error_count = 0_usize;
    let mut open = 0_usize;
    let mut in_progress = 0_usize;
    let mut blocked = 0_usize;
    let mut deferred = 0_usize;
    let mut closed = 0_usize;
    let mut active_assignees = HashSet::new();
    let mut ready_candidates = Vec::new();
    let mut stale_candidates = Vec::new();

    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            parse_error_count += 1;
            continue;
        };
        total += 1;
        let status = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        match status {
            "open" => open += 1,
            "in_progress" => in_progress += 1,
            "blocked" => blocked += 1,
            "deferred" => deferred += 1,
            "closed" => closed += 1,
            _ => {}
        }
        if let Some(assignee) = value.get("assignee").and_then(serde_json::Value::as_str)
            && !assignee.trim().is_empty()
        {
            active_assignees.insert(assignee.to_string());
        }
        if status == "open" && ready_candidates.len() < 25 {
            ready_candidates.push(bead_snapshot_row(&value));
        }
        if matches!(status, "in_progress" | "blocked") && stale_candidates.len() < 25 {
            stale_candidates.push(bead_snapshot_row(&value));
        }
    }

    let mut active_assignees = active_assignees.into_iter().collect::<Vec<_>>();
    active_assignees.sort();
    serde_json::json!({
        "collector": "read-only .beads/issues.jsonl snapshot",
        "counts": {
            "total": total,
            "open": open,
            "in_progress": in_progress,
            "blocked": blocked,
            "deferred": deferred,
            "closed": closed,
            "parse_errors": parse_error_count,
        },
        "active_assignees": active_assignees,
        "ready_candidates": ready_candidates,
        "ready_candidates_truncated": open > ready_candidates.len(),
        "stale_reopen_review_candidates": stale_candidates,
        "stale_reopen_review_candidates_truncated": in_progress + blocked > stale_candidates.len(),
        "parse_error_count": parse_error_count,
        "mutated_beads": false,
    })
}

fn bead_snapshot_row(value: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": value.get("id").and_then(serde_json::Value::as_str).unwrap_or("unknown"),
        "title": value.get("title").and_then(serde_json::Value::as_str).unwrap_or(""),
        "status": value.get("status").and_then(serde_json::Value::as_str).unwrap_or("unknown"),
        "priority": value.get("priority").and_then(serde_json::Value::as_u64),
        "assignee": value.get("assignee").and_then(serde_json::Value::as_str),
        "updated_at": value.get("updated_at").and_then(serde_json::Value::as_str),
    })
}

// Git dirty-tree evidence preserves repository, command, and bundle context together.
#[allow(clippy::too_many_arguments)]
fn add_git_dirty_tree_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    let started = Instant::now();
    match run_bounded_command_stdout(
        "git",
        &["status", "--porcelain=v1", "--untracked-files=all"],
        500,
    ) {
        Ok(stdout) => {
            let branch = run_bounded_command_stdout("git", &["branch", "--show-current"], 250)
                .ok()
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty());
            let payload = git_dirty_tree_payload(&stdout, branch.as_deref());
            sources.push(write_incident_json_source(
                IncidentJsonSourceMeta {
                    name: "git_dirty_tree",
                    file: "sources/git_dirty_tree.json",
                    source_surface: "bounded git status --porcelain=v1 --untracked-files=all",
                    evidence_state: IncidentEvidenceState::Measured,
                    max_age_ms: Some(30_000),
                    started,
                },
                &payload,
                exported_at,
                bundle_dir,
                redactor,
                files,
                total_size,
                redaction_entries,
            )?);
        }
        Err(BoundedCommandError::Unavailable(message)) => {
            sources.push(degraded_source(
                "git_dirty_tree",
                IncidentSourceStatus::Unavailable,
                "bounded git status --porcelain=v1 --untracked-files=all",
                Some(30_000),
                elapsed_ms(started),
                "git_dirty_tree.unavailable",
                message,
                warnings,
            ));
        }
        Err(BoundedCommandError::Timeout { timeout_ms }) => {
            sources.push(degraded_source(
                "git_dirty_tree",
                IncidentSourceStatus::Failed,
                "bounded git status --porcelain=v1 --untracked-files=all",
                Some(30_000),
                elapsed_ms(started),
                "git_dirty_tree.timeout",
                format!("git dirty-tree collection exceeded its {timeout_ms} ms timeout"),
                warnings,
            ));
        }
        Err(BoundedCommandError::Failed(message)) => {
            sources.push(degraded_source(
                "git_dirty_tree",
                IncidentSourceStatus::Failed,
                "bounded git status --porcelain=v1 --untracked-files=all",
                Some(30_000),
                elapsed_ms(started),
                "git_dirty_tree.failed",
                message,
                warnings,
            ));
        }
    }
    Ok(())
}

fn git_dirty_tree_payload(stdout: &str, branch: Option<&str>) -> serde_json::Value {
    let rows = stdout
        .lines()
        .filter_map(parse_git_porcelain_row)
        .collect::<Vec<_>>();
    let mut categories = HashSet::new();
    let mut tracked_dirty = 0_usize;
    let mut untracked = 0_usize;
    let mut staged = 0_usize;
    let mut unstaged = 0_usize;
    let mut deleted = 0_usize;
    for (status, path) in &rows {
        if status == "??" {
            untracked += 1;
        } else {
            tracked_dirty += 1;
            let mut chars = status.chars();
            let index_status = chars.next().unwrap_or(' ');
            let worktree_status = chars.next().unwrap_or(' ');
            if index_status != ' ' {
                staged += 1;
            }
            if worktree_status != ' ' {
                unstaged += 1;
            }
            if index_status == 'D' || worktree_status == 'D' {
                deleted += 1;
            }
        }
        if let Some(category) = dirty_tree_risk_category(path) {
            categories.insert(category);
        }
    }

    let mut category_rows = categories.into_iter().collect::<Vec<_>>();
    category_rows.sort_unstable();
    let mut entries = rows
        .iter()
        .take(200)
        .map(|(status, path)| {
            serde_json::json!({
                "status": status,
                "path": path,
                "risk_category": dirty_tree_risk_category(path),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["path"].as_str().unwrap_or_default())
    });

    serde_json::json!({
        "collector": "git status --porcelain=v1 --untracked-files=all",
        "branch": branch,
        "status": if rows.is_empty() { "clean" } else { "dirty" },
        "counts": {
            "total": rows.len(),
            "tracked_dirty": tracked_dirty,
            "untracked": untracked,
            "staged": staged,
            "unstaged": unstaged,
            "deleted": deleted,
        },
        "risk": {
            "dirty_tree": !rows.is_empty(),
            "high_risk_path_count": rows
                .iter()
                .filter(|(_, path)| dirty_tree_risk_category(path).is_some())
                .count(),
            "categories": category_rows,
        },
        "entries_truncated": rows.len() > entries.len(),
        "entries": entries,
    })
}

fn parse_git_porcelain_row(line: &str) -> Option<(String, String)> {
    if line.len() < 4 {
        return None;
    }
    let status = line.get(0..2)?.to_string();
    let path = line.get(3..)?.trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some((status, path))
    }
}

fn dirty_tree_risk_category(path: &str) -> Option<&'static str> {
    if path == "Cargo.toml" || path == "Cargo.lock" {
        Some("workspace_manifest")
    } else if path == ".beads/issues.jsonl" || path.starts_with(".beads/") {
        Some("coordination_state")
    } else if path.starts_with("crates/frankenterm-core/") {
        Some("core_crate")
    } else if path.starts_with("crates/frankenterm-gui/") {
        Some("gui_crate")
    } else if path.starts_with("docs/robot-contracts/")
        || path.starts_with("docs/json-schema/")
        || path.starts_with("docs/incident-bundles")
    {
        Some("operator_contracts")
    } else if path.starts_with("fixtures/") || path.contains("/fixtures/") {
        Some("fixtures")
    } else {
        None
    }
}

// Agent Mail evidence keeps service, command, and bundle context in one audit builder.
#[allow(clippy::too_many_arguments)]
fn add_agent_mail_source(
    sources: &mut Vec<IncidentSourceEntry>,
    warnings: &mut Vec<IncidentBundleWarning>,
    exported_at: &str,
    bundle_dir: &Path,
    redactor: &Redactor,
    files: &mut Vec<String>,
    total_size: &mut u64,
    redaction_entries: &mut Vec<FileRedactionEntry>,
) -> std::io::Result<()> {
    let started = Instant::now();
    if let Some(snapshot) = IncidentAgentMailSnapshot::get_global() {
        let source_surface_raw = if snapshot.source_surface.trim().is_empty() {
            "IncidentAgentMailSnapshot::get_global".to_string()
        } else {
            snapshot.source_surface.clone()
        };
        let source_surface = sanitize_manifest_source_text(
            &source_surface_raw,
            "sources/agent_mail.json",
            redactor,
            redaction_entries,
        );
        let freshness_ms = epoch_millis().saturating_sub(snapshot.captured_at_ms);
        let sanitized_attempt_rows = snapshot
            .attempts
            .iter()
            .cloned()
            .map(|attempt| sanitize_agent_mail_attempt_for_payload(attempt, redactor))
            .collect::<Vec<_>>();
        let attempt_message_redactions: usize = sanitized_attempt_rows
            .iter()
            .map(|(_, redactions)| *redactions)
            .sum();
        let attempts = sanitized_attempt_rows
            .iter()
            .map(|(attempt, _)| attempt)
            .map(|attempt| {
                serde_json::json!({
                    "operation": &attempt.operation,
                    "status": &attempt.status,
                    "reason_code": &attempt.reason_code,
                    "reason_category": agent_mail_reason_category(&attempt.reason_code),
                    "message": &attempt.message,
                    "elapsed_ms": attempt.elapsed_ms,
                    "mutates_state": false,
                    "message_bodies_included": false,
                })
            })
            .collect::<Vec<_>>();
        let payload = serde_json::json!({
            "generated_at": exported_at,
            "captured_at_ms": snapshot.captured_at_ms,
            "freshness_ms": freshness_ms,
            "source_surface": source_surface.clone(),
            "status": &snapshot.status,
            "health_level": &snapshot.health_level,
            "reason_code": &snapshot.reason_code,
            "reason_category": agent_mail_reason_category(&snapshot.reason_code),
            "project_count": snapshot.project_count,
            "agent_count": snapshot.agent_count,
            "message_count": snapshot.message_count,
            "active_agents": &snapshot.active_agents,
            "attempts": attempts,
            "attempt_count": snapshot.attempts.len(),
            "retry_count": snapshot.retry_count,
            "max_retry_count": 1,
            "collector_mutated_state": false,
            "message_bodies_included": false,
            "inbox_bodies_included": false,
            "repair_restart_kill_attempted": snapshot.repair_restart_kill_attempted,
            "forbidden_actions": agent_mail_forbidden_actions(),
            "provenance": {
                "source_surface": source_surface.clone(),
                "collector_mutated_state": false,
                "repair_allowed": false,
                "restart_allowed": false,
                "kill_allowed": false,
                "registration_attempted": false,
                "acknowledgement_attempted": false,
                "message_bodies_included": false,
                "inbox_bodies_included": false,
            },
        });
        let mut entry = write_incident_json_source(
            IncidentJsonSourceMeta {
                name: "agent_mail",
                file: "sources/agent_mail.json",
                source_surface: &source_surface,
                evidence_state: agent_mail_evidence_state(&snapshot),
                max_age_ms: Some(30_000),
                started,
            },
            &payload,
            exported_at,
            bundle_dir,
            redactor,
            files,
            total_size,
            redaction_entries,
        )?;
        entry.freshness_ms = Some(freshness_ms);
        if attempt_message_redactions > 0 {
            record_file_redactions(
                redaction_entries,
                "sources/agent_mail.json",
                attempt_message_redactions,
            );
            entry.redaction = IncidentRedactionState::Partial;
        }
        if snapshot.status != "ok" || snapshot.repair_restart_kill_attempted {
            let warning_id = if snapshot.repair_restart_kill_attempted {
                "agent_mail.forbidden_action_attempted"
            } else {
                agent_mail_warning_id(&snapshot.reason_code)
            };
            warnings.push(incident_warning(
                warning_id,
                "agent_mail",
                format!(
                    "Agent Mail status {}; collector did not repair, restart, kill, acknowledge, or fetch message bodies",
                    snapshot.status
                ),
            ));
            entry.warning_ids.push(warning_id.to_string());
        }
        sources.push(entry);
    } else {
        sources.push(degraded_source(
            "agent_mail",
            IncidentSourceStatus::Unavailable,
            "IncidentAgentMailSnapshot::get_global",
            Some(30_000),
            elapsed_ms(started),
            "agent_mail.not_attached",
            "no Agent Mail snapshot was supplied to the crash collector; incident collection must not repair, restart, or kill shared Agent Mail services".to_string(),
            warnings,
        ));
    }
    Ok(())
}

fn agent_mail_evidence_state(snapshot: &IncidentAgentMailSnapshot) -> IncidentEvidenceState {
    if snapshot.status == "ok" && !snapshot.repair_restart_kill_attempted {
        IncidentEvidenceState::Measured
    } else {
        IncidentEvidenceState::Unavailable
    }
}

fn agent_mail_warning_id(reason_code: &str) -> &'static str {
    match agent_mail_reason_category(reason_code) {
        "database" => "agent_mail.database_error",
        "api_unreachable" => "agent_mail.api_unreachable",
        "timeout" => "agent_mail.timeout",
        "recovery_mode" => "agent_mail.recovery_mode",
        "forbidden_action" => "agent_mail.forbidden_action_attempted",
        _ => "agent_mail.unavailable",
    }
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn agent_mail_reason_category(reason_code: &str) -> &'static str {
    let reason = reason_code.to_ascii_lowercase();
    if reason == "agent_mail.ok" || reason.ends_with(".ok") {
        "ok"
    } else if reason.contains("database")
        || reason.contains("sqlite")
        || reason.contains("corrupt")
        || reason.contains("enospc")
        || reason.contains("no_space")
    {
        "database"
    } else if reason.contains("recovery") || reason.contains("read_only") {
        "recovery_mode"
    } else if reason.contains("timeout") {
        "timeout"
    } else if reason.contains("unreachable")
        || reason.contains("connection")
        || reason.contains("http")
        || reason.contains("api")
    {
        "api_unreachable"
    } else if reason.contains("repair")
        || reason.contains("restart")
        || reason.contains("kill")
        || reason.contains("forbidden")
    {
        "forbidden_action"
    } else {
        "unknown"
    }
}

fn agent_mail_forbidden_actions() -> Vec<&'static str> {
    vec![
        "am service restart",
        "am service stop",
        "am doctor fix",
        "am doctor repair",
        "am doctor reconstruct",
        "kill am",
        "kill am serve-http",
        "kill mcp-agent-mail",
    ]
}

#[derive(Debug)]
enum BoundedCommandError {
    Unavailable(String),
    Timeout { timeout_ms: u64 },
    Failed(String),
}

fn run_bounded_command_stdout(
    program: &str,
    args: &[&str],
    timeout_ms: u64,
) -> Result<String, BoundedCommandError> {
    if timeout_ms == 0 {
        return Err(BoundedCommandError::Timeout { timeout_ms });
    }

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                BoundedCommandError::Unavailable(format!("{program} command was unavailable"))
            } else {
                BoundedCommandError::Failed(format!("failed to start {program}: {error}"))
            }
        })?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let output = child.wait_with_output().map_err(|error| {
                    BoundedCommandError::Failed(format!(
                        "failed to collect {program} output: {error}"
                    ))
                })?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(BoundedCommandError::Failed(format!(
                        "{program} exited with status {}: {}",
                        output.status,
                        stderr.trim()
                    )));
                }
                return String::from_utf8(output.stdout).map_err(|error| {
                    BoundedCommandError::Failed(format!(
                        "{program} output was not valid UTF-8: {error}"
                    ))
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BoundedCommandError::Timeout { timeout_ms });
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BoundedCommandError::Failed(format!(
                    "failed while waiting for {program}: {error}"
                )));
            }
        }
    }
}

fn warnings_jsonl(warnings: &[IncidentBundleWarning]) -> std::io::Result<String> {
    let mut out = String::new();
    for warning in warnings {
        let line = serde_json::to_string(warning).map_err(std::io::Error::other)?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

fn generate_incident_bundle_readme(
    kind: IncidentKind,
    exported_at: &str,
    files: &[String],
    source_count: usize,
    warning_count: usize,
) -> String {
    let mut out = String::new();
    out.push_str("# ft Incident Bundle\n\n");
    out.push_str(&format!("Kind: {kind}\n"));
    out.push_str(&format!("Exported: {exported_at}\n"));
    out.push_str("Collector: read-only enhanced incident bundle\n\n");
    out.push_str("## Files\n\n");
    for file in files {
        out.push_str("- `");
        out.push_str(file);
        out.push_str("`\n");
    }
    out.push_str("\n## Swarm Source Provenance\n\n");
    out.push_str(&format!(
        "The manifest records {source_count} source entry/entries and {warning_count} warning(s).\n"
    ));
    out.push_str(
        "Collection is non-mutating: it must not send pane input, claim Beads, repair Agent Mail, mutate git state, or run new proof commands.\n\n",
    );
    out.push_str("## Validation\n\n");
    out.push_str("Run `ft reproduce replay <bundle-dir> --mode policy` before sharing.\n");
    out.push_str(
        "Use `ft reproduce replay <bundle-dir> --mode policy --format json` when another agent needs the structured replay and verifier result.\n\n",
    );
    out.push_str("## Operator Handoff\n\n");
    out.push_str("- Capture before cleanup, restarts, Beads reassignment, or pane interaction.\n");
    out.push_str(
        "- Treat missing Agent Mail, RCH, Beads, git, robot, or process data as a degraded source with a warning; do not repair or restart shared services during bundle capture.\n",
    );
    out.push_str(
        "- Keep `incident_manifest.json`, `warnings.jsonl`, `redaction_report.json`, and this `README.md` with any `sources/` payloads.\n",
    );
    out.push_str(
        "- Classify RCH setup, sync, worker, package, and transport failures separately from verifier or source-test failures.\n",
    );
    out.push_str(
        "- Run heavy Cargo validation through remote-required RCH and retain the exact command plus log or artifact path; local Cargo is not proof for the handoff lane.\n",
    );
    out
}

/// Collect a comprehensive incident bundle with DB metadata, recent events,
/// and a redaction report.
///
/// This is an enhanced version of [`export_incident_bundle`] that additionally
/// gathers storage metadata and recent event summaries.
pub fn collect_incident_bundle(
    opts: &IncidentBundleOptions<'_>,
) -> std::io::Result<IncidentBundleResult> {
    collect_incident_bundle_inner(opts, None)
}

/// Collect a comprehensive incident bundle with an opt-in bounded process sampler.
pub fn collect_incident_bundle_with_process_sampler(
    opts: &IncidentBundleOptions<'_>,
    process_sampler: &IncidentProcessSamplerConfig,
) -> std::io::Result<IncidentBundleResult> {
    collect_incident_bundle_inner(opts, Some(process_sampler))
}

fn collect_incident_bundle_inner(
    opts: &IncidentBundleOptions<'_>,
    process_sampler: Option<&IncidentProcessSamplerConfig>,
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
    let exported_at = format_iso8601(ts);
    let mut sources: Vec<IncidentSourceEntry> = Vec::new();
    let mut warnings: Vec<IncidentBundleWarning> = Vec::new();
    add_swarm_incident_sources(
        &mut sources,
        &mut warnings,
        &exported_at,
        &bundle_dir,
        &redactor,
        &mut files,
        &mut total_size,
        &mut redaction_entries,
        opts.db_path,
    )?;

    // 1. Include latest crash bundle contents (if crash kind)
    let source_started = Instant::now();
    if opts.kind == IncidentKind::Crash {
        if let Some(crash) = latest_crash_bundle(opts.crash_dir) {
            let redaction_start = redaction_entries.len();
            let mut crash_primary_file: Option<String> = None;
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
                crash_primary_file.get_or_insert_with(|| "crash_report.json".to_string());
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
                crash_primary_file.get_or_insert_with(|| "crash_manifest.json".to_string());
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
                    crash_primary_file.get_or_insert_with(|| "health_snapshot.json".to_string());
                }
            }
            if let Some(primary_file) = crash_primary_file {
                sources.push(IncidentSourceEntry {
                    name: "crash_bundle".to_string(),
                    file: Some(primary_file.clone()),
                    status: IncidentSourceStatus::Collected,
                    evidence_state: IncidentEvidenceState::Measured,
                    source_surface: "latest_crash_bundle".to_string(),
                    mutates_state: false,
                    generated_at: Some(exported_at.clone()),
                    freshness_ms: Some(0),
                    max_age_ms: Some(300_000),
                    redaction: if redaction_entries.len() > redaction_start {
                        IncidentRedactionState::Partial
                    } else {
                        IncidentRedactionState::None
                    },
                    privacy_tier: "default".to_string(),
                    size_bytes: bundle_file_size(&bundle_dir, &primary_file),
                    elapsed_ms: elapsed_ms(source_started),
                    warning_ids: Vec::new(),
                });
            } else {
                let warning_id = "crash_bundle.empty";
                warnings.push(incident_warning(
                    warning_id,
                    "crash_bundle",
                    "latest crash bundle existed, but no readable payload files were available"
                        .to_string(),
                ));
                sources.push(IncidentSourceEntry {
                    name: "crash_bundle".to_string(),
                    file: None,
                    status: IncidentSourceStatus::Unavailable,
                    evidence_state: IncidentEvidenceState::Unavailable,
                    source_surface: "latest_crash_bundle".to_string(),
                    mutates_state: false,
                    generated_at: None,
                    freshness_ms: None,
                    max_age_ms: Some(300_000),
                    redaction: IncidentRedactionState::NotApplicable,
                    privacy_tier: "default".to_string(),
                    size_bytes: 0,
                    elapsed_ms: elapsed_ms(source_started),
                    warning_ids: vec![warning_id.to_string()],
                });
            }
        } else {
            let warning_id = "crash_bundle.unavailable";
            warnings.push(incident_warning(
                warning_id,
                "crash_bundle",
                "crash incident requested, but no crash bundle was available".to_string(),
            ));
            sources.push(IncidentSourceEntry {
                name: "crash_bundle".to_string(),
                file: None,
                status: IncidentSourceStatus::Unavailable,
                evidence_state: IncidentEvidenceState::Unavailable,
                source_surface: "latest_crash_bundle".to_string(),
                mutates_state: false,
                generated_at: None,
                freshness_ms: None,
                max_age_ms: Some(300_000),
                redaction: IncidentRedactionState::NotApplicable,
                privacy_tier: "default".to_string(),
                size_bytes: 0,
                elapsed_ms: elapsed_ms(source_started),
                warning_ids: vec![warning_id.to_string()],
            });
        }
    } else {
        sources.push(skipped_source(
            "crash_bundle",
            "latest_crash_bundle".to_string(),
            Some(300_000),
            elapsed_ms(source_started),
            "crash_bundle.skipped",
            "bundle kind did not request crash payload collection".to_string(),
            &mut warnings,
        ));
    }

    // 2. Include config summary (redacted, max 64 KiB)
    let source_started = Instant::now();
    if let Some(cfg_path) = opts.config_path {
        if cfg_path.exists() {
            match fs::read_to_string(cfg_path) {
                Ok(contents) => {
                    let truncated = truncate_utf8_with_marker(
                        &contents,
                        64 * 1024,
                        "\n... [truncated at 64 KiB]",
                    );
                    write_redacted_file(
                        "config_summary.toml",
                        &truncated,
                        &bundle_dir,
                        &redactor,
                        &mut files,
                        &mut total_size,
                        &mut redaction_entries,
                    )?;
                    sources.push(IncidentSourceEntry {
                        name: "config_summary".to_string(),
                        file: Some("config_summary.toml".to_string()),
                        status: IncidentSourceStatus::Collected,
                        evidence_state: IncidentEvidenceState::Measured,
                        source_surface: format!("read redacted config {}", cfg_path.display()),
                        mutates_state: false,
                        generated_at: Some(exported_at.clone()),
                        freshness_ms: Some(0),
                        max_age_ms: Some(300_000),
                        redaction: redaction_state_for_file(
                            &redaction_entries,
                            "config_summary.toml",
                        ),
                        privacy_tier: "default".to_string(),
                        size_bytes: bundle_file_size(&bundle_dir, "config_summary.toml"),
                        elapsed_ms: elapsed_ms(source_started),
                        warning_ids: Vec::new(),
                    });
                }
                Err(error) => {
                    let warning_id = "config_summary.read_failed";
                    warnings.push(incident_warning(
                        warning_id,
                        "config_summary",
                        format!("failed to read config summary source: {error}"),
                    ));
                    sources.push(IncidentSourceEntry {
                        name: "config_summary".to_string(),
                        file: None,
                        status: IncidentSourceStatus::Failed,
                        evidence_state: IncidentEvidenceState::Unavailable,
                        source_surface: format!("read redacted config {}", cfg_path.display()),
                        mutates_state: false,
                        generated_at: None,
                        freshness_ms: None,
                        max_age_ms: Some(300_000),
                        redaction: IncidentRedactionState::NotApplicable,
                        privacy_tier: "default".to_string(),
                        size_bytes: 0,
                        elapsed_ms: elapsed_ms(source_started),
                        warning_ids: vec![warning_id.to_string()],
                    });
                }
            }
        } else {
            let warning_id = "config_summary.unavailable";
            warnings.push(incident_warning(
                warning_id,
                "config_summary",
                format!("config path does not exist: {}", cfg_path.display()),
            ));
            sources.push(IncidentSourceEntry {
                name: "config_summary".to_string(),
                file: None,
                status: IncidentSourceStatus::Unavailable,
                evidence_state: IncidentEvidenceState::Unavailable,
                source_surface: format!("read redacted config {}", cfg_path.display()),
                mutates_state: false,
                generated_at: None,
                freshness_ms: None,
                max_age_ms: Some(300_000),
                redaction: IncidentRedactionState::NotApplicable,
                privacy_tier: "default".to_string(),
                size_bytes: 0,
                elapsed_ms: elapsed_ms(source_started),
                warning_ids: vec![warning_id.to_string()],
            });
        }
    } else {
        sources.push(skipped_source(
            "config_summary",
            "optional config path not provided".to_string(),
            Some(300_000),
            elapsed_ms(source_started),
            "config_summary.skipped",
            "optional config path was not provided".to_string(),
            &mut warnings,
        ));
    }

    // 3. Gather DB metadata + recent events
    let db_source_started = Instant::now();
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
            sources.push(IncidentSourceEntry {
                name: "db_metadata".to_string(),
                file: Some("db_metadata.json".to_string()),
                status: IncidentSourceStatus::Collected,
                evidence_state: if db_meta.schema_version.is_some() {
                    IncidentEvidenceState::Measured
                } else {
                    IncidentEvidenceState::Mixed
                },
                source_surface: format!("rusqlite read-only {}", db_path.display()),
                mutates_state: false,
                generated_at: Some(exported_at.clone()),
                freshness_ms: Some(0),
                max_age_ms: Some(300_000),
                redaction: redaction_state_for_file(&redaction_entries, "db_metadata.json"),
                privacy_tier: "default".to_string(),
                size_bytes: bundle_file_size(&bundle_dir, "db_metadata.json"),
                elapsed_ms: elapsed_ms(db_source_started),
                warning_ids: Vec::new(),
            });

            // Recent events (sanitized summaries)
            let events_source_started = Instant::now();
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
                    sources.push(IncidentSourceEntry {
                        name: "recent_events".to_string(),
                        file: Some("recent_events.json".to_string()),
                        status: IncidentSourceStatus::Collected,
                        evidence_state: IncidentEvidenceState::Measured,
                        source_surface: format!(
                            "rusqlite read-only recent events {}",
                            db_path.display()
                        ),
                        mutates_state: false,
                        generated_at: Some(exported_at.clone()),
                        freshness_ms: Some(0),
                        max_age_ms: Some(300_000),
                        redaction: redaction_state_for_file(
                            &redaction_entries,
                            "recent_events.json",
                        ),
                        privacy_tier: "default".to_string(),
                        size_bytes: bundle_file_size(&bundle_dir, "recent_events.json"),
                        elapsed_ms: elapsed_ms(events_source_started),
                        warning_ids: Vec::new(),
                    });
                } else {
                    let warning_id = "recent_events.query_failed";
                    warnings.push(incident_warning(
                        warning_id,
                        "recent_events",
                        "failed to query recent events from the read-only database".to_string(),
                    ));
                    sources.push(IncidentSourceEntry {
                        name: "recent_events".to_string(),
                        file: None,
                        status: IncidentSourceStatus::Failed,
                        evidence_state: IncidentEvidenceState::Unavailable,
                        source_surface: format!(
                            "rusqlite read-only recent events {}",
                            db_path.display()
                        ),
                        mutates_state: false,
                        generated_at: None,
                        freshness_ms: None,
                        max_age_ms: Some(300_000),
                        redaction: IncidentRedactionState::NotApplicable,
                        privacy_tier: "default".to_string(),
                        size_bytes: 0,
                        elapsed_ms: elapsed_ms(events_source_started),
                        warning_ids: vec![warning_id.to_string()],
                    });
                }
            } else {
                sources.push(skipped_source(
                    "recent_events",
                    "max_events=0".to_string(),
                    Some(300_000),
                    elapsed_ms(events_source_started),
                    "recent_events.max_events_zero",
                    "max_events=0 disabled recent event collection".to_string(),
                    &mut warnings,
                ));
            }
        } else {
            let warning_id = "db_metadata.unavailable";
            warnings.push(incident_warning(
                warning_id,
                "db_metadata",
                format!("database path does not exist: {}", db_path.display()),
            ));
            sources.push(IncidentSourceEntry {
                name: "db_metadata".to_string(),
                file: None,
                status: IncidentSourceStatus::Unavailable,
                evidence_state: IncidentEvidenceState::Unavailable,
                source_surface: format!("rusqlite read-only {}", db_path.display()),
                mutates_state: false,
                generated_at: None,
                freshness_ms: None,
                max_age_ms: Some(300_000),
                redaction: IncidentRedactionState::NotApplicable,
                privacy_tier: "default".to_string(),
                size_bytes: 0,
                elapsed_ms: elapsed_ms(db_source_started),
                warning_ids: vec![warning_id.to_string()],
            });
            if opts.max_events > 0 {
                let events_warning_id = "recent_events.unavailable";
                warnings.push(incident_warning(
                    events_warning_id,
                    "recent_events",
                    "recent events were requested, but the database path was unavailable"
                        .to_string(),
                ));
                sources.push(IncidentSourceEntry {
                    name: "recent_events".to_string(),
                    file: None,
                    status: IncidentSourceStatus::Unavailable,
                    evidence_state: IncidentEvidenceState::Unavailable,
                    source_surface: format!(
                        "rusqlite read-only recent events {}",
                        db_path.display()
                    ),
                    mutates_state: false,
                    generated_at: None,
                    freshness_ms: None,
                    max_age_ms: Some(300_000),
                    redaction: IncidentRedactionState::NotApplicable,
                    privacy_tier: "default".to_string(),
                    size_bytes: 0,
                    elapsed_ms: elapsed_ms(db_source_started),
                    warning_ids: vec![events_warning_id.to_string()],
                });
            }
        }
    } else {
        let elapsed = elapsed_ms(db_source_started);
        sources.push(skipped_source(
            "db_metadata",
            "optional database path not provided".to_string(),
            Some(300_000),
            elapsed,
            "db_metadata.skipped",
            "optional database path was not provided".to_string(),
            &mut warnings,
        ));
        sources.push(skipped_source(
            "recent_events",
            "optional database path not provided".to_string(),
            Some(300_000),
            elapsed,
            "recent_events.db_not_configured",
            "recent event collection was skipped because no database path was provided".to_string(),
            &mut warnings,
        ));
    }

    // 4. Optionally collect a bounded process sample.
    let process_source_started = Instant::now();
    if let Some(config) = process_sampler {
        match run_process_sampler(config, &exported_at) {
            Ok(sample) => {
                let sample_json =
                    serde_json::to_string_pretty(&sample).map_err(std::io::Error::other)?;
                write_redacted_file(
                    "process_sample.json",
                    &sample_json,
                    &bundle_dir,
                    &redactor,
                    &mut files,
                    &mut total_size,
                    &mut redaction_entries,
                )?;
                sources.push(IncidentSourceEntry {
                    name: "process_sample".to_string(),
                    file: Some("process_sample.json".to_string()),
                    status: IncidentSourceStatus::Collected,
                    evidence_state: IncidentEvidenceState::Mixed,
                    source_surface: config.source_surface(),
                    mutates_state: false,
                    generated_at: Some(exported_at.clone()),
                    freshness_ms: Some(0),
                    max_age_ms: Some(config.timeout_ms),
                    redaction: redaction_state_for_file(&redaction_entries, "process_sample.json"),
                    privacy_tier: config.privacy_tier.to_string(),
                    size_bytes: bundle_file_size(&bundle_dir, "process_sample.json"),
                    elapsed_ms: elapsed_ms(process_source_started),
                    warning_ids: Vec::new(),
                });
            }
            Err(ProcessSamplerError::Unavailable(message)) => {
                let warning_id = "process_sample.unavailable";
                warnings.push(incident_warning(warning_id, "process_sample", message));
                sources.push(process_sample_degraded_source(
                    config,
                    IncidentSourceStatus::Unavailable,
                    elapsed_ms(process_source_started),
                    warning_id,
                ));
            }
            Err(ProcessSamplerError::Timeout { timeout_ms }) => {
                let warning_id = "process_sample.timeout";
                warnings.push(incident_warning(
                    warning_id,
                    "process_sample",
                    format!("process sampler exceeded its {timeout_ms} ms timeout"),
                ));
                sources.push(process_sample_degraded_source(
                    config,
                    IncidentSourceStatus::Failed,
                    elapsed_ms(process_source_started),
                    warning_id,
                ));
            }
            Err(ProcessSamplerError::Failed(message)) => {
                let warning_id = "process_sample.failed";
                warnings.push(incident_warning(warning_id, "process_sample", message));
                sources.push(process_sample_degraded_source(
                    config,
                    IncidentSourceStatus::Failed,
                    elapsed_ms(process_source_started),
                    warning_id,
                ));
            }
        }
    } else {
        sources.push(skipped_source(
            "process_sample",
            "process sampler disabled by incident-bundle privacy policy".to_string(),
            Some(5_000),
            elapsed_ms(process_source_started),
            "process_sample.skipped",
            "process sampling is opt-in and was not enabled for this bundle".to_string(),
            &mut warnings,
        ));
    }

    // 5. Sanitize fields that are written directly into manifest/warnings
    // payloads instead of going through write_redacted_file.
    sanitize_incident_manifest_fields_for_payload(
        &mut sources,
        &mut warnings,
        &redactor,
        &mut redaction_entries,
    );
    let manifest_path_redactions = redactor.detect(&bundle_dir.display().to_string()).len();
    record_file_redactions(
        &mut redaction_entries,
        "incident_manifest.json",
        manifest_path_redactions,
    );

    // 6. Write redaction report
    let total_redactions: usize = redaction_entries.iter().map(|e| e.count).sum();
    let redacted_files = redaction_entries.len();
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

    // 7. Write source warnings and README before the manifest so the manifest
    // file list is complete.
    let warnings_body = warnings_jsonl(&warnings)?;
    let warning_bytes = warnings_body.as_bytes();
    total_size += warning_bytes.len() as u64;
    write_file_sync(&bundle_dir.join("warnings.jsonl"), warning_bytes)?;
    files.push("warnings.jsonl".to_string());

    let mut manifest_files = files.clone();
    manifest_files.push("README.md".to_string());
    manifest_files.push("incident_manifest.json".to_string());

    let readme = generate_incident_bundle_readme(
        opts.kind,
        &exported_at,
        &manifest_files,
        sources.len(),
        warnings.len(),
    );
    let readme_bytes = readme.as_bytes();
    total_size += readme_bytes.len() as u64;
    write_file_sync(&bundle_dir.join("README.md"), readme_bytes)?;

    // 8. Write incident manifest. The manifest lists itself, so compute
    // total_size_bytes to a fixed point before writing it.
    let process_sample_allowed = process_sampler.is_some();
    let mut result = IncidentBundleResult {
        path: bundle_dir.clone(),
        kind: opts.kind,
        files: manifest_files,
        total_size_bytes: total_size,
        wa_version: crate::VERSION.to_string(),
        exported_at: exported_at.clone(),
        swarm: Some(SwarmIncidentBundleManifest {
            contract_id: "ft.swarm_incident_bundle.v1".to_string(),
            schema_version: 1,
            format_version: "1.0".to_string(),
            bundle_id: bundle_name.clone(),
            kind: opts.kind,
            created_at: exported_at.clone(),
            generator: IncidentBundleGenerator::current(),
            privacy_budget: if process_sample_allowed {
                IncidentPrivacyBudget::default_for_process_sampling(opts.max_events, true)
            } else {
                IncidentPrivacyBudget::default_for_process_sampling(opts.max_events, false)
            },
            collection_policy: IncidentCollectionPolicy::with_process_sampler(
                if process_sample_allowed {
                    "bounded_snapshot"
                } else {
                    "disabled"
                },
            ),
            environment: IncidentEnvironmentSummary::current(),
            sources,
            warnings,
            redaction_summary: IncidentRedactionSummary {
                total_redactions,
                redacted_files,
            },
            total_size_bytes: total_size,
        }),
    };

    let mut manifest_result = result.clone();
    manifest_result.path = PathBuf::from(&bundle_name);
    let manifest_json = loop {
        let manifest_json =
            serde_json::to_string_pretty(&manifest_result).map_err(std::io::Error::other)?;
        let next_total = total_size.saturating_add(manifest_json.len() as u64);
        if next_total == manifest_result.total_size_bytes {
            break manifest_json;
        }
        manifest_result.total_size_bytes = next_total;
        if let Some(swarm) = &mut manifest_result.swarm {
            swarm.total_size_bytes = next_total;
        }
    };
    result.total_size_bytes = manifest_result.total_size_bytes;
    if let (Some(result_swarm), Some(manifest_swarm)) = (&mut result.swarm, &manifest_result.swarm)
    {
        result_swarm.total_size_bytes = manifest_swarm.total_size_bytes;
    }
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
            "SELECT schema_version FROM ft_meta WHERE id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .ok()
        .or_else(|| {
            conn.query_row(
                "SELECT schema_version FROM wa_meta WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .ok()
        });

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
    serde_json::to_string_pretty(&events)
        .inspect_err(|e| tracing::warn!(error = %e, "crash dump events serialization failed"))
        .ok()
}

#[derive(Debug)]
enum ProcessSamplerError {
    Unavailable(String),
    Timeout { timeout_ms: u64 },
    Failed(String),
}

fn process_sample_degraded_source(
    config: &IncidentProcessSamplerConfig,
    status: IncidentSourceStatus,
    elapsed_ms: u64,
    warning_id: &str,
) -> IncidentSourceEntry {
    IncidentSourceEntry {
        name: "process_sample".to_string(),
        file: None,
        status,
        evidence_state: IncidentEvidenceState::Unavailable,
        source_surface: config.source_surface(),
        mutates_state: false,
        generated_at: None,
        freshness_ms: None,
        max_age_ms: Some(config.timeout_ms),
        redaction: IncidentRedactionState::NotApplicable,
        privacy_tier: config.privacy_tier.clone(),
        size_bytes: 0,
        elapsed_ms,
        warning_ids: vec![warning_id.to_string()],
    }
}

fn run_process_sampler(
    config: &IncidentProcessSamplerConfig,
    sampled_at: &str,
) -> Result<IncidentProcessSample, ProcessSamplerError> {
    if config.timeout_ms == 0 {
        return Err(ProcessSamplerError::Timeout { timeout_ms: 0 });
    }

    let mut command = match config.program {
        IncidentProcessSamplerProgram::Ps => Command::new("ps"),
        IncidentProcessSamplerProgram::MissingToolForTest => {
            Command::new("__ft_missing_process_sampler__")
        }
    };
    command
        .args(config.command_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProcessSamplerError::Unavailable(format!(
                "process sampler command was unavailable: {}",
                config.program.label()
            ))
        } else {
            ProcessSamplerError::Failed(format!(
                "failed to start process sampler command {}: {error}",
                config.program.label()
            ))
        }
    })?;

    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let output = child.wait_with_output().map_err(|error| {
                    ProcessSamplerError::Failed(format!(
                        "failed to collect process sampler output: {error}"
                    ))
                })?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(ProcessSamplerError::Failed(format!(
                        "process sampler exited with status {}: {}",
                        output.status,
                        stderr.trim()
                    )));
                }
                let stdout = String::from_utf8(output.stdout).map_err(|error| {
                    ProcessSamplerError::Failed(format!(
                        "process sampler output was not valid UTF-8: {error}"
                    ))
                })?;
                return Ok(build_process_sample(config, sampled_at, &stdout));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcessSamplerError::Timeout {
                        timeout_ms: config.timeout_ms,
                    });
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProcessSamplerError::Failed(format!(
                    "failed while waiting for process sampler command: {error}"
                )));
            }
        }
    }
}

fn build_process_sample(
    config: &IncidentProcessSamplerConfig,
    sampled_at: &str,
    stdout: &str,
) -> IncidentProcessSample {
    let processes = parse_process_rows(stdout);
    let resident_bytes = sum_optional_bytes(processes.iter().map(|row| row.resident_bytes));
    let virtual_bytes = sum_optional_bytes(processes.iter().map(|row| row.virtual_bytes));
    let memory_categories = vec![
        memory_category(
            "resident_memory",
            resident_bytes,
            IncidentSourceStatus::Collected,
            IncidentEvidenceState::Measured,
        ),
        memory_category(
            "virtual_memory",
            virtual_bytes,
            IncidentSourceStatus::Collected,
            IncidentEvidenceState::Measured,
        ),
        memory_category(
            "heap_memory",
            None,
            IncidentSourceStatus::Unavailable,
            IncidentEvidenceState::Unavailable,
        ),
        memory_category(
            "graphics_media_memory",
            None,
            IncidentSourceStatus::Unavailable,
            IncidentEvidenceState::Unavailable,
        ),
    ];

    IncidentProcessSample {
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        sampled_at: sampled_at.to_string(),
        timeout_ms: config.timeout_ms,
        collector: config.program.label().to_string(),
        processes,
        memory_categories,
    }
}

fn parse_process_rows(stdout: &str) -> Vec<IncidentProcessRow> {
    stdout
        .lines()
        .filter_map(parse_process_row)
        .take(512)
        .collect()
}

fn parse_process_row(line: &str) -> Option<IncidentProcessRow> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let parent_pid = parts.next().and_then(|value| value.parse::<u32>().ok());
    let resident_bytes = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(kib_to_bytes);
    let virtual_bytes = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(kib_to_bytes);
    let command = sanitize_process_command(&parts.collect::<Vec<_>>().join(" "));
    Some(IncidentProcessRow {
        pid,
        parent_pid,
        resident_bytes,
        virtual_bytes,
        command,
    })
}

fn sum_optional_bytes(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut saw_value = false;
    let mut total = 0_u64;
    for value in values.flatten() {
        saw_value = true;
        total = total.saturating_add(value);
    }
    saw_value.then_some(total)
}

fn memory_category(
    category: &str,
    bytes: Option<u64>,
    status: IncidentSourceStatus,
    evidence_state: IncidentEvidenceState,
) -> IncidentProcessMemoryCategory {
    IncidentProcessMemoryCategory {
        category: category.to_string(),
        bytes,
        status,
        evidence_state,
    }
}

fn kib_to_bytes(kib: u64) -> u64 {
    kib.saturating_mul(1024)
}

fn sanitize_process_command(command: &str) -> String {
    let label = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .chars()
        .take(200)
        .collect::<String>();
    if label.is_empty() {
        "unknown".to_string()
    } else {
        label
    }
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

const SWARM_INCIDENT_BUNDLE_CONTRACT_ID: &str = "ft.swarm_incident_bundle.v1";

/// Per-status count for a verified incident bundle's source inventory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IncidentSourceStatusCounts {
    /// Sources that wrote a payload.
    pub collected: usize,
    /// Sources intentionally skipped by policy/options.
    pub skipped: usize,
    /// Sources unavailable in the captured environment.
    pub unavailable: usize,
    /// Sources that were attempted and failed.
    pub failed: usize,
    /// Sources outside their freshness budget.
    pub stale: usize,
}

impl IncidentSourceStatusCounts {
    fn record(&mut self, status: IncidentSourceStatus) {
        match status {
            IncidentSourceStatus::Collected => self.collected += 1,
            IncidentSourceStatus::Skipped => self.skipped += 1,
            IncidentSourceStatus::Unavailable => self.unavailable += 1,
            IncidentSourceStatus::Failed => self.failed += 1,
            IncidentSourceStatus::Stale => self.stale += 1,
        }
    }
}

/// Operator-facing summary extracted from a verified incident bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentBundleVerificationSummary {
    /// Bundle directory that was verified.
    pub bundle_path: String,
    /// Incident kind parsed from the manifest, when available.
    pub kind: Option<IncidentKind>,
    /// Swarm extension contract id.
    pub contract_id: Option<String>,
    /// Swarm extension schema version.
    pub schema_version: Option<u32>,
    /// Swarm extension format version.
    pub format_version: Option<String>,
    /// Privacy tier recorded by the collector.
    pub privacy_tier: Option<String>,
    /// Counts by source collection status.
    pub source_counts: IncidentSourceStatusCounts,
    /// Sources with skipped, unavailable, failed, or stale status.
    pub degraded_sources: Vec<String>,
    /// Highest-signal source problems for the operator to inspect first.
    pub suspect_sources: Vec<String>,
    /// Active Beads blockers discovered in bundle payloads.
    pub active_blockers: Vec<String>,
    /// RCH/proof evidence summary discovered in bundle payloads.
    pub proof_rch_status: Option<String>,
    /// Resource pressure summary discovered in bundle payloads.
    pub resource_pressure: Option<String>,
    /// Process sampler summary or degraded state.
    pub process_sample: Option<String>,
    /// Concrete next commands a second agent can run from the bundle summary.
    pub next_commands: Vec<String>,
}

impl IncidentBundleVerificationSummary {
    fn new(bundle_path: &Path) -> Self {
        Self {
            bundle_path: bundle_path.display().to_string(),
            kind: None,
            contract_id: None,
            schema_version: None,
            format_version: None,
            privacy_tier: None,
            source_counts: IncidentSourceStatusCounts::default(),
            degraded_sources: Vec::new(),
            suspect_sources: Vec::new(),
            active_blockers: Vec::new(),
            proof_rch_status: None,
            resource_pressure: None,
            process_sample: None,
            next_commands: Vec::new(),
        }
    }
}

/// Strict verifier result for an incident bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentBundleVerificationResult {
    /// Overall verifier status: "pass" or "fail".
    pub status: String,
    /// Concise operator summary derived from the manifest and payloads.
    pub summary: IncidentBundleVerificationSummary,
    /// Individual structural, privacy, and consistency checks.
    pub checks: Vec<ReplayCheck>,
    /// Non-fatal warnings, including readable newer-minor format versions.
    pub warnings: Vec<String>,
}

/// Verify an incident bundle as a portable handoff artifact.
///
/// Unlike replay mode, this checks the complete bundle contract: required
/// files, swarm schema version, warnings/source consistency, recursive
/// redaction, and source-payload provenance.
pub fn verify_incident_bundle(
    bundle_path: &Path,
) -> std::io::Result<IncidentBundleVerificationResult> {
    if !bundle_path.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Bundle directory not found: {}", bundle_path.display()),
        ));
    }

    let mut checks = Vec::new();
    let mut warnings = Vec::new();
    let mut summary = IncidentBundleVerificationSummary::new(bundle_path);
    let manifest_path = bundle_path.join("incident_manifest.json");

    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(content) => match serde_json::from_str::<IncidentBundleResult>(&content) {
            Ok(manifest) => {
                push_replay_check(
                    &mut checks,
                    "manifest_valid",
                    true,
                    "incident_manifest.json is valid",
                );
                manifest
            }
            Err(error) => {
                push_replay_check(
                    &mut checks,
                    "manifest_valid",
                    false,
                    format!("Invalid manifest JSON: {error}"),
                );
                return Ok(finalize_incident_bundle_verification(
                    bundle_path,
                    summary,
                    checks,
                    warnings,
                ));
            }
        },
        Err(error) => {
            push_replay_check(
                &mut checks,
                "manifest_valid",
                false,
                format!("Cannot read manifest: {error}"),
            );
            return Ok(finalize_incident_bundle_verification(
                bundle_path,
                summary,
                checks,
                warnings,
            ));
        }
    };

    summary.kind = Some(manifest.kind);
    let mut manifest_files = HashSet::new();
    let mut manifest_file_paths_valid = true;
    let mut manifest_listed_files_present = true;
    for file in &manifest.files {
        if let Err(error) = validate_bundle_relative_path(file) {
            manifest_file_paths_valid = false;
            warnings.push(format!("manifest file path {file} is invalid: {error}"));
            continue;
        }
        manifest_files.insert(file.clone());
        if !bundle_path.join(file).is_file() {
            manifest_listed_files_present = false;
        }
    }
    push_replay_check(
        &mut checks,
        "manifest_file_paths_safe",
        manifest_file_paths_valid,
        if manifest_file_paths_valid {
            "All manifest file paths are bundle-relative".to_string()
        } else {
            "One or more manifest file paths are unsafe".to_string()
        },
    );
    push_replay_check(
        &mut checks,
        "files_complete",
        manifest_listed_files_present,
        if manifest_listed_files_present {
            format!(
                "All {} manifest-listed files are present",
                manifest.files.len()
            )
        } else {
            "One or more manifest-listed files are missing".to_string()
        },
    );

    let required_files_present = [
        "incident_manifest.json",
        "README.md",
        "redaction_report.json",
        "warnings.jsonl",
    ]
    .into_iter()
    .all(|file| manifest_files.contains(file) && bundle_path.join(file).is_file());
    push_replay_check(
        &mut checks,
        "required_files_present",
        required_files_present,
        if required_files_present {
            "Required bundle files are listed and present".to_string()
        } else {
            "Required bundle files must be listed in the manifest and present on disk".to_string()
        },
    );

    let Some(swarm) = manifest.swarm.as_ref() else {
        push_replay_check(
            &mut checks,
            "swarm_extension_present",
            false,
            "manifest missing swarm incident-bundle extension",
        );
        return Ok(finalize_incident_bundle_verification(
            bundle_path,
            summary,
            checks,
            warnings,
        ));
    };

    summary.contract_id = Some(swarm.contract_id.clone());
    summary.schema_version = Some(swarm.schema_version);
    summary.format_version = Some(swarm.format_version.clone());
    summary.privacy_tier = Some(swarm.privacy_budget.tier.clone());

    push_replay_check(
        &mut checks,
        "swarm_contract_id",
        swarm.contract_id == SWARM_INCIDENT_BUNDLE_CONTRACT_ID,
        format!("contract_id={}", swarm.contract_id),
    );
    push_replay_check(
        &mut checks,
        "schema_version_supported",
        swarm.schema_version == 1,
        format!("schema_version={}", swarm.schema_version),
    );
    push_replay_check(
        &mut checks,
        "version_compatible",
        incident_format_version_compatible(&swarm.format_version, &mut warnings),
        format!("format_version={}", swarm.format_version),
    );

    let manifest_warning_ids = swarm
        .warnings
        .iter()
        .map(|warning| warning.id.clone())
        .collect::<HashSet<_>>();
    match warning_jsonl_ids(bundle_path) {
        Ok(jsonl_warning_ids) => {
            push_replay_check(
                &mut checks,
                "warnings_jsonl_consistent",
                jsonl_warning_ids == manifest_warning_ids,
                if jsonl_warning_ids == manifest_warning_ids {
                    format!("{} warning id(s) match manifest", jsonl_warning_ids.len())
                } else {
                    format!(
                        "warnings.jsonl ids {jsonl_warning_ids:?} do not match manifest ids {manifest_warning_ids:?}"
                    )
                },
            );
        }
        Err(error) => {
            push_replay_check(&mut checks, "warnings_jsonl_consistent", false, error);
        }
    }

    let mut source_files = HashMap::new();
    let mut source_warning_refs_valid = true;
    let mut degraded_sources_explained = true;
    let mut source_payload_refs_valid = true;
    for source in &swarm.sources {
        summary.source_counts.record(source.status);
        if source.status != IncidentSourceStatus::Collected {
            let degraded = format!(
                "{}:{}",
                source.name,
                incident_source_status_label(source.status)
            );
            summary.degraded_sources.push(degraded.clone());
            if matches!(
                source.status,
                IncidentSourceStatus::Unavailable
                    | IncidentSourceStatus::Failed
                    | IncidentSourceStatus::Stale
            ) {
                summary.suspect_sources.push(degraded);
            }
            if source.warning_ids.is_empty() {
                degraded_sources_explained = false;
            }
        }
        for warning_id in &source.warning_ids {
            if !manifest_warning_ids.contains(warning_id) {
                source_warning_refs_valid = false;
            }
        }
        if source.status == IncidentSourceStatus::Collected && source.file.is_none() {
            source_payload_refs_valid = false;
        }
        if source.status == IncidentSourceStatus::Unavailable && source.file.is_some() {
            source_payload_refs_valid = false;
        }
        if let Some(file) = &source.file {
            if validate_bundle_relative_path(file).is_err()
                || !manifest_files.contains(file)
                || !bundle_path.join(file).is_file()
            {
                source_payload_refs_valid = false;
            } else {
                source_files.insert(file.clone(), source.name.clone());
                enrich_incident_verification_summary(bundle_path, source, &mut summary);
            }
        } else if source.name == "process_sample"
            && source.status != IncidentSourceStatus::Collected
        {
            summary.process_sample = Some(incident_source_status_label(source.status).to_string());
        }
    }
    push_replay_check(
        &mut checks,
        "degraded_sources_explained",
        degraded_sources_explained,
        if degraded_sources_explained {
            "All degraded sources carry warning ids".to_string()
        } else {
            "One or more degraded sources lack warning ids".to_string()
        },
    );
    push_replay_check(
        &mut checks,
        "source_warning_refs_valid",
        source_warning_refs_valid,
        if source_warning_refs_valid {
            "All source warning ids resolve to manifest warnings".to_string()
        } else {
            "One or more source warning ids are missing from manifest warnings".to_string()
        },
    );
    push_replay_check(
        &mut checks,
        "source_payload_refs_valid",
        source_payload_refs_valid,
        if source_payload_refs_valid {
            "All collected source payloads are listed and present".to_string()
        } else {
            "One or more source payload references are missing or unsafe".to_string()
        },
    );

    let source_payloads_have_provenance = match source_payload_files(bundle_path) {
        Ok(payloads) => payloads.into_iter().all(|path| {
            bundle_relative_display(bundle_path, &path)
                .is_ok_and(|relative| source_files.contains_key(&relative))
        }),
        Err(error) => {
            warnings.push(error);
            false
        }
    };
    push_replay_check(
        &mut checks,
        "source_payloads_have_provenance",
        source_payloads_have_provenance,
        if source_payloads_have_provenance {
            "All files under sources/ are referenced by manifest source entries".to_string()
        } else {
            "One or more files under sources/ lack manifest source provenance".to_string()
        },
    );

    match read_redaction_report(bundle_path) {
        Ok(report) => {
            push_replay_check(
                &mut checks,
                "redaction_report_valid",
                true,
                format!(
                    "{} total redactions across {} files",
                    report.total_redactions,
                    report.per_file.len()
                ),
            );
            push_replay_check(
                &mut checks,
                "redaction_summary_consistent",
                report.total_redactions == swarm.redaction_summary.total_redactions,
                if report.total_redactions == swarm.redaction_summary.total_redactions {
                    "redaction_report total matches manifest swarm summary".to_string()
                } else {
                    format!(
                        "redaction_report total {} does not match manifest total {}",
                        report.total_redactions, swarm.redaction_summary.total_redactions
                    )
                },
            );
        }
        Err(error) => {
            push_replay_check(&mut checks, "redaction_report_valid", false, error);
        }
    }

    match scan_bundle_for_raw_secrets(bundle_path) {
        Ok(leaks) => {
            push_replay_check(
                &mut checks,
                "no_raw_secrets_recursive",
                leaks.is_empty(),
                if leaks.is_empty() {
                    "No raw secrets detected in bundle text files".to_string()
                } else {
                    format!("raw secret candidates detected in {}", leaks.join(", "))
                },
            );
        }
        Err(error) => {
            push_replay_check(
                &mut checks,
                "no_raw_secrets_recursive",
                false,
                error.to_string(),
            );
        }
    }

    Ok(finalize_incident_bundle_verification(
        bundle_path,
        summary,
        checks,
        warnings,
    ))
}

/// Render the strict verifier result into a concise human-readable summary.
#[must_use]
pub fn render_incident_bundle_verification_summary(
    result: &IncidentBundleVerificationResult,
) -> String {
    let summary = &result.summary;
    let mut out = String::new();
    out.push_str("Verifier summary\n");
    out.push_str(&format!("  Status:  {}\n", result.status));
    if let Some(kind) = summary.kind {
        out.push_str(&format!("  Kind:    {kind}\n"));
    }
    if let Some(tier) = &summary.privacy_tier {
        out.push_str(&format!("  Privacy: {tier}\n"));
    }
    out.push_str(&format!(
        "  Sources: collected={}, skipped={}, unavailable={}, failed={}, stale={}\n",
        summary.source_counts.collected,
        summary.source_counts.skipped,
        summary.source_counts.unavailable,
        summary.source_counts.failed,
        summary.source_counts.stale,
    ));
    if !summary.suspect_sources.is_empty() {
        out.push_str("  Suspect sources:\n");
        for source in &summary.suspect_sources {
            out.push_str(&format!("    - {source}\n"));
        }
    }
    if !summary.active_blockers.is_empty() {
        out.push_str("  Active blockers:\n");
        for blocker in &summary.active_blockers {
            out.push_str(&format!("    - {blocker}\n"));
        }
    }
    if let Some(rch_status) = &summary.proof_rch_status {
        out.push_str(&format!("  RCH/proof: {rch_status}\n"));
    }
    if let Some(resource_pressure) = &summary.resource_pressure {
        out.push_str(&format!("  Resource pressure: {resource_pressure}\n"));
    }
    if let Some(process_sample) = &summary.process_sample {
        out.push_str(&format!("  Process sample: {process_sample}\n"));
    }
    if !summary.next_commands.is_empty() {
        out.push_str("  Next commands:\n");
        for command in &summary.next_commands {
            out.push_str(&format!("    - {command}\n"));
        }
    }
    out
}

fn finalize_incident_bundle_verification(
    bundle_path: &Path,
    mut summary: IncidentBundleVerificationSummary,
    checks: Vec<ReplayCheck>,
    warnings: Vec<String>,
) -> IncidentBundleVerificationResult {
    let status = if checks.iter().all(|check| check.passed) {
        "pass"
    } else {
        "fail"
    }
    .to_string();
    summary.next_commands = incident_verification_next_commands(bundle_path, &summary, &checks);
    IncidentBundleVerificationResult {
        status,
        summary,
        checks,
        warnings,
    }
}

fn push_replay_check(
    checks: &mut Vec<ReplayCheck>,
    name: impl Into<String>,
    passed: bool,
    detail: impl Into<String>,
) {
    checks.push(ReplayCheck {
        name: name.into(),
        passed,
        detail: Some(detail.into()),
    });
}

fn incident_format_version_compatible(format_version: &str, warnings: &mut Vec<String>) -> bool {
    let Some((major, minor)) = parse_incident_format_version(format_version) else {
        return false;
    };
    let reader = crate::incident_bundle::CURRENT_FORMAT_VERSION;
    if major != reader.major {
        return false;
    }
    if minor > reader.minor {
        warnings.push(format!(
            "bundle minor format {minor} is newer than reader minor {}",
            reader.minor
        ));
    }
    true
}

fn parse_incident_format_version(format_version: &str) -> Option<(u16, u16)> {
    let (major, minor) = format_version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn validate_bundle_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path is empty".to_string());
    }
    if path.contains('\\') {
        return Err("path contains a backslash separator".to_string());
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err("path is absolute".to_string());
    }
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err("path contains a non-normal component".to_string());
        }
    }
    Ok(())
}

fn warning_jsonl_ids(bundle_path: &Path) -> Result<HashSet<String>, String> {
    let warnings_path = bundle_path.join("warnings.jsonl");
    let warnings = fs::read_to_string(&warnings_path)
        .map_err(|error| format!("cannot read {}: {error}", warnings_path.display()))?;
    let mut ids = HashSet::new();
    for (index, line) in warnings.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let warning: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|error| format!("warnings.jsonl line {} invalid: {error}", index + 1))?;
        let id = warning
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("warnings.jsonl line {} missing id", index + 1))?;
        ids.insert(id.to_string());
    }
    Ok(ids)
}

fn read_redaction_report(bundle_path: &Path) -> Result<RedactionReport, String> {
    let path = bundle_path.join("redaction_report.json");
    serde_json::from_str(
        &fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("invalid redaction_report.json: {error}"))
}

fn source_payload_files(bundle_path: &Path) -> Result<Vec<PathBuf>, String> {
    let sources = bundle_path.join("sources");
    if !sources.exists() {
        return Ok(Vec::new());
    }
    collect_bundle_files(&sources)
}

fn collect_bundle_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_bundle_files_inner(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_bundle_files_inner(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in
        fs::read_dir(root).map_err(|error| format!("cannot read {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read directory entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot read file type for {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_bundle_files_inner(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn scan_bundle_for_raw_secrets(bundle_path: &Path) -> std::io::Result<Vec<String>> {
    let redactor = Redactor::new();
    let mut leaks = Vec::new();
    for path in collect_bundle_files(bundle_path).map_err(std::io::Error::other)? {
        if !should_scan_bundle_text_file(&path) {
            continue;
        }
        let content = fs::read_to_string(&path)?;
        if !redactor.detect(&content).is_empty() {
            leaks.push(
                bundle_relative_display(bundle_path, &path)
                    .unwrap_or_else(|_| path.display().to_string()),
            );
        }
    }
    Ok(leaks)
}

fn should_scan_bundle_text_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("json" | "jsonl" | "toml" | "md" | "txt")
    )
}

fn bundle_relative_display(bundle_path: &Path, path: &Path) -> Result<String, String> {
    let relative = path.strip_prefix(bundle_path).map_err(|error| {
        format!(
            "{} is outside {}: {error}",
            path.display(),
            bundle_path.display()
        )
    })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn incident_source_status_label(status: IncidentSourceStatus) -> &'static str {
    match status {
        IncidentSourceStatus::Collected => "collected",
        IncidentSourceStatus::Skipped => "skipped",
        IncidentSourceStatus::Unavailable => "unavailable",
        IncidentSourceStatus::Failed => "failed",
        IncidentSourceStatus::Stale => "stale",
    }
}

fn enrich_incident_verification_summary(
    bundle_path: &Path,
    source: &IncidentSourceEntry,
    summary: &mut IncidentBundleVerificationSummary,
) {
    let Some(file) = &source.file else {
        return;
    };
    let path = bundle_path.join(file);
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };

    match source.name.as_str() {
        "beads_blocker_snapshot" => {
            if let Some(blocked) = value.get("blocked").and_then(serde_json::Value::as_array) {
                for item in blocked {
                    let id = item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    let blockers = item
                        .get("blocked_by")
                        .and_then(serde_json::Value::as_array)
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    if blockers.is_empty() {
                        summary.active_blockers.push(id.to_string());
                    } else {
                        summary
                            .active_blockers
                            .push(format!("{id} blocked by {blockers}"));
                    }
                }
            }
        }
        "beads_coordination_snapshot" => {
            if let Some(candidates) = value
                .get("stale_reopen_review_candidates")
                .and_then(serde_json::Value::as_array)
            {
                for item in candidates {
                    let id = item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    let status = item
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    summary
                        .active_blockers
                        .push(format!("{id} status={status}"));
                }
            }
        }
        "resource_pressure_snapshot" | "resource_pressure_cockpit" => {
            if let Some(pressure) = value.get("pressure").and_then(serde_json::Value::as_object) {
                let mut parts = pressure
                    .iter()
                    .map(|(key, value)| {
                        format!(
                            "{key}={}",
                            value
                                .as_str()
                                .map_or_else(|| value.to_string(), str::to_string)
                        )
                    })
                    .collect::<Vec<_>>();
                parts.sort();
                summary.resource_pressure = Some(parts.join(", "));
            }
        }
        "rch_timeout_evidence" | "proof_rch_evidence" => {
            let verdict = value
                .get("verdict")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let reason = value
                .get("reason_code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let category = value
                .get("reason_category")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let timeout = value
                .get("timeout_ms")
                .and_then(serde_json::Value::as_u64)
                .map(|timeout| format!(" after {timeout}ms"))
                .unwrap_or_default();
            summary.proof_rch_status = Some(format!("{verdict}:{category}:{reason}{timeout}"));
        }
        "process_sample" => {
            let process_count = value
                .get("processes")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            summary.process_sample = Some(format!("{process_count} process row(s)"));
        }
        _ => {}
    }
}

fn incident_verification_next_commands(
    bundle_path: &Path,
    summary: &IncidentBundleVerificationSummary,
    checks: &[ReplayCheck],
) -> Vec<String> {
    let mut commands = vec![format!(
        "ft reproduce replay {} --mode policy --format json",
        bundle_path.display()
    )];
    if checks
        .iter()
        .any(|check| !check.passed && matches!(check.name.as_str(), "no_raw_secrets_recursive"))
    {
        commands.push("ft reproduce export --kind manual --format json".to_string());
    }
    for blocker in &summary.active_blockers {
        if let Some(id) = blocker.split_whitespace().next() {
            commands.push(format!("br show {id} --json"));
        }
    }
    if summary
        .proof_rch_status
        .as_deref()
        .is_some_and(|status| status.contains("timeout"))
    {
        commands.push("rch --json status --workers".to_string());
    }
    if summary.resource_pressure.is_some() {
        commands.push("ft doctor --format json".to_string());
    }
    commands.sort();
    commands.dedup();
    commands
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
                            let detail = format!("schema_version={sv}, events={ec}, segments={sc}");
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
                                        .is_some_and(|s| s.chars().count() > 200)
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

/// Truncate UTF-8 text to at most `max_bytes`, appending `marker` when truncated.
///
/// The returned string is always valid UTF-8 and never exceeds `max_bytes`.
fn truncate_utf8_with_marker(content: &str, max_bytes: usize, marker: &str) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }

    // If the marker itself would exceed the budget, return a bounded marker prefix.
    if marker.len() >= max_bytes {
        let mut marker_end = max_bytes;
        while marker_end > 0 && !marker.is_char_boundary(marker_end) {
            marker_end -= 1;
        }
        return marker[..marker_end].to_string();
    }

    let marker_bytes = marker.len();
    let mut content_end = max_bytes - marker_bytes;
    while content_end > 0 && !content.is_char_boundary(content_end) {
        content_end -= 1;
    }

    let mut out = String::with_capacity(content_end + marker_bytes);
    out.push_str(&content[..content_end]);
    out.push_str(marker);
    out
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
    record_file_redactions(redaction_entries, name, before_count);
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

// ---------------------------------------------------------------------------
// Crash loop detection + backoff
// ---------------------------------------------------------------------------

/// Configuration for crash loop detection and backoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashLoopConfig {
    /// Window in seconds to count recent crashes (default: 300 = 5 min).
    pub window_secs: u64,
    /// Number of crashes within window to trigger "loop" state (default: 3).
    pub crash_threshold: u32,
    /// Initial backoff delay in milliseconds (default: 1000).
    pub initial_delay_ms: u64,
    /// Maximum backoff delay in milliseconds (default: 60000 = 1 min).
    pub max_delay_ms: u64,
    /// Backoff multiplier (default: 2.0).
    pub backoff_factor: f64,
}

impl Default for CrashLoopConfig {
    fn default() -> Self {
        Self {
            window_secs: 300,
            crash_threshold: 3,
            initial_delay_ms: 1_000,
            max_delay_ms: 60_000,
            backoff_factor: 2.0,
        }
    }
}

/// Tracks crash history and computes exponential backoff delays.
///
/// Used to detect rapid repeated crashes (crash loops) and apply capped
/// exponential backoff before allowing restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashLoopDetector {
    config: CrashLoopConfig,
    /// Timestamps of recent crashes (epoch seconds), oldest first.
    crash_timestamps: Vec<u64>,
    /// Number of consecutive crashes without a successful run.
    consecutive_crashes: u32,
}

impl CrashLoopDetector {
    /// Create a new detector with the given configuration.
    #[must_use]
    pub fn new(config: CrashLoopConfig) -> Self {
        Self {
            config,
            crash_timestamps: Vec::new(),
            consecutive_crashes: 0,
        }
    }

    /// Record a crash event at the given timestamp (epoch seconds).
    pub fn record_crash(&mut self, timestamp: u64) {
        self.crash_timestamps.push(timestamp);
        self.consecutive_crashes += 1;
        // Prune timestamps older than the window
        self.prune_old(timestamp);
    }

    /// Record a successful run, resetting the consecutive crash counter.
    pub fn record_success(&mut self) {
        self.consecutive_crashes = 0;
    }

    /// Whether the system is in a crash loop (enough crashes within the window).
    #[must_use]
    pub fn is_crash_loop(&self) -> bool {
        let Some(&now) = self.crash_timestamps.last() else {
            return false;
        };
        self.crashes_in_window(now) >= self.config.crash_threshold
    }

    /// Number of consecutive crashes without a successful run.
    #[must_use]
    pub fn consecutive_crashes(&self) -> u32 {
        self.consecutive_crashes
    }

    /// Compute the next backoff delay in milliseconds based on consecutive crashes.
    ///
    /// Returns 0 if there are no consecutive crashes. Otherwise computes
    /// `initial_delay_ms * backoff_factor^(consecutive - 1)`, capped at `max_delay_ms`.
    #[must_use]
    pub fn next_delay_ms(&self) -> u64 {
        if self.consecutive_crashes == 0 {
            return 0;
        }
        let exponent = (self.consecutive_crashes - 1) as f64;
        let delay = self.config.initial_delay_ms as f64 * self.config.backoff_factor.powf(exponent);
        let capped = delay.min(self.config.max_delay_ms as f64) as u64;
        capped.min(self.config.max_delay_ms)
    }

    /// Count crashes within the detection window relative to `now`.
    #[must_use]
    pub fn crashes_in_window(&self, now: u64) -> u32 {
        let cutoff = now.saturating_sub(self.config.window_secs);
        self.crash_timestamps
            .iter()
            .filter(|&&ts| ts >= cutoff)
            .count() as u32
    }

    /// Total number of recorded restarts (crash timestamps in history).
    #[must_use]
    pub fn total_restarts(&self) -> u32 {
        self.crash_timestamps.len() as u32
    }

    /// Timestamp of the most recent crash, if any.
    #[must_use]
    pub fn last_crash_timestamp(&self) -> Option<u64> {
        self.crash_timestamps.last().copied()
    }

    /// Produce diagnostics fields for inclusion in [`HealthSnapshot`].
    #[must_use]
    pub fn diagnostics(&self) -> CrashLoopDiagnostics {
        CrashLoopDiagnostics {
            restart_count: self.total_restarts(),
            last_crash_at: self.last_crash_timestamp(),
            consecutive_crashes: self.consecutive_crashes,
            current_backoff_ms: self.next_delay_ms(),
            in_crash_loop: self.is_crash_loop(),
        }
    }

    /// Prune crash timestamps older than the window.
    fn prune_old(&mut self, now: u64) {
        let cutoff = now.saturating_sub(self.config.window_secs);
        self.crash_timestamps.retain(|&ts| ts >= cutoff);
    }
}

/// Diagnostic summary from a [`CrashLoopDetector`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashLoopDiagnostics {
    /// Total number of watcher restarts in the detection window.
    pub restart_count: u32,
    /// Timestamp of the most recent crash (epoch seconds).
    pub last_crash_at: Option<u64>,
    /// Number of consecutive crashes without a successful run.
    pub consecutive_crashes: u32,
    /// Current backoff delay in milliseconds.
    pub current_backoff_ms: u64,
    /// Whether the detector considers the system in a crash loop.
    pub in_crash_loop: bool,
}

// ---------------------------------------------------------------------------
// Capture checkpoint
// ---------------------------------------------------------------------------

/// Format version for checkpoint serialization.
const CHECKPOINT_FORMAT_VERSION: u32 = 1;

/// Per-pane capture state saved in a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneCaptureState {
    /// Pane identifier.
    pub pane_id: u64,
    /// Last persisted sequence number for this pane.
    pub last_seq: i64,
    /// Byte offset of the last captured cursor position.
    pub cursor_offset: u64,
    /// Epoch seconds when this pane was last captured.
    pub last_capture_at: u64,
}

/// Checkpoint for resuming capture after restart without duplicate segments.
///
/// The checkpoint is versioned so future changes can be detected and handled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureCheckpoint {
    /// Format version (always `CHECKPOINT_FORMAT_VERSION`).
    pub version: u32,
    /// Epoch seconds when the checkpoint was created.
    pub created_at: u64,
    /// Per-pane capture states.
    pub panes: Vec<PaneCaptureState>,
    /// ft version that created the checkpoint.
    pub wa_version: String,
}

impl CaptureCheckpoint {
    /// Create a new checkpoint with the given pane states.
    #[must_use]
    pub fn new(panes: Vec<PaneCaptureState>) -> Self {
        Self {
            version: CHECKPOINT_FORMAT_VERSION,
            created_at: epoch_secs(),
            panes,
            wa_version: crate::VERSION.to_string(),
        }
    }

    /// Create a checkpoint with an explicit timestamp (for deterministic tests).
    #[must_use]
    pub fn with_timestamp(panes: Vec<PaneCaptureState>, created_at: u64) -> Self {
        Self {
            version: CHECKPOINT_FORMAT_VERSION,
            created_at,
            panes,
            wa_version: crate::VERSION.to_string(),
        }
    }

    /// Save the checkpoint to a JSON file atomically (write-to-tmp then rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp_path = path.with_extension("tmp");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp_path, json.as_bytes())?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Load a checkpoint from a JSON file.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let data = fs::read_to_string(path)?;
        let checkpoint: Self = serde_json::from_str(&data).map_err(std::io::Error::other)?;
        if checkpoint.version != CHECKPOINT_FORMAT_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "unsupported checkpoint version {} (expected {})",
                    checkpoint.version, CHECKPOINT_FORMAT_VERSION
                ),
            ));
        }
        Ok(checkpoint)
    }

    /// Look up the capture state for a specific pane.
    #[must_use]
    pub fn pane_state(&self, pane_id: u64) -> Option<&PaneCaptureState> {
        self.panes.iter().find(|p| p.pane_id == pane_id)
    }

    /// Whether a segment should be skipped (already captured before the checkpoint).
    ///
    /// Returns `true` if the pane has a recorded state and `seq` is at or before
    /// the last persisted sequence number.
    #[must_use]
    pub fn should_skip_segment(&self, pane_id: u64, seq: i64) -> bool {
        self.pane_state(pane_id)
            .is_some_and(|state| seq <= state.last_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static INCIDENT_ROBOT_STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static INCIDENT_PROOF_RCH_EVIDENCE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static INCIDENT_AGENT_MAIL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_incident_source_globals_for_test() -> (
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        (
            INCIDENT_ROBOT_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            INCIDENT_PROOF_RCH_EVIDENCE_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            INCIDENT_AGENT_MAIL_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn clear_incident_source_globals_for_test() {
        IncidentRobotStateSnapshot::clear_global_for_test();
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();
        IncidentProofRchEvidenceSnapshot::clear_global_for_test();
        IncidentAgentMailSnapshot::clear_global_for_test();
    }

    fn seed_incident_panes_db() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("incident.sqlite3");
        let conn = rusqlite::Connection::open(&db_path).expect("open incident db");
        conn.execute_batch(
            "CREATE TABLE panes (
                pane_id INTEGER PRIMARY KEY,
                pane_uuid TEXT,
                domain TEXT NOT NULL,
                window_id INTEGER,
                tab_id INTEGER,
                title TEXT,
                cwd TEXT,
                tty_name TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                observed INTEGER NOT NULL,
                ignore_reason TEXT,
                last_decision_at INTEGER
            );
            INSERT INTO panes (
                pane_id, pane_uuid, domain, window_id, tab_id, title, cwd,
                tty_name, first_seen_at, last_seen_at, observed, ignore_reason,
                last_decision_at
            ) VALUES
                (7, 'pane-uuid-7', 'local', 1, 2, 'build pane',
                 '/repo/frankenterm', '/dev/ttys007', 1700000000000, 1700000004000,
                 1, NULL, 1700000004000),
                (8, NULL, 'local', 1, 3, 'ignored pane',
                 '/tmp', '/dev/ttys008', 1700000001000, 1700000003000,
                 0, 'title excluded by pane filter', 1700000003000);",
        )
        .expect("seed panes table");
        drop(conn);
        (tmp, db_path)
    }

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
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![(1, 1_234_567_890), (2, 1_234_567_800)],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
            fleet_pressure_tier: None,
            swarm_capacity: None,
            leak_risk_inventory: LeakRiskInventorySnapshot::default(),
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
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
            fleet_pressure_tier: None,
            swarm_capacity: None,
            leak_risk_inventory: LeakRiskInventorySnapshot::default(),
        };

        HealthSnapshot::update_global(snapshot);

        let retrieved = HealthSnapshot::get_global();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().timestamp, 1000);
    }

    #[test]
    fn incident_robot_state_source_collects_published_pane_metadata() {
        let _guard = INCIDENT_ROBOT_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentRobotStateSnapshot::clear_global_for_test();

        let captured_at_ms = epoch_millis();
        let snapshot = IncidentRobotStateSnapshot::from_robot_panes(
            captured_at_ms,
            "ft robot state test provider",
            vec![
                crate::robot_types::PaneStateData {
                    pane_id: 7,
                    pane_uuid: Some("pane-uuid-7".to_string()),
                    tab_id: 2,
                    window_id: 1,
                    domain: "local".to_string(),
                    title: Some("build pane".to_string()),
                    cwd: Some("/repo/frankenterm".to_string()),
                    observed: true,
                    ignore_reason: None,
                },
                crate::robot_types::PaneStateData {
                    pane_id: 8,
                    pane_uuid: None,
                    tab_id: 3,
                    window_id: 1,
                    domain: "local".to_string(),
                    title: Some("ignored pane".to_string()),
                    cwd: Some("/tmp".to_string()),
                    observed: false,
                    ignore_reason: Some("title excluded by pane filter".to_string()),
                },
            ],
        );
        IncidentRobotStateSnapshot::update_global(snapshot);

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_robot_state_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("robot_state source");

        IncidentRobotStateSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "robot_state");
        assert_eq!(source.status, IncidentSourceStatus::Collected);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Measured);
        assert_eq!(source.source_surface, "ft robot state test provider");
        assert!(!source.mutates_state);
        assert_eq!(source.file.as_deref(), Some("sources/robot_state.json"));
        assert_eq!(source.max_age_ms, Some(30_000));
        assert!(source.freshness_ms.is_some());

        let payload_path = tmp.path().join("sources/robot_state.json");
        let payload: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(payload_path).expect("payload file"))
                .expect("payload json");
        assert_eq!(payload["full_text_included"], false);
        assert!(payload.get("pane_text").is_none());
        assert_eq!(payload["pane_count"], 2);
        assert_eq!(payload["observed_count"], 1);
        assert_eq!(payload["ignored_count"], 1);
        assert_eq!(payload["unobserved_count"], 0);
        assert_eq!(
            payload["freshness_ms"].as_u64(),
            source.freshness_ms,
            "payload and source metadata should agree on freshness"
        );

        let panes = payload["panes"].as_array().expect("panes array");
        assert_eq!(panes[0]["pane_id"], 7);
        assert_eq!(panes[0]["pane_uuid"], "pane-uuid-7");
        assert_eq!(panes[0]["title"], "build pane");
        assert_eq!(panes[0]["cwd"], "/repo/frankenterm");
        assert_eq!(panes[0]["state"], "observed");
        assert_eq!(panes[1]["pane_id"], 8);
        assert_eq!(panes[1]["state"], "ignored");
        assert_eq!(panes[1]["ignore_reason"], "title excluded by pane filter");
    }

    #[test]
    fn incident_robot_state_source_records_unavailable_without_provider() {
        let _guard = INCIDENT_ROBOT_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentRobotStateSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_robot_state_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("robot_state source");

        assert!(files.is_empty());
        assert_eq!(total_size, 0);
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "robot_state");
        assert_eq!(source.status, IncidentSourceStatus::Unavailable);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Unavailable);
        assert_eq!(source.file, None);
        assert_eq!(
            source.warning_ids,
            vec!["robot_state.snapshot_unavailable".to_string()]
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "robot_state.snapshot_unavailable");
        assert!(!tmp.path().join("sources/robot_state.json").exists());
    }

    #[test]
    fn incident_robot_state_db_fallback_collects_panes_without_provider() {
        let _guard = INCIDENT_ROBOT_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentRobotStateSnapshot::clear_global_for_test();

        let (_db_tmp, db_path) = seed_incident_panes_db();
        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_robot_state_source_with_db(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
            Some(&db_path),
        )
        .expect("robot_state source");

        assert!(warnings.is_empty());
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "robot_state");
        assert_eq!(source.status, IncidentSourceStatus::Collected);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Measured);
        assert!(
            source
                .source_surface
                .contains("rusqlite read-only panes table")
        );
        assert_eq!(source.file.as_deref(), Some("sources/robot_state.json"));
        assert!(!source.mutates_state);

        let payload_path = tmp.path().join("sources/robot_state.json");
        let payload: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(payload_path).expect("payload file"))
                .expect("payload json");
        assert_eq!(payload["full_text_included"], false);
        assert!(payload.get("pane_text").is_none());
        assert_eq!(payload["pane_count"], 2);
        assert_eq!(payload["observed_count"], 1);
        assert_eq!(payload["ignored_count"], 1);

        let panes = payload["panes"].as_array().expect("panes array");
        assert_eq!(panes[0]["pane_id"], 7);
        assert_eq!(panes[0]["pane_uuid"], "pane-uuid-7");
        assert_eq!(panes[0]["title"], "build pane");
        assert_eq!(panes[0]["cwd"], "/repo/frankenterm");
        assert_eq!(panes[0]["state"], "observed");
        assert_eq!(panes[0]["observed_at_ms"], 1_700_000_000_000_u64);
        assert_eq!(panes[0]["last_activity_at_ms"], 1_700_000_004_000_u64);
        assert_eq!(panes[1]["pane_id"], 8);
        assert_eq!(panes[1]["state"], "ignored");
        assert_eq!(panes[1]["ignore_reason"], "title excluded by pane filter");
    }

    #[test]
    fn incident_pane_text_summaries_collect_redacted_bounded_rows() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let raw = format!(
            "build log contained AKIAABCDEFGHIJKLMNOP and then emitted {}\n",
            "x".repeat(200)
        );
        let summary = IncidentPaneTextSummary::from_text(41, &raw, 20, 96, &redactor);
        assert_eq!(summary.redactions, 1);
        assert!(summary.truncated);
        IncidentPaneTextSummariesSnapshot::update_global(IncidentPaneTextSummariesSnapshot::new(
            epoch_millis(),
            "fixture::pane_text_summaries",
            20,
            96,
            true,
            vec![summary],
        ));

        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("pane_text_summaries source");

        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "pane_text_summaries");
        assert_eq!(source.status, IncidentSourceStatus::Collected);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Measured);
        assert_eq!(source.source_surface, "fixture::pane_text_summaries");
        assert_eq!(source.redaction, IncidentRedactionState::Partial);
        assert_eq!(
            source.file.as_deref(),
            Some("sources/pane_text_summaries.json")
        );

        let entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "sources/pane_text_summaries.json")
            .expect("pane text redaction entry");
        assert_eq!(entry.count, 1);

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(payload_text.contains("[REDACTED]"));
        assert!(payload_text.contains("[PANE_TEXT_TRUNCATED]"));

        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(payload["privacy_allowed"], true);
        assert_eq!(payload["summary_count"], 1);
        assert_eq!(payload["redaction_count"], 1);
        assert_eq!(payload["truncated_count"], 1);
        assert_eq!(payload["panes"][0]["pane_id"], 41);
        assert_eq!(payload["panes"][0]["status"], "summary");
        assert_eq!(payload["panes"][0]["redactions"], 1);
    }

    #[test]
    fn incident_pane_text_summaries_sanitize_provider_rows_before_payload() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        IncidentPaneTextSummariesSnapshot::update_global(IncidentPaneTextSummariesSnapshot::new(
            epoch_millis(),
            "fixture::pane_text_summaries",
            20,
            96,
            true,
            vec![IncidentPaneTextSummary {
                pane_id: 42,
                status: "summary".to_string(),
                tail_lines: 20,
                summary: "provider row accidentally included AKIAABCDEFGHIJKLMNOP".to_string(),
                redactions: 0,
                truncated: false,
                truncation_info: None,
                code: None,
                message: None,
            }],
        ));

        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("pane_text_summaries source");

        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        let source = &sources[0];
        assert_eq!(source.redaction, IncidentRedactionState::Partial);

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(payload_text.contains("[REDACTED]"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(payload["redaction_count"], 1);
        assert_eq!(payload["panes"][0]["redactions"], 1);
        assert_eq!(
            payload["panes"][0]["summary"],
            "provider row accidentally included [REDACTED]"
        );
    }

    #[test]
    fn incident_pane_text_summaries_sanitize_provider_error_messages() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        IncidentPaneTextSummariesSnapshot::update_global(IncidentPaneTextSummariesSnapshot::new(
            epoch_millis(),
            "fixture::pane_text_summaries",
            20,
            96,
            true,
            vec![IncidentPaneTextSummary::error(
                43,
                20,
                "pane.read_failed",
                "provider diagnostic included AKIAABCDEFGHIJKLMNOP",
            )],
        ));

        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("pane_text_summaries source");

        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        let source = &sources[0];
        assert_eq!(source.redaction, IncidentRedactionState::Partial);
        let entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "sources/pane_text_summaries.json")
            .expect("pane text redaction entry");
        assert_eq!(entry.count, 1);

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(payload_text.contains("[REDACTED]"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(payload["redaction_count"], 1);
        assert_eq!(payload["panes"][0]["redactions"], 1);
        assert_eq!(
            payload["panes"][0]["message"],
            "provider diagnostic included [REDACTED]"
        );
    }

    #[test]
    fn incident_pane_text_summaries_redact_allowed_privacy_reason() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let reason = "operator note included AKIAABCDEFGHIJKLMNOP";
        IncidentPaneTextSummariesSnapshot::update_global(
            IncidentPaneTextSummariesSnapshot::new(
                epoch_millis(),
                "fixture::pane_text_summaries",
                20,
                96,
                true,
                vec![IncidentPaneTextSummary::from_text(
                    44,
                    "clean output",
                    20,
                    96,
                    &redactor,
                )],
            )
            .with_privacy_reason(reason),
        );

        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("pane_text_summaries source");

        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        assert_eq!(sources[0].redaction, IncidentRedactionState::Partial);
        let source_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "sources/pane_text_summaries.json")
            .expect("pane text redaction entry");
        assert_eq!(source_entry.count, 1);

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(payload["redaction_count"], 1);
        assert_eq!(
            payload["privacy_reason"],
            "operator note included [REDACTED]"
        );
        assert_eq!(payload["panes"][0]["redactions"], 0);
    }

    #[test]
    fn incident_pane_text_summaries_redact_provider_source_surface() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        IncidentPaneTextSummariesSnapshot::update_global(IncidentPaneTextSummariesSnapshot::new(
            epoch_millis(),
            "provider surface included AKIAABCDEFGHIJKLMNOP",
            20,
            96,
            true,
            vec![IncidentPaneTextSummary::from_text(
                45,
                "clean output",
                20,
                96,
                &redactor,
            )],
        ));

        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("pane_text_summaries source");

        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        let source = &sources[0];
        assert_eq!(
            source.source_surface,
            "provider surface included [REDACTED]"
        );
        assert_eq!(source.redaction, IncidentRedactionState::Partial);
        let source_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "sources/pane_text_summaries.json")
            .expect("pane text redaction entry");
        assert_eq!(source_entry.count, 1);
        let manifest_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "incident_manifest.json")
            .expect("manifest redaction entry");
        assert_eq!(manifest_entry.count, 1);

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(
            payload["source_surface"],
            "provider surface included [REDACTED]"
        );
        assert_eq!(payload["redaction_count"], 0);
    }

    #[test]
    fn incident_pane_text_summaries_write_privacy_excluded_placeholders() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let reason = "incident privacy budget pane_text_allowed=false";
        let accidental_summary = IncidentPaneTextSummary::from_text(
            41,
            "privacy disabled but producer accidentally included AKIAABCDEFGHIJKLMNOP",
            20,
            96,
            &redactor,
        );
        IncidentPaneTextSummariesSnapshot::update_global(
            IncidentPaneTextSummariesSnapshot::new(
                epoch_millis(),
                "incident privacy policy",
                20,
                96,
                false,
                vec![accidental_summary],
            )
            .with_privacy_reason(reason),
        );

        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("pane_text_summaries source");

        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "pane_text_summaries.privacy_disabled");
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "pane_text_summaries");
        assert_eq!(source.status, IncidentSourceStatus::Skipped);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Unavailable);
        assert_eq!(
            source.file.as_deref(),
            Some("sources/pane_text_summaries.json")
        );
        assert_eq!(
            source.warning_ids,
            vec!["pane_text_summaries.privacy_disabled".to_string()]
        );

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(!payload_text.contains("[REDACTED]"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(payload["privacy_allowed"], false);
        assert_eq!(payload["privacy_reason"], reason);
        assert_eq!(payload["excluded_count"], 1);
        assert_eq!(payload["redaction_count"], 0);
        assert_eq!(payload["truncated_count"], 0);
        assert_eq!(payload["panes"][0]["summary"], "[PANE_TEXT_EXCLUDED]");
        assert_eq!(payload["panes"][0]["code"], "pane_text.privacy_disabled");
        assert_eq!(payload["panes"][0]["message"], reason);
    }

    #[test]
    fn incident_pane_text_summaries_db_fallback_writes_privacy_placeholders() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let (_db_tmp, db_path) = seed_incident_panes_db();
        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source_with_db(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
            Some(&db_path),
        )
        .expect("pane_text_summaries source");

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "pane_text_summaries.privacy_disabled");
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "pane_text_summaries");
        assert_eq!(source.status, IncidentSourceStatus::Skipped);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Unavailable);
        assert!(
            source
                .source_surface
                .contains("rusqlite read-only panes table")
        );
        assert_eq!(
            source.file.as_deref(),
            Some("sources/pane_text_summaries.json")
        );
        assert!(!source.mutates_state);

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("build pane"));
        assert!(!payload_text.contains("/repo/frankenterm"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(payload["privacy_allowed"], false);
        assert_eq!(payload["summary_count"], 2);
        assert_eq!(payload["excluded_count"], 2);
        assert_eq!(payload["redaction_count"], 0);
        assert_eq!(payload["panes"][0]["pane_id"], 7);
        assert_eq!(payload["panes"][0]["summary"], "[PANE_TEXT_EXCLUDED]");
        assert_eq!(payload["panes"][0]["code"], "pane_text.privacy_disabled");
        assert_eq!(payload["panes"][1]["pane_id"], 8);
        assert_eq!(payload["panes"][1]["summary"], "[PANE_TEXT_EXCLUDED]");
    }

    #[test]
    fn incident_pane_text_summaries_redact_privacy_reason_warnings() {
        let _guard = INCIDENT_PANE_TEXT_SUMMARY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let reason = "privacy disabled after seeing AKIAABCDEFGHIJKLMNOP";
        IncidentPaneTextSummariesSnapshot::update_global(
            IncidentPaneTextSummariesSnapshot::new(
                epoch_millis(),
                "incident privacy policy",
                20,
                96,
                false,
                vec![IncidentPaneTextSummary::excluded(41, 20, reason)],
            )
            .with_privacy_reason(reason),
        );

        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_pane_text_summaries_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("pane_text_summaries source");

        IncidentPaneTextSummariesSnapshot::clear_global_for_test();

        assert_eq!(warnings.len(), 1);
        assert!(!warnings[0].message.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(warnings[0].message.contains("[REDACTED]"));
        assert_eq!(sources[0].redaction, IncidentRedactionState::Partial);
        let source_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "sources/pane_text_summaries.json")
            .expect("pane text redaction entry");
        assert_eq!(source_entry.count, 2);
        let warning_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "warnings.jsonl")
            .expect("warnings redaction entry");
        assert_eq!(warning_entry.count, 1);
        let manifest_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "incident_manifest.json")
            .expect("manifest redaction entry");
        assert_eq!(manifest_entry.count, 1);

        let payload_path = tmp.path().join("sources/pane_text_summaries.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(payload["redaction_count"], 2);
        assert_eq!(
            payload["privacy_reason"],
            "privacy disabled after seeing [REDACTED]"
        );
        assert_eq!(payload["panes"][0]["redactions"], 1);
        assert_eq!(
            payload["panes"][0]["message"],
            "privacy disabled after seeing [REDACTED]"
        );
    }

    #[test]
    fn incident_manifest_field_sanitizer_redacts_manifest_only_fields() {
        let redactor = Redactor::new();
        let mut sources = vec![IncidentSourceEntry {
            name: "db_metadata".to_string(),
            file: None,
            status: IncidentSourceStatus::Unavailable,
            evidence_state: IncidentEvidenceState::Unavailable,
            source_surface: "rusqlite read-only /tmp/AKIAABCDEFGHIJKLMNOP.sqlite".to_string(),
            mutates_state: false,
            generated_at: None,
            freshness_ms: None,
            max_age_ms: Some(300_000),
            redaction: IncidentRedactionState::NotApplicable,
            privacy_tier: "default".to_string(),
            size_bytes: 0,
            elapsed_ms: 0,
            warning_ids: vec!["db_metadata.unavailable".to_string()],
        }];
        let mut warnings = vec![incident_warning(
            "db_metadata.unavailable",
            "db_metadata",
            "database path included AKIAABCDEFGHIJKLMNOP".to_string(),
        )];
        let mut redaction_entries = Vec::new();

        sanitize_incident_manifest_fields_for_payload(
            &mut sources,
            &mut warnings,
            &redactor,
            &mut redaction_entries,
        );

        assert!(!sources[0].source_surface.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(sources[0].source_surface.contains("[REDACTED]"));
        assert!(!warnings[0].message.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(warnings[0].message.contains("[REDACTED]"));
        let manifest_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "incident_manifest.json")
            .expect("manifest redaction entry");
        assert_eq!(manifest_entry.count, 2);
        let warning_entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "warnings.jsonl")
            .expect("warnings redaction entry");
        assert_eq!(warning_entry.count, 1);
    }

    #[test]
    fn incident_proof_rch_evidence_collects_retained_snapshot_without_running_proof() {
        let _guard = INCIDENT_PROOF_RCH_EVIDENCE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentProofRchEvidenceSnapshot::clear_global_for_test();

        IncidentProofRchEvidenceSnapshot::update_global(
            IncidentProofRchEvidenceSnapshot::new(
                epoch_millis(),
                "fixture::proof-ledger",
                "blocked",
                "rch.no_workers_passed_health",
            )
            .with_artifact_paths(vec![
                "tests/e2e/logs/ft-zh4t3/proof_rch_evidence.log".to_string(),
            ])
            .with_attempts(vec![
                IncidentProofRchAttempt::new(
                    "cargo test -p frankenterm-core incident_bundle_tests::verify_",
                    "no_verdict",
                    "rch.result_capture_enospc",
                )
                .with_artifact_path("tests/e2e/logs/ft-zh4t3/proof_rch_evidence.log")
                .with_remote_execution_confirmed(true)
                .with_local_fallback_rejected(true),
            ])
            .with_local_fallback_rejected(true),
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_proof_rch_evidence_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("proof RCH evidence source");

        IncidentProofRchEvidenceSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "proof_rch_evidence");
        assert_eq!(source.status, IncidentSourceStatus::Collected);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Measured);
        assert_eq!(source.source_surface, "fixture::proof-ledger");
        assert_eq!(
            source.file.as_deref(),
            Some("sources/proof_rch_evidence.json")
        );
        assert!(!source.mutates_state);
        assert_eq!(source.warning_ids, Vec::<String>::new());
        assert_eq!(source.max_age_ms, Some(300_000));
        assert!(source.freshness_ms.is_some());
        assert!(files.contains(&"sources/proof_rch_evidence.json".to_string()));
        assert!(total_size > 0);

        let payload_path = tmp.path().join("sources/proof_rch_evidence.json");
        let payload: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(payload_path).expect("payload file"))
                .expect("payload json");
        assert_eq!(payload["verdict"], "blocked");
        assert_eq!(payload["reason_code"], "rch.no_workers_passed_health");
        assert_eq!(payload["reason_category"], "no_worker");
        assert_eq!(payload["artifact_count"], 1);
        assert_eq!(payload["attempt_count"], 1);
        assert_eq!(payload["local_fallback_rejected"], true);
        assert_eq!(payload["setup_chatter_only"], false);
        assert_eq!(payload["collector_launched_proof_commands"], false);
        assert_eq!(payload["collector_mutated_state"], false);
        assert_eq!(payload["local_cargo_counted_as_proof"], false);
        assert_eq!(payload["sync_chatter_counted_as_proof"], false);
        assert_eq!(
            payload["provenance"]["collector_launched_proof_commands"],
            false
        );
        assert_eq!(payload["provenance"]["local_cargo_counted_as_proof"], false);
        assert_eq!(
            payload["provenance"]["sync_chatter_counted_as_proof"],
            false
        );
        assert_eq!(
            payload["attempts"][0]["reason_code"],
            "rch.result_capture_enospc"
        );
        assert_eq!(payload["attempts"][0]["reason_category"], "result");
        assert_eq!(payload["attempts"][0]["remote_execution_confirmed"], true);
        assert_eq!(payload["attempts"][0]["local_fallback_rejected"], true);
        assert_eq!(payload["attempts"][0]["setup_chatter_only"], false);
    }

    #[test]
    fn incident_proof_rch_evidence_warns_when_only_setup_chatter_is_attached() {
        let _guard = INCIDENT_PROOF_RCH_EVIDENCE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentProofRchEvidenceSnapshot::clear_global_for_test();

        IncidentProofRchEvidenceSnapshot::update_global(
            IncidentProofRchEvidenceSnapshot::new(
                epoch_millis(),
                "fixture::rch-sync-log",
                "no_verdict",
                "rch.sync_chatter_only",
            )
            .with_attempts(vec![
                IncidentProofRchAttempt::new(
                    "rch sync --no-cargo",
                    "no_verdict",
                    "rch.queue_sync_chatter",
                )
                .with_setup_chatter_only(true),
            ])
            .with_setup_chatter_only(true),
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_proof_rch_evidence_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("proof RCH evidence source");

        IncidentProofRchEvidenceSnapshot::clear_global_for_test();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "proof_rch_evidence.setup_chatter_only");
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "proof_rch_evidence");
        assert_eq!(source.status, IncidentSourceStatus::Collected);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Mixed);
        assert_eq!(
            source.warning_ids,
            vec!["proof_rch_evidence.setup_chatter_only".to_string()]
        );

        let payload_path = tmp.path().join("sources/proof_rch_evidence.json");
        let payload: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(payload_path).expect("payload file"))
                .expect("payload json");
        assert_eq!(payload["setup_chatter_only"], true);
        assert_eq!(payload["reason_category"], "setup_sync");
        assert_eq!(payload["sync_chatter_counted_as_proof"], false);
        assert_eq!(payload["local_cargo_counted_as_proof"], false);
        assert_eq!(payload["collector_launched_proof_commands"], false);
        assert_eq!(payload["attempts"][0]["setup_chatter_only"], true);
        assert_eq!(payload["attempts"][0]["reason_category"], "setup_sync");
    }

    #[test]
    fn proof_rch_reason_category_classifies_stable_failure_families() {
        assert_eq!(
            proof_rch_reason_category("rch.no_workers_passed_health"),
            "no_worker"
        );
        assert_eq!(
            proof_rch_reason_category("rch.topology_preflight_failed"),
            "topology"
        );
        assert_eq!(
            proof_rch_reason_category("proof.local_fallback_rejected"),
            "local_fallback"
        );
        assert_eq!(
            proof_rch_reason_category("rch.package_materialization_failed"),
            "package_materialization"
        );
        assert_eq!(
            proof_rch_reason_category("ssh.transport_error"),
            "transport"
        );
        assert_eq!(proof_rch_reason_category("cargo.test_failed"), "result");
    }

    #[test]
    fn incident_agent_mail_collects_available_read_only_snapshot() {
        let _guard = INCIDENT_AGENT_MAIL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentAgentMailSnapshot::clear_global_for_test();

        IncidentAgentMailSnapshot::update_global(
            IncidentAgentMailSnapshot::new(
                epoch_millis(),
                "fixture::agent-mail-health",
                "ok",
                "agent_mail.ok",
            )
            .with_health_level("green")
            .with_inventory_counts(Some(19), Some(1_261), Some(1_907))
            .with_active_agents(vec!["BlueLake".to_string(), "GreenCastle".to_string()])
            .with_attempts(vec![
                IncidentAgentMailAttempt::new("health_check", "ok", "agent_mail.ok")
                    .with_elapsed_ms(42),
            ]),
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_agent_mail_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("agent_mail source");

        IncidentAgentMailSnapshot::clear_global_for_test();

        assert!(warnings.is_empty());
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "agent_mail");
        assert_eq!(source.status, IncidentSourceStatus::Collected);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Measured);
        assert_eq!(source.source_surface, "fixture::agent-mail-health");
        assert_eq!(source.file.as_deref(), Some("sources/agent_mail.json"));
        assert!(!source.mutates_state);
        assert!(source.warning_ids.is_empty());
        assert!(files.contains(&"sources/agent_mail.json".to_string()));
        assert!(total_size > 0);

        let payload_path = tmp.path().join("sources/agent_mail.json");
        let payload: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(payload_path).expect("payload file"))
                .expect("payload json");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["health_level"], "green");
        assert_eq!(payload["reason_code"], "agent_mail.ok");
        assert_eq!(payload["reason_category"], "ok");
        assert_eq!(payload["project_count"], 19);
        assert_eq!(payload["agent_count"], 1_261);
        assert_eq!(payload["message_count"], 1_907);
        assert_eq!(payload["attempt_count"], 1);
        assert_eq!(payload["retry_count"], 0);
        assert_eq!(payload["max_retry_count"], 1);
        assert_eq!(payload["collector_mutated_state"], false);
        assert_eq!(payload["message_bodies_included"], false);
        assert_eq!(payload["inbox_bodies_included"], false);
        assert_eq!(payload["repair_restart_kill_attempted"], false);
        assert_eq!(payload["provenance"]["repair_allowed"], false);
        assert_eq!(payload["provenance"]["restart_allowed"], false);
        assert_eq!(payload["provenance"]["kill_allowed"], false);
        assert_eq!(payload["provenance"]["registration_attempted"], false);
        assert_eq!(payload["provenance"]["acknowledgement_attempted"], false);
        assert_eq!(payload["attempts"][0]["operation"], "health_check");
        assert_eq!(payload["attempts"][0]["message_bodies_included"], false);
        assert!(
            payload["forbidden_actions"]
                .as_array()
                .expect("forbidden actions")
                .iter()
                .any(|action| action.as_str() == Some("am doctor repair"))
        );
    }

    #[test]
    fn incident_agent_mail_records_unavailable_after_allowed_retry() {
        let _guard = INCIDENT_AGENT_MAIL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentAgentMailSnapshot::clear_global_for_test();

        IncidentAgentMailSnapshot::update_global(
            IncidentAgentMailSnapshot::new(
                epoch_millis(),
                "fixture::agent-mail-health",
                "unavailable",
                "agent_mail.database_error",
            )
            .with_retry_count(1)
            .with_attempts(vec![
                IncidentAgentMailAttempt::new("health_check", "error", "agent_mail.sqlite_enospc")
                    .with_message("sqlite open failed: no space left on device")
                    .with_elapsed_ms(9),
                IncidentAgentMailAttempt::new(
                    "health_check_retry",
                    "error",
                    "agent_mail.sqlite_enospc",
                )
                .with_message("retry failed: no space left on device")
                .with_elapsed_ms(7),
            ]),
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_agent_mail_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("agent_mail source");

        IncidentAgentMailSnapshot::clear_global_for_test();

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].id, "agent_mail.database_error");
        assert_eq!(sources.len(), 1);
        let source = &sources[0];
        assert_eq!(source.name, "agent_mail");
        assert_eq!(source.status, IncidentSourceStatus::Collected);
        assert_eq!(source.evidence_state, IncidentEvidenceState::Unavailable);
        assert_eq!(source.file.as_deref(), Some("sources/agent_mail.json"));
        assert_eq!(
            source.warning_ids,
            vec!["agent_mail.database_error".to_string()]
        );

        let payload_path = tmp.path().join("sources/agent_mail.json");
        let payload: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(payload_path).expect("payload file"))
                .expect("payload json");
        assert_eq!(payload["status"], "unavailable");
        assert_eq!(payload["reason_code"], "agent_mail.database_error");
        assert_eq!(payload["reason_category"], "database");
        assert_eq!(payload["retry_count"], 1);
        assert_eq!(payload["max_retry_count"], 1);
        assert_eq!(payload["attempt_count"], 2);
        assert_eq!(payload["collector_mutated_state"], false);
        assert_eq!(payload["repair_restart_kill_attempted"], false);
        assert_eq!(payload["message_bodies_included"], false);
        assert_eq!(payload["provenance"]["repair_allowed"], false);
        assert_eq!(payload["provenance"]["restart_allowed"], false);
        assert_eq!(payload["provenance"]["kill_allowed"], false);
        assert_eq!(payload["attempts"][0]["reason_category"], "database");
        assert_eq!(payload["attempts"][1]["operation"], "health_check_retry");
    }

    #[test]
    fn incident_agent_mail_redacts_attempt_messages_before_payload() {
        let _guard = INCIDENT_AGENT_MAIL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        IncidentAgentMailSnapshot::clear_global_for_test();

        IncidentAgentMailSnapshot::update_global(
            IncidentAgentMailSnapshot::new(
                epoch_millis(),
                "fixture::agent_mail_health",
                "unavailable",
                "agent_mail.api_unreachable",
            )
            .with_attempts(vec![
                IncidentAgentMailAttempt::new(
                    "health_check",
                    "error",
                    "agent_mail.http_connection_error",
                )
                .with_message("diagnostic included AKIAABCDEFGHIJKLMNOP")
                .with_elapsed_ms(5),
            ]),
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let redactor = Redactor::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        let mut redaction_entries = Vec::new();

        add_agent_mail_source(
            &mut sources,
            &mut warnings,
            "2026-05-16T00:00:00Z",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut redaction_entries,
        )
        .expect("agent_mail source");

        IncidentAgentMailSnapshot::clear_global_for_test();

        assert_eq!(sources[0].redaction, IncidentRedactionState::Partial);
        let entry = redaction_entries
            .iter()
            .find(|entry| entry.file == "sources/agent_mail.json")
            .expect("agent mail redaction entry");
        assert_eq!(entry.count, 1);

        let payload_path = tmp.path().join("sources/agent_mail.json");
        let payload_text = fs::read_to_string(payload_path).expect("payload file");
        assert!(!payload_text.contains("AKIAABCDEFGHIJKLMNOP"));
        assert!(payload_text.contains("[REDACTED]"));
        let payload: serde_json::Value = serde_json::from_str(&payload_text).expect("payload json");
        assert_eq!(
            payload["attempts"][0]["message"],
            "diagnostic included [REDACTED]"
        );
    }

    #[test]
    fn agent_mail_reason_category_classifies_stable_failure_families() {
        assert_eq!(agent_mail_reason_category("agent_mail.ok"), "ok");
        assert_eq!(
            agent_mail_reason_category("agent_mail.sqlite_enospc"),
            "database"
        );
        assert_eq!(
            agent_mail_reason_category("agent_mail.recovery_read_only"),
            "recovery_mode"
        );
        assert_eq!(
            agent_mail_reason_category("agent_mail.http_connection_error"),
            "api_unreachable"
        );
        assert_eq!(agent_mail_reason_category("agent_mail.timeout"), "timeout");
        assert_eq!(
            agent_mail_reason_category("forbidden.agent_mail.repair"),
            "forbidden_action"
        );
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
            has_resize_forensics: false,
            has_environment_markers: false,
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
        let bundle_path = write_crash_bundle(&crash_dir, &report, Some(&health), None).unwrap();

        assert!(bundle_path.exists());
        assert!(bundle_path.join("manifest.json").exists());
        assert!(bundle_path.join("crash_report.json").exists());
        assert!(bundle_path.join("environment_markers.json").exists());
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

        let bundle_path = write_crash_bundle(&crash_dir, &report, None, None).unwrap();

        assert!(bundle_path.join("manifest.json").exists());
        assert!(bundle_path.join("crash_report.json").exists());
        assert!(bundle_path.join("environment_markers.json").exists());
        assert!(!bundle_path.join("health_snapshot.json").exists());

        // Verify manifest records no health snapshot
        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();
        assert!(!manifest.has_health_snapshot);
        assert!(manifest.has_environment_markers);
        assert_eq!(manifest.files.len(), 2);
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

        let bundle_path = write_crash_bundle(&crash_dir, &report, None, None).unwrap();

        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();

        assert_eq!(manifest.wa_version, crate::VERSION);
        assert!(!manifest.created_at.is_empty());
    }

    #[test]
    fn write_crash_bundle_redacts_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        // Build at runtime (split string literals) to avoid push-protection
        // treating the test token as a real secret.
        let api_key = [
            "sk",
            "-ant-api03-",
            "secret123456789012345678901234567890ABCDEF",
        ]
        .concat();
        let report = CrashReport {
            message: format!("failed with key {api_key}"),
            location: Some(format!("plugin/{api_key}:17")),
            backtrace: Some("token=my_secret_token_1234567890 in frame".to_string()),
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: Some(format!("worker-{api_key}")),
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None, None).unwrap();

        let report_json = fs::read_to_string(bundle_path.join("crash_report.json")).unwrap();
        let parsed: CrashReport = serde_json::from_str(&report_json).unwrap();

        // Secrets should be redacted
        let prefix = ["sk", "-ant-api03"].concat();
        assert!(
            !parsed.message.contains(&prefix),
            "API key should be redacted: {}",
            parsed.message
        );
        assert!(
            parsed.message.contains("[REDACTED]"),
            "Should contain REDACTED marker: {}",
            parsed.message
        );
        assert!(
            !report_json.contains(&prefix),
            "location/thread fields must be redacted with the same policy"
        );
    }

    #[test]
    fn crash_bundle_sanitizes_health_warnings_and_environment_markers() {
        let redactor = Redactor::new();
        let api_key = [
            "sk",
            "-ant-api03-",
            "secret123456789012345678901234567890ABCDEF",
        ]
        .concat();
        let secret_prefix = ["sk", "-ant-api03"].concat();
        let hostile = format!("{api_key}\n\u{1b}[31m{}", "x".repeat(2048));

        let mut health = test_snapshot();
        health.warnings = vec![hostile.clone(); MAX_CRASH_HEALTH_WARNINGS + 1];
        health.backpressure_tier = Some(hostile.clone());
        health.fleet_pressure_tier = Some(hostile.clone());
        let bounded_health = redacted_health_snapshot(&health, &redactor);

        let source_json = serde_json::to_value(&health).expect("source health JSON");
        let bounded_json =
            serde_json::to_value(&bounded_health).expect("bounded health JSON projection");
        let source_keys: HashSet<_> = source_json
            .as_object()
            .expect("source health object")
            .keys()
            .collect();
        let bounded_keys: HashSet<_> = bounded_json
            .as_object()
            .expect("bounded health object")
            .keys()
            .collect();
        assert_eq!(bounded_keys, source_keys, "crash health schema drift");

        assert_eq!(bounded_health.warnings.len(), MAX_CRASH_HEALTH_WARNINGS);
        assert_eq!(
            bounded_health.warnings.last().map(String::as_str),
            Some("2 additional health warnings omitted")
        );
        for warning in &bounded_health.warnings {
            assert!(warning.len() <= MAX_CRASH_DIAGNOSTIC_FIELD_LEN);
            assert!(!warning.contains(&secret_prefix));
            assert!(!warning.contains('\n'));
            assert!(!warning.contains('\u{1b}'));
        }
        for tier in [
            bounded_health.backpressure_tier.as_deref(),
            bounded_health.fleet_pressure_tier.as_deref(),
        ] {
            let tier = tier.expect("bounded health tier");
            assert!(tier.len() <= MAX_CRASH_DIAGNOSTIC_FIELD_LEN);
            assert!(!tier.contains(&secret_prefix));
            assert!(!tier.contains('\n'));
            assert!(!tier.contains('\u{1b}'));
        }

        let markers = CrashEnvironmentMarkers {
            gate_phase: hostile.clone(),
            session_phase: hostile.clone(),
            screen_mode: hostile.clone(),
            feature_flags: vec!["tui".to_string()],
            terminal_type: hostile.clone(),
            terminal_program: hostile.clone(),
            backpressure_tier: hostile,
        }
        .redacted(&redactor);
        for marker in [
            markers.gate_phase,
            markers.session_phase,
            markers.screen_mode,
            markers.terminal_type,
            markers.terminal_program,
            markers.backpressure_tier,
        ] {
            assert!(marker.len() <= MAX_CRASH_DIAGNOSTIC_FIELD_LEN);
            assert!(!marker.contains(&secret_prefix));
            assert!(!marker.contains('\n'));
            assert!(!marker.contains('\u{1b}'));
        }
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

        let path1 = write_crash_bundle(&crash_dir, &report, None, None).unwrap();

        let report2 = CrashReport {
            message: "second".to_string(),
            ..report.clone()
        };

        let path2 = write_crash_bundle(&crash_dir, &report2, None, None).unwrap();

        assert_ne!(path1, path2);
        assert!(path1.exists());
        assert!(path2.exists());
    }

    #[test]
    fn concurrent_crash_bundle_writers_never_share_staging_or_final_paths() {
        use std::sync::{Arc, Barrier};

        const WRITERS: usize = 8;
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = Arc::new(tmp.path().join("crash"));
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut writers = Vec::with_capacity(WRITERS);

        for writer in 0..WRITERS {
            let crash_dir = Arc::clone(&crash_dir);
            let barrier = Arc::clone(&barrier);
            writers.push(std::thread::spawn(move || {
                let report = CrashReport {
                    message: format!("concurrent writer {writer}"),
                    location: None,
                    backtrace: None,
                    timestamp: 1_700_000_000,
                    pid: std::process::id(),
                    thread_name: None,
                };
                barrier.wait();
                write_crash_bundle(&crash_dir, &report, None, None)
            }));
        }

        let paths: HashSet<PathBuf> = writers
            .into_iter()
            .map(|writer| writer.join().expect("crash writer thread"))
            .collect::<std::io::Result<_>>()
            .expect("every concurrent crash bundle");
        assert_eq!(paths.len(), WRITERS);
        for path in &paths {
            assert!(path.is_dir(), "missing completed bundle: {}", path.display());
            assert!(path.join("crash_report.json").is_file());
            assert!(path.join("manifest.json").is_file());
        }

        let hidden_staging_paths = fs::read_dir(crash_dir.as_ref())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .count();
        assert_eq!(hidden_staging_paths, 0);
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

        let bundle_path = write_crash_bundle(&crash_dir, &report, None, None).unwrap();
        let dir_name = bundle_path.file_name().unwrap().to_str().unwrap();

        assert!(
            dir_name.starts_with("ft_crash_"),
            "should start with ft_crash_: {dir_name}"
        );
        // Should contain a timestamp-like string
        assert!(dir_name.len() > "ft_crash_".len());
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

        let bundle_path = write_crash_bundle(&crash_dir, &report, None, None).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bundle_perms = fs::metadata(&bundle_path).unwrap().permissions();
            let bundle_mode = bundle_perms.mode() & 0o777;
            assert_eq!(
                bundle_mode, 0o700,
                "crash bundle should be owner-only: {bundle_mode:o}"
            );
            let crash_file = bundle_path.join("crash_report.json");
            let perms = fs::metadata(&crash_file).unwrap().permissions();
            let mode = perms.mode() & 0o777;
            assert_eq!(mode, 0o600, "crash report should be owner-only: {mode:o}");
        }
    }

    // -- Helper tests --

    #[test]
    fn panic_snapshot_clone_never_waits_for_an_active_writer() {
        let lock = RwLock::new(Some(String::from("snapshot")));
        let writer = lock.write().expect("local diagnostic snapshot writer");
        assert!(try_clone_diagnostic_snapshot(&lock).is_none());
        drop(writer);
        assert_eq!(
            try_clone_diagnostic_snapshot(&lock).as_deref(),
            Some("snapshot")
        );
    }

    #[test]
    fn diagnostic_snapshot_helpers_recover_poison_without_losing_state() {
        use std::sync::Arc;

        let lock = Arc::new(RwLock::new(None));
        let poison_lock = Arc::clone(&lock);
        let poisoner = std::thread::spawn(move || {
            let mut writer = poison_lock.write().expect("clean local snapshot lock");
            *writer = Some(String::from("last-good"));
            panic!("intentional local diagnostic snapshot poison");
        });
        assert!(poisoner.join().is_err());
        assert_eq!(
            try_clone_diagnostic_snapshot(&lock).as_deref(),
            Some("last-good")
        );
        assert!(!lock.is_poisoned());

        replace_diagnostic_snapshot(&lock, Some(String::from("replacement")));
        assert_eq!(
            clone_diagnostic_snapshot(&lock).as_deref(),
            Some("replacement")
        );
    }

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
        const {
            assert!(MAX_BACKTRACE_LEN <= MAX_BUNDLE_SIZE);
        }
    }

    #[test]
    fn max_bundle_size_is_reasonable() {
        const {
            assert!(MAX_BUNDLE_SIZE >= 1024, "bundle size too small");
            assert!(MAX_BUNDLE_SIZE <= 10 * 1024 * 1024, "bundle size too large");
        }
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

        let bundle_path = write_crash_bundle(&crash_dir, &report, Some(&health), None).unwrap();

        let health_json = fs::read_to_string(bundle_path.join("health_snapshot.json")).unwrap();
        let parsed: HealthSnapshot = serde_json::from_str(&health_json).unwrap();

        assert_eq!(parsed.timestamp, health.timestamp);
        assert_eq!(parsed.observed_panes, health.observed_panes);
        assert_eq!(parsed.capture_queue_depth, health.capture_queue_depth);
    }

    #[test]
    fn write_crash_bundle_environment_markers_include_ftui_context() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let mut health = test_snapshot();
        health.backpressure_tier = Some("Yellow".to_string());
        clear_crash_terminal_session_markers_for_test();
        update_crash_terminal_session_markers("Suspended", "Inline { ui_height: 12 }");

        let bundle_path = write_crash_bundle(&crash_dir, &test_report(), Some(&health), None)
            .expect("write crash bundle");

        let markers_json =
            fs::read_to_string(bundle_path.join("environment_markers.json")).unwrap();
        let markers: CrashEnvironmentMarkers = serde_json::from_str(&markers_json).unwrap();
        assert!(!markers.session_phase.is_empty());
        assert!(!markers.screen_mode.is_empty());
        assert_eq!(markers.backpressure_tier, "Yellow");
        assert!(!markers.feature_flags.is_empty());

        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();
        assert!(manifest.has_environment_markers);
        assert!(
            manifest
                .files
                .contains(&"environment_markers.json".to_string())
        );
    }

    #[test]
    fn write_crash_bundle_bounds_oversized_report_fields_before_budgeting() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        // Caller-controlled report fields are bounded before serialization, so
        // an oversized backtrace cannot crowd the useful crash report out of
        // the bundle-wide privacy budget.
        let huge_bt = "x".repeat(MAX_BUNDLE_SIZE + 1000);
        let report = CrashReport {
            message: "m".repeat(MAX_CRASH_MESSAGE_LEN + 1000),
            location: Some("l".repeat(MAX_CRASH_LOCATION_LEN + 1000)),
            backtrace: Some(huge_bt),
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: Some("t".repeat(MAX_CRASH_THREAD_NAME_LEN + 1000)),
        };

        let bundle_path = write_crash_bundle(&crash_dir, &report, None, None).unwrap();

        // Manifest should always exist regardless of budget
        assert!(bundle_path.join("manifest.json").exists());

        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();
        assert!(
            manifest.files.contains(&"crash_report.json".to_string()),
            "bounded report should remain useful, files: {:?}",
            manifest.files
        );
        let report_json = fs::read_to_string(bundle_path.join("crash_report.json")).unwrap();
        let bounded: CrashReport = serde_json::from_str(&report_json).unwrap();
        assert!(bounded.message.len() <= MAX_CRASH_MESSAGE_LEN);
        assert!(bounded.message.ends_with("..."));
        let location = bounded.location.expect("bounded location retained");
        assert!(location.len() <= MAX_CRASH_LOCATION_LEN);
        assert!(location.ends_with("..."));
        let backtrace = bounded.backtrace.expect("bounded backtrace retained");
        assert!(backtrace.len() <= MAX_BACKTRACE_LEN);
        assert!(backtrace.ends_with("\n... [truncated]"));
        let thread_name = bounded.thread_name.expect("bounded thread name retained");
        assert!(thread_name.len() <= MAX_CRASH_THREAD_NAME_LEN);
        assert!(thread_name.ends_with("..."));
    }

    #[test]
    fn skipped_oversized_optional_file_does_not_consume_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");

        let report = CrashReport {
            message: "small panic".to_string(),
            location: None,
            backtrace: None,
            timestamp: 1_700_000_000,
            pid: 1,
            thread_name: None,
        };

        let mut health = test_snapshot();
        health.warnings = vec!["x".repeat(MAX_BUNDLE_SIZE + 1024)];
        health.last_seq_by_pane = vec![(u64::MAX, i64::MAX); MAX_BUNDLE_SIZE / 16];

        let resize = crate::resize_crash_forensics::ResizeCrashContextBuilder::new(42).build();

        let bundle_path =
            write_crash_bundle(&crash_dir, &report, Some(&health), Some(&resize)).unwrap();

        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();

        assert!(manifest.files.contains(&"crash_report.json".to_string()));
        assert!(!manifest.files.contains(&"health_snapshot.json".to_string()));
        assert!(
            manifest
                .files
                .contains(&"resize_forensics.json".to_string())
        );
        assert!(
            manifest
                .files
                .contains(&"environment_markers.json".to_string())
        );
        assert!(!manifest.has_health_snapshot);
        assert!(manifest.has_resize_forensics);
        assert!(manifest.has_environment_markers);

        let actual_bytes: u64 = manifest
            .files
            .iter()
            .map(|name| fs::metadata(bundle_path.join(name)).unwrap().len())
            .sum();
        assert_eq!(manifest.bundle_size_bytes, actual_bytes);
        assert!(manifest.bundle_size_bytes < MAX_BUNDLE_SIZE as u64);
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
        let bundle_path = write_crash_bundle(&crash_dir, &report, Some(&health), None).unwrap();

        let manifest_json = fs::read_to_string(bundle_path.join("manifest.json")).unwrap();
        let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();

        assert_eq!(manifest.files.len(), 3);
        assert!(manifest.files.contains(&"crash_report.json".to_string()));
        assert!(manifest.files.contains(&"health_snapshot.json".to_string()));
        assert!(
            manifest
                .files
                .contains(&"environment_markers.json".to_string())
        );
        assert!(manifest.has_health_snapshot);
        assert!(manifest.has_environment_markers);
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

        let path1 = write_crash_bundle(&crash_dir1, &report, Some(&health), None).unwrap();
        let path2 = write_crash_bundle(&crash_dir2, &report, Some(&health), None).unwrap();

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
        write_crash_bundle(crash_dir, &report, None, None).unwrap();

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
        write_crash_bundle(crash_dir, &r1, None, None).unwrap();

        let mut r2 = test_report();
        r2.timestamp = 2000;
        r2.message = "second".to_string();
        write_crash_bundle(crash_dir, &r2, None, None).unwrap();

        let bundles = list_crash_bundles(crash_dir, 10);
        assert_eq!(bundles.len(), 2);
        assert_eq!(bundles[0].report.as_ref().unwrap().message, "second");
        assert_eq!(bundles[1].report.as_ref().unwrap().message, "first");
    }

    #[test]
    fn list_crash_bundles_uses_sequence_for_same_process_and_second() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        let mut first = test_report();
        first.timestamp = 1000;
        first.message = "first".to_string();
        let first_path = write_crash_bundle(crash_dir, &first, None, None).unwrap();

        let mut second = test_report();
        second.timestamp = 1000;
        second.message = "second".to_string();
        let second_path = write_crash_bundle(crash_dir, &second, None, None).unwrap();

        let (first_pid, first_sequence) =
            crash_bundle_process_sequence(&first_path).expect("first process/sequence");
        let (second_pid, second_sequence) =
            crash_bundle_process_sequence(&second_path).expect("second process/sequence");
        assert_eq!(first_pid, second_pid);
        assert!(second_sequence > first_sequence);

        let bundles = list_crash_bundles(crash_dir, 2);
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
            write_crash_bundle(crash_dir, &r, None, None).unwrap();
        }

        let bundles = list_crash_bundles(crash_dir, 3);
        assert_eq!(bundles.len(), 3);
        assert!(list_crash_bundles(crash_dir, 0).is_empty());
    }

    #[test]
    fn list_crash_bundles_skips_non_crash_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        // Create a non-crash directory
        fs::create_dir(crash_dir.join("some_other_dir")).unwrap();
        // Create a crash bundle
        let report = test_report();
        write_crash_bundle(crash_dir, &report, None, None).unwrap();

        let bundles = list_crash_bundles(crash_dir, 10);
        assert_eq!(bundles.len(), 1);
    }

    #[test]
    fn list_crash_bundles_skips_empty_crash_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        // Create an empty ft_crash_ directory (no manifest or report)
        fs::create_dir(crash_dir.join("ft_crash_empty")).unwrap();
        // Create a valid crash bundle
        let report = test_report();
        write_crash_bundle(crash_dir, &report, None, None).unwrap();

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
        write_crash_bundle(crash_dir, &r1, None, None).unwrap();

        let mut r2 = test_report();
        r2.timestamp = 2000;
        r2.message = "newer".to_string();
        write_crash_bundle(crash_dir, &r2, None, None).unwrap();

        let latest = latest_crash_bundle(crash_dir).unwrap();
        assert_eq!(latest.report.as_ref().unwrap().message, "newer");
    }

    #[test]
    fn latest_discovery_keeps_selection_memory_bounded_across_10k_names() {
        let mut heap = BinaryHeap::new();
        for sequence in 0_u64..10_000 {
            retain_newest_candidate(
                &mut heap,
                RankedCrashBundleCandidate {
                    timestamp: "20260805_120000".to_string(),
                    modified: Some(UNIX_EPOCH),
                    process_sequence: Some((7, sequence)),
                    path: PathBuf::from(format!(
                        "ft_crash_20260805_120000_p7_{sequence}"
                    )),
                },
                LATEST_CRASH_BUNDLE_CANDIDATE_WINDOW,
            );
        }
        assert_eq!(heap.len(), LATEST_CRASH_BUNDLE_CANDIDATE_WINDOW);
        let sequences = heap
            .into_iter()
            .map(|candidate| candidate.0.process_sequence.unwrap().1)
            .collect::<Vec<_>>();
        assert_eq!(sequences.iter().copied().min(), Some(9_968));
        assert_eq!(sequences.iter().copied().max(), Some(9_999));
    }

    #[test]
    fn crash_bundle_name_key_rejects_impossible_gregorian_dates() {
        assert!(
            crash_bundle_name_key(Path::new("ft_crash_20240229_235959_p7_1")).is_some(),
            "Gregorian leap day must remain rankable"
        );
        assert!(crash_bundle_name_key(Path::new("ft_crash_20250229_120000_p7_1")).is_none());
        assert!(crash_bundle_name_key(Path::new("ft_crash_20260431_120000_p7_1")).is_none());
        assert!(crash_bundle_name_key(Path::new("ft_crash_20261301_120000_p7_1")).is_none());
        assert!(crash_bundle_name_key(Path::new("ft_crash_00000101_120000_p7_1")).is_none());
    }

    #[test]
    fn candidate_metadata_failure_is_explicitly_incomplete_authority() {
        let mut metadata_unreadable = false;
        let modified = crash_bundle_candidate_modified(
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "synthetic metadata denial",
            )),
            &mut metadata_unreadable,
        );
        assert!(modified.is_none());
        assert!(metadata_unreadable);
    }

    #[test]
    fn candidate_payload_io_classification_distinguishes_invalid_from_unreadable() {
        assert!(crash_bundle_io_error_withholds_authority(
            &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "synthetic denial")
        ));
        assert!(crash_bundle_io_error_withholds_authority(
            &std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "synthetic short read")
        ));
        assert!(crash_bundle_io_error_withholds_authority(
            &std::io::Error::other("synthetic unclassified failure")
        ));
        assert!(!crash_bundle_io_error_withholds_authority(
            &std::io::Error::new(std::io::ErrorKind::InvalidData, "synthetic malformed payload")
        ));
        assert!(!crash_bundle_io_error_withholds_authority(
            &std::io::Error::new(std::io::ErrorKind::NotFound, "synthetic absence")
        ));
    }

    #[test]
    fn bounded_list_zero_limit_opens_no_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let mut report = test_report();
        report.timestamp = 2_000;
        write_crash_bundle(tmp.path(), &report, None, None).unwrap();

        let discovery = discover_crash_bundles(tmp.path(), 0);
        assert!(discovery.completeness.is_complete());
        assert!(discovery.bundles.is_empty());
        assert_eq!(discovery.directory_entries_examined, 0);
        assert_eq!(discovery.payload_files_opened, 0);
        assert_eq!(discovery.payload_bytes_read, 0);
    }

    #[test]
    fn bounded_list_reports_requested_limit_cap_even_for_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let discovery = discover_crash_bundles(
            tmp.path(),
            MAX_CRASH_BUNDLE_LIST_RESULTS.saturating_add(1),
        );
        assert_eq!(discovery.effective_limit, MAX_CRASH_BUNDLE_LIST_RESULTS);
        assert_eq!(
            discovery.completeness,
            CrashBundleDiscoveryCompleteness::Incomplete {
                reasons: vec![CrashBundleDiscoveryIncompleteReason::RequestedLimitExceeded],
            }
        );
        assert_eq!(discovery.payload_files_opened, 0);
    }

    #[test]
    fn bounded_list_never_scans_unbounded_corrupt_payloads() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let ranked_window = 1_usize.saturating_add(CRASH_BUNDLE_LIST_INVALID_CANDIDATE_SLACK);
        for sequence in 0_u64..100 {
            let timestamp = 1_000_u64.saturating_add(sequence);
            let path = tmp.path().join(format!(
                "ft_crash_{}_p9_{sequence}",
                format_timestamp(timestamp)
            ));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("manifest.json"), b"{").unwrap();
            fs::write(path.join("crash_report.json"), b"{").unwrap();
        }

        let discovery = discover_crash_bundles(tmp.path(), 1);
        assert!(discovery.bundles.is_empty());
        assert_eq!(discovery.ranked_candidates, 100);
        assert_eq!(discovery.payload_files_opened, ranked_window * 2);
        assert_eq!(
            crash_bundle_parse_drop_count(),
            u64::try_from(ranked_window * 2).unwrap()
        );
        assert_eq!(
            CRASH_BUNDLE_PARSE_DROP_LOG_COUNT.load(std::sync::atomic::Ordering::Relaxed),
            CRASH_BUNDLE_PARSE_DROP_LOG_LIMIT.saturating_add(1),
            "individual warnings plus one suppression notice are the finite log budget"
        );
        assert_eq!(
            discovery.completeness,
            CrashBundleDiscoveryCompleteness::Incomplete {
                reasons: vec![
                    CrashBundleDiscoveryIncompleteReason::RankedCandidateWindowExhausted,
                ],
            }
        );
    }

    #[test]
    fn latest_ranked_manifest_only_bundle_is_not_demoted_to_older_report() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let mut older = test_report();
        older.timestamp = 1_000;
        write_crash_bundle(tmp.path(), &older, None, None).unwrap();
        let mut newer = test_report();
        newer.timestamp = 2_000;
        let newer_path = write_crash_bundle(tmp.path(), &newer, None, None).unwrap();
        fs::write(newer_path.join("crash_report.json"), b"{").unwrap();

        let discovery = discover_latest_crash_bundle(tmp.path());
        assert!(discovery.completeness.is_complete());
        let bundle = discovery.bundle.expect("newest manifest remains authoritative");
        assert_eq!(bundle.path, newer_path);
        assert!(bundle.manifest.is_some());
        assert!(bundle.report.is_none());
        assert_eq!(discovery.payload_files_opened, 2);
    }

    #[test]
    fn incomplete_discovery_warning_emission_is_finitely_capped() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_discovery_incomplete_count_for_test();
        let completeness = CrashBundleDiscoveryCompleteness::Incomplete {
            reasons: vec![CrashBundleDiscoveryIncompleteReason::RequestedLimitExceeded],
        };
        let calls = CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_LIMIT.saturating_add(9);
        for _ in 0..calls {
            record_crash_bundle_discovery_incomplete(
                "test",
                &completeness,
                0,
                0,
                0,
                0,
                0,
            );
        }
        assert_eq!(crash_bundle_discovery_incomplete_count(), calls);
        assert_eq!(
            CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_COUNT
                .load(std::sync::atomic::Ordering::Relaxed),
            CRASH_BUNDLE_DISCOVERY_INCOMPLETE_LOG_LIMIT.saturating_add(1)
        );
    }

    #[test]
    fn latest_discovery_does_not_open_oversized_older_payload() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let mut older = test_report();
        older.timestamp = 1_000;
        older.message = "older".to_string();
        let older_path = write_crash_bundle(tmp.path(), &older, None, None).unwrap();
        fs::write(
            older_path.join("crash_report.json"),
            vec![b'x'; usize::try_from(MAX_CRASH_REPORT_JSON_READ_BYTES).unwrap() + 1_024],
        )
        .unwrap();

        let mut newer = test_report();
        newer.timestamp = 2_000;
        newer.message = "newer".to_string();
        write_crash_bundle(tmp.path(), &newer, None, None).unwrap();

        let discovery = discover_latest_crash_bundle(tmp.path());
        assert!(discovery.completeness.is_complete());
        assert_eq!(discovery.payload_files_opened, 2);
        assert!(
            discovery.payload_bytes_read
                <= MAX_CRASH_MANIFEST_JSON_READ_BYTES + MAX_CRASH_REPORT_JSON_READ_BYTES
        );
        assert_eq!(
            discovery.bundle.unwrap().report.unwrap().message,
            "newer"
        );
        assert_eq!(crash_bundle_parse_drop_count(), 0);
    }

    #[test]
    fn latest_discovery_skips_corrupt_newest_with_bounded_reads() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let mut older = test_report();
        older.timestamp = 1_000;
        older.message = "older".to_string();
        write_crash_bundle(tmp.path(), &older, None, None).unwrap();
        let mut newer = test_report();
        newer.timestamp = 2_000;
        let newer_path = write_crash_bundle(tmp.path(), &newer, None, None).unwrap();
        fs::write(newer_path.join("manifest.json"), b"{").unwrap();
        fs::write(newer_path.join("crash_report.json"), b"{").unwrap();

        let discovery = discover_latest_crash_bundle(tmp.path());
        assert!(discovery.completeness.is_complete());
        assert_eq!(discovery.payload_files_opened, 4);
        assert_eq!(
            discovery.bundle.unwrap().report.unwrap().message,
            "older"
        );
        assert_eq!(crash_bundle_parse_drop_count(), 2);
    }

    #[test]
    fn latest_discovery_reports_ranked_window_exhaustion() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let mut valid = test_report();
        valid.timestamp = 500;
        valid.message = "outside-window".to_string();
        write_crash_bundle(tmp.path(), &valid, None, None).unwrap();
        for sequence in 0_u64..33 {
            let timestamp = 1_000_u64 + sequence;
            let path = tmp.path().join(format!(
                "ft_crash_{}_p9_{sequence}",
                format_timestamp(timestamp)
            ));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("manifest.json"), b"{").unwrap();
            fs::write(path.join("crash_report.json"), b"{").unwrap();
        }

        let discovery = discover_latest_crash_bundle(tmp.path());
        assert_eq!(discovery.ranked_candidates, 34);
        assert_eq!(discovery.payload_files_opened, 64);
        assert!(discovery.bundle.is_none());
        assert_eq!(
            discovery.completeness,
            CrashBundleDiscoveryCompleteness::Incomplete {
                reasons: vec![
                    CrashBundleDiscoveryIncompleteReason::RankedCandidateWindowExhausted,
                ],
            }
        );
        assert!(latest_crash_bundle(tmp.path()).is_none());
    }

    #[test]
    fn latest_discovery_reports_too_many_unranked_names() {
        let tmp = tempfile::tempdir().unwrap();
        for index in 0..=LATEST_CRASH_BUNDLE_UNRANKED_WINDOW {
            fs::create_dir(tmp.path().join(format!("ft_crash_legacy_{index}"))).unwrap();
        }
        let discovery = discover_latest_crash_bundle(tmp.path());
        assert_eq!(
            discovery.completeness,
            CrashBundleDiscoveryCompleteness::Incomplete {
                reasons: vec![
                    CrashBundleDiscoveryIncompleteReason::UnrankedCandidateWindowExceeded,
                ],
            }
        );
        assert_eq!(discovery.payload_files_opened, 0);
    }

    #[test]
    fn bounded_crash_payload_read_rejects_oversize_before_allocation_growth() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("manifest.json");
        fs::write(
            &path,
            vec![b'x'; usize::try_from(MAX_CRASH_MANIFEST_JSON_READ_BYTES).unwrap() + 1_024],
        )
        .unwrap();
        let mut stats = CrashBundlePayloadReadStats::default();
        let result = read_optional_json_bundle_file_bounded::<CrashManifest>(
            tmp.path(),
            &path,
            "manifest_read_fail",
            "manifest_parse_fail",
            MAX_CRASH_MANIFEST_JSON_READ_BYTES,
            &mut stats,
        );
        assert!(result.is_none());
        assert_eq!(stats.files_opened, 1);
        assert_eq!(stats.bytes_read, MAX_CRASH_MANIFEST_JSON_READ_BYTES + 1);
        assert_eq!(crash_bundle_parse_drop_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_crash_payload_read_never_follows_symlink_leaves() {
        use std::os::unix::fs::symlink;

        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("ft_crash_20260805_120000_p7_1");
        fs::create_dir(&bundle).unwrap();
        let outside = tmp.path().join("outside.json");
        fs::write(&outside, serde_json::to_vec(&test_report()).unwrap()).unwrap();
        symlink(&outside, bundle.join("crash_report.json")).unwrap();

        let discovery = discover_latest_crash_bundle(tmp.path());
        assert!(discovery.completeness.is_complete());
        assert!(discovery.bundle.is_none());
        assert_eq!(discovery.payload_files_opened, 0);
        assert_eq!(crash_bundle_parse_drop_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn crash_bundle_capability_never_follows_replaced_directory_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside_bundle");
        fs::create_dir(&outside).unwrap();
        let candidate = tmp.path().join("ft_crash_20260805_120000_p7_1");
        symlink(&outside, &candidate).unwrap();
        assert!(open_crash_bundle_dir_nofollow(&candidate).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_crash_payload_read_rejects_fifo_without_blocking() {
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        reset_crash_bundle_parse_drop_count_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("manifest.json");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("invoke mkfifo for hostile crash payload");
        assert!(status.success());
        let started = Instant::now();
        let result = read_optional_json_bundle_file::<CrashManifest>(
            tmp.path(),
            &fifo,
            "manifest_read_fail",
            "manifest_parse_fail",
        );
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(crash_bundle_parse_drop_count(), 1);
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
        write_crash_bundle(&crash_dir, &report, Some(&test_snapshot()), None).unwrap();

        let result =
            export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Crash).unwrap();

        assert_eq!(result.kind, IncidentKind::Crash);
        assert!(result.path.exists());
        assert!(result.files.contains(&"crash_report.json".to_string()));
        assert!(result.files.contains(&"crash_manifest.json".to_string()));
        assert!(result.files.contains(&"health_snapshot.json".to_string()));
        assert!(result.files.contains(&"incident_manifest.json".to_string()));
        assert!(result.total_size_bytes > 0);

        let manifest_path = result.path.join("incident_manifest.json");
        assert!(manifest_path.exists());
        let disk_total: u64 = result
            .files
            .iter()
            .map(|file| fs::metadata(result.path.join(file)).unwrap().len())
            .sum();
        assert_eq!(result.total_size_bytes, disk_total);
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
        assert_eq!(result.files, vec!["incident_manifest.json".to_string()]);
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
    fn export_incident_bundle_manifest_path_is_bundle_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let secret = "AKIAABCDEFGHIJKLMNOP";
        let out_dir = tmp.path().join(format!("out-{secret}"));

        let result =
            export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Manual).unwrap();
        assert!(result.path.to_string_lossy().contains(secret));

        let manifest_text = fs::read_to_string(result.path.join("incident_manifest.json")).unwrap();
        assert!(!manifest_text.contains(secret));
        let manifest: IncidentBundleResult = serde_json::from_str(&manifest_text).unwrap();
        assert!(!manifest.path.to_string_lossy().contains(secret));
        assert!(!manifest.path.is_absolute());
        assert!(
            manifest
                .files
                .contains(&"incident_manifest.json".to_string())
        );
        assert_eq!(manifest.total_size_bytes, result.total_size_bytes);
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

    // -----------------------------------------------------------------------
    // Crash loop detection + backoff tests (bd-24cz TDD)
    // -----------------------------------------------------------------------

    #[test]
    fn crash_loop_config_defaults() {
        let config = CrashLoopConfig::default();
        assert_eq!(config.window_secs, 300);
        assert_eq!(config.crash_threshold, 3);
        assert_eq!(config.initial_delay_ms, 1_000);
        assert_eq!(config.max_delay_ms, 60_000);
        assert!((config.backoff_factor - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn crash_loop_config_serialization() {
        let config = CrashLoopConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: CrashLoopConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.window_secs, config.window_secs);
        assert_eq!(parsed.crash_threshold, config.crash_threshold);
        assert_eq!(parsed.initial_delay_ms, config.initial_delay_ms);
        assert_eq!(parsed.max_delay_ms, config.max_delay_ms);
    }

    #[test]
    fn detector_new_has_zero_crashes() {
        let det = CrashLoopDetector::new(CrashLoopConfig::default());
        assert_eq!(det.consecutive_crashes(), 0);
        assert!(!det.is_crash_loop());
        assert_eq!(det.next_delay_ms(), 0);
    }

    #[test]
    fn detector_single_crash_not_loop() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig::default());
        det.record_crash(1000);
        assert_eq!(det.consecutive_crashes(), 1);
        assert!(!det.is_crash_loop());
    }

    #[test]
    fn detector_backoff_growth_exponential() {
        let config = CrashLoopConfig {
            initial_delay_ms: 1_000,
            backoff_factor: 2.0,
            max_delay_ms: 60_000,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        // 1st crash: delay = 1000 * 2^0 = 1000
        det.record_crash(1000);
        assert_eq!(det.next_delay_ms(), 1_000);

        // 2nd crash: delay = 1000 * 2^1 = 2000
        det.record_crash(1001);
        assert_eq!(det.next_delay_ms(), 2_000);

        // 3rd crash: delay = 1000 * 2^2 = 4000
        det.record_crash(1002);
        assert_eq!(det.next_delay_ms(), 4_000);

        // 4th crash: delay = 1000 * 2^3 = 8000
        det.record_crash(1003);
        assert_eq!(det.next_delay_ms(), 8_000);

        // 5th crash: delay = 1000 * 2^4 = 16000
        det.record_crash(1004);
        assert_eq!(det.next_delay_ms(), 16_000);
    }

    #[test]
    fn detector_backoff_capped_at_max() {
        let config = CrashLoopConfig {
            initial_delay_ms: 1_000,
            backoff_factor: 2.0,
            max_delay_ms: 5_000,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        // Record many crashes
        for i in 0..20 {
            det.record_crash(1000 + i);
        }
        // Should be capped at 5000
        assert_eq!(det.next_delay_ms(), 5_000);
    }

    #[test]
    fn detector_reset_after_success() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig::default());

        det.record_crash(1000);
        det.record_crash(1001);
        det.record_crash(1002);
        assert_eq!(det.consecutive_crashes(), 3);
        assert!(det.is_crash_loop());

        det.record_success();
        assert_eq!(det.consecutive_crashes(), 0);
        assert_eq!(det.next_delay_ms(), 0);
    }

    #[test]
    fn detector_crash_loop_threshold() {
        let config = CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 60,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        det.record_crash(1000);
        assert!(!det.is_crash_loop());

        det.record_crash(1010);
        assert!(!det.is_crash_loop());

        det.record_crash(1020);
        assert!(det.is_crash_loop());
    }

    #[test]
    fn detector_crashes_outside_window_not_counted() {
        let config = CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 60,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        // Two crashes at time 100 and 110 (within window)
        det.record_crash(100);
        det.record_crash(110);

        // Third crash much later (time 500) — the first two are outside the window
        det.record_crash(500);

        // Only 1 crash in the last 60s window (at 500)
        assert_eq!(det.crashes_in_window(500), 1);
        assert!(!det.is_crash_loop());
    }

    #[test]
    fn detector_rapid_crash_loop_detected() {
        let config = CrashLoopConfig {
            crash_threshold: 5,
            window_secs: 10,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        // Five crashes within 10 seconds
        for i in 0..5 {
            det.record_crash(1000 + i);
        }
        assert!(det.is_crash_loop());
        assert_eq!(det.crashes_in_window(1004), 5);
    }

    #[test]
    fn detector_success_resets_but_preserves_timestamps() {
        let config = CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 300,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        det.record_crash(1000);
        det.record_crash(1001);
        det.record_success();

        // Consecutive counter is reset but timestamps remain
        assert_eq!(det.consecutive_crashes(), 0);
        assert_eq!(det.crashes_in_window(1001), 2);

        // One more crash triggers loop (3 total in window)
        det.record_crash(1002);
        assert!(det.is_crash_loop());
        // But consecutive is only 1 since last success
        assert_eq!(det.consecutive_crashes(), 1);
    }

    #[test]
    fn detector_serialization_round_trip() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig::default());
        det.record_crash(1000);
        det.record_crash(1001);

        let json = serde_json::to_string(&det).unwrap();
        let parsed: CrashLoopDetector = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.consecutive_crashes(), 2);
        assert_eq!(parsed.crash_timestamps.len(), 2);
    }

    #[test]
    fn detector_backoff_with_custom_factor() {
        let config = CrashLoopConfig {
            initial_delay_ms: 500,
            backoff_factor: 3.0,
            max_delay_ms: 100_000,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        det.record_crash(1000);
        assert_eq!(det.next_delay_ms(), 500); // 500 * 3^0

        det.record_crash(1001);
        assert_eq!(det.next_delay_ms(), 1_500); // 500 * 3^1

        det.record_crash(1002);
        assert_eq!(det.next_delay_ms(), 4_500); // 500 * 3^2
    }

    #[test]
    fn detector_crashes_in_window_empty() {
        let det = CrashLoopDetector::new(CrashLoopConfig::default());
        assert_eq!(det.crashes_in_window(1000), 0);
    }

    #[test]
    fn detector_prune_removes_old_timestamps() {
        let config = CrashLoopConfig {
            window_secs: 10,
            ..CrashLoopConfig::default()
        };
        let mut det = CrashLoopDetector::new(config);

        det.record_crash(100);
        det.record_crash(105);
        det.record_crash(200); // >10s after first two

        // After recording crash at 200, timestamps at 100 and 105 are pruned
        assert_eq!(det.crash_timestamps.len(), 1);
        assert_eq!(det.crash_timestamps[0], 200);
    }

    #[test]
    fn diagnostics_reflects_detector_state() {
        let config = CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 300,
            initial_delay_ms: 1000,
            backoff_factor: 2.0,
            max_delay_ms: 60_000,
        };
        let mut det = CrashLoopDetector::new(config);

        // Fresh detector — all zeros / defaults
        let diag = det.diagnostics();
        assert_eq!(diag.restart_count, 0);
        assert_eq!(diag.last_crash_at, None);
        assert_eq!(diag.consecutive_crashes, 0);
        assert_eq!(diag.current_backoff_ms, 0);
        assert!(!diag.in_crash_loop);

        // Record two crashes (below threshold)
        det.record_crash(1000);
        det.record_crash(1001);
        let diag = det.diagnostics();
        assert_eq!(diag.restart_count, 2);
        assert_eq!(diag.last_crash_at, Some(1001));
        assert_eq!(diag.consecutive_crashes, 2);
        assert_eq!(diag.current_backoff_ms, 2000); // 1000 * 2^1
        assert!(!diag.in_crash_loop);

        // Third crash triggers crash loop detection
        det.record_crash(1002);
        let diag = det.diagnostics();
        assert_eq!(diag.restart_count, 3);
        assert_eq!(diag.consecutive_crashes, 3);
        assert!(diag.in_crash_loop);
        assert_eq!(diag.current_backoff_ms, 4000); // 1000 * 2^2

        // Record a stable run — resets consecutive count but window still
        // contains 3 crashes, so is_crash_loop() remains true (window-based).
        det.record_success();
        let diag = det.diagnostics();
        assert_eq!(diag.restart_count, 3); // total unchanged
        assert_eq!(diag.consecutive_crashes, 0);
        assert!(diag.in_crash_loop); // window-based: 3 crashes still in 300s window
        assert_eq!(diag.current_backoff_ms, 0); // consecutive=0 → no backoff
    }

    // -----------------------------------------------------------------------
    // Capture checkpoint tests (bd-24cz TDD)
    // -----------------------------------------------------------------------

    fn sample_pane_states() -> Vec<PaneCaptureState> {
        vec![
            PaneCaptureState {
                pane_id: 1,
                last_seq: 100,
                cursor_offset: 4096,
                last_capture_at: 1_700_000_000,
            },
            PaneCaptureState {
                pane_id: 2,
                last_seq: 200,
                cursor_offset: 8192,
                last_capture_at: 1_700_000_001,
            },
            PaneCaptureState {
                pane_id: 5,
                last_seq: 50,
                cursor_offset: 1024,
                last_capture_at: 1_700_000_002,
            },
        ]
    }

    #[test]
    fn checkpoint_new_sets_version() {
        let cp = CaptureCheckpoint::with_timestamp(vec![], 1000);
        assert_eq!(cp.version, CHECKPOINT_FORMAT_VERSION);
        assert_eq!(cp.created_at, 1000);
        assert_eq!(cp.wa_version, crate::VERSION);
        assert!(cp.panes.is_empty());
    }

    #[test]
    fn checkpoint_with_panes() {
        let panes = sample_pane_states();
        let cp = CaptureCheckpoint::with_timestamp(panes.clone(), 2000);
        assert_eq!(cp.panes.len(), 3);
        assert_eq!(cp.panes[0].pane_id, 1);
        assert_eq!(cp.panes[1].pane_id, 2);
        assert_eq!(cp.panes[2].pane_id, 5);
    }

    #[test]
    fn checkpoint_save_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkpoint.json");

        let panes = sample_pane_states();
        let cp = CaptureCheckpoint::with_timestamp(panes, 1_700_000_000);
        cp.save(&path).unwrap();

        let loaded = CaptureCheckpoint::load(&path).unwrap();
        assert_eq!(loaded.version, CHECKPOINT_FORMAT_VERSION);
        assert_eq!(loaded.created_at, 1_700_000_000);
        assert_eq!(loaded.panes.len(), 3);
        assert_eq!(loaded.panes[0], cp.panes[0]);
        assert_eq!(loaded.panes[1], cp.panes[1]);
        assert_eq!(loaded.panes[2], cp.panes[2]);
    }

    #[test]
    fn checkpoint_save_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("deep")
            .join("nested")
            .join("checkpoint.json");

        let cp = CaptureCheckpoint::with_timestamp(vec![], 1000);
        cp.save(&path).unwrap();

        assert!(path.exists());
        let loaded = CaptureCheckpoint::load(&path).unwrap();
        assert_eq!(loaded.version, CHECKPOINT_FORMAT_VERSION);
    }

    #[test]
    fn checkpoint_load_rejects_wrong_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkpoint.json");

        // Write a checkpoint with a different version
        let json = serde_json::json!({
            "version": 99,
            "created_at": 1000,
            "panes": [],
            "wa_version": "0.0.0"
        });
        fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let result = CaptureCheckpoint::load(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unsupported checkpoint version"));
    }

    #[test]
    fn checkpoint_load_nonexistent_file() {
        let result = CaptureCheckpoint::load(Path::new("/nonexistent/checkpoint.json"));
        assert!(result.is_err());
    }

    #[test]
    fn checkpoint_load_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkpoint.json");
        fs::write(&path, "not valid json").unwrap();

        let result = CaptureCheckpoint::load(&path);
        assert!(result.is_err());
    }

    #[test]
    fn checkpoint_pane_state_lookup() {
        let panes = sample_pane_states();
        let cp = CaptureCheckpoint::with_timestamp(panes, 1000);

        let state = cp.pane_state(1).unwrap();
        assert_eq!(state.last_seq, 100);
        assert_eq!(state.cursor_offset, 4096);

        let state = cp.pane_state(5).unwrap();
        assert_eq!(state.last_seq, 50);

        assert!(cp.pane_state(99).is_none());
    }

    #[test]
    fn checkpoint_should_skip_segment_at_or_before() {
        let panes = sample_pane_states();
        let cp = CaptureCheckpoint::with_timestamp(panes, 1000);

        // Pane 1: last_seq = 100
        assert!(cp.should_skip_segment(1, 50)); // before last_seq
        assert!(cp.should_skip_segment(1, 100)); // at last_seq
        assert!(!cp.should_skip_segment(1, 101)); // after last_seq
    }

    #[test]
    fn checkpoint_should_skip_unknown_pane() {
        let panes = sample_pane_states();
        let cp = CaptureCheckpoint::with_timestamp(panes, 1000);

        // Unknown pane should never skip
        assert!(!cp.should_skip_segment(99, 1));
        assert!(!cp.should_skip_segment(99, 1000));
    }

    #[test]
    fn checkpoint_empty_panes_skip_nothing() {
        let cp = CaptureCheckpoint::with_timestamp(vec![], 1000);
        assert!(!cp.should_skip_segment(1, 1));
        assert!(!cp.should_skip_segment(1, 0));
    }

    #[test]
    fn checkpoint_serialization_json_structure() {
        let panes = vec![PaneCaptureState {
            pane_id: 42,
            last_seq: 999,
            cursor_offset: 65536,
            last_capture_at: 1_700_000_000,
        }];
        let cp = CaptureCheckpoint::with_timestamp(panes, 1_700_000_000);

        let json = serde_json::to_value(&cp).unwrap();
        assert_eq!(json["version"], CHECKPOINT_FORMAT_VERSION);
        assert_eq!(json["created_at"], 1_700_000_000_u64);
        assert_eq!(json["panes"][0]["pane_id"], 42);
        assert_eq!(json["panes"][0]["last_seq"], 999);
        assert_eq!(json["panes"][0]["cursor_offset"], 65536);
    }

    #[test]
    fn checkpoint_overwrite_save() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkpoint.json");

        // Save first checkpoint
        let cp1 = CaptureCheckpoint::with_timestamp(
            vec![PaneCaptureState {
                pane_id: 1,
                last_seq: 10,
                cursor_offset: 0,
                last_capture_at: 100,
            }],
            100,
        );
        cp1.save(&path).unwrap();

        // Overwrite with second checkpoint
        let cp2 = CaptureCheckpoint::with_timestamp(
            vec![PaneCaptureState {
                pane_id: 1,
                last_seq: 50,
                cursor_offset: 4096,
                last_capture_at: 200,
            }],
            200,
        );
        cp2.save(&path).unwrap();

        // Load should get the latest
        let loaded = CaptureCheckpoint::load(&path).unwrap();
        assert_eq!(loaded.created_at, 200);
        assert_eq!(loaded.panes[0].last_seq, 50);
    }

    #[test]
    fn checkpoint_resume_without_duplicates() {
        // Simulate: save checkpoint with pane 1 at seq 100, pane 2 at seq 200
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkpoint.json");

        let cp = CaptureCheckpoint::with_timestamp(sample_pane_states(), 1000);
        cp.save(&path).unwrap();

        // On restart, load checkpoint
        let loaded = CaptureCheckpoint::load(&path).unwrap();

        // Simulate incoming segments: should skip old ones, accept new ones
        let segments = vec![
            (1u64, 99i64, "old-duplicate"),
            (1, 100, "exactly-at-checkpoint"),
            (1, 101, "new-segment"),
            (2, 200, "exactly-at-checkpoint-pane2"),
            (2, 201, "new-segment-pane2"),
            (3, 1, "unknown-pane-always-accept"),
        ];

        let mut accepted = Vec::new();
        let mut skipped = Vec::new();
        for (pane_id, seq, label) in &segments {
            if loaded.should_skip_segment(*pane_id, *seq) {
                skipped.push(*label);
            } else {
                accepted.push(*label);
            }
        }

        assert_eq!(
            skipped,
            vec![
                "old-duplicate",
                "exactly-at-checkpoint",
                "exactly-at-checkpoint-pane2"
            ]
        );
        assert_eq!(
            accepted,
            vec![
                "new-segment",
                "new-segment-pane2",
                "unknown-pane-always-accept"
            ]
        );
    }

    #[test]
    fn pane_capture_state_equality() {
        let a = PaneCaptureState {
            pane_id: 1,
            last_seq: 100,
            cursor_offset: 4096,
            last_capture_at: 1000,
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = PaneCaptureState {
            pane_id: 1,
            last_seq: 101,
            cursor_offset: 4096,
            last_capture_at: 1000,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn detector_and_checkpoint_combined_recovery_flow() {
        // Simulate: crash loop detected, save checkpoint, restart, resume
        let tmp = tempfile::tempdir().unwrap();
        let cp_path = tmp.path().join("checkpoint.json");

        let mut det = CrashLoopDetector::new(CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 60,
            ..CrashLoopConfig::default()
        });

        // Three rapid crashes
        det.record_crash(1000);
        det.record_crash(1001);
        det.record_crash(1002);
        assert!(det.is_crash_loop());

        // Save checkpoint before restart
        let cp = CaptureCheckpoint::with_timestamp(
            vec![PaneCaptureState {
                pane_id: 1,
                last_seq: 50,
                cursor_offset: 2048,
                last_capture_at: 1002,
            }],
            1002,
        );
        cp.save(&cp_path).unwrap();

        // Wait for backoff delay
        let delay = det.next_delay_ms();
        assert!(delay > 0);

        // On restart, load checkpoint and resume
        let loaded = CaptureCheckpoint::load(&cp_path).unwrap();
        assert!(loaded.should_skip_segment(1, 50)); // skip old
        assert!(!loaded.should_skip_segment(1, 51)); // accept new

        // Record success after restart
        det.record_success();
        assert_eq!(det.consecutive_crashes(), 0);
        assert_eq!(det.next_delay_ms(), 0);
    }

    // -----------------------------------------------------------------------
    // Batch 2: RubyBeaver wa-1u90p.7.1 — Struct + replay + enhanced collector
    // -----------------------------------------------------------------------

    #[test]
    fn replay_mode_serialization_round_trip() {
        let policy_json = serde_json::to_string(&ReplayMode::Policy).unwrap();
        assert_eq!(policy_json, "\"policy\"");
        let parsed: ReplayMode = serde_json::from_str(&policy_json).unwrap();
        assert_eq!(parsed, ReplayMode::Policy);

        let rules_json = serde_json::to_string(&ReplayMode::Rules).unwrap();
        assert_eq!(rules_json, "\"rules\"");
        let parsed: ReplayMode = serde_json::from_str(&rules_json).unwrap();
        assert_eq!(parsed, ReplayMode::Rules);
    }

    #[test]
    fn replay_mode_display() {
        assert_eq!(format!("{}", ReplayMode::Policy), "policy");
        assert_eq!(format!("{}", ReplayMode::Rules), "rules");
    }

    #[test]
    fn replay_check_serialization() {
        let check = ReplayCheck {
            name: "manifest_valid".to_string(),
            passed: true,
            detail: Some("All 3 files present".to_string()),
        };
        let json = serde_json::to_string(&check).unwrap();
        let parsed: ReplayCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "manifest_valid");
        assert!(parsed.passed);
        assert_eq!(parsed.detail.as_deref(), Some("All 3 files present"));
    }

    #[test]
    fn replay_check_without_detail() {
        let check = ReplayCheck {
            name: "simple_check".to_string(),
            passed: false,
            detail: None,
        };
        let json = serde_json::to_string(&check).unwrap();
        let parsed: ReplayCheck = serde_json::from_str(&json).unwrap();
        assert!(!parsed.passed);
        assert!(parsed.detail.is_none());
    }

    #[test]
    fn replay_result_serialization() {
        let result = ReplayResult {
            mode: ReplayMode::Policy,
            status: "pass".to_string(),
            checks: vec![ReplayCheck {
                name: "test".to_string(),
                passed: true,
                detail: None,
            }],
            warnings: vec!["minor issue".to_string()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ReplayResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mode, ReplayMode::Policy);
        assert_eq!(parsed.status, "pass");
        assert_eq!(parsed.checks.len(), 1);
        assert_eq!(parsed.warnings.len(), 1);
    }

    #[test]
    fn redaction_report_serialization() {
        let report = RedactionReport {
            total_redactions: 5,
            per_file: vec![
                FileRedactionEntry {
                    file: "crash_report.json".to_string(),
                    count: 3,
                },
                FileRedactionEntry {
                    file: "config_summary.toml".to_string(),
                    count: 2,
                },
            ],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: RedactionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_redactions, 5);
        assert_eq!(parsed.per_file.len(), 2);
        assert_eq!(parsed.per_file[0].file, "crash_report.json");
        assert_eq!(parsed.per_file[0].count, 3);
    }

    #[test]
    fn redaction_report_empty() {
        let report = RedactionReport {
            total_redactions: 0,
            per_file: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: RedactionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_redactions, 0);
        assert!(parsed.per_file.is_empty());
    }

    #[test]
    fn db_metadata_serialization_all_fields() {
        let meta = DbMetadata {
            schema_version: Some(3),
            db_size_bytes: Some(1_048_576),
            journal_mode: Some("wal".to_string()),
            event_count: Some(500),
            segment_count: Some(100),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: DbMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, Some(3));
        assert_eq!(parsed.db_size_bytes, Some(1_048_576));
        assert_eq!(parsed.journal_mode.as_deref(), Some("wal"));
        assert_eq!(parsed.event_count, Some(500));
        assert_eq!(parsed.segment_count, Some(100));
    }

    #[test]
    fn db_metadata_all_none_fields() {
        let meta = DbMetadata {
            schema_version: None,
            db_size_bytes: None,
            journal_mode: None,
            event_count: None,
            segment_count: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: DbMetadata = serde_json::from_str(&json).unwrap();
        assert!(parsed.schema_version.is_none());
        assert!(parsed.db_size_bytes.is_none());
        assert!(parsed.journal_mode.is_none());
        assert!(parsed.event_count.is_none());
        assert!(parsed.segment_count.is_none());
    }

    #[test]
    fn pane_priority_override_snapshot_serialization() {
        let snap = PanePriorityOverrideSnapshot {
            pane_id: 42,
            priority: 1,
            expires_at: Some(1_700_000_000),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: PanePriorityOverrideSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 42);
        assert_eq!(parsed.priority, 1);
        assert_eq!(parsed.expires_at, Some(1_700_000_000));
    }

    #[test]
    fn pane_priority_override_without_expiry() {
        let snap = PanePriorityOverrideSnapshot {
            pane_id: 7,
            priority: 5,
            expires_at: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: PanePriorityOverrideSnapshot = serde_json::from_str(&json).unwrap();
        assert!(parsed.expires_at.is_none());
    }

    #[test]
    fn crash_loop_diagnostics_serialization() {
        let diag = CrashLoopDiagnostics {
            restart_count: 5,
            last_crash_at: Some(1_700_000_000),
            consecutive_crashes: 3,
            current_backoff_ms: 4_000,
            in_crash_loop: true,
        };
        let json = serde_json::to_string(&diag).unwrap();
        let parsed: CrashLoopDiagnostics = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.restart_count, 5);
        assert_eq!(parsed.last_crash_at, Some(1_700_000_000));
        assert_eq!(parsed.consecutive_crashes, 3);
        assert_eq!(parsed.current_backoff_ms, 4_000);
        assert!(parsed.in_crash_loop);
    }

    #[test]
    fn crash_loop_diagnostics_healthy_state() {
        let diag = CrashLoopDiagnostics {
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };
        let json = serde_json::to_string(&diag).unwrap();
        let parsed: CrashLoopDiagnostics = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.restart_count, 0);
        assert!(parsed.last_crash_at.is_none());
        assert!(!parsed.in_crash_loop);
    }

    #[test]
    fn incident_kind_serialization_round_trip() {
        let crash_json = serde_json::to_string(&IncidentKind::Crash).unwrap();
        assert_eq!(crash_json, "\"crash\"");
        let manual_json = serde_json::to_string(&IncidentKind::Manual).unwrap();
        assert_eq!(manual_json, "\"manual\"");

        let parsed: IncidentKind = serde_json::from_str("\"crash\"").unwrap();
        assert_eq!(parsed, IncidentKind::Crash);
        let parsed: IncidentKind = serde_json::from_str("\"manual\"").unwrap();
        assert_eq!(parsed, IncidentKind::Manual);
    }

    #[test]
    fn incident_bundle_result_serialization() {
        let result = IncidentBundleResult {
            path: PathBuf::from("/tmp/test_bundle"),
            kind: IncidentKind::Crash,
            files: vec!["crash_report.json".to_string()],
            total_size_bytes: 1024,
            wa_version: "0.1.0".to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: IncidentBundleResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kind, IncidentKind::Crash);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.total_size_bytes, 1024);
        assert_eq!(parsed.wa_version, "0.1.0");
    }

    #[test]
    fn health_snapshot_with_all_optional_fields() {
        let snapshot = HealthSnapshot {
            timestamp: 1_700_000_000,
            observed_panes: 10,
            capture_queue_depth: 5,
            write_queue_depth: 3,
            last_seq_by_pane: vec![(1, 100), (2, 200), (3, 50)],
            warnings: vec!["backpressure active".to_string()],
            ingest_lag_avg_ms: 25.5,
            ingest_lag_max_ms: 100,
            db_writable: true,
            db_last_write_at: Some(1_699_999_990),
            pane_priority_overrides: vec![PanePriorityOverrideSnapshot {
                pane_id: 1,
                priority: 0,
                expires_at: Some(1_700_001_000),
            }],
            scheduler: None,
            backpressure_tier: Some("Yellow".to_string()),
            last_activity_by_pane: vec![(1, 1_700_000_000), (2, 1_699_999_500)],
            restart_count: 2,
            last_crash_at: Some(1_699_990_000),
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
            fleet_pressure_tier: Some("Normal".to_string()),
            swarm_capacity: None,
            leak_risk_inventory: LeakRiskInventorySnapshot {
                tracked_pane_entries: 10,
                observed_pane_count: 8,
                window_count: 3,
                tab_count: 4,
                workspace_count: 2,
                pane_arena_count: 10,
                pane_arena_tracked_bytes: 32 * 1024,
                pane_arena_peak_tracked_bytes: 48 * 1024,
                cursor_snapshot_bytes: 16 * 1024,
                cursor_snapshot_peak_bytes: 24 * 1024,
                storage_lock_contention_events: 3,
                storage_lock_wait_max_ms: 12.5,
                storage_lock_hold_max_ms: 4.0,
                watchdog: LeakRiskWatchdogSnapshot {
                    overall: Some(crate::watchdog::HealthStatus::Degraded),
                    unhealthy_components: vec![LeakRiskWatchdogComponentSnapshot {
                        component: crate::watchdog::Component::Capture,
                        status: crate::watchdog::HealthStatus::Degraded,
                        age_ms: Some(6_000),
                        threshold_ms: 5_000,
                    }],
                    telemetry: Some(crate::watchdog::WatchdogTelemetrySnapshot {
                        discovery_heartbeats: 10,
                        capture_heartbeats: 100,
                        persistence_heartbeats: 95,
                        maintenance_heartbeats: 4,
                        health_checks: 8,
                    }),
                },
            },
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: HealthSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.observed_panes, 10);
        assert_eq!(parsed.pane_priority_overrides.len(), 1);
        assert_eq!(parsed.pane_priority_overrides[0].pane_id, 1);
        assert_eq!(parsed.backpressure_tier.as_deref(), Some("Yellow"));
        assert_eq!(parsed.last_activity_by_pane.len(), 2);
        assert_eq!(parsed.restart_count, 2);
        assert_eq!(parsed.fleet_pressure_tier.as_deref(), Some("Normal"));
        assert_eq!(parsed.last_crash_at, Some(1_699_990_000));
        assert_eq!(parsed.leak_risk_inventory.window_count, 3);
        assert_eq!(
            parsed.leak_risk_inventory.watchdog.overall,
            Some(crate::watchdog::HealthStatus::Degraded)
        );
    }

    #[test]
    fn health_snapshot_default_optional_fields_deserialize() {
        // JSON without the newer optional fields (tests serde(default))
        let json = r#"{
            "timestamp": 1000,
            "observed_panes": 1,
            "capture_queue_depth": 0,
            "write_queue_depth": 0,
            "last_seq_by_pane": [],
            "warnings": [],
            "ingest_lag_avg_ms": 0.0,
            "ingest_lag_max_ms": 0,
            "db_writable": true,
            "db_last_write_at": null
        }"#;
        let parsed: HealthSnapshot = serde_json::from_str(json).unwrap();
        assert!(parsed.pane_priority_overrides.is_empty());
        assert!(parsed.scheduler.is_none());
        assert!(parsed.backpressure_tier.is_none());
        assert!(parsed.last_activity_by_pane.is_empty());
        assert_eq!(parsed.restart_count, 0);
        assert!(parsed.last_crash_at.is_none());
        assert_eq!(parsed.consecutive_crashes, 0);
        assert_eq!(parsed.current_backoff_ms, 0);
        assert!(!parsed.in_crash_loop);
        assert!(parsed.fleet_pressure_tier.is_none());
        assert!(parsed.swarm_capacity.is_none());
        assert!(parsed.leak_risk_inventory.is_empty());
    }

    #[test]
    fn health_snapshot_fleet_pressure_tier_roundtrip() {
        let snapshot = HealthSnapshot {
            timestamp: 2000,
            observed_panes: 10,
            capture_queue_depth: 0,
            write_queue_depth: 0,
            last_seq_by_pane: vec![],
            warnings: vec![],
            ingest_lag_avg_ms: 0.0,
            ingest_lag_max_ms: 0,
            db_writable: true,
            db_last_write_at: None,
            pane_priority_overrides: vec![],
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
            fleet_pressure_tier: Some("Critical".to_string()),
            swarm_capacity: None,
            leak_risk_inventory: LeakRiskInventorySnapshot::default(),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: HealthSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.fleet_pressure_tier.as_deref(), Some("Critical"));
        assert!(json.contains("fleet_pressure_tier"));
        assert!(json.contains("Critical"));
    }

    // -- replay_incident_bundle tests --

    #[test]
    fn replay_bundle_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not_a_dir.txt");
        fs::write(&file_path, "hello").unwrap();

        let result = replay_incident_bundle(&file_path, ReplayMode::Policy);
        assert!(result.is_err());
    }

    #[test]
    fn replay_bundle_missing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty directory — no manifest.json or incident_manifest.json
        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert_eq!(result.status, "fail");
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "manifest_valid" && !c.passed)
        );
    }

    #[test]
    fn replay_bundle_invalid_manifest_json() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("incident_manifest.json"), "not json!!!").unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert_eq!(result.status, "fail");
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "manifest_valid" && !c.passed)
        );
    }

    #[test]
    fn replay_bundle_valid_manifest_no_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Crash,
            files: vec![],
            total_size_bytes: 0,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        fs::write(tmp.path().join("incident_manifest.json"), &json).unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        // Manifest is valid
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "manifest_valid" && c.passed)
        );
        // No redaction report → warning
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("redaction_report"))
        );
        // No secrets found → passes
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "no_secrets_leaked" && c.passed)
        );
    }

    #[test]
    fn replay_bundle_with_valid_redaction_report() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Manual,
            files: vec!["redaction_report.json".to_string()],
            total_size_bytes: 100,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let redaction = RedactionReport {
            total_redactions: 2,
            per_file: vec![FileRedactionEntry {
                file: "crash_report.json".to_string(),
                count: 2,
            }],
        };
        fs::write(
            tmp.path().join("redaction_report.json"),
            serde_json::to_string_pretty(&redaction).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "redaction_report_valid" && c.passed)
        );
    }

    #[test]
    fn replay_bundle_with_invalid_redaction_report() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Manual,
            files: vec!["redaction_report.json".to_string()],
            total_size_bytes: 100,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(tmp.path().join("redaction_report.json"), "{ bad json }").unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "redaction_report_valid" && !c.passed)
        );
    }

    #[test]
    fn replay_bundle_policy_mode_with_crash_report() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Crash,
            files: vec!["crash_report.json".to_string()],
            total_size_bytes: 200,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let report = test_report();
        fs::write(
            tmp.path().join("crash_report.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "crash_report_valid" && c.passed)
        );
    }

    #[test]
    fn replay_bundle_policy_mode_invalid_crash_report() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Crash,
            files: vec!["crash_report.json".to_string()],
            total_size_bytes: 200,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(tmp.path().join("crash_report.json"), "not valid crash json").unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "crash_report_valid" && !c.passed)
        );
    }

    #[test]
    fn replay_bundle_policy_mode_with_db_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Manual,
            files: vec![],
            total_size_bytes: 0,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let db_meta = DbMetadata {
            schema_version: Some(3),
            db_size_bytes: Some(1024),
            journal_mode: Some("wal".to_string()),
            event_count: Some(50),
            segment_count: Some(10),
        };
        fs::write(
            tmp.path().join("db_metadata.json"),
            serde_json::to_string_pretty(&db_meta).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "db_metadata_valid" && c.passed)
        );
    }

    #[test]
    fn replay_bundle_rules_mode_with_valid_events() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Crash,
            files: vec!["recent_events.json".to_string()],
            total_size_bytes: 100,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let events = serde_json::json!([
            {
                "rule_id": "r1",
                "event_type": "pattern_match",
                "severity": "warning",
                "matched_text_preview": "short text"
            },
            {
                "rule_id": "r2",
                "event_type": "anomaly",
                "severity": "critical",
                "matched_text_preview": "another"
            }
        ]);
        fs::write(
            tmp.path().join("recent_events.json"),
            serde_json::to_string_pretty(&events).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Rules).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "events_structure_valid" && c.passed)
        );
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "events_text_bounded" && c.passed)
        );
    }

    #[test]
    fn replay_bundle_rules_mode_missing_events() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Manual,
            files: vec![],
            total_size_bytes: 0,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Rules).unwrap();
        assert!(result.warnings.iter().any(|w| w.contains("recent_events")));
    }

    #[test]
    fn replay_bundle_rules_mode_oversized_text_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Crash,
            files: vec!["recent_events.json".to_string()],
            total_size_bytes: 100,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        // Event with oversized matched_text_preview (>200 chars)
        let oversized = "x".repeat(300);
        let events = serde_json::json!([{
            "rule_id": "r1",
            "event_type": "match",
            "severity": "warning",
            "matched_text_preview": oversized
        }]);
        fs::write(
            tmp.path().join("recent_events.json"),
            serde_json::to_string_pretty(&events).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Rules).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "events_text_bounded" && !c.passed)
        );
    }

    #[test]
    fn replay_bundle_rules_mode_multibyte_preview_at_char_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Crash,
            files: vec!["recent_events.json".to_string()],
            total_size_bytes: 100,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // 200 chars, but much more than 200 bytes in UTF-8.
        let preview = "🦀".repeat(200);
        let events = serde_json::json!([{
            "rule_id": "r1",
            "event_type": "match",
            "severity": "warning",
            "matched_text_preview": preview
        }]);
        fs::write(
            tmp.path().join("recent_events.json"),
            serde_json::to_string_pretty(&events).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Rules).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "events_text_bounded" && c.passed)
        );
    }

    #[test]
    fn replay_bundle_files_complete_check() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Crash,
            files: vec![
                "crash_report.json".to_string(),
                "missing_file.json".to_string(),
            ],
            total_size_bytes: 100,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        // Only create one of the two listed files
        let report = test_report();
        fs::write(
            tmp.path().join("crash_report.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "files_complete" && !c.passed)
        );
    }

    #[test]
    fn replay_bundle_all_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = IncidentBundleResult {
            path: tmp.path().to_path_buf(),
            kind: IncidentKind::Manual,
            files: vec!["data.json".to_string()],
            total_size_bytes: 50,
            wa_version: crate::VERSION.to_string(),
            exported_at: "2023-11-14T22:13:20Z".to_string(),
            swarm: None,
        };
        fs::write(
            tmp.path().join("incident_manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(tmp.path().join("data.json"), "{}").unwrap();

        let result = replay_incident_bundle(tmp.path(), ReplayMode::Policy).unwrap();
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.name == "files_complete" && c.passed)
        );
    }

    // -- write_redacted_file tests --

    #[test]
    fn write_redacted_file_tracks_no_redactions() {
        let tmp = tempfile::tempdir().unwrap();
        let redactor = Redactor::new();
        let mut files = Vec::new();
        let mut total_size = 0u64;
        let mut entries = Vec::new();

        write_redacted_file(
            "test.json",
            "clean content",
            tmp.path(),
            &redactor,
            &mut files,
            &mut total_size,
            &mut entries,
        )
        .unwrap();

        assert_eq!(files, vec!["test.json"]);
        assert!(total_size > 0);
        assert!(entries.is_empty()); // No redactions
        assert!(tmp.path().join("test.json").exists());
    }

    // -- collect_incident_bundle tests --

    #[test]
    fn collect_incident_bundle_manual_creates_redaction_report() {
        let _incident_source_guards = lock_incident_source_globals_for_test();
        clear_incident_source_globals_for_test();

        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");

        let opts = IncidentBundleOptions {
            crash_dir: &crash_dir,
            config_path: None,
            out_dir: &out_dir,
            kind: IncidentKind::Manual,
            db_path: None,
            max_events: 0,
        };

        let result = collect_incident_bundle(&opts).unwrap();
        assert_eq!(result.kind, IncidentKind::Manual);
        assert!(result.files.contains(&"redaction_report.json".to_string()));

        // Verify redaction report exists and is valid
        let report_path = result.path.join("redaction_report.json");
        assert!(report_path.exists());
        let report: RedactionReport =
            serde_json::from_str(&fs::read_to_string(report_path).unwrap()).unwrap();
        assert_eq!(report.total_redactions, 0);
    }

    #[test]
    fn collect_incident_bundle_manual_records_skipped_source_warnings() {
        let _incident_source_guards = lock_incident_source_globals_for_test();
        clear_incident_source_globals_for_test();

        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");

        let opts = IncidentBundleOptions {
            crash_dir: &crash_dir,
            config_path: None,
            out_dir: &out_dir,
            kind: IncidentKind::Manual,
            db_path: None,
            max_events: 0,
        };

        let result = collect_incident_bundle(&opts).unwrap();
        let swarm = result.swarm.as_ref().expect("swarm provenance manifest");
        let warning_ids: std::collections::HashSet<&str> = swarm
            .warnings
            .iter()
            .map(|warning| warning.id.as_str())
            .collect();

        for source in &swarm.sources {
            if source.status != IncidentSourceStatus::Collected {
                assert!(
                    !source.warning_ids.is_empty(),
                    "degraded source {} should carry a typed warning id",
                    source.name
                );
                for warning_id in &source.warning_ids {
                    assert!(
                        warning_ids.contains(warning_id.as_str()),
                        "source {} references missing warning id {}",
                        source.name,
                        warning_id
                    );
                }
            }
        }

        assert!(
            warning_ids
                .iter()
                .all(|warning_id| !warning_id.ends_with(".not_wired")),
            "incident sources should report concrete reasons rather than placeholder not_wired warnings"
        );
        assert!(warning_ids.contains("pane_text_summaries.privacy_disabled"));
        assert!(warning_ids.contains("proof_rch_evidence.not_attached"));
        assert!(warning_ids.contains("agent_mail.not_attached"));
        assert!(
            swarm
                .sources
                .iter()
                .any(|source| source.name == "beads_coordination_snapshot")
        );
        assert!(warning_ids.contains("crash_bundle.skipped"));
        assert!(warning_ids.contains("config_summary.skipped"));
        assert!(warning_ids.contains("db_metadata.skipped"));
        assert!(warning_ids.contains("recent_events.db_not_configured"));
    }

    #[test]
    fn beads_coordination_payload_counts_statuses_without_mutation() {
        let payload = beads_coordination_payload(
            r#"{"id":"ft-a","title":"A","status":"open","priority":2,"assignee":null,"updated_at":"2026-05-16T00:00:00Z"}
{"id":"ft-b","title":"B","status":"in_progress","priority":1,"assignee":"Codex","updated_at":"2026-05-16T00:01:00Z"}
{"id":"ft-c","title":"C","status":"blocked","priority":3,"assignee":"BlueLake","updated_at":"2026-05-16T00:02:00Z"}
not-json
"#,
        );

        assert_eq!(payload["counts"]["total"], 3);
        assert_eq!(payload["counts"]["open"], 1);
        assert_eq!(payload["counts"]["in_progress"], 1);
        assert_eq!(payload["counts"]["blocked"], 1);
        assert_eq!(payload["parse_error_count"], 1);
        assert_eq!(payload["mutated_beads"], false);
        assert_eq!(payload["ready_candidates"].as_array().unwrap().len(), 1);
        assert_eq!(
            payload["stale_reopen_review_candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn git_dirty_tree_payload_classifies_counts_and_risk() {
        let payload = git_dirty_tree_payload(
            " M crates/frankenterm-core/src/crash.rs\nA  Cargo.toml\n?? fixtures/example.json\n D .beads/issues.jsonl\n",
            Some("main"),
        );

        assert_eq!(payload["branch"], "main");
        assert_eq!(payload["status"], "dirty");
        assert_eq!(payload["counts"]["total"], 4);
        assert_eq!(payload["counts"]["tracked_dirty"], 3);
        assert_eq!(payload["counts"]["untracked"], 1);
        // Only `A  Cargo.toml` has an index (X-column) change; ` M` and ` D`
        // are worktree-only. Per git porcelain XY semantics that is 1 staged
        // and 2 unstaged entries.
        assert_eq!(payload["counts"]["staged"], 1);
        assert_eq!(payload["counts"]["unstaged"], 2);
        assert_eq!(payload["counts"]["deleted"], 1);

        let categories = payload["risk"]["categories"].as_array().unwrap();
        assert!(categories.iter().any(|category| category == "core_crate"));
        assert!(
            categories
                .iter()
                .any(|category| category == "workspace_manifest")
        );
        assert!(
            categories
                .iter()
                .any(|category| category == "coordination_state")
        );
        assert!(categories.iter().any(|category| category == "fixtures"));
    }

    #[test]
    fn collect_incident_bundle_crash_with_existing_bundle() {
        let _incident_source_guards = lock_incident_source_globals_for_test();
        clear_incident_source_globals_for_test();

        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");

        // Create a crash bundle first
        let report = test_report();
        write_crash_bundle(&crash_dir, &report, Some(&test_snapshot()), None).unwrap();

        let opts = IncidentBundleOptions {
            crash_dir: &crash_dir,
            config_path: None,
            out_dir: &out_dir,
            kind: IncidentKind::Crash,
            db_path: None,
            max_events: 0,
        };

        let result = collect_incident_bundle(&opts).unwrap();
        assert_eq!(result.kind, IncidentKind::Crash);
        assert!(result.files.contains(&"crash_report.json".to_string()));
        assert!(result.files.contains(&"redaction_report.json".to_string()));
    }

    #[test]
    fn collect_incident_bundle_with_config() {
        let _incident_source_guards = lock_incident_source_globals_for_test();
        clear_incident_source_globals_for_test();

        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");
        let config_path = tmp.path().join("config.toml");

        fs::write(&config_path, "[ingest]\nbuffer_size = 2048\n").unwrap();

        let opts = IncidentBundleOptions {
            crash_dir: &crash_dir,
            config_path: Some(&config_path),
            out_dir: &out_dir,
            kind: IncidentKind::Manual,
            db_path: None,
            max_events: 0,
        };

        let result = collect_incident_bundle(&opts).unwrap();
        assert!(result.files.contains(&"config_summary.toml".to_string()));
    }

    #[test]
    fn collect_incident_bundle_invalid_db_records_failed_recent_events() {
        let _incident_source_guards = lock_incident_source_globals_for_test();
        clear_incident_source_globals_for_test();

        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");
        let db_path = tmp.path().join("invalid.db");
        fs::write(&db_path, "this is not sqlite").unwrap();

        let opts = IncidentBundleOptions {
            crash_dir: &crash_dir,
            config_path: None,
            out_dir: &out_dir,
            kind: IncidentKind::Manual,
            db_path: Some(&db_path),
            max_events: 10,
        };

        let result = collect_incident_bundle(&opts).unwrap();
        assert!(result.path.join("db_metadata.json").exists());
        assert!(!result.path.join("recent_events.json").exists());

        let swarm = result.swarm.as_ref().expect("swarm provenance manifest");
        assert!(swarm.sources.iter().any(|source| {
            source.name == "recent_events"
                && source.status == IncidentSourceStatus::Failed
                && source
                    .warning_ids
                    .iter()
                    .any(|id| id == "recent_events.query_failed")
        }));
        assert!(
            swarm
                .warnings
                .iter()
                .any(|warning| warning.id == "recent_events.query_failed")
        );

        let replay = replay_incident_bundle(&result.path, ReplayMode::Policy).unwrap();
        assert_eq!(replay.status, "pass");
    }

    #[test]
    fn collect_incident_bundle_redacts_manifest_and_warning_direct_fields() {
        let _incident_source_guards = lock_incident_source_globals_for_test();
        clear_incident_source_globals_for_test();

        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let secret = "AKIAABCDEFGHIJKLMNOP";
        let out_dir = tmp.path().join(format!("out-{secret}"));
        let db_path = tmp.path().join(format!("{secret}.sqlite"));

        let opts = IncidentBundleOptions {
            crash_dir: &crash_dir,
            config_path: None,
            out_dir: &out_dir,
            kind: IncidentKind::Manual,
            db_path: Some(&db_path),
            max_events: 10,
        };

        let result = collect_incident_bundle(&opts).unwrap();
        let manifest_text = fs::read_to_string(result.path.join("incident_manifest.json")).unwrap();
        let warnings_text = fs::read_to_string(result.path.join("warnings.jsonl")).unwrap();
        let report: RedactionReport = serde_json::from_str(
            &fs::read_to_string(result.path.join("redaction_report.json")).unwrap(),
        )
        .unwrap();

        assert!(!manifest_text.contains(secret));
        assert!(!warnings_text.contains(secret));
        assert!(manifest_text.contains("[REDACTED:aws_access_key_id]"));
        assert!(warnings_text.contains("[REDACTED:aws_access_key_id]"));
        assert!(result.path.to_string_lossy().contains(secret));
        let manifest: IncidentBundleResult = serde_json::from_str(&manifest_text).unwrap();
        assert!(!manifest.path.to_string_lossy().contains(secret));
        assert!(!manifest.path.is_absolute());

        let manifest_entry = report
            .per_file
            .iter()
            .find(|entry| entry.file == "incident_manifest.json")
            .expect("incident manifest redaction entry");
        // 8 = db_metadata surface + db_metadata warning + recent_events
        // surface + robot_state degraded surface + robot_state db_read_failed
        // warning + pane_text_summaries degraded surface +
        // pane_text_summaries db_read_failed warning + bundle-dir path. The
        // last four contributors arrived with the ft-9sy9e incident-DB
        // fallback collectors; a count DROP below this means a
        // secret-bearing surface stopped being redacted (ft-kccj8).
        assert_eq!(manifest_entry.count, 8);
        let warnings_entry = report
            .per_file
            .iter()
            .find(|entry| entry.file == "warnings.jsonl")
            .expect("warnings redaction entry");
        // db_metadata + robot_state + pane_text_summaries warnings each
        // embed the secret db path.
        assert_eq!(warnings_entry.count, 3);
    }

    #[test]
    fn collect_incident_bundle_truncates_multibyte_config_safely() {
        let _incident_source_guards = lock_incident_source_globals_for_test();
        clear_incident_source_globals_for_test();

        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let out_dir = tmp.path().join("out");
        let config_path = tmp.path().join("config.toml");

        // 4-byte UTF-8 scalar repeated to exceed 64 KiB.
        let huge_config = "🦀".repeat((64 * 1024 / 4) + 100);
        fs::write(&config_path, huge_config).unwrap();

        let opts = IncidentBundleOptions {
            crash_dir: &crash_dir,
            config_path: Some(&config_path),
            out_dir: &out_dir,
            kind: IncidentKind::Manual,
            db_path: None,
            max_events: 0,
        };

        let result = collect_incident_bundle(&opts).unwrap();
        let config_summary = fs::read_to_string(result.path.join("config_summary.toml")).unwrap();

        assert!(config_summary.len() <= 64 * 1024);
        assert!(config_summary.contains("truncated at 64 KiB"));
    }

    // -- Additional edge case tests --

    #[test]
    fn days_to_ymd_dec31_to_jan1() {
        // 2023-12-31
        let (y, m, d) = days_to_ymd(19_722);
        assert_eq!((y, m, d), (2023, 12, 31));
        // 2024-01-01
        let (y, m, d) = days_to_ymd(19_723);
        assert_eq!((y, m, d), (2024, 1, 1));
    }

    #[test]
    fn days_to_ymd_feb28_non_leap() {
        // 2023-02-28
        let (y, m, d) = days_to_ymd(19_416);
        assert_eq!((y, m, d), (2023, 2, 28));
        // 2023-03-01
        let (y, m, d) = days_to_ymd(19_417);
        assert_eq!((y, m, d), (2023, 3, 1));
    }

    #[test]
    fn detector_total_restarts_increments() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig {
            window_secs: 3600,
            ..CrashLoopConfig::default()
        });
        assert_eq!(det.total_restarts(), 0);
        det.record_crash(1000);
        assert_eq!(det.total_restarts(), 1);
        det.record_crash(1001);
        assert_eq!(det.total_restarts(), 2);
        det.record_success();
        assert_eq!(det.total_restarts(), 2); // success doesn't clear timestamps
    }

    #[test]
    fn detector_last_crash_timestamp() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig::default());
        assert_eq!(det.last_crash_timestamp(), None);
        det.record_crash(42);
        assert_eq!(det.last_crash_timestamp(), Some(42));
        det.record_crash(99);
        assert_eq!(det.last_crash_timestamp(), Some(99));
    }

    #[test]
    fn shutdown_summary_with_warnings() {
        let summary = ShutdownSummary {
            elapsed_secs: 120,
            final_capture_queue: 5,
            final_write_queue: 2,
            segments_persisted: 50,
            events_recorded: 10,
            last_seq_by_pane: vec![(1, 50), (2, 30)],
            clean: false,
            warnings: vec![
                "timeout waiting for flush".to_string(),
                "queue not empty".to_string(),
            ],
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: ShutdownSummary = serde_json::from_str(&json).unwrap();
        assert!(!parsed.clean);
        assert_eq!(parsed.warnings.len(), 2);
        assert_eq!(parsed.final_capture_queue, 5);
        assert_eq!(parsed.final_write_queue, 2);
    }

    #[test]
    fn crash_manifest_with_resize_forensics() {
        let manifest = CrashManifest {
            wa_version: "0.2.0".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            files: vec![
                "crash_report.json".to_string(),
                "resize_forensics.json".to_string(),
            ],
            has_health_snapshot: true,
            has_resize_forensics: true,
            has_environment_markers: false,
            bundle_size_bytes: 4096,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: CrashManifest = serde_json::from_str(&json).unwrap();
        assert!(parsed.has_resize_forensics);
        assert_eq!(parsed.files.len(), 2);
    }

    #[test]
    fn crash_manifest_default_resize_forensics() {
        // Old manifest JSON without has_resize_forensics field
        let json = r#"{
            "wa_version": "0.1.0",
            "created_at": "2023-01-01T00:00:00Z",
            "files": [],
            "has_health_snapshot": false,
            "bundle_size_bytes": 0
        }"#;
        let parsed: CrashManifest = serde_json::from_str(json).unwrap();
        assert!(!parsed.has_resize_forensics); // defaults to false
        assert!(!parsed.has_environment_markers); // defaults to false
    }
}

// ---------------------------------------------------------------------------
// E2E crash loop recovery tests (bd-1gf6)
// ---------------------------------------------------------------------------
//
// These tests simulate realistic multi-crash scenarios end-to-end:
// - crash loop detection with escalating backoff
// - checkpoint persistence across simulated restarts
// - duplicate segment rejection after recovery
// - restart history tracking with artifact generation
//
// Unlike the unit tests above, these exercise the full detector + checkpoint
// pipeline in multi-step sequences that mirror production crash/restart cycles.

#[cfg(test)]
mod e2e_crash_recovery {
    use super::*;

    static CRASH_BUNDLE_PARSE_DROP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Simulate a full watcher lifecycle: start, run for N "ticks", crash.
    /// Returns the pane states at the time of crash (for checkpointing).
    fn simulate_watcher_run(
        start_time: u64,
        pane_ids: &[u64],
        base_seq: i64,
        ticks: i64,
    ) -> Vec<PaneCaptureState> {
        pane_ids
            .iter()
            .map(|&pane_id| PaneCaptureState {
                pane_id,
                last_seq: base_seq + ticks,
                cursor_offset: (base_seq + ticks) as u64 * 512,
                last_capture_at: start_time + ticks as u64,
            })
            .collect()
    }

    // -- E2E Scenario 1: Escalating backoff across multiple crashes --

    #[test]
    fn e2e_crash_loop_backoff_escalation() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 300,
            initial_delay_ms: 1_000,
            max_delay_ms: 60_000,
            backoff_factor: 2.0,
        });

        // Collect (crash_number, delay_ms, in_loop) for each crash
        let mut history: Vec<(u32, u64, bool)> = Vec::new();

        // Simulate 7 rapid crashes within the 5-minute window
        for i in 0..7u64 {
            det.record_crash(1000 + i);
            let delay = det.next_delay_ms();
            let in_loop = det.is_crash_loop();
            history.push((det.consecutive_crashes(), delay, in_loop));
        }

        // Verify escalating backoff: 1s, 2s, 4s, 8s, 16s, 32s, 60s
        assert_eq!(history[0], (1, 1_000, false)); // 1st crash: 1s, not loop
        assert_eq!(history[1], (2, 2_000, false)); // 2nd: 2s, not loop
        assert_eq!(history[2], (3, 4_000, true)); // 3rd: 4s, LOOP DETECTED
        assert_eq!(history[3], (4, 8_000, true)); // 4th: 8s
        assert_eq!(history[4], (5, 16_000, true)); // 5th: 16s
        assert_eq!(history[5], (6, 32_000, true)); // 6th: 32s
        assert_eq!(history[6], (7, 60_000, true)); // 7th: capped at 60s

        // After successful run, backoff resets
        det.record_success();
        assert_eq!(det.consecutive_crashes(), 0);
        assert_eq!(det.next_delay_ms(), 0);
        // But crash timestamps remain in window
        assert!(det.crashes_in_window(1010) >= 7);
    }

    // -- E2E Scenario 2: Stable run resets crash history --

    #[test]
    fn e2e_stable_run_clears_crash_history() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 60, // 1-minute window
            ..CrashLoopConfig::default()
        });

        // Two crashes in quick succession
        det.record_crash(100);
        det.record_crash(101);
        assert_eq!(det.consecutive_crashes(), 2);
        assert!(!det.is_crash_loop());

        // Record success (simulates watcher ran stably for >5 min)
        det.record_success();
        assert_eq!(det.consecutive_crashes(), 0);

        // Now crash again — but old timestamps have aged out of window
        det.record_crash(200); // 200 - 100 = 100s > 60s window
        assert_eq!(det.consecutive_crashes(), 1);
        assert_eq!(det.crashes_in_window(200), 1); // old crashes pruned
        assert!(!det.is_crash_loop());
    }

    // -- E2E Scenario 3: Checkpoint prevents duplicate segments --

    #[test]
    fn e2e_checkpoint_dedup_across_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let cp_path = tmp.path().join("wa_checkpoint.json");

        // === First run: capture segments 1-50 on panes 1, 2, 3 ===
        let panes = simulate_watcher_run(1000, &[1, 2, 3], 0, 50);
        assert_eq!(panes[0].last_seq, 50);
        assert_eq!(panes[1].last_seq, 50);
        assert_eq!(panes[2].last_seq, 50);

        // Crash! Save checkpoint.
        let cp = CaptureCheckpoint::with_timestamp(panes, 1050);
        cp.save(&cp_path).unwrap();

        // === Second run: load checkpoint and verify dedup ===
        let loaded = CaptureCheckpoint::load(&cp_path).unwrap();

        // Segments at or before seq 50 should be skipped (dedup)
        for pane_id in [1, 2, 3] {
            assert!(
                loaded.should_skip_segment(pane_id, 1),
                "pane {pane_id}: should skip seq 1 (already captured)"
            );
            assert!(
                loaded.should_skip_segment(pane_id, 50),
                "pane {pane_id}: should skip seq 50 (boundary)"
            );
            assert!(
                !loaded.should_skip_segment(pane_id, 51),
                "pane {pane_id}: should NOT skip seq 51 (new)"
            );
        }

        // Unknown pane should not skip anything
        assert!(
            !loaded.should_skip_segment(99, 1),
            "unknown pane should not skip"
        );
    }

    // -- E2E Scenario 4: Multi-restart with checkpoint updates --

    #[test]
    fn e2e_multi_restart_checkpoint_progression() {
        let tmp = tempfile::tempdir().unwrap();
        let cp_path = tmp.path().join("wa_checkpoint.json");
        let mut det = CrashLoopDetector::new(CrashLoopConfig::default());

        // === Run 1: capture seq 1-20 ===
        let panes_r1 = simulate_watcher_run(1000, &[1, 2], 0, 20);
        det.record_crash(1020);
        CaptureCheckpoint::with_timestamp(panes_r1.clone(), 1020)
            .save(&cp_path)
            .unwrap();

        let cp1 = CaptureCheckpoint::load(&cp_path).unwrap();
        assert_eq!(cp1.pane_state(1).unwrap().last_seq, 20);

        // === Run 2: resume from seq 20, capture to 45 ===
        let panes_r2 = simulate_watcher_run(1025, &[1, 2], 20, 25);
        det.record_crash(1050);
        CaptureCheckpoint::with_timestamp(panes_r2, 1050)
            .save(&cp_path)
            .unwrap();

        let cp2 = CaptureCheckpoint::load(&cp_path).unwrap();
        assert_eq!(cp2.pane_state(1).unwrap().last_seq, 45);
        // Verify dedup: seq 20 from run 1 should be skipped
        assert!(cp2.should_skip_segment(1, 20));
        assert!(cp2.should_skip_segment(1, 45));
        assert!(!cp2.should_skip_segment(1, 46));

        // === Run 3: resume from seq 45, capture to 100, SUCCESS ===
        det.record_success();
        assert_eq!(det.consecutive_crashes(), 0);

        let panes_r3 = simulate_watcher_run(1055, &[1, 2], 45, 55);
        CaptureCheckpoint::with_timestamp(panes_r3, 1110)
            .save(&cp_path)
            .unwrap();

        let cp3 = CaptureCheckpoint::load(&cp_path).unwrap();
        assert_eq!(cp3.pane_state(1).unwrap().last_seq, 100);
        assert!(cp3.should_skip_segment(1, 100));
        assert!(!cp3.should_skip_segment(1, 101));

        // Total backoff pattern: 2 consecutive crashes → delays of 1s, 2s
        // then success resets
        assert_eq!(det.next_delay_ms(), 0);
    }

    // -- E2E Scenario 5: Crash bundle + detector + checkpoint integration --

    #[test]
    fn e2e_full_recovery_with_crash_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path().join("crash");
        let cp_path = tmp.path().join("wa_checkpoint.json");

        let mut det = CrashLoopDetector::new(CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 300,
            initial_delay_ms: 500,
            max_delay_ms: 30_000,
            backoff_factor: 2.0,
        });

        // Simulate 4 crash/restart cycles, collecting artifacts
        let mut artifacts: Vec<serde_json::Value> = Vec::new();

        for cycle in 0..4u64 {
            let start_ts = 1000 + cycle * 10;
            let crash_ts = start_ts + 5;

            // Capture some data
            let panes = simulate_watcher_run(start_ts, &[1], cycle as i64 * 10, 5);

            // Crash
            det.record_crash(crash_ts);

            // Save checkpoint
            CaptureCheckpoint::with_timestamp(panes, crash_ts)
                .save(&cp_path)
                .unwrap();

            // Write crash bundle
            let bundle_dir = crash_dir.join(format!("ft_crash_{crash_ts}"));
            std::fs::create_dir_all(&bundle_dir).unwrap();

            let report = CrashReport {
                message: format!("simulated panic in cycle {cycle}"),
                location: Some("e2e_test:0:0".to_string()),
                backtrace: None,
                timestamp: crash_ts,
                pid: std::process::id(),
                thread_name: Some("test".to_string()),
            };
            let report_json = serde_json::to_string_pretty(&report).unwrap();
            std::fs::write(bundle_dir.join("crash_report.json"), &report_json).unwrap();

            // Collect artifact data
            artifacts.push(serde_json::json!({
                "cycle": cycle,
                "crash_ts": crash_ts,
                "consecutive_crashes": det.consecutive_crashes(),
                "backoff_ms": det.next_delay_ms(),
                "in_crash_loop": det.is_crash_loop(),
                "checkpoint_seq": CaptureCheckpoint::load(&cp_path)
                    .unwrap().pane_state(1).unwrap().last_seq,
            }));
        }

        // Verify escalating backoff across cycles
        assert_eq!(artifacts[0]["backoff_ms"], 500);
        assert_eq!(artifacts[1]["backoff_ms"], 1_000);
        assert_eq!(artifacts[2]["backoff_ms"], 2_000);
        assert_eq!(artifacts[3]["backoff_ms"], 4_000);

        // Crash loop detected at cycle 2 (3rd crash)
        assert_eq!(artifacts[0]["in_crash_loop"], false);
        assert_eq!(artifacts[1]["in_crash_loop"], false);
        assert_eq!(artifacts[2]["in_crash_loop"], true);
        assert_eq!(artifacts[3]["in_crash_loop"], true);

        // Checkpoint progresses: seq 5, 15, 25, 35
        assert_eq!(artifacts[0]["checkpoint_seq"], 5);
        assert_eq!(artifacts[1]["checkpoint_seq"], 15);
        assert_eq!(artifacts[2]["checkpoint_seq"], 25);
        assert_eq!(artifacts[3]["checkpoint_seq"], 35);

        // Verify crash bundles on disk
        let bundles: Vec<_> = std::fs::read_dir(&crash_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(bundles.len(), 4, "expected 4 crash bundles");

        // Write E2E artifact report
        let report = serde_json::json!({
            "test": "e2e_full_recovery_with_crash_bundle",
            "cycles": artifacts,
            "crash_bundles": bundles.len(),
            "final_checkpoint_seq": 35,
            "final_backoff_ms": 4_000,
            "crash_loop_detected_at_cycle": 2,
        });
        let artifact_path = tmp.path().join("e2e_crash_recovery_report.json");
        std::fs::write(
            &artifact_path,
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();

        // Verify the artifact is valid JSON
        let loaded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&artifact_path).unwrap()).unwrap();
        assert_eq!(loaded["crash_bundles"], 4);
    }

    // -- E2E Scenario 6: New pane discovered after restart --

    #[test]
    fn e2e_new_pane_after_restart_not_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let cp_path = tmp.path().join("wa_checkpoint.json");

        // Run 1: observe panes 1, 2
        let panes = simulate_watcher_run(1000, &[1, 2], 0, 30);
        CaptureCheckpoint::with_timestamp(panes, 1030)
            .save(&cp_path)
            .unwrap();

        // Run 2: pane 3 is new (not in checkpoint)
        let loaded = CaptureCheckpoint::load(&cp_path).unwrap();

        // Existing panes: skip old segments
        assert!(loaded.should_skip_segment(1, 30));
        assert!(loaded.should_skip_segment(2, 30));
        assert!(!loaded.should_skip_segment(1, 31));

        // New pane 3: should NOT skip anything
        assert!(!loaded.should_skip_segment(3, 1));
        assert!(!loaded.should_skip_segment(3, 100));
    }

    // -- E2E Scenario 7: Checkpoint corruption recovery --

    #[test]
    fn e2e_corrupt_checkpoint_starts_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let cp_path = tmp.path().join("wa_checkpoint.json");

        // Write a valid checkpoint
        let panes = simulate_watcher_run(1000, &[1], 0, 50);
        CaptureCheckpoint::with_timestamp(panes, 1050)
            .save(&cp_path)
            .unwrap();

        // Corrupt it
        std::fs::write(&cp_path, "{ invalid json !!!").unwrap();

        // Loading should fail gracefully
        let result = CaptureCheckpoint::load(&cp_path);
        assert!(result.is_err());

        // Missing file should also fail gracefully
        let missing = tmp.path().join("nonexistent.json");
        assert!(CaptureCheckpoint::load(&missing).is_err());
    }

    // -- E2E Scenario 8: Backoff cap prevents unbounded delay --

    #[test]
    fn e2e_backoff_cap_under_sustained_crashes() {
        let mut det = CrashLoopDetector::new(CrashLoopConfig {
            crash_threshold: 3,
            window_secs: 3600, // 1-hour window
            initial_delay_ms: 100,
            max_delay_ms: 5_000,
            backoff_factor: 2.0,
        });

        // Simulate 20 consecutive crashes
        let mut max_delay = 0u64;
        for i in 0..20u64 {
            det.record_crash(1000 + i);
            let delay = det.next_delay_ms();
            max_delay = max_delay.max(delay);
        }

        // Delay should never exceed configured max
        assert!(
            max_delay <= 5_000,
            "max delay {max_delay}ms exceeded cap of 5000ms"
        );

        // Should be exactly at cap after enough crashes
        assert_eq!(det.next_delay_ms(), 5_000);
    }

    // ── br-ft-94cdu: crash bundle parse-drop counter ──

    #[test]
    fn read_optional_json_bundle_file_returns_none_when_absent_ft_94cdu() {
        // File-not-found is NOT a drop — it's legitimate absence.
        // The counter must NOT bump.
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        super::reset_crash_bundle_parse_drop_count_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle_path = dir.path().to_path_buf();
        let missing = bundle_path.join("does_not_exist.json");
        let result = super::read_optional_json_bundle_file::<super::CrashManifest>(
            &bundle_path,
            &missing,
            "manifest_read_fail",
            "manifest_parse_fail",
        );
        assert!(result.is_none());
        assert_eq!(
            super::crash_bundle_parse_drop_count(),
            0,
            "missing file must not bump parse-drop counter"
        );
    }

    #[test]
    fn read_optional_json_bundle_file_bumps_on_parse_fail_ft_94cdu() {
        // br-ft-94cdu: file present but malformed JSON triggers
        // the parse_fail phase. Counter bumps once.
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        super::reset_crash_bundle_parse_drop_count_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle_path = dir.path().to_path_buf();
        let path = bundle_path.join("manifest.json");
        std::fs::write(&path, b"{this is not valid json").unwrap();
        let result = super::read_optional_json_bundle_file::<super::CrashManifest>(
            &bundle_path,
            &path,
            "manifest_read_fail",
            "manifest_parse_fail",
        );
        assert!(result.is_none());
        assert_eq!(
            super::crash_bundle_parse_drop_count(),
            1,
            "malformed JSON must bump the parse-drop counter exactly once"
        );
    }

    #[test]
    fn read_optional_json_bundle_file_no_bump_on_well_formed_ft_94cdu() {
        // Sanity: a well-formed JSON file parses cleanly and does
        // NOT bump the counter.
        let _guard = CRASH_BUNDLE_PARSE_DROP_TEST_LOCK
            .lock()
            .expect("crash bundle parse-drop test lock");
        super::reset_crash_bundle_parse_drop_count_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle_path = dir.path().to_path_buf();
        let path = bundle_path.join("manifest.json");
        let manifest = super::CrashManifest {
            wa_version: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            files: vec!["crash_report.json".to_string()],
            has_health_snapshot: false,
            has_resize_forensics: false,
            has_environment_markers: false,
            bundle_size_bytes: 0,
        };
        std::fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
        let result = super::read_optional_json_bundle_file::<super::CrashManifest>(
            &bundle_path,
            &path,
            "manifest_read_fail",
            "manifest_parse_fail",
        );
        assert!(result.is_some());
        assert_eq!(super::crash_bundle_parse_drop_count(), 0);
    }
}
