//! Session data retention and cleanup.
//!
//! Implements retention policies for session persistence data to prevent
//! unbounded growth. Cleans up old sessions by age, count, and total size.
//!
//! # Cleanup order
//!
//! 1. Delete sessions older than `max_age_days` (skip active sessions)
//! 2. Delete excess closed sessions beyond `max_closed_sessions` (oldest first)
//! 3. If the schema-v40 exact logical retained-payload authority exceeds
//!    `max_total_size_mb`, delete oldest eligible closed sessions
//! 4. Clean orphaned data (lifecycle/checkpoint rows without a session and
//!    pane-state rows without a checkpoint)
//!
//! Cascade: session deletion cascades to checkpoints -> pane_state via FK.

#[cfg(test)]
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::checkpoint_witness::{
    MAX_CHECKPOINT_METADATA_BYTES, MAX_CHECKPOINT_SESSION_ID_BYTES,
    MAX_PERSISTED_CHECKPOINT_TEXT_BYTES, MAX_SESSION_HOST_ID_BYTES,
};
use crate::config::SessionRetentionConfig;

// Version 2 introduced a microsecond-resolution macOS process-start token.
// Version 3 replaced the wall-clock-derived macOS boot timestamp with the
// kernel boot-session UUID. Version 4 also fences Linux PID namespaces so a
// same-boot container cannot pronounce another namespace's owner dead.
// Version 5 adds an application-scoped persistent machine fence so two hosts
// with the same hostname cannot mistake different boot IDs for a local reboot.
// Older identities remain unknown rather than crossing any stronger fence.
const SESSION_HOST_IDENTITY_VERSION: u8 = 5;
const MAX_HOSTNAME_BYTES: usize = 255;
const MACHINE_FENCE_BYTES: usize = 64;
const BOOT_ID_BYTES: usize = 36;
const MAX_PROCESS_DOMAIN_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionHostIdentity {
    version: u8,
    hostname: String,
    machine_fence: String,
    boot_id: String,
    process_domain_id: String,
}

/// Stable ownership fence recorded on every session created by current code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionOwnerIdentity {
    pub(crate) host_id: String,
    pub(crate) pid: i64,
    pub(crate) process_start: i64,
}

/// Liveness classification for an unclean session owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UncleanSessionOwnerState {
    /// The exact host boot, PID, and process-start token are still present.
    Live,
    /// The same host has authoritatively proven that the owner incarnation ended.
    RecoveryCandidate,
    /// Legacy, foreign-host, malformed, or unobservable ownership; fail closed.
    Unknown,
}

trait SessionOwnerObserver {
    fn current_host(&self) -> Option<&SessionHostIdentity>;
    fn observe_process_start(&self, pid: u32) -> procinfo::ProcessStartTimeObservation;
}

struct SystemSessionOwnerObserver {
    current_host: Option<SessionHostIdentity>,
}

/// Reusable system observer so a restore/doctor scan resolves host boot
/// identity once instead of repeating platform syscalls for every session.
pub(crate) struct SessionOwnerClassifier {
    observer: SystemSessionOwnerObserver,
}

impl SessionOwnerClassifier {
    pub(crate) fn new() -> Self {
        Self {
            observer: SystemSessionOwnerObserver::new(),
        }
    }

    pub(crate) fn classify(
        &self,
        host_id: Option<&str>,
        owner_pid: Option<i64>,
        owner_process_start: Option<i64>,
        owner_heartbeat_at: Option<i64>,
    ) -> UncleanSessionOwnerState {
        classify_unclean_session_owner_with_observer(
            host_id,
            owner_pid,
            owner_process_start,
            owner_heartbeat_at,
            &self.observer,
        )
    }
}

impl SystemSessionOwnerObserver {
    fn new() -> Self {
        Self {
            current_host: current_session_host_identity(),
        }
    }
}

impl SessionOwnerObserver for SystemSessionOwnerObserver {
    fn current_host(&self) -> Option<&SessionHostIdentity> {
        self.current_host.as_ref()
    }

    fn observe_process_start(&self, pid: u32) -> procinfo::ProcessStartTimeObservation {
        procinfo::LocalProcessInfo::observe_process_start_time(pid)
    }
}

fn bounded_identity_component(raw: String, max_bytes: usize) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() || value.len() > max_bytes {
        return None;
    }
    Some(value.to_string())
}

fn is_canonical_machine_fence(value: &str) -> bool {
    value.len() == MACHINE_FENCE_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn normalized_boot_id(raw: String) -> Option<String> {
    let value = raw.as_bytes();
    if value.len() != BOOT_ID_BYTES
        || value.iter().enumerate().any(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte != b'-'
            } else {
                !byte.is_ascii_hexdigit()
            }
        })
        || value
            .iter()
            .filter(|byte| **byte != b'-')
            .all(|byte| *byte == b'0')
        || value
            .iter()
            .filter(|byte| **byte != b'-')
            .all(|byte| byte.eq_ignore_ascii_case(&b'f'))
    {
        return None;
    }
    Some(raw.to_ascii_lowercase())
}

fn current_session_host_identity() -> Option<SessionHostIdentity> {
    let hostname = hostname::get().ok()?.into_string().ok()?;
    let hostname = bounded_identity_component(hostname, MAX_HOSTNAME_BYTES)?;
    let machine_fence = procinfo::LocalProcessInfo::host_machine_fence()?;
    if !is_canonical_machine_fence(&machine_fence) {
        return None;
    }
    let boot_id = procinfo::LocalProcessInfo::host_boot_id()?;
    let boot_id = normalized_boot_id(boot_id)?;
    let process_domain_id = current_process_domain_id()?;
    let process_domain_id =
        bounded_identity_component(process_domain_id, MAX_PROCESS_DOMAIN_ID_BYTES)?;
    Some(SessionHostIdentity {
        version: SESSION_HOST_IDENTITY_VERSION,
        hostname,
        machine_fence,
        boot_id,
        process_domain_id,
    })
}

fn current_process_domain_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link("/proc/self/ns/pid")
            .ok()
            .map(|value| value.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(std::env::consts::OS.to_string())
    }
}

/// Capture the current process incarnation once for a snapshot-engine lifetime.
pub(crate) fn current_session_owner_identity() -> Option<SessionOwnerIdentity> {
    let host = current_session_host_identity()?;
    let host_id = serde_json::to_string(&host).ok()?;
    if host_id.len() > MAX_SESSION_HOST_ID_BYTES {
        return None;
    }
    let pid = std::process::id();
    let process_start = procinfo::LocalProcessInfo::process_start_time(pid)?;
    Some(SessionOwnerIdentity {
        host_id,
        pid: i64::from(pid),
        process_start: i64::try_from(process_start).ok()?,
    })
}

fn parse_session_host_identity(host_id: &str) -> Option<SessionHostIdentity> {
    if host_id.is_empty() || host_id.len() > MAX_SESSION_HOST_ID_BYTES {
        return None;
    }
    let mut identity: SessionHostIdentity = serde_json::from_str(host_id).ok()?;
    if identity.version != SESSION_HOST_IDENTITY_VERSION
        || identity.hostname.trim().is_empty()
        || identity.hostname.len() > MAX_HOSTNAME_BYTES
        || !is_canonical_machine_fence(&identity.machine_fence)
        || identity.process_domain_id.trim().is_empty()
        || identity.process_domain_id.len() > MAX_PROCESS_DOMAIN_ID_BYTES
    {
        return None;
    }
    identity.boot_id = normalized_boot_id(identity.boot_id)?;
    Some(identity)
}

fn classify_unclean_session_owner_with_observer(
    host_id: Option<&str>,
    owner_pid: Option<i64>,
    owner_process_start: Option<i64>,
    owner_heartbeat_at: Option<i64>,
    observer: &impl SessionOwnerObserver,
) -> UncleanSessionOwnerState {
    let (Some(host_id), Some(owner_pid), Some(owner_process_start), Some(owner_heartbeat_at)) =
        (host_id, owner_pid, owner_process_start, owner_heartbeat_at)
    else {
        return UncleanSessionOwnerState::Unknown;
    };
    let Ok(owner_pid) = u32::try_from(owner_pid) else {
        return UncleanSessionOwnerState::Unknown;
    };
    let Ok(owner_process_start) = u64::try_from(owner_process_start) else {
        return UncleanSessionOwnerState::Unknown;
    };
    if owner_heartbeat_at < 0 {
        return UncleanSessionOwnerState::Unknown;
    }
    if owner_pid == 0 || owner_process_start == 0 {
        return UncleanSessionOwnerState::Unknown;
    }
    let Some(stored_host) = parse_session_host_identity(host_id) else {
        return UncleanSessionOwnerState::Unknown;
    };
    let Some(current_host) = observer.current_host() else {
        return UncleanSessionOwnerState::Unknown;
    };
    // The v5 machine fence is the persistent host authority. Hostnames are
    // retained for diagnostics, but they are mutable configuration and may
    // change during one boot; treating a rename as a foreign host would strand
    // every earlier crash candidate in the fail-closed `Unknown` state.
    if stored_host.machine_fence != current_host.machine_fence {
        return UncleanSessionOwnerState::Unknown;
    }
    if stored_host.boot_id != current_host.boot_id {
        return UncleanSessionOwnerState::RecoveryCandidate;
    }
    if stored_host.process_domain_id != current_host.process_domain_id {
        return UncleanSessionOwnerState::Unknown;
    }
    match observer.observe_process_start(owner_pid) {
        procinfo::ProcessStartTimeObservation::Running(observed_start)
            if observed_start == owner_process_start =>
        {
            UncleanSessionOwnerState::Live
        }
        procinfo::ProcessStartTimeObservation::Running(_)
        | procinfo::ProcessStartTimeObservation::Absent => {
            UncleanSessionOwnerState::RecoveryCandidate
        }
        procinfo::ProcessStartTimeObservation::Unknown => UncleanSessionOwnerState::Unknown,
    }
}

// [ft-xcsm0 / ft-8nqx0 Phase 4] CleanupResult lifted to the audit-types
// leaf crate so the cleanup summary contract can be reviewed
// independently from the operational SQL pipeline below. Re-exported
// here so existing `crate::session_retention::CleanupResult` and
// `frankenterm_core::session_retention::CleanupResult` callers (and
// the proptest_session_retention.rs proptest) need zero edits.
pub use frankenterm_core_audit_types::session_retention_types::CleanupResult;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SizeCleanupOutcome {
    deleted: usize,
    measured_bytes: u64,
    deleted_bytes: u64,
    retained_bytes: u64,
    ineligible_shortfall_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetentionPhaseOutcome<T> {
    value: T,
    recovery_reconciliation_pending: bool,
}

impl<T> RetentionPhaseOutcome<T> {
    const fn ready(value: T) -> Self {
        Self {
            value,
            recovery_reconciliation_pending: false,
        }
    }

    const fn pending(value: T) -> Self {
        Self {
            value,
            recovery_reconciliation_pending: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OrphanCleanupOutcome {
    orphaned_restore_lifecycle_rows: usize,
    orphaned_checkpoints: usize,
    orphaned_pane_states: usize,
}

/// Finite phase in which session-cleanup completion became unobservable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCleanupIndeterminatePhase {
    /// The blocking cleanup task was admitted, but its terminal result was not
    /// observed. The closure may still be running or may already have committed.
    BlockingTaskSettlement,
    /// The cleanup SQL pipeline returned an error after execution began. Earlier
    /// independently committed phases may already be durable.
    CleanupExecution,
}

impl SessionCleanupIndeterminatePhase {
    const fn label(self) -> &'static str {
        match self {
            Self::BlockingTaskSettlement => "blocking_task_settlement",
            Self::CleanupExecution => "cleanup_execution",
        }
    }
}

impl std::fmt::Display for SessionCleanupIndeterminatePhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Finite asynchronous session-cleanup failure contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionCleanupError {
    /// The caller was already cancelled, proving no blocking cleanup handoff
    /// was admitted.
    #[error("session cleanup cancelled before blocking handoff")]
    CancelledBeforeHandoff,
    /// SQLite could not open the cleanup connection, so no cleanup SQL ran.
    #[error("session cleanup database open failed")]
    DatabaseOpen,
    /// The cleanup connection could not establish its required PRAGMAs, so the
    /// cleanup pipeline was not invoked.
    #[error("session cleanup database preparation failed")]
    DatabasePreparation,
    /// Cleanup may have durably changed SQLite state, but no authoritative
    /// completion receipt is available. Callers must reconcile and must not
    /// automatically retry.
    #[error(
        "session cleanup outcome is indeterminate during {phase}; reconcile durable state before retrying"
    )]
    IndeterminateCleanup {
        /// Finite observation-loss phase; never a database path or SQL string.
        phase: SessionCleanupIndeterminatePhase,
    },
}

impl SessionCleanupError {
    /// Whether durable state must be reconciled before any retry decision.
    #[must_use]
    pub const fn requires_reconciliation(self) -> bool {
        matches!(self, Self::IndeterminateCleanup { .. })
    }
}

/// Run the full session cleanup pipeline.
///
/// Designed to be called from `runtime_async::spawn_blocking` since all
/// operations are synchronous SQLite calls.
///
/// # Errors
/// Returns error if database operations fail.
pub fn cleanup_sessions(
    conn: &Connection,
    config: &SessionRetentionConfig,
) -> Result<CleanupResult, rusqlite::Error> {
    let mut result = CleanupResult::default();
    // Host, boot, and process-domain identity cannot legitimately change
    // within one cleanup invocation. Resolve that platform state once for all
    // enabled policy phases; PID start observations remain fresh at each
    // candidate classification.
    let owner_observer =
        (config.max_age_days > 0 || config.max_closed_sessions > 0 || config.max_total_size_mb > 0)
            .then(SystemSessionOwnerObserver::new);

    // 1. Delete sessions older than max_age_days
    if config.max_age_days > 0 {
        let phase = delete_sessions_by_age_phase_with_observer(
            conn,
            config.max_age_days,
            owner_observer
                .as_ref()
                .ok_or_else(|| recovery_authority_error("age retention has no owner observer"))?,
        )?;
        result.deleted_by_age = phase.value;
        result.recovery_reconciliation_pending |= phase.recovery_reconciliation_pending;
        if result.deleted_by_age > 0 {
            info!(
                deleted = result.deleted_by_age,
                max_age_days = config.max_age_days,
                "Cleaned up old sessions by age"
            );
        }
    }

    // 2. Delete excess closed sessions
    if config.max_closed_sessions > 0 {
        let phase = delete_excess_closed_sessions_phase_with_observer(
            conn,
            config.max_closed_sessions,
            owner_observer
                .as_ref()
                .ok_or_else(|| recovery_authority_error("count retention has no owner observer"))?,
        )?;
        result.deleted_by_count = phase.value;
        result.recovery_reconciliation_pending |= phase.recovery_reconciliation_pending;
        if result.deleted_by_count > 0 {
            info!(
                deleted = result.deleted_by_count,
                max = config.max_closed_sessions,
                "Cleaned up excess closed sessions"
            );
        }
    }

    // 3. Delete by size budget
    if config.max_total_size_mb > 0 {
        let phase = delete_sessions_by_size_phase_with_observer(
            conn,
            config.max_total_size_mb,
            owner_observer
                .as_ref()
                .ok_or_else(|| recovery_authority_error("size retention has no owner observer"))?,
        )?;
        let size_outcome = phase.value;
        result.recovery_reconciliation_pending |= phase.recovery_reconciliation_pending;
        result.deleted_by_size = size_outcome.deleted;
        result.size_measured_bytes = size_outcome.measured_bytes;
        result.size_deleted_bytes = size_outcome.deleted_bytes;
        result.size_retained_bytes = size_outcome.retained_bytes;
        result.size_ineligible_shortfall_bytes = size_outcome.ineligible_shortfall_bytes;
        if size_outcome.ineligible_shortfall_bytes > 0 {
            warn!(
                deleted = size_outcome.deleted,
                measured_bytes = size_outcome.measured_bytes,
                deleted_bytes = size_outcome.deleted_bytes,
                retained_bytes = size_outcome.retained_bytes,
                shortfall_bytes = size_outcome.ineligible_shortfall_bytes,
                max_mb = config.max_total_size_mb,
                "Session size budget remains above its configured limit because no more closed sessions are eligible"
            );
        } else if size_outcome.deleted > 0 {
            info!(
                deleted = size_outcome.deleted,
                measured_bytes = size_outcome.measured_bytes,
                deleted_bytes = size_outcome.deleted_bytes,
                retained_bytes = size_outcome.retained_bytes,
                max_mb = config.max_total_size_mb,
                "Applied session size budget"
            );
        }
    }

    // 4. Clean orphaned data
    let orphaned = cleanup_orphaned_data(conn)?;
    result.orphaned_restore_lifecycle_rows = orphaned.orphaned_restore_lifecycle_rows;
    result.orphaned_checkpoints = orphaned.orphaned_checkpoints;
    result.orphaned_pane_states = orphaned.orphaned_pane_states;
    if orphaned != OrphanCleanupOutcome::default() {
        warn!(
            orphaned_restore_lifecycle_rows = orphaned.orphaned_restore_lifecycle_rows,
            orphaned_checkpoints = orphaned.orphaned_checkpoints,
            orphaned_pane_states = orphaned.orphaned_pane_states,
            "Cleaned up orphaned session data"
        );
    }

    // Deliberately issue no VACUUM or incremental_vacuum operation here. Under
    // FrankenTerm's normal `auto_vacuum=NONE` database policy, freed pages stay
    // on SQLite's freelist for reuse. An externally-created database configured
    // with `auto_vacuum=FULL` may still relocate and truncate pages as part of a
    // committing DELETE; absence of an explicit VACUUM is not a universal
    // no-compaction guarantee for that non-default mode. The runtime's ordinary
    // online-maintenance lane owns PASSIVE WAL checkpointing and PRAGMA optimize.
    // Explicit operator-requested physical compaction remains a separate,
    // disruptive maintenance operation.
    Ok(result)
}

fn u64_to_sqlite_integer(value: u64) -> Result<i64, rusqlite::Error> {
    i64::try_from(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn clean_authority_error(error: crate::session_restore::RestoreError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error.to_string())))
}

fn begin_retention_transaction(conn: &Connection) -> rusqlite::Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
}

#[derive(Debug)]
struct SessionRetentionAuthority {
    session_id: String,
    shutdown_clean: i64,
    clean_checkpoint_id: Option<i64>,
    host_id: Option<String>,
    owner_pid: Option<i64>,
    owner_process_start: Option<i64>,
    owner_heartbeat_at: Option<i64>,
    recovery_acknowledged_at: Option<i64>,
}

// One retention transaction performs at most one of these bounded advancement
// phases before returning `Pending`. Population executes one ordered query,
// at most 64 idempotent inserts, and one cursor update. Reconciliation invokes
// the canonical restore loader at most four times; that loader preflights and
// admits at most MAX_PERSISTED_CHECKPOINT_TEXT_BYTES per candidate. Selection
// reads at most 64 indexed authority rows and performs at most two indexed
// eligibility queries per row. The cooperative wall budget is checked between
// canonical candidates; one already-bounded loader call always reaches an
// authoritative result so an overloaded host cannot livelock at row zero.
const RECOVERY_AUTHORITY_POPULATION_BATCH_ROWS: usize = 64;
const RECOVERY_AUTHORITY_RECONCILE_BATCH_ROWS: usize = 4;
const RECOVERY_SELECTION_SCAN_BATCH_ROWS: usize = 64;
const RECOVERY_AUTHORITY_RECONCILE_MAX_ADMITTED_BYTES: usize =
    RECOVERY_AUTHORITY_RECONCILE_BATCH_ROWS * MAX_PERSISTED_CHECKPOINT_TEXT_BYTES;
const RECOVERY_AUTHORITY_RECONCILE_WALL_BUDGET: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProtectedRecoverySelection {
    Ready(Option<ProtectedRecoveryPoint>),
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtectedRecoveryPoint {
    session_id: String,
    checkpoint_id: i64,
}

#[derive(Debug)]
struct RecoverySelectionState {
    mutation_generation: i64,
    population_after_rowid: Option<i64>,
    population_complete: bool,
    scan_generation: i64,
    scan_after_checkpoint_id: Option<i64>,
    scan_after_session_id: Option<String>,
    scan_complete: bool,
}

fn load_recovery_selection_state(
    conn: &Connection,
) -> Result<RecoverySelectionState, rusqlite::Error> {
    conn.query_row(
        "SELECT mutation_generation,
                population_after_rowid,
                population_complete,
                scan_generation,
                scan_after_checkpoint_id,
                scan_after_session_id,
                scan_complete
         FROM session_recovery_selection
         WHERE singleton = 1",
        [],
        |row| {
            Ok(RecoverySelectionState {
                mutation_generation: row.get(0)?,
                population_after_rowid: row.get(1)?,
                population_complete: row.get(2)?,
                scan_generation: row.get(3)?,
                scan_after_checkpoint_id: row.get(4)?,
                scan_after_session_id: row.get(5)?,
                scan_complete: row.get(6)?,
            })
        },
    )
}

fn advance_recovery_authority_population(
    conn: &Connection,
    state: &RecoverySelectionState,
) -> Result<bool, rusqlite::Error> {
    if state.population_complete {
        return Ok(false);
    }
    let query_limit = RECOVERY_AUTHORITY_POPULATION_BATCH_ROWS.saturating_add(1);
    let max_session_id_bytes = i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX);
    let query_limit = i64::try_from(query_limit).unwrap_or(i64::MAX);
    let sessions: Vec<(i64, Option<String>)> =
        if let Some(after_rowid) = state.population_after_rowid {
            let mut statement = conn.prepare(
                "SELECT rowid,
                        CASE
                            WHEN typeof(session_id) = 'text'
                             AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?2
                            THEN session_id
                        END
                 FROM mux_sessions
                 WHERE rowid > ?1
                 ORDER BY rowid ASC
                 LIMIT ?3",
            )?;
            statement
                .query_map(
                    rusqlite::params![after_rowid, max_session_id_bytes, query_limit],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<Result<_, _>>()?
        } else {
            let mut statement = conn.prepare(
                "SELECT rowid,
                        CASE
                            WHEN typeof(session_id) = 'text'
                             AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?1
                            THEN session_id
                        END
                 FROM mux_sessions
                 ORDER BY rowid ASC
                 LIMIT ?2",
            )?;
            statement
                .query_map(
                    rusqlite::params![max_session_id_bytes, query_limit],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<Result<_, _>>()?
        };
    let population_complete = sessions.len() <= RECOVERY_AUTHORITY_POPULATION_BATCH_ROWS;
    let batch_len = sessions.len().min(RECOVERY_AUTHORITY_POPULATION_BATCH_ROWS);
    let batch = &sessions[..batch_len];
    for (_, session_id) in batch {
        let Some(session_id) = session_id else {
            continue;
        };
        conn.execute(
            "INSERT OR IGNORE INTO session_recovery_usability (
                 session_id, state, validated_checkpoint_id, dirty_generation
             ) VALUES (?1, 'dirty', NULL, ?2)",
            rusqlite::params![session_id, state.mutation_generation],
        )?;
    }
    let next_cursor = batch
        .last()
        .map(|(rowid, _)| *rowid)
        .or(state.population_after_rowid);
    let changed = conn.execute(
        "UPDATE session_recovery_selection
         SET population_after_rowid = ?1,
             population_complete = ?2
         WHERE singleton = 1",
        rusqlite::params![next_cursor, population_complete],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(!batch.is_empty() || !population_complete)
}

fn advance_dirty_recovery_authority(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let dirty_rows: Vec<(i64, Option<String>, i64)> = {
        let mut statement = conn.prepare(
            "SELECT rowid,
                    CASE
                        WHEN typeof(session_id) = 'text'
                         AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?1
                        THEN session_id
                    END,
                    dirty_generation
             FROM session_recovery_usability
                  INDEXED BY idx_session_recovery_usability_dirty
             WHERE state = 'dirty'
             ORDER BY dirty_generation ASC, session_id ASC
             LIMIT ?2",
        )?;
        statement
            .query_map(
                rusqlite::params![
                    i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
                    i64::try_from(RECOVERY_AUTHORITY_RECONCILE_BATCH_ROWS).unwrap_or(i64::MAX),
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<Result<_, _>>()?
    };
    let started = Instant::now();
    let mut processed = 0_usize;
    for (authority_rowid, session_id, dirty_generation) in &dirty_rows {
        if processed > 0 && started.elapsed() >= RECOVERY_AUTHORITY_RECONCILE_WALL_BUDGET {
            break;
        }
        let checkpoint_id = session_id
            .as_deref()
            .map(|session_id| {
                crate::session_restore::usable_recovery_checkpoint_id_from_conn(conn, session_id)
                    .map_err(clean_authority_error)
            })
            .transpose()?
            .flatten();
        let (state, checkpoint_id) = checkpoint_id.map_or(("unusable", None), |checkpoint_id| {
            ("usable", Some(checkpoint_id))
        });
        let changed = conn.execute(
            "UPDATE session_recovery_usability
             SET state = ?3, validated_checkpoint_id = ?4
             WHERE rowid = ?1
               AND state = 'dirty'
               AND dirty_generation = ?2",
            rusqlite::params![authority_rowid, dirty_generation, state, checkpoint_id],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
        processed = processed.saturating_add(1);
    }
    debug_assert!(
        RECOVERY_AUTHORITY_RECONCILE_MAX_ADMITTED_BYTES >= MAX_PERSISTED_CHECKPOINT_TEXT_BYTES
    );
    Ok(processed < dirty_rows.len() || dirty_rows.len() == RECOVERY_AUTHORITY_RECONCILE_BATCH_ROWS)
}

fn invalidate_recovery_authority_row(
    conn: &Connection,
    session_id: &str,
) -> Result<(), rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE session_recovery_selection
         SET mutation_generation = mutation_generation + 1,
             scan_generation = 0,
             scan_after_checkpoint_id = NULL,
             scan_after_session_id = NULL,
             protected_session_id = NULL,
             protected_checkpoint_id = NULL,
             scan_complete = 0
         WHERE singleton = 1
           AND mutation_generation < 9223372036854775807",
        [],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    let changed = conn.execute(
        "UPDATE session_recovery_usability
         SET state = 'dirty',
             validated_checkpoint_id = NULL,
             dirty_generation = (
                 SELECT mutation_generation
                 FROM session_recovery_selection
                 WHERE singleton = 1
             )
         WHERE session_id = ?1",
        [session_id],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

fn verify_protected_recovery_checkpoint(
    conn: &Connection,
    session_id: &str,
    expected_checkpoint_id: i64,
) -> Result<bool, rusqlite::Error> {
    let observed =
        crate::session_restore::usable_recovery_checkpoint_id_from_conn(conn, session_id)
            .map_err(clean_authority_error)?;
    Ok(observed == Some(expected_checkpoint_id))
}

fn recovery_session_is_protectable(
    conn: &Connection,
    session_id: &str,
    observer: &impl SessionOwnerObserver,
) -> Result<bool, rusqlite::Error> {
    let authority = conn
        .query_row(
            "SELECT shutdown_clean,
                    CASE
                        WHEN typeof(host_id) = 'text'
                         AND length(CAST(host_id AS BLOB)) <= ?2
                        THEN host_id
                    END,
                    owner_pid,
                    owner_process_start,
                    owner_heartbeat_at,
                    recovery_acknowledged_at
             FROM mux_sessions
             WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        shutdown_clean,
        host_id,
        owner_pid,
        owner_process_start,
        owner_heartbeat_at,
        recovery_acknowledged_at,
    )) = authority
    else {
        return Err(recovery_authority_error(
            "recovery usability authority has no persisted session",
        ));
    };
    if shutdown_clean != 0 || recovery_acknowledged_at.is_some() {
        return Ok(false);
    }
    if classify_unclean_session_owner_with_observer(
        host_id.as_deref(),
        owner_pid,
        owner_process_start,
        owner_heartbeat_at,
        observer,
    ) != UncleanSessionOwnerState::RecoveryCandidate
    {
        return Ok(false);
    }
    Ok(!has_unresolved_restore_intent(conn, session_id)?)
}

fn reset_recovery_selection_scan(conn: &Connection) -> Result<(), rusqlite::Error> {
    let changed = conn.execute(
        "UPDATE session_recovery_selection
         SET scan_generation = mutation_generation,
             scan_after_checkpoint_id = NULL,
             scan_after_session_id = NULL,
             protected_session_id = NULL,
             protected_checkpoint_id = NULL,
             scan_complete = 0
         WHERE singleton = 1",
        [],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    Ok(())
}

fn newest_usable_recovery_session(
    conn: &Transaction<'_>,
    observer: &impl SessionOwnerObserver,
) -> Result<ProtectedRecoverySelection, rusqlite::Error> {
    let mut state = load_recovery_selection_state(conn)?;
    if advance_recovery_authority_population(conn, &state)? {
        return Ok(ProtectedRecoverySelection::Pending);
    }
    if advance_dirty_recovery_authority(conn)? {
        return Ok(ProtectedRecoverySelection::Pending);
    }

    state = load_recovery_selection_state(conn)?;
    // Owner liveness is external to SQLite: a process can die without any
    // trigger advancing mutation_generation. A completed scan is therefore a
    // durable receipt for one cleanup invocation, never a reusable liveness
    // verdict. Incomplete scans retain their cursor; completed scans restart.
    if state.scan_generation != state.mutation_generation || state.scan_complete {
        reset_recovery_selection_scan(conn)?;
        state = load_recovery_selection_state(conn)?;
    }

    let selection_limit = i64::try_from(RECOVERY_SELECTION_SCAN_BATCH_ROWS).unwrap_or(i64::MAX);
    let usable_rows: Vec<(String, i64)> = match (
        state.scan_after_checkpoint_id,
        state.scan_after_session_id.as_deref(),
    ) {
        (Some(after_checkpoint_id), Some(after_session_id)) => {
            let mut statement = conn.prepare(
                "SELECT session_id, validated_checkpoint_id
                 FROM session_recovery_usability
                      INDEXED BY idx_session_recovery_usability_state
                 WHERE state = 'usable'
                   AND validated_checkpoint_id <= ?1
                   AND (
                       validated_checkpoint_id < ?1
                       OR (validated_checkpoint_id = ?1 AND session_id > ?2)
                   )
                 ORDER BY validated_checkpoint_id DESC, session_id ASC
                 LIMIT ?3",
            )?;
            statement
                .query_map(
                    rusqlite::params![after_checkpoint_id, after_session_id, selection_limit],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<Result<_, _>>()?
        }
        (None, None) => {
            let mut statement = conn.prepare(
                "SELECT session_id, validated_checkpoint_id
                 FROM session_recovery_usability
                      INDEXED BY idx_session_recovery_usability_state
                 WHERE state = 'usable'
                 ORDER BY validated_checkpoint_id DESC, session_id ASC
                 LIMIT ?1",
            )?;
            statement
                .query_map([selection_limit], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?
        }
        _ => {
            return Err(recovery_authority_error(
                "recovery selection cursor identity is partial",
            ));
        }
    };

    let mut protected = None;
    for (session_id, checkpoint_id) in &usable_rows {
        if recovery_session_is_protectable(conn, session_id, observer)? {
            protected = Some((session_id.as_str(), *checkpoint_id));
            break;
        }
    }

    if let Some((session_id, checkpoint_id)) = protected {
        if !verify_protected_recovery_checkpoint(conn, session_id, checkpoint_id)? {
            invalidate_recovery_authority_row(conn, session_id)?;
            return Ok(ProtectedRecoverySelection::Pending);
        }
        let changed = conn.execute(
            "UPDATE session_recovery_selection
             SET protected_session_id = ?1,
                 protected_checkpoint_id = ?2,
                 scan_complete = 1
             WHERE singleton = 1
               AND scan_generation = mutation_generation",
            rusqlite::params![session_id, checkpoint_id],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::StatementChangedRows(changed));
        }
        return Ok(ProtectedRecoverySelection::Ready(Some(
            ProtectedRecoveryPoint {
                session_id: session_id.to_string(),
                checkpoint_id,
            },
        )));
    }

    let scan_complete = usable_rows.len() < RECOVERY_SELECTION_SCAN_BATCH_ROWS;
    let last = usable_rows.last();
    let changed = conn.execute(
        "UPDATE session_recovery_selection
         SET scan_after_checkpoint_id = ?1,
             scan_after_session_id = ?2,
             protected_session_id = NULL,
             protected_checkpoint_id = NULL,
             scan_complete = ?3
         WHERE singleton = 1
           AND scan_generation = mutation_generation",
        rusqlite::params![
            last.map(|(_, checkpoint_id)| *checkpoint_id)
                .or(state.scan_after_checkpoint_id),
            last.map(|(session_id, _)| session_id.as_str())
                .or(state.scan_after_session_id.as_deref()),
            scan_complete,
        ],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    if scan_complete {
        Ok(ProtectedRecoverySelection::Ready(None))
    } else {
        Ok(ProtectedRecoverySelection::Pending)
    }
}

fn session_is_deletion_eligible(
    conn: &Connection,
    candidate: &SessionRetentionAuthority,
    protected_recovery_point: Option<&ProtectedRecoveryPoint>,
    observer: &impl SessionOwnerObserver,
) -> Result<bool, rusqlite::Error> {
    let owner_state = classify_unclean_session_owner_with_observer(
        candidate.host_id.as_deref(),
        candidate.owner_pid,
        candidate.owner_process_start,
        candidate.owner_heartbeat_at,
        observer,
    );
    if owner_state == UncleanSessionOwnerState::Live
        || has_unresolved_restore_intent(conn, &candidate.session_id)?
    {
        return Ok(false);
    }
    let clean_authority = crate::session_restore::assess_clean_authority(
        conn,
        &candidate.session_id,
        candidate.shutdown_clean,
        candidate.clean_checkpoint_id,
    )
    .map_err(clean_authority_error)?;
    if clean_authority {
        return Ok(true);
    }
    if candidate.shutdown_clean != 0 {
        // A claimed-clean row whose exact receipt does not verify is corrupt
        // authority, not an ordinary crash candidate.
        return Ok(false);
    }
    if owner_state != UncleanSessionOwnerState::RecoveryCandidate {
        return Ok(false);
    }
    if candidate
        .recovery_acknowledged_at
        .is_some_and(|acknowledged_at| acknowledged_at < 0)
    {
        return Ok(false);
    }
    if candidate.recovery_acknowledged_at.is_some() {
        return Ok(true);
    }
    let usability = conn
        .query_row(
            "SELECT state, validated_checkpoint_id
             FROM session_recovery_usability
             WHERE session_id = ?1",
            [&candidate.session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .optional()?;
    match usability {
        // If no candidate was protectable in this transaction, retain every
        // unacknowledged session, including corrupt/unusable rows.
        Some((state, Some(candidate_checkpoint_id))) if state == "usable" => {
            if candidate_checkpoint_id <= 0 {
                return Err(recovery_authority_error(
                    "recovery usability authority has an invalid checkpoint identity",
                ));
            }
            let Some(protected) = protected_recovery_point else {
                return Ok(false);
            };
            // Liveness can change after an earlier cursor batch. Even with a
            // valid older protected point, a newly dead newer usable session
            // must survive until the next completed scan promotes it.
            Ok(
                candidate.session_id.as_str() != protected.session_id.as_str()
                    && candidate_checkpoint_id < protected.checkpoint_id,
            )
        }
        Some((state, None)) if state == "unusable" => Ok(protected_recovery_point.is_some()),
        Some((state, None)) if state == "dirty" => Ok(false),
        None => Ok(false),
        Some(_) => Err(recovery_authority_error(
            "recovery usability authority has an invalid state",
        )),
    }
}

fn recovery_authority_error(message: &'static str) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(message)))
}

fn set_recovery_acknowledgement_with_observer(
    conn: &Connection,
    session_id: &str,
    acknowledged_at: Option<u64>,
    observer: &impl SessionOwnerObserver,
) -> Result<(), rusqlite::Error> {
    if session_id.is_empty() || session_id.len() > MAX_CHECKPOINT_SESSION_ID_BYTES {
        return Err(recovery_authority_error(
            "session recovery acknowledgement rejected an invalid selector",
        ));
    }
    let acknowledged_at = acknowledged_at.map(u64_to_sqlite_integer).transpose()?;
    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    let row = tx
        .query_row(
            "SELECT shutdown_clean,
                    CASE
                        WHEN typeof(host_id) = 'text'
                         AND length(CAST(host_id AS BLOB)) <= ?2
                        THEN host_id
                    END,
                    owner_pid,
                    owner_process_start,
                    owner_heartbeat_at
             FROM mux_sessions
             WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((shutdown_clean, host_id, owner_pid, owner_process_start, owner_heartbeat_at)) = row
    else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    if shutdown_clean != 0 {
        return Err(recovery_authority_error(
            "session recovery acknowledgement requires an unclean session",
        ));
    }
    let owner_state = classify_unclean_session_owner_with_observer(
        host_id.as_deref(),
        owner_pid,
        owner_process_start,
        owner_heartbeat_at,
        observer,
    );
    if acknowledged_at.is_some() && owner_state != UncleanSessionOwnerState::RecoveryCandidate {
        return Err(recovery_authority_error(
            "session recovery acknowledgement requires a proven-dead owner",
        ));
    }
    let changed = tx.execute(
        "UPDATE mux_sessions
         SET recovery_acknowledged_at = ?2
         WHERE session_id = ?1 AND shutdown_clean = 0",
        rusqlite::params![session_id, acknowledged_at],
    )?;
    if changed != 1 {
        return Err(rusqlite::Error::StatementChangedRows(changed));
    }
    tx.commit()
}

/// Authorize retention cleanup of one proven-dead recovery candidate.
///
/// Live, foreign-host, legacy, and otherwise unobservable owners are rejected.
pub fn acknowledge_recovery_session(
    conn: &Connection,
    session_id: &str,
    acknowledged_at: u64,
) -> Result<(), rusqlite::Error> {
    set_recovery_acknowledgement_with_observer(
        conn,
        session_id,
        Some(acknowledged_at),
        &SystemSessionOwnerObserver::new(),
    )
}

/// Clear an earlier recovery-cleanup acknowledgement, preserving the session.
pub fn preserve_recovery_session(
    conn: &Connection,
    session_id: &str,
) -> Result<(), rusqlite::Error> {
    set_recovery_acknowledgement_with_observer(
        conn,
        session_id,
        None,
        &SystemSessionOwnerObserver::new(),
    )
}

/// Return whether a session has any restore attempt that lacks an explicit
/// durable `resolved` lifecycle state. The metadata branch preserves fail-safe
/// behavior for v37-era intent rows written under the overloaded
/// `restore_receipt` role before the schema gained that type. Neither a linked
/// outcome nor a later snapshot implies resolution: retrying external mux
/// mutations without reconciliation can duplicate tabs, panes, or processes.
pub(crate) fn has_unresolved_restore_intent(
    conn: &Connection,
    session_id: &str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS (
             SELECT 1
             FROM restore_attempt_lifecycle AS lifecycle
             WHERE lifecycle.session_id = ?1
               AND CASE
                       WHEN typeof(lifecycle.status) = 'text'
                        AND lifecycle.status = 'resolved'
                       THEN 0
                       ELSE 1
                   END = 1
         ) OR EXISTS (
             SELECT 1
             FROM session_checkpoints AS intent
             WHERE intent.session_id = ?1
               AND intent.checkpoint_role = 'restore_intent'
               AND NOT EXISTS (
                   SELECT 1
                   FROM restore_attempt_lifecycle AS lifecycle
                   WHERE lifecycle.intent_checkpoint_id = intent.id
                     AND lifecycle.session_id = intent.session_id
               )
         ) OR EXISTS (
             SELECT 1
             FROM session_checkpoints AS intent
             WHERE intent.session_id = ?1
               AND intent.checkpoint_role = 'restore_receipt'
               AND CASE
                       WHEN typeof(intent.metadata_json) != 'text'
                         OR length(CAST(intent.metadata_json AS BLOB)) > ?2
                       THEN 1
                       WHEN NOT json_valid(intent.metadata_json)
                       THEN 1
                       WHEN json_extract(
                           intent.metadata_json,
                           '$.restore_attempt.phase'
                       ) = 'outcome'
                       THEN 0
                       WHEN json_type(
                               intent.metadata_json,
                               '$.restore_attempt'
                            ) IS NULL
                        AND json_type(
                               intent.metadata_json,
                               '$.old_to_new'
                            ) = 'object'
                       THEN 0
                       ELSE 1
                   END = 1
               AND NOT EXISTS (
                   SELECT 1
                   FROM restore_attempt_lifecycle AS lifecycle
                   WHERE lifecycle.intent_checkpoint_id = intent.id
                     AND lifecycle.session_id = intent.session_id
               )
         )",
        rusqlite::params![
            session_id,
            i64::try_from(MAX_CHECKPOINT_METADATA_BYTES).unwrap_or(i64::MAX),
        ],
        |row| row.get::<_, bool>(0),
    )
}

/// Fail closed when unaudited authority-table triggers could invalidate direct
/// SQLite row-count receipts or cached recovery usability. Migration
/// validation pins the v40 retained-size and v44 recovery-usability bodies;
/// cleanup depends on both before any destructive decision.
///
/// Current schema requires 27 audited triggers: 15 retained-size guards and 12
/// recovery invalidators. SQLite identifiers are case-insensitive while both
/// schema catalogs preserve the spelling used by `CREATE TRIGGER`, so
/// comparisons use `NOCASE`.
///
/// # Errors
///
/// Returns the underlying catalog-query error, or a fail-closed conversion
/// error when a canonical trigger is missing or any unaudited trigger targets
/// an authority table.
pub(crate) fn ensure_session_authority_tables_have_no_unaudited_triggers(
    conn: &Connection,
) -> Result<(), rusqlite::Error> {
    crate::storage::migrations::validate_session_retained_size_mutation_schema(conn).map_err(
        |error| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
                "session authority mutation requires the exact canonical schema: {error}"
            ))))
        },
    )
}

/// Delete eligible closed or proven-dead recovery sessions older than
/// `max_age_days`.
///
/// Exact live owners override every age policy. Unclean sessions are eligible
/// only after owner-death fencing, and the newest usable recovery point remains
/// protected unless an explicit acknowledgement authorized its deletion.
#[cfg(test)]
fn delete_sessions_by_age(conn: &Connection, max_age_days: u64) -> Result<usize, rusqlite::Error> {
    let observer = SystemSessionOwnerObserver::new();
    delete_sessions_by_age_with_observer(conn, max_age_days, &observer)
}

#[cfg(test)]
fn delete_sessions_by_age_with_observer(
    conn: &Connection,
    max_age_days: u64,
    observer: &impl SessionOwnerObserver,
) -> Result<usize, rusqlite::Error> {
    Ok(delete_sessions_by_age_phase_with_observer(conn, max_age_days, observer)?.value)
}

fn delete_sessions_by_age_phase_with_observer(
    conn: &Connection,
    max_age_days: u64,
    observer: &impl SessionOwnerObserver,
) -> Result<RetentionPhaseOutcome<usize>, rusqlite::Error> {
    delete_sessions_by_age_phase_at_with_observer(conn, max_age_days, observer, epoch_ms())
}

fn delete_sessions_by_age_phase_at_with_observer(
    conn: &Connection,
    max_age_days: u64,
    observer: &impl SessionOwnerObserver,
    now_ms: u64,
) -> Result<RetentionPhaseOutcome<usize>, rusqlite::Error> {
    let cutoff_ms = now_ms.saturating_sub(max_age_days.saturating_mul(86_400_000));
    // An unrepresentable future cutoff must fail without deleting anything;
    // clamping to i64::MAX would make nearly every closed session eligible.
    let cutoff_ms = u64_to_sqlite_integer(cutoff_ms)?;

    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    let protected_recovery_session = match newest_usable_recovery_session(&tx, observer)? {
        ProtectedRecoverySelection::Ready(session_id) => session_id,
        ProtectedRecoverySelection::Pending => {
            tx.commit()?;
            return Ok(RetentionPhaseOutcome::pending(0));
        }
    };
    let candidates: Vec<SessionRetentionAuthority> = {
        let mut stmt = tx.prepare(
            "SELECT session_id,
                    shutdown_clean,
                    clean_checkpoint_id,
                    CASE
                        WHEN typeof(host_id) = 'text'
                         AND length(CAST(host_id AS BLOB)) <= ?3
                        THEN host_id
                    END,
                    owner_pid,
                    owner_process_start,
                    owner_heartbeat_at,
                    recovery_acknowledged_at
             FROM mux_sessions
             WHERE MAX(
                       created_at,
                       COALESCE(last_checkpoint_at, created_at),
                       COALESCE(
                           (SELECT MAX(c.checkpoint_at)
                            FROM session_checkpoints c
                            WHERE c.session_id = mux_sessions.session_id),
                           created_at
                       )
                   ) < ?1
               AND typeof(session_id) = 'text'
               AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?2
             ORDER BY session_id ASC",
        )?;
        stmt.query_map(
            rusqlite::params![
                cutoff_ms,
                i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
            ],
            |row| {
                Ok(SessionRetentionAuthority {
                    session_id: row.get(0)?,
                    shutdown_clean: row.get(1)?,
                    clean_checkpoint_id: row.get(2)?,
                    host_id: row.get(3)?,
                    owner_pid: row.get(4)?,
                    owner_process_start: row.get(5)?,
                    owner_heartbeat_at: row.get(6)?,
                    recovery_acknowledged_at: row.get(7)?,
                })
            },
        )?
        .collect::<Result<_, _>>()?
    };
    let mut deleted = 0usize;
    for candidate in candidates {
        if !session_is_deletion_eligible(
            &tx,
            &candidate,
            protected_recovery_session.as_ref(),
            observer,
        )? {
            continue;
        }
        let affected = tx.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [&candidate.session_id],
        )?;
        if affected != 1 {
            return Err(rusqlite::Error::StatementChangedRows(affected));
        }
        deleted = deleted.saturating_add(1);
    }
    tx.commit()?;
    Ok(RetentionPhaseOutcome::ready(deleted))
}

/// Delete excess closed sessions, keeping the most recent `max_count`.
#[cfg(test)]
fn delete_excess_closed_sessions(
    conn: &Connection,
    max_count: usize,
) -> Result<usize, rusqlite::Error> {
    let observer = SystemSessionOwnerObserver::new();
    delete_excess_closed_sessions_with_observer(conn, max_count, &observer)
}

#[cfg(test)]
fn retention_phase_test_step_budget(conn: &Connection) -> Result<usize, rusqlite::Error> {
    let row_count: i64 = conn.query_row(
        "SELECT MAX(
             (SELECT COUNT(*) FROM mux_sessions),
             (SELECT COUNT(*) FROM session_recovery_usability)
         )",
        [],
        |row| row.get(0),
    )?;
    let row_count = usize::try_from(row_count)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, row_count))?;

    // Population, dirty-row reconciliation, and final candidate selection
    // each make durable progress over a finite row set. Three steps per row
    // plus terminal empty-batch checks is therefore a deliberately loose but
    // history-derived upper bound that also covers wall-budget batches that
    // settle only one canonical recovery candidate at a time.
    Ok(row_count.saturating_mul(3).saturating_add(6))
}

#[cfg(test)]
fn delete_excess_closed_sessions_with_observer(
    conn: &Connection,
    max_count: usize,
    observer: &impl SessionOwnerObserver,
) -> Result<usize, rusqlite::Error> {
    let step_budget = retention_phase_test_step_budget(conn)?;
    for _ in 0..step_budget {
        let outcome = delete_excess_closed_sessions_phase_with_observer(conn, max_count, observer)?;
        if !outcome.recovery_reconciliation_pending {
            return Ok(outcome.value);
        }
    }
    panic!("count retention did not converge within {step_budget} bounded recovery steps");
}

fn delete_excess_closed_sessions_phase_with_observer(
    conn: &Connection,
    max_count: usize,
    observer: &impl SessionOwnerObserver,
) -> Result<RetentionPhaseOutcome<usize>, rusqlite::Error> {
    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    let protected_recovery_session = match newest_usable_recovery_session(&tx, observer)? {
        ProtectedRecoverySelection::Ready(session_id) => session_id,
        ProtectedRecoverySelection::Pending => {
            tx.commit()?;
            return Ok(RetentionPhaseOutcome::pending(0));
        }
    };
    let candidates: Vec<SessionRetentionAuthority> = {
        let mut stmt = tx.prepare(
            "SELECT session_id,
                    shutdown_clean,
                    clean_checkpoint_id,
                    CASE
                        WHEN typeof(host_id) = 'text'
                         AND length(CAST(host_id AS BLOB)) <= ?2
                        THEN host_id
                    END,
                    owner_pid,
                    owner_process_start,
                    owner_heartbeat_at,
                    recovery_acknowledged_at
             FROM mux_sessions
             WHERE typeof(session_id) = 'text'
               AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?1
             ORDER BY MAX(
                          created_at,
                          COALESCE(last_checkpoint_at, created_at),
                          COALESCE(
                              (SELECT MAX(c.checkpoint_at)
                               FROM session_checkpoints c
                               WHERE c.session_id = mux_sessions.session_id),
                              created_at
                          )
                      ) DESC,
                      session_id DESC",
        )?;
        stmt.query_map(
            rusqlite::params![
                i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
            ],
            |row| {
                Ok(SessionRetentionAuthority {
                    session_id: row.get(0)?,
                    shutdown_clean: row.get(1)?,
                    clean_checkpoint_id: row.get(2)?,
                    host_id: row.get(3)?,
                    owner_pid: row.get(4)?,
                    owner_process_start: row.get(5)?,
                    owner_heartbeat_at: row.get(6)?,
                    recovery_acknowledged_at: row.get(7)?,
                })
            },
        )?
        .collect::<Result<_, _>>()?
    };
    let mut retained = 0usize;
    let mut deleted = 0usize;
    for candidate in candidates {
        if !session_is_deletion_eligible(
            &tx,
            &candidate,
            protected_recovery_session.as_ref(),
            observer,
        )? {
            continue;
        }
        if retained < max_count {
            retained = retained.saturating_add(1);
            continue;
        }
        let affected = tx.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [&candidate.session_id],
        )?;
        if affected != 1 {
            return Err(rusqlite::Error::StatementChangedRows(affected));
        }
        deleted = deleted.saturating_add(1);
    }
    tx.commit()?;
    Ok(RetentionPhaseOutcome::ready(deleted))
}

fn retained_size_contract_error(message: &'static str) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(message)))
}

/// Recompute the complete logical retained-payload contract and require exact
/// agreement with the trigger-maintained O(sessions) authority before cleanup
/// trusts a byte. This scan is intentionally confined to the infrequent
/// retention transaction; normal mutation and decision paths remain bounded.
fn validate_session_retained_size_authority(conn: &Connection) -> Result<(), rusqlite::Error> {
    let invalid_row: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM mux_sessions s
             WHERE typeof(s.session_id) != 'text'
                OR length(CAST(s.session_id AS BLOB)) NOT BETWEEN 1 AND ?1
                OR typeof(s.created_at) != 'integer' OR s.created_at < 0
                OR (s.last_checkpoint_at IS NOT NULL AND
                    (typeof(s.last_checkpoint_at) != 'integer' OR s.last_checkpoint_at < 0))
                OR typeof(s.shutdown_clean) != 'integer' OR s.shutdown_clean NOT IN (0, 1)
                OR typeof(s.topology_json) != 'text'
                OR (s.window_metadata_json IS NOT NULL AND
                    typeof(s.window_metadata_json) != 'text')
                OR typeof(s.ft_version) != 'text'
                OR (s.host_id IS NOT NULL AND (
                    typeof(s.host_id) != 'text'
                    OR length(CAST(s.host_id AS BLOB)) > ?2))
                OR (s.owner_pid IS NOT NULL AND
                    (typeof(s.owner_pid) != 'integer' OR s.owner_pid <= 0))
                OR (s.owner_process_start IS NOT NULL AND
                    (typeof(s.owner_process_start) != 'integer'
                     OR s.owner_process_start <= 0))
                OR (s.owner_heartbeat_at IS NOT NULL AND
                    (typeof(s.owner_heartbeat_at) != 'integer'
                     OR s.owner_heartbeat_at < 0))
                OR (s.recovery_acknowledged_at IS NOT NULL AND
                    (typeof(s.recovery_acknowledged_at) != 'integer'
                     OR s.recovery_acknowledged_at < 0))
                OR NOT (
                    (s.owner_pid IS NULL
                     AND s.owner_process_start IS NULL
                     AND s.owner_heartbeat_at IS NULL)
                    OR (s.owner_pid IS NOT NULL
                        AND s.owner_process_start IS NOT NULL
                        AND s.owner_heartbeat_at IS NOT NULL
                        AND s.host_id IS NOT NULL)
                )
                OR (s.clean_checkpoint_id IS NOT NULL AND
                    (typeof(s.clean_checkpoint_id) != 'integer' OR s.clean_checkpoint_id <= 0))
             UNION ALL
             SELECT 1 FROM session_checkpoints c
             INNER JOIN mux_sessions s ON s.session_id = c.session_id
             WHERE typeof(c.id) != 'integer' OR c.id <= 0
                OR typeof(c.session_id) != 'text'
                OR typeof(c.checkpoint_at) != 'integer' OR c.checkpoint_at < 0
                OR typeof(c.checkpoint_type) != 'text'
                OR typeof(c.state_hash) != 'text'
                OR typeof(c.pane_count) != 'integer' OR c.pane_count < 0
                OR typeof(c.total_bytes) != 'integer' OR c.total_bytes < 0
                OR (c.metadata_json IS NOT NULL AND typeof(c.metadata_json) != 'text')
                OR typeof(c.checkpoint_role) != 'text'
                OR (c.topology_json IS NOT NULL AND typeof(c.topology_json) != 'text')
                OR (c.restore_intent_checkpoint_id IS NOT NULL AND
                    (typeof(c.restore_intent_checkpoint_id) != 'integer' OR
                     c.restore_intent_checkpoint_id <= 0))
             UNION ALL
             SELECT 1 FROM mux_pane_state p
             INNER JOIN session_checkpoints c ON c.id = p.checkpoint_id
             INNER JOIN mux_sessions s ON s.session_id = c.session_id
             WHERE typeof(p.id) != 'integer' OR p.id <= 0
                OR typeof(p.checkpoint_id) != 'integer' OR p.checkpoint_id <= 0
                OR typeof(p.pane_id) != 'integer' OR p.pane_id < 0
                OR (p.cwd IS NOT NULL AND typeof(p.cwd) != 'text')
                OR (p.command IS NOT NULL AND typeof(p.command) != 'text')
                OR (p.env_json IS NOT NULL AND typeof(p.env_json) != 'text')
                OR typeof(p.terminal_state_json) != 'text'
                OR (p.agent_metadata_json IS NOT NULL AND
                    typeof(p.agent_metadata_json) != 'text')
                OR (p.scrollback_checkpoint_seq IS NOT NULL AND
                    (typeof(p.scrollback_checkpoint_seq) != 'integer' OR
                     p.scrollback_checkpoint_seq < 0))
                OR (p.last_output_at IS NOT NULL AND
                    (typeof(p.last_output_at) != 'integer' OR p.last_output_at < 0))
             UNION ALL
             SELECT 1 FROM restore_attempt_lifecycle r
             INNER JOIN mux_sessions s ON s.session_id = r.session_id
             WHERE typeof(r.intent_checkpoint_id) != 'integer' OR r.intent_checkpoint_id <= 0
                OR typeof(r.session_id) != 'text'
                OR typeof(r.source_checkpoint_id) != 'integer' OR r.source_checkpoint_id <= 0
                OR (r.outcome_checkpoint_id IS NOT NULL AND
                    (typeof(r.outcome_checkpoint_id) != 'integer' OR
                     r.outcome_checkpoint_id <= 0))
                OR typeof(r.status) != 'text'
                OR typeof(r.created_at) != 'integer' OR r.created_at < 0
                OR (r.resolved_at IS NOT NULL AND
                    (typeof(r.resolved_at) != 'integer' OR r.resolved_at < r.created_at))
         )",
        rusqlite::params![
            i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
            i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
        ],
        |row| row.get(0),
    )?;
    if invalid_row {
        return Err(retained_size_contract_error(
            "session retained-size authority rejected invalid persisted row metadata",
        ));
    }

    let drift: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM mux_sessions s
             LEFT JOIN session_retained_size z ON z.session_id = s.session_id
             LEFT JOIN session_retained_size_recomputed r ON r.session_id = s.session_id
             WHERE z.session_id IS NULL OR r.session_id IS NULL
                OR z.session_row_bytes != r.session_row_bytes
                OR z.checkpoint_row_bytes != r.checkpoint_row_bytes
                OR z.pane_state_row_bytes != r.pane_state_row_bytes
                OR z.restore_lifecycle_row_bytes != r.restore_lifecycle_row_bytes
                OR z.retained_bytes !=
                   r.session_row_bytes + r.checkpoint_row_bytes
                     + r.pane_state_row_bytes + r.restore_lifecycle_row_bytes
             UNION ALL
             SELECT 1
             FROM session_retained_size z
             LEFT JOIN mux_sessions s ON s.session_id = z.session_id
             WHERE s.session_id IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    if drift {
        return Err(retained_size_contract_error(
            "session retained-size authority drifted from stored payload rows",
        ));
    }
    Ok(())
}

/// Delete oldest eligible closed sessions until exact logical retained-payload
/// bytes are under budget. Every stored field is charged once by schema-v40
/// `session_retained_size`; SQLite page, index, freelist, and WAL bytes remain
/// outside this explicitly logical budget.
#[cfg(test)]
fn delete_sessions_by_size(
    conn: &Connection,
    max_total_mb: u64,
) -> Result<SizeCleanupOutcome, rusqlite::Error> {
    let observer = SystemSessionOwnerObserver::new();
    delete_sessions_by_size_with_observer(conn, max_total_mb, &observer)
}

#[cfg(test)]
fn delete_sessions_by_size_with_observer(
    conn: &Connection,
    max_total_mb: u64,
    observer: &impl SessionOwnerObserver,
) -> Result<SizeCleanupOutcome, rusqlite::Error> {
    let step_budget = retention_phase_test_step_budget(conn)?;
    for _ in 0..step_budget {
        let outcome = delete_sessions_by_size_phase_with_observer(conn, max_total_mb, observer)?;
        if !outcome.recovery_reconciliation_pending {
            return Ok(outcome.value);
        }
    }
    panic!("size retention did not converge within {step_budget} bounded recovery steps");
}

fn delete_sessions_by_size_phase_with_observer(
    conn: &Connection,
    max_total_mb: u64,
    observer: &impl SessionOwnerObserver,
) -> Result<RetentionPhaseOutcome<SizeCleanupOutcome>, rusqlite::Error> {
    let max_bytes = max_total_mb
        .checked_mul(1_024)
        .and_then(|bytes| bytes.checked_mul(1_024))
        .ok_or_else(|| {
            retained_size_contract_error("session retained-size budget conversion overflow")
        })?;

    // Keep the measurement, candidate set, and deletions in one transaction.
    // Without this boundary, a later DELETE failure left earlier session
    // deletions committed even though this stage returned no accounting
    // result, and a concurrent cleanup could make us claim bytes for a row our
    // connection did not delete.
    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    let protected_recovery_session = match newest_usable_recovery_session(&tx, observer)? {
        ProtectedRecoverySelection::Ready(session_id) => session_id,
        ProtectedRecoverySelection::Pending => {
            tx.commit()?;
            return Ok(RetentionPhaseOutcome::pending(SizeCleanupOutcome::default()));
        }
    };
    validate_session_retained_size_authority(&tx)?;

    // The generated retained_bytes column is checked non-negative and each
    // summary row belongs to exactly one live mux session. SUM overflow is a
    // SQLite error, so an unrepresentable total fails before any deletion.
    let (total_bytes, minimum_bytes): (i64, i64) = tx.query_row(
        "SELECT COALESCE(SUM(retained_bytes), 0),
                COALESCE(MIN(retained_bytes), 0)
         FROM session_retained_size",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if minimum_bytes < 0 {
        return Err(rusqlite::Error::IntegralValueOutOfRange(1, minimum_bytes));
    }
    let total_bytes = u64::try_from(total_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, total_bytes))?;

    if total_bytes <= max_bytes {
        let outcome = SizeCleanupOutcome {
            measured_bytes: total_bytes,
            retained_bytes: total_bytes,
            ..SizeCleanupOutcome::default()
        };
        tx.commit()?;
        return Ok(RetentionPhaseOutcome::ready(outcome));
    }

    let to_free = total_bytes.checked_sub(max_bytes).ok_or_else(|| {
        retained_size_contract_error("session retained-size budget subtraction underflow")
    })?;
    let mut freed: u64 = 0;
    let mut deleted = 0_usize;

    // Get closed sessions ordered oldest first. Session-level payload bytes are
    // read once from the summary row; no join can multiply topology or other
    // session fields by checkpoint/pane cardinality.
    let candidate_rows: Vec<(SessionRetentionAuthority, u64)> = {
        let mut stmt = tx.prepare(
            "SELECT s.session_id,
                    z.retained_bytes,
                    s.shutdown_clean,
                    s.clean_checkpoint_id,
                    CASE
                        WHEN typeof(s.host_id) = 'text'
                         AND length(CAST(s.host_id AS BLOB)) <= ?2
                        THEN s.host_id
                    END,
                    s.owner_pid,
                    s.owner_process_start,
                    s.owner_heartbeat_at,
                    s.recovery_acknowledged_at
             FROM mux_sessions s
             INNER JOIN session_retained_size z ON z.session_id = s.session_id
             WHERE typeof(s.session_id) = 'text'
               AND length(CAST(s.session_id AS BLOB)) BETWEEN 1 AND ?1
               AND z.retained_bytes > 0
             ORDER BY MAX(
                          s.created_at,
                          COALESCE(s.last_checkpoint_at, s.created_at),
                          COALESCE(
                              (SELECT MAX(c.checkpoint_at)
                               FROM session_checkpoints c
                               WHERE c.session_id = s.session_id),
                              s.created_at
                          )
                      ) ASC,
                      s.session_id ASC",
        )?;

        let sessions = stmt
            .query_map(
                rusqlite::params![
                    i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
                    i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
                ],
                |row| {
                    let session_bytes: i64 = row.get(1)?;
                    let session_bytes = u64::try_from(session_bytes)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, session_bytes))?;
                    Ok((
                        SessionRetentionAuthority {
                            session_id: row.get(0)?,
                            shutdown_clean: row.get(2)?,
                            clean_checkpoint_id: row.get(3)?,
                            host_id: row.get(4)?,
                            owner_pid: row.get(5)?,
                            owner_process_start: row.get(6)?,
                            owner_heartbeat_at: row.get(7)?,
                            recovery_acknowledged_at: row.get(8)?,
                        },
                        session_bytes,
                    ))
                },
            )?
            .collect::<Result<_, _>>()?;
        sessions
    };

    let mut sessions = Vec::with_capacity(candidate_rows.len());
    for (candidate, session_bytes) in candidate_rows {
        if session_is_deletion_eligible(
            &tx,
            &candidate,
            protected_recovery_session.as_ref(),
            observer,
        )? {
            sessions.push((candidate.session_id, session_bytes));
        }
    }

    for (session_id, session_bytes) in sessions {
        if freed >= to_free {
            break;
        }

        let affected = tx.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [&session_id],
        )?;

        // The candidate query names one primary-key row. A trigger that ignores
        // it (or any other non-exact effect) means the size target was not
        // enforced as measured, so roll back rather than return false cleanup
        // accounting.
        if affected != 1 {
            return Err(rusqlite::Error::StatementChangedRows(affected));
        }

        freed = freed.checked_add(session_bytes).ok_or_else(|| {
            retained_size_contract_error("session retained-size deletion receipt overflow")
        })?;
        deleted = deleted.saturating_add(1);
    }

    let retained_bytes = total_bytes.checked_sub(freed).ok_or_else(|| {
        retained_size_contract_error("session retained-size deletion exceeded measurement")
    })?;
    let stored_retained_bytes: i64 = tx.query_row(
        "SELECT COALESCE(SUM(retained_bytes), 0) FROM session_retained_size",
        [],
        |row| row.get(0),
    )?;
    let stored_retained_bytes = u64::try_from(stored_retained_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, stored_retained_bytes))?;
    if stored_retained_bytes != retained_bytes {
        return Err(retained_size_contract_error(
            "session retained-size deletion receipt disagrees with durable summary",
        ));
    }

    tx.commit()?;
    if deleted > 0 {
        debug!(
            deleted,
            measured_bytes = total_bytes,
            deleted_bytes = freed,
            retained_bytes,
            target_bytes = to_free,
            "Committed bounded session set for size budget"
        );
    }
    Ok(RetentionPhaseOutcome::ready(SizeCleanupOutcome {
        deleted,
        measured_bytes: total_bytes,
        deleted_bytes: freed,
        retained_bytes,
        ineligible_shortfall_bytes: retained_bytes.saturating_sub(max_bytes),
    }))
}

/// Clean orphaned data that lost its parent reference.
///
/// Returns exact direct-row counts for every orphan authority shape removed by
/// this transaction. Foreign-key cascade effects are excluded from SQLite's
/// direct change counts, so children are deleted explicitly before parents.
fn cleanup_orphaned_data(conn: &Connection) -> Result<OrphanCleanupOutcome, rusqlite::Error> {
    // ft-rt6ol + ft-kccj8: delete pane_state CHILDREN first, with a predicate
    // that names both orphan shapes explicitly — rows whose checkpoint is
    // already gone, and rows whose checkpoint is about to be removed as a
    // session-orphan below. The previous checkpoint-first ordering relied on
    // the second DELETE to sweep the linked shape, which is correct with
    // `foreign_keys` OFF but count-false with it ON: the checkpoint DELETE
    // cascades the children away and sqlite3_changes() excludes rows removed
    // by FK actions, so the reported pane_state count came back 0 while the
    // data was in fact collected. Naming both shapes in one child DELETE is
    // correct AND correctly counted under either FK setting. The transaction
    // makes both deletes commit atomically.
    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;

    // If an external writer disabled FK enforcement before deleting a session,
    // remove its lifecycle rows first. Otherwise the deferred intent FK can
    // correctly prevent the following checkpoint cleanup from erasing durable
    // evidence whose parent session is already irretrievably absent.
    let orphaned_restore_lifecycle_rows = tx.execute(
        "DELETE FROM restore_attempt_lifecycle
         WHERE session_id NOT IN (
             SELECT session_id FROM mux_sessions
         )",
        [],
    )?;

    // Orphaned pane_state rows: checkpoint already deleted, OR checkpoint is
    // itself a session-orphan that the next statement removes.
    let orphan_ps = tx.execute(
        "DELETE FROM mux_pane_state
         WHERE checkpoint_id NOT IN (
             SELECT id FROM session_checkpoints
         )
         OR checkpoint_id IN (
             SELECT id FROM session_checkpoints
             WHERE session_id NOT IN (SELECT session_id FROM mux_sessions)
         )",
        [],
    )?;

    // Orphaned checkpoint rows (session_id references a deleted session).
    let orphan_cp = tx.execute(
        "DELETE FROM session_checkpoints
         WHERE session_id NOT IN (
             SELECT session_id FROM mux_sessions
         )",
        [],
    )?;

    tx.commit()?;
    Ok(OrphanCleanupOutcome {
        orphaned_restore_lifecycle_rows,
        orphaned_checkpoints: orphan_cp,
        orphaned_pane_states: orphan_ps,
    })
}

/// Test-only cleanup under an explicit `&Cx` (ft-xbnl0.2.2).
///
/// Cx-first entry point: caller-supplied cancellation is honored before the
/// blocking handoff. After admission, cancellation, executor failure, or result
/// loss is reported as [`SessionCleanupError::IndeterminateCleanup`], because
/// the SQLite pipeline may still be running or may already have committed.
///
/// # Errors
///
/// Returns a finite [`SessionCleanupError`]. In particular, an indeterminate
/// result requires durable-state reconciliation and is never retry-safe merely
/// because the async join did not produce a receipt.
#[cfg(test)]
pub(crate) async fn cleanup_sessions_async_cx(
    cx: &crate::cx::Cx,
    db_path: Arc<String>,
    config: SessionRetentionConfig,
) -> Result<CleanupResult, SessionCleanupError> {
    // Honor caller cancellation before we hand work off to the blocking pool.
    cx.checkpoint()
        .map_err(|_| SessionCleanupError::CancelledBeforeHandoff)?;

    let outcome = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
        cleanup_sessions_from_path(db_path.as_str(), &config)
    })
    .await;

    match outcome {
        Ok(result) => result,
        Err(error) => Err(classify_session_cleanup_blocking_failure(error)),
    }
}

/// Synchronous path-owned cleanup entry used when an outer authority
/// coordinator owns the exact queued-vs-started blocking handoff. Keeping
/// connection setup here also gives the test-only async seam the same finite
/// failure classification without nesting a second blocking task.
pub(crate) fn cleanup_sessions_from_path(
    db_path: &str,
    config: &SessionRetentionConfig,
) -> Result<CleanupResult, SessionCleanupError> {
    #[cfg(unix)]
    const SESSION_RETENTION_SQLITE_DEFAULT_VFS: &str = "unix";
    #[cfg(windows)]
    const SESSION_RETENTION_SQLITE_DEFAULT_VFS: &str = "win32";

    #[cfg(any(unix, windows))]
    let connection = Connection::open_with_flags_and_vfs(
        db_path,
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        SESSION_RETENTION_SQLITE_DEFAULT_VFS,
    );
    #[cfg(not(any(unix, windows)))]
    let connection = Connection::open_with_flags(
        db_path,
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    );
    let conn = connection.map_err(|error| {
        warn!(
            error_class = session_cleanup_database_error_class(&error),
            "session cleanup database open failed"
        );
        SessionCleanupError::DatabaseOpen
    })?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(|error| {
        warn!(
            error_class = session_cleanup_database_error_class(&error),
            "session cleanup busy policy setup failed"
        );
        SessionCleanupError::DatabasePreparation
    })?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|error| {
            warn!(
                error_class = session_cleanup_database_error_class(&error),
                "session cleanup database preparation failed"
            );
            SessionCleanupError::DatabasePreparation
        })?;
    cleanup_sessions(&conn, config).map_err(|error| {
        // The cleanup pipeline currently commits policy phases independently.
        // Any earlier phase may be durable when a later phase fails, so a
        // generic database error would fabricate retry safety until exact
        // partial receipts/continuations land.
        warn!(
            error_class = session_cleanup_database_error_class(&error),
            "session cleanup execution outcome is indeterminate"
        );
        SessionCleanupError::IndeterminateCleanup {
            phase: SessionCleanupIndeterminatePhase::CleanupExecution,
        }
    })
}

/// Collapse SQLite/rusqlite failures into a finite, content-free telemetry
/// class. Raw error displays are deliberately excluded because SQLite error
/// messages can contain database paths, schema fragments, or persisted values.
fn session_cleanup_database_error_class(error: &rusqlite::Error) -> &'static str {
    use rusqlite::ffi::ErrorCode;

    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => "contention",
        Some(ErrorCode::OperationAborted | ErrorCode::OperationInterrupted) => "interrupted",
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly | ErrorCode::CannotOpen) => "access",
        Some(ErrorCode::SystemIoFailure | ErrorCode::DiskFull) => "storage_io",
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => "invalid_database",
        Some(
            ErrorCode::TooBig
            | ErrorCode::ConstraintViolation
            | ErrorCode::TypeMismatch
            | ErrorCode::ParameterOutOfRange,
        ) => "data_contract",
        Some(ErrorCode::OutOfMemory) => "resource_exhausted",
        Some(_) => "database_failure",
        None => "rusqlite_contract",
    }
}

#[cfg(test)]
fn classify_session_cleanup_blocking_failure(
    error: crate::runtime_async::SpawnBlockingWithCxError,
) -> SessionCleanupError {
    match error {
        crate::runtime_async::SpawnBlockingWithCxError::CancelledBeforeSpawn { .. } => {
            SessionCleanupError::CancelledBeforeHandoff
        }
        crate::runtime_async::SpawnBlockingWithCxError::CancelledMidFlight { .. }
        | crate::runtime_async::SpawnBlockingWithCxError::RuntimeFailure
        | crate::runtime_async::SpawnBlockingWithCxError::CancellationWatcherTimerFailure => {
            SessionCleanupError::IndeterminateCleanup {
                phase: SessionCleanupIndeterminatePhase::BlockingTaskSettlement,
            }
        }
    }
}

/// Get current epoch time in milliseconds.
fn epoch_ms() -> u64 {
    crate::clock_anomaly::epoch_ms_u64("ft.session_retention.clock")
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    use std::sync::{LazyLock, Mutex};

    use super::*;

    const TEST_MACHINE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TEST_MACHINE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TEST_BOOT_A: &str = "11111111-1111-4111-8111-111111111111";
    const TEST_BOOT_B: &str = "22222222-2222-4222-8222-222222222222";
    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    static RECOVERY_TRACE_STATEMENTS: LazyLock<Mutex<Vec<String>>> =
        LazyLock::new(|| Mutex::new(Vec::new()));

    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    fn record_recovery_trace_statement(sql: &str) {
        RECOVERY_TRACE_STATEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(sql.to_string());
    }

    struct FakeOwnerObserver {
        current_host: Option<SessionHostIdentity>,
        processes: BTreeMap<u32, procinfo::ProcessStartTimeObservation>,
    }

    impl SessionOwnerObserver for FakeOwnerObserver {
        fn current_host(&self) -> Option<&SessionHostIdentity> {
            self.current_host.as_ref()
        }

        fn observe_process_start(&self, pid: u32) -> procinfo::ProcessStartTimeObservation {
            self.processes
                .get(&pid)
                .copied()
                .unwrap_or(procinfo::ProcessStartTimeObservation::Absent)
        }
    }

    fn test_host_on_machine(
        hostname: &str,
        machine_fence: &str,
        boot_id: &str,
    ) -> SessionHostIdentity {
        let boot_id = match boot_id {
            "boot-a" => TEST_BOOT_A,
            "boot-b" => TEST_BOOT_B,
            value => value,
        };
        SessionHostIdentity {
            version: SESSION_HOST_IDENTITY_VERSION,
            hostname: hostname.to_string(),
            machine_fence: machine_fence.to_string(),
            boot_id: boot_id.to_string(),
            process_domain_id: "test-process-domain".to_string(),
        }
    }

    fn test_host(hostname: &str, boot_id: &str) -> SessionHostIdentity {
        test_host_on_machine(hostname, TEST_MACHINE_A, boot_id)
    }

    fn encoded_test_host(hostname: &str, boot_id: &str) -> String {
        serde_json::to_string(&test_host(hostname, boot_id)).unwrap()
    }

    #[test]
    fn boot_identity_normalization_is_uuid_shaped_and_whitespace_strict() {
        assert_eq!(
            normalized_boot_id(TEST_BOOT_A.to_ascii_uppercase()).as_deref(),
            Some(TEST_BOOT_A)
        );
        for invalid in [
            "",
            " boot-a",
            "11111111-1111-4111-8111-111111111111 ",
            "111111111111-4111-8111-111111111111",
            "00000000-0000-0000-0000-000000000000",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ] {
            assert_eq!(normalized_boot_id(invalid.to_string()), None);
        }
    }

    #[test]
    fn owner_classification_fences_pid_reuse_reboot_and_foreign_hosts() {
        let host_id = encoded_test_host("trj", "boot-a");
        let observer = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::from([
                (41, procinfo::ProcessStartTimeObservation::Running(900)),
                (42, procinfo::ProcessStartTimeObservation::Running(901)),
            ]),
        };

        assert_eq!(
            classify_unclean_session_owner_with_observer(
                Some(&host_id),
                Some(41),
                Some(900),
                Some(1),
                &observer,
            ),
            UncleanSessionOwnerState::Live
        );
        assert_eq!(
            classify_unclean_session_owner_with_observer(
                Some(&host_id),
                Some(42),
                Some(900),
                Some(1),
                &observer,
            ),
            UncleanSessionOwnerState::RecoveryCandidate,
            "a reused PID with a different start token is not the owner"
        );
        assert_eq!(
            classify_unclean_session_owner_with_observer(
                Some(&host_id),
                Some(42),
                Some(901),
                Some(1),
                &observer,
            ),
            UncleanSessionOwnerState::Live,
            "concurrent engines remain fenced to their own PID/start pair"
        );

        let rebooted = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-b")),
            processes: BTreeMap::new(),
        };
        assert_eq!(
            classify_unclean_session_owner_with_observer(
                Some(&host_id),
                Some(41),
                Some(900),
                Some(1),
                &rebooted,
            ),
            UncleanSessionOwnerState::RecoveryCandidate
        );

        let same_name_other_machine = FakeOwnerObserver {
            current_host: Some(test_host_on_machine("trj", TEST_MACHINE_B, "boot-b")),
            processes: BTreeMap::new(),
        };
        assert_eq!(
            classify_unclean_session_owner_with_observer(
                Some(&host_id),
                Some(41),
                Some(900),
                Some(1),
                &same_name_other_machine,
            ),
            UncleanSessionOwnerState::Unknown,
            "a boot mismatch proves a reboot only after the persistent machine fence matches"
        );

        let renamed_same_machine = FakeOwnerObserver {
            current_host: Some(test_host("mac-studio", "boot-a")),
            processes: BTreeMap::from([(41, procinfo::ProcessStartTimeObservation::Running(900))]),
        };
        assert_eq!(
            classify_unclean_session_owner_with_observer(
                Some(&host_id),
                Some(41),
                Some(900),
                Some(1),
                &renamed_same_machine,
            ),
            UncleanSessionOwnerState::Live,
            "a hostname rename must not invalidate the stable machine and process fences"
        );

        let other_process_domain = FakeOwnerObserver {
            current_host: Some(SessionHostIdentity {
                process_domain_id: "other-process-domain".to_string(),
                ..test_host("trj", "boot-a")
            }),
            processes: BTreeMap::new(),
        };
        assert_eq!(
            classify_unclean_session_owner_with_observer(
                Some(&host_id),
                Some(41),
                Some(900),
                Some(1),
                &other_process_domain,
            ),
            UncleanSessionOwnerState::Unknown,
            "same-boot PID namespace mismatch cannot prove another namespace's owner dead"
        );
    }

    #[test]
    fn malformed_incomplete_and_unobservable_owners_fail_closed() {
        let observer = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::from([(41, procinfo::ProcessStartTimeObservation::Unknown)]),
        };
        let valid_host = encoded_test_host("trj", "boot-a");
        let legacy_host = r#"{"version":4,"hostname":"trj","boot_id":"boot-a","process_domain_id":"test-process-domain"}"#;
        let short_machine_fence = r#"{"version":5,"hostname":"trj","machine_fence":"abcd","boot_id":"boot-a","process_domain_id":"test-process-domain"}"#;
        let whitespace_boot = format!(
            r#"{{"version":5,"hostname":"trj","machine_fence":"{TEST_MACHINE_A}","boot_id":" {TEST_BOOT_A}","process_domain_id":"test-process-domain"}}"#
        );
        for (host_id, pid, process_start, heartbeat_at) in [
            (None, Some(41), Some(900), Some(1)),
            (Some("not-json"), Some(41), Some(900), Some(1)),
            (Some(legacy_host), Some(41), Some(900), Some(1)),
            (Some(short_machine_fence), Some(41), Some(900), Some(1)),
            (Some(whitespace_boot.as_str()), Some(41), Some(900), Some(1)),
            (Some(""), None, None, None),
            (Some(valid_host.as_str()), Some(41), Some(900), None),
            (Some(valid_host.as_str()), Some(41), Some(900), Some(-1)),
            (Some(valid_host.as_str()), Some(41), Some(900), Some(1)),
        ] {
            assert_eq!(
                classify_unclean_session_owner_with_observer(
                    host_id,
                    pid,
                    process_start,
                    heartbeat_at,
                    &observer,
                ),
                UncleanSessionOwnerState::Unknown
            );
        }
    }

    /// LabRuntime-based determinism test (ft-xbnl0.2.2): prove that the
    /// Cx-first `cleanup_sessions_async_cx` path respects the caller's
    /// checkpoint boundary under seed-locked virtual-time scheduling.
    /// We point it at a path that will fail fast at `Connection::open`
    /// so no real SQLite work is done and no real time elapses; the
    /// test asserts the spawn_blocking handoff + result plumbing run
    /// under the LabRuntime scheduler without wall-clock dependence.
    #[test]
    fn cleanup_sessions_async_cx_runs_under_labruntime() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const SEED: u64 = 0x5E55_1022_C410_D0BE;
        let wall_start = std::time::Instant::now();
        let observed_error = Arc::new(AtomicBool::new(false));
        let observed_error_task = Arc::clone(&observed_error);

        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(SEED)
                .with_auto_advance()
                .worker_count(1)
                .max_steps(50_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::for_request();
                // Path that cannot exist / cannot open as a SQLite DB.
                let bad_db = Arc::new("/ft-xbnl0-2-2/does-not-exist/bogus.sqlite".to_string());
                let config = SessionRetentionConfig::default();
                let result = cleanup_sessions_async_cx(&cx, bad_db, config).await;
                match result {
                    Err(SessionCleanupError::DatabaseOpen) => {
                        // The finite open failure proves the Cx-first plumbing
                        // ran end to end without deadlocking or burning real
                        // time, while keeping the path out of the error surface.
                        observed_error_task.store(true, Ordering::SeqCst);
                    }
                    Err(other) => panic!("unexpected error surface: {other}"),
                    Ok(_) => panic!("expected an error from nonexistent DB"),
                }
            })
            .expect("spawn session_retention task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.step_for_test();
        let _ = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert!(
            observed_error.load(Ordering::SeqCst),
            "cleanup_sessions_async_cx must surface an error for a bogus DB path"
        );
        assert!(
            report.oracle_report.all_passed(),
            "LabRuntime oracles must all pass: {report:?}"
        );
        assert!(
            wall_start.elapsed() < std::time::Duration::from_secs(2),
            "Cx-first cleanup must not burn real time; elapsed {:?}",
            wall_start.elapsed()
        );
    }

    #[test]
    fn blocking_settlement_classifier_never_fabricates_retry_safety() {
        use crate::runtime_async::SpawnBlockingWithCxError;

        let not_admitted = classify_session_cleanup_blocking_failure(
            SpawnBlockingWithCxError::CancelledBeforeSpawn { kind: None },
        );
        assert_eq!(not_admitted, SessionCleanupError::CancelledBeforeHandoff);
        assert!(!not_admitted.requires_reconciliation());

        let observation_losses = [
            SpawnBlockingWithCxError::CancelledMidFlight { kind: None },
            SpawnBlockingWithCxError::RuntimeFailure,
            SpawnBlockingWithCxError::CancellationWatcherTimerFailure,
        ];
        for failure in observation_losses {
            let classified = classify_session_cleanup_blocking_failure(failure);
            assert_eq!(
                classified,
                SessionCleanupError::IndeterminateCleanup {
                    phase: SessionCleanupIndeterminatePhase::BlockingTaskSettlement,
                }
            );
            assert!(classified.requires_reconciliation());
            let message = classified.to_string();
            assert!(message.contains("reconcile durable state"));
            assert!(!message.contains("None"));
        }
    }

    #[test]
    fn cleanup_execution_failure_is_content_free_and_requires_reconciliation() {
        let error = SessionCleanupError::IndeterminateCleanup {
            phase: SessionCleanupIndeterminatePhase::CleanupExecution,
        };
        assert!(error.requires_reconciliation());
        assert_eq!(
            error.to_string(),
            "session cleanup outcome is indeterminate during cleanup_execution; reconcile durable state before retrying"
        );
    }

    #[test]
    fn database_error_telemetry_class_never_exposes_sqlite_message_content() {
        let canary = "credential-canary /private/database/path";
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::CannotOpen,
                extended_code: rusqlite::ffi::SQLITE_CANTOPEN,
            },
            Some(canary.to_owned()),
        );

        assert!(error.to_string().contains(canary));
        let class = session_cleanup_database_error_class(&error);
        assert_eq!(class, "access");
        assert!(!class.contains(canary));

        let wrapped_content =
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(canary)));
        assert!(wrapped_content.to_string().contains(canary));
        let wrapped_class = session_cleanup_database_error_class(&wrapped_content);
        assert_eq!(wrapped_class, "rusqlite_contract");
        assert!(!wrapped_class.contains(canary));
    }

    fn make_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Pin the physical-reclamation proof contract rather than relying on a
        // platform SQLite build's compile-time auto-vacuum default.
        conn.execute_batch("PRAGMA auto_vacuum = NONE; PRAGMA foreign_keys = ON;")
            .unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        conn
    }

    fn drive_recovery_selection(
        conn: &Connection,
        observer: &impl SessionOwnerObserver,
        max_steps: usize,
    ) -> Option<String> {
        for _ in 0..max_steps {
            let transaction = begin_retention_transaction(conn).unwrap();
            ensure_session_authority_tables_have_no_unaudited_triggers(&transaction).unwrap();
            let selection = newest_usable_recovery_session(&transaction, observer).unwrap();
            transaction.commit().unwrap();
            if let ProtectedRecoverySelection::Ready(point) = selection {
                return point.map(|point| point.session_id);
            }
        }
        panic!("recovery selection did not converge within {max_steps} bounded steps");
    }

    fn settle_test_recovery_selection(conn: &Connection) {
        let observer = SystemSessionOwnerObserver::new();
        assert_eq!(
            drive_recovery_selection(conn, &observer, 64),
            None,
            "closed-session fixture must settle without advertising a crash recovery point"
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum LegacyOrphanInsertSurface {
        Checkpoint,
        PaneState,
        RestoreLifecycle,
    }

    fn seed_legacy_orphan<T>(
        conn: &Connection,
        surface: LegacyOrphanInsertSurface,
        seed: impl FnOnce(&Connection) -> T,
    ) -> T {
        let drop_trigger_sql = match surface {
            LegacyOrphanInsertSurface::Checkpoint => {
                "DROP TRIGGER session_checkpoints_retained_size_ai;"
            }
            LegacyOrphanInsertSurface::PaneState => "DROP TRIGGER mux_pane_state_retained_size_ai;",
            LegacyOrphanInsertSurface::RestoreLifecycle => {
                "DROP TRIGGER restore_attempt_lifecycle_retained_size_ai;"
            }
        };
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(drop_trigger_sql).unwrap();
        let output = seed(conn);
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        output
    }

    fn insert_session(conn: &Connection, id: &str, created_at: i64, shutdown_clean: bool) {
        insert_session_with_topology(conn, id, created_at, shutdown_clean, "{}");
    }

    fn insert_session_with_topology(
        conn: &Connection,
        id: &str,
        created_at: i64,
        shutdown_clean: bool,
        topology_json: &str,
    ) {
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, shutdown_clean, topology_json, ft_version)
             VALUES (?1, ?2, ?3, ?4, '0.1.0')",
            rusqlite::params![id, created_at, shutdown_clean as i64, topology_json],
        )
        .unwrap();
        if shutdown_clean {
            insert_v2_clean_shutdown_snapshot(conn, id, created_at);
        }
    }

    fn insert_checkpoint(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
        total_bytes: i64,
    ) -> i64 {
        let logical_payload = usize::try_from(total_bytes)
            .ok()
            .map(|bytes| "x".repeat(bytes));
        conn.execute(
            "DELETE FROM session_checkpoints
             WHERE id = (
                 SELECT clean_checkpoint_id
                 FROM mux_sessions
                 WHERE session_id = ?1
             )
               AND checkpoint_type = 'shutdown'
               AND checkpoint_role = 'snapshot'
               AND total_bytes = 0",
            [session_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json)
             VALUES (?1, ?2, 'periodic', '0123456789abcdef', 1, ?3, ?4)",
            rusqlite::params![session_id, checkpoint_at, total_bytes, logical_payload],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();
        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 clean_checkpoint_id = NULL
             WHERE session_id = ?1",
            rusqlite::params![session_id, checkpoint_at],
        )
        .unwrap();
        if shutdown_clean {
            insert_v2_clean_shutdown_snapshot(conn, session_id, checkpoint_at);
        }
        checkpoint_id
    }

    fn insert_pane_state(conn: &Connection, checkpoint_id: i64, pane_id: u64) {
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, '{}')",
            rusqlite::params![checkpoint_id, pane_id as i64],
        )
        .unwrap();
    }

    fn insert_v2_clean_shutdown_snapshot(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
    ) -> i64 {
        let topology_json: String = conn
            .query_row(
                "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .expect("load clean shutdown topology");
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role, topology_json)
             VALUES (?1, ?2, 'shutdown', 'pending:snp2', 0, 0, NULL,
                     'snapshot', ?3)",
            rusqlite::params![session_id, checkpoint_at, &topology_json],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();
        let state_hash = crate::checkpoint_witness::checkpoint_witness(
            crate::checkpoint_witness::CHECKPOINT_ROLE_SNAPSHOT,
            session_id,
            checkpoint_id,
            checkpoint_at,
            "shutdown",
            0,
            0,
            None,
            Some(&topology_json),
            &[],
        )
        .expect("compute v2 clean shutdown witness");
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            rusqlite::params![state_hash, checkpoint_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 last_checkpoint_at = ?2,
                 clean_checkpoint_id = ?3
             WHERE session_id = ?1",
            rusqlite::params![session_id, checkpoint_at, checkpoint_id],
        )
        .unwrap();
        checkpoint_id
    }

    fn set_unclean_owner(
        conn: &Connection,
        session_id: &str,
        host_id: &str,
        owner_pid: i64,
        process_start: i64,
        heartbeat_at: i64,
    ) {
        conn.execute(
            "UPDATE mux_sessions
             SET host_id = ?2,
                 owner_pid = ?3,
                 owner_process_start = ?4,
                 owner_heartbeat_at = ?5
             WHERE session_id = ?1",
            rusqlite::params![session_id, host_id, owner_pid, process_start, heartbeat_at],
        )
        .unwrap();
    }

    fn insert_v2_recovery_snapshot(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
        topology_pane_id: u64,
        persisted_pane_id: u64,
    ) -> i64 {
        let topology_json = format!(
            r#"{{"schema_version":1,"captured_at":{checkpoint_at},"windows":[{{"window_id":1,"tabs":[{{"tab_id":1,"pane_tree":{{"type":"Leaf","pane_id":{topology_pane_id},"rows":24,"cols":80,"is_active":true}},"active_pane_id":{topology_pane_id}}}],"active_tab_index":0}}]}}"#
        );
        let terminal_state_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"recovery"}"#;
        let pane = crate::checkpoint_witness::PersistedPaneState {
            pane_id: i64::try_from(persisted_pane_id).unwrap(),
            cwd: None,
            command: None,
            env_json: None,
            terminal_state_json: terminal_state_json.to_string(),
            agent_metadata_json: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let total_bytes = i64::try_from(terminal_state_json.len()).unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, checkpoint_role, topology_json)
             VALUES (?1, ?2, 'periodic', 'pending:snp2', 1, ?3,
                     'snapshot', ?4)",
            rusqlite::params![session_id, checkpoint_at, total_bytes, topology_json],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![checkpoint_id, pane.pane_id, terminal_state_json],
        )
        .unwrap();
        let state_hash = crate::checkpoint_witness::checkpoint_witness(
            crate::checkpoint_witness::CHECKPOINT_ROLE_SNAPSHOT,
            session_id,
            checkpoint_id,
            checkpoint_at,
            "periodic",
            1,
            total_bytes,
            None,
            Some(&topology_json),
            &[pane],
        )
        .unwrap();
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            rusqlite::params![state_hash, checkpoint_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 topology_json = ?3
             WHERE session_id = ?1",
            rusqlite::params![session_id, checkpoint_at, topology_json],
        )
        .unwrap();
        checkpoint_id
    }

    #[test]
    fn live_owner_overrides_retention_and_ack_requires_proven_death() {
        let conn = make_test_db();
        insert_session(&conn, "owned-active", 1, false);
        let host_id = encoded_test_host("trj", "boot-a");
        conn.execute(
            "UPDATE mux_sessions
             SET host_id = ?2,
                 owner_pid = 41,
                 owner_process_start = 900,
                 owner_heartbeat_at = 9223372036854775807
             WHERE session_id = ?1",
            rusqlite::params!["owned-active", host_id],
        )
        .unwrap();
        let candidate = SessionRetentionAuthority {
            session_id: "owned-active".to_string(),
            shutdown_clean: 0,
            clean_checkpoint_id: None,
            host_id: Some(host_id.clone()),
            owner_pid: Some(41),
            owner_process_start: Some(900),
            owner_heartbeat_at: Some(i64::MAX),
            recovery_acknowledged_at: None,
        };
        let live = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::from([(41, procinfo::ProcessStartTimeObservation::Running(900))]),
        };
        assert!(!session_is_deletion_eligible(&conn, &candidate, None, &live).unwrap());
        assert!(
            set_recovery_acknowledgement_with_observer(&conn, "owned-active", Some(1_000), &live,)
                .is_err(),
            "heartbeat wall-clock skew and an extreme acknowledgement timestamp cannot override liveness"
        );

        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::from([(41, procinfo::ProcessStartTimeObservation::Absent)]),
        };
        assert!(!session_is_deletion_eligible(&conn, &candidate, None, &dead).unwrap());
        set_recovery_acknowledgement_with_observer(&conn, "owned-active", Some(1_000), &dead)
            .unwrap();
        let negative_acknowledgement = SessionRetentionAuthority {
            session_id: candidate.session_id.clone(),
            shutdown_clean: candidate.shutdown_clean,
            clean_checkpoint_id: candidate.clean_checkpoint_id,
            host_id: candidate.host_id.clone(),
            owner_pid: candidate.owner_pid,
            owner_process_start: candidate.owner_process_start,
            owner_heartbeat_at: candidate.owner_heartbeat_at,
            recovery_acknowledged_at: Some(-1),
        };
        assert!(
            !session_is_deletion_eligible(&conn, &negative_acknowledgement, None, &dead).unwrap(),
            "corrupt negative acknowledgement metadata must never authorize deletion"
        );
        let acknowledged = SessionRetentionAuthority {
            recovery_acknowledged_at: Some(1_000),
            ..candidate
        };
        assert!(session_is_deletion_eligible(&conn, &acknowledged, None, &dead).unwrap());
        set_recovery_acknowledgement_with_observer(&conn, "owned-active", None, &dead).unwrap();
        let stored_ack: Option<i64> = conn
            .query_row(
                "SELECT recovery_acknowledged_at
                 FROM mux_sessions WHERE session_id = 'owned-active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_ack, None);
    }

    #[test]
    fn malformed_owner_lifecycle_timestamps_never_authorize_cleanup_or_acknowledgement() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };

        for (index, session_id) in ["negative-lifecycle", "missing-heartbeat"]
            .into_iter()
            .enumerate()
        {
            let timestamp = 1 + i64::try_from(index).unwrap();
            insert_session(&conn, session_id, timestamp, false);
            set_unclean_owner(
                &conn,
                session_id,
                &host_id,
                71 + i64::try_from(index).unwrap(),
                1_071 + i64::try_from(index).unwrap(),
                timestamp,
            );
            insert_v2_recovery_snapshot(
                &conn,
                session_id,
                timestamp,
                71 + u64::try_from(index).unwrap(),
                71 + u64::try_from(index).unwrap(),
            );
        }

        // Seed historical corruption without weakening the behavior under
        // test. SQLite's `ignore_check_constraints` pragma does not bypass the
        // canonical retained-size UPDATE trigger, so suspend that one trigger,
        // keep its byte summary exact by hand, and reinstall/validate the
        // canonical trigger set before invoking any retention authority.
        conn.execute_batch(
            "DROP TRIGGER mux_sessions_retained_size_au;
             PRAGMA ignore_check_constraints = ON;",
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET owner_heartbeat_at = -1,
                 recovery_acknowledged_at = -1
             WHERE session_id = 'negative-lifecycle'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET owner_heartbeat_at = NULL,
                 recovery_acknowledged_at = 1
             WHERE session_id = 'missing-heartbeat'",
            [],
        )
        .unwrap();
        assert_eq!(
            conn.execute(
                "UPDATE session_retained_size
                 SET session_row_bytes = session_row_bytes + 8
                 WHERE session_id = 'negative-lifecycle'",
                [],
            )
            .unwrap(),
            1,
            "the newly non-NULL acknowledgement contributes one SQLite integer"
        );
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .unwrap();
        conn.execute_batch(crate::storage::migrations::session_retained_size_schema_sql().unwrap())
            .expect("restore canonical retained-size trigger authority");
        crate::storage::migrations::validate_session_retained_size_schema(&conn)
            .expect("corrupt lifecycle values retain exact byte authority");

        for session_id in ["negative-lifecycle", "missing-heartbeat"] {
            assert!(
                set_recovery_acknowledgement_with_observer(&conn, session_id, Some(10), &dead,)
                    .is_err(),
                "malformed owner heartbeat for {session_id} must reject acknowledgement"
            );
        }
        assert_eq!(
            delete_sessions_by_age_with_observer(&conn, 0, &dead).unwrap(),
            0
        );
        assert_eq!(
            delete_excess_closed_sessions_with_observer(&conn, 0, &dead).unwrap(),
            0
        );
        assert!(
            delete_sessions_by_size_with_observer(&conn, 0, &dead).is_err(),
            "exact-size cleanup must reject malformed lifecycle accounting authority"
        );
        assert_eq!(count_sessions(&conn), 2);
    }

    #[test]
    fn repeated_abrupt_crashes_are_bounded_to_the_newest_usable_recovery_point() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };

        for (index, session_id) in ["crash-old", "crash-middle", "crash-new"]
            .into_iter()
            .enumerate()
        {
            let timestamp = 100 + i64::try_from(index).unwrap();
            insert_session(&conn, session_id, timestamp, false);
            set_unclean_owner(
                &conn,
                session_id,
                &host_id,
                40 + i64::try_from(index).unwrap(),
                900 + i64::try_from(index).unwrap(),
                timestamp,
            );
            insert_v2_recovery_snapshot(
                &conn,
                session_id,
                timestamp,
                1 + u64::try_from(index).unwrap(),
                1 + u64::try_from(index).unwrap(),
            );
        }

        assert_eq!(
            delete_excess_closed_sessions_with_observer(&conn, 0, &dead).unwrap(),
            2
        );
        assert_eq!(count_sessions(&conn), 1);
        assert!(
            crate::session_restore::usable_recovery_checkpoint_id_from_conn(&conn, "crash-new")
                .unwrap()
                .is_some()
        );

        insert_session(&conn, "crash-newer", 200, false);
        set_unclean_owner(&conn, "crash-newer", &host_id, 44, 904, 200);
        insert_v2_recovery_snapshot(&conn, "crash-newer", 200, 4, 4);
        assert_eq!(
            delete_excess_closed_sessions_with_observer(&conn, 0, &dead).unwrap(),
            1,
            "the previously protected crash must become reclaimable once a newer usable recovery exists"
        );
        assert_eq!(count_sessions(&conn), 1);
        let survivor: String = conn
            .query_row("SELECT session_id FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(survivor, "crash-newer");
    }

    #[test]
    fn corrupt_empty_and_topology_mismatched_recovery_points_fail_closed() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };

        insert_session(&conn, "empty-crash", 1, false);
        set_unclean_owner(&conn, "empty-crash", &host_id, 41, 901, 1);

        insert_session(&conn, "mismatched-crash", 2, false);
        set_unclean_owner(&conn, "mismatched-crash", &host_id, 42, 902, 2);
        insert_v2_recovery_snapshot(&conn, "mismatched-crash", 2, 20, 21);

        insert_session(&conn, "corrupt-crash", 3, false);
        set_unclean_owner(&conn, "corrupt-crash", &host_id, 43, 903, 3);
        let corrupt_checkpoint = insert_v2_recovery_snapshot(&conn, "corrupt-crash", 3, 30, 30);
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = 'snp2:corrupt' WHERE id = ?1",
            [corrupt_checkpoint],
        )
        .unwrap();

        insert_session(&conn, "legacy-crash", 4, false);
        set_unclean_owner(&conn, "legacy-crash", &host_id, 44, 904, 4);
        let legacy_checkpoint = insert_v2_recovery_snapshot(&conn, "legacy-crash", 4, 40, 40);
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = 'legacy-unverified' WHERE id = ?1",
            [legacy_checkpoint],
        )
        .unwrap();

        insert_session(&conn, "missing-pane-crash", 5, false);
        set_unclean_owner(&conn, "missing-pane-crash", &host_id, 45, 905, 5);
        let missing_pane_checkpoint =
            insert_v2_recovery_snapshot(&conn, "missing-pane-crash", 5, 50, 50);
        conn.execute(
            "DELETE FROM mux_pane_state WHERE checkpoint_id = ?1",
            [missing_pane_checkpoint],
        )
        .unwrap();

        for session_id in [
            "empty-crash",
            "mismatched-crash",
            "corrupt-crash",
            "legacy-crash",
            "missing-pane-crash",
        ] {
            assert!(
                crate::session_restore::usable_recovery_checkpoint_id_from_conn(&conn, session_id)
                    .unwrap()
                    .is_none(),
                "{session_id} must not be represented as a usable recovery authority"
            );
        }
        assert_eq!(
            delete_excess_closed_sessions_with_observer(&conn, 0, &dead).unwrap(),
            0,
            "ambiguous or unusable recovery state must require explicit acknowledgement"
        );
        assert_eq!(count_sessions(&conn), 5);
        assert_eq!(drive_recovery_selection(&conn, &dead, 4), None);
        let usable_authorities: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_recovery_usability WHERE state = 'usable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(usable_authorities, 0);
    }

    #[test]
    fn stale_selected_checkpoint_identity_is_invalidated_fail_closed() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        insert_session(&conn, "stale-selected-identity", 1, false);
        set_unclean_owner(&conn, "stale-selected-identity", &host_id, 46, 906, 1);
        let canonical_checkpoint =
            insert_v2_recovery_snapshot(&conn, "stale-selected-identity", 1, 46, 46);
        conn.execute(
            "UPDATE session_recovery_usability
             SET state = 'usable',
                 validated_checkpoint_id = ?2
             WHERE session_id = ?1",
            rusqlite::params!["stale-selected-identity", canonical_checkpoint + 1],
        )
        .unwrap();

        let generation_before: i64 = conn
            .query_row(
                "SELECT mutation_generation FROM session_recovery_selection",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let transaction = begin_retention_transaction(&conn).unwrap();
        assert_eq!(
            newest_usable_recovery_session(&transaction, &dead).unwrap(),
            ProtectedRecoverySelection::Pending,
            "a stale selected identity must be invalidated before cleanup can continue"
        );
        transaction.commit().unwrap();
        let (state, checkpoint_id, generation_after): (String, Option<i64>, i64) = conn
            .query_row(
                "SELECT usability.state,
                        usability.validated_checkpoint_id,
                        selection.mutation_generation
                 FROM session_recovery_usability AS usability
                 CROSS JOIN session_recovery_selection AS selection
                 WHERE usability.session_id = 'stale-selected-identity'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "dirty");
        assert_eq!(checkpoint_id, None);
        assert_eq!(generation_after, generation_before + 1);
    }

    #[test]
    fn legacy_population_is_durable_and_bounded_across_thousands_of_sessions() {
        const SESSION_COUNT: usize = 2_049;
        let conn = make_test_db();
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        for index in 0..SESSION_COUNT {
            insert_session(
                &conn,
                &format!("legacy-{index:05}"),
                i64::try_from(index).unwrap(),
                false,
            );
        }
        conn.execute("DELETE FROM session_recovery_usability", [])
            .unwrap();
        conn.execute(
            "UPDATE session_recovery_selection
             SET population_after_rowid = NULL,
                 population_complete = 0,
                 scan_generation = 0,
                 scan_after_checkpoint_id = NULL,
                 scan_after_session_id = NULL,
                 protected_session_id = NULL,
                 protected_checkpoint_id = NULL,
                 scan_complete = 0",
            [],
        )
        .unwrap();

        let first = delete_excess_closed_sessions_phase_with_observer(&conn, 0, &dead).unwrap();
        assert!(
            first.recovery_reconciliation_pending,
            "the first bounded legacy batch must remain explicitly pending"
        );
        assert_eq!(
            first.value, 0,
            "the first bounded legacy batch must never authorize deletion"
        );
        let first_batch: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_recovery_usability",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            first_batch,
            i64::try_from(RECOVERY_AUTHORITY_POPULATION_BATCH_ROWS).unwrap()
        );
        assert_eq!(count_sessions(&conn), i64::try_from(SESSION_COUNT).unwrap());

        for _ in 0..SESSION_COUNT.div_ceil(RECOVERY_AUTHORITY_POPULATION_BATCH_ROWS) {
            let transaction = begin_retention_transaction(&conn).unwrap();
            let _ = newest_usable_recovery_session(&transaction, &dead).unwrap();
            transaction.commit().unwrap();
            let complete: bool = conn
                .query_row(
                    "SELECT population_complete FROM session_recovery_selection",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            if complete {
                break;
            }
        }
        let (authority_rows, population_complete): (i64, bool) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM session_recovery_usability),
                        population_complete
                 FROM session_recovery_selection",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(authority_rows, i64::try_from(SESSION_COUNT).unwrap());
        assert!(population_complete);
        assert_eq!(count_sessions(&conn), i64::try_from(SESSION_COUNT).unwrap());
    }

    #[test]
    fn legacy_population_cursor_admits_negative_sqlite_rowids() {
        let conn = make_test_db();
        insert_session(&conn, "negative-rowid", 1, false);
        conn.execute(
            "UPDATE mux_sessions SET rowid = -7 WHERE session_id = 'negative-rowid'",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM session_recovery_usability", [])
            .unwrap();
        conn.execute(
            "UPDATE session_recovery_selection
             SET population_after_rowid = NULL,
                 population_complete = 0,
                 scan_generation = 0,
                 scan_after_checkpoint_id = NULL,
                 scan_after_session_id = NULL,
                 protected_session_id = NULL,
                 protected_checkpoint_id = NULL,
                 scan_complete = 0",
            [],
        )
        .unwrap();

        let transaction = begin_retention_transaction(&conn).unwrap();
        assert_eq!(
            newest_usable_recovery_session(
                &transaction,
                &FakeOwnerObserver {
                    current_host: Some(test_host("trj", "boot-a")),
                    processes: BTreeMap::new(),
                },
            )
            .unwrap(),
            ProtectedRecoverySelection::Pending
        );
        transaction.commit().unwrap();
        let (cursor, complete, authority_rows): (i64, bool, i64) = conn
            .query_row(
                "SELECT population_after_rowid,
                        population_complete,
                        (SELECT COUNT(*) FROM session_recovery_usability)
                 FROM session_recovery_selection",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(cursor, -7);
        assert!(complete);
        assert_eq!(authority_rows, 1);
    }

    #[test]
    fn bounded_recovery_queries_have_fixed_limits_and_keyset_plans() {
        assert_eq!(RECOVERY_AUTHORITY_POPULATION_BATCH_ROWS, 64);
        assert_eq!(RECOVERY_AUTHORITY_RECONCILE_BATCH_ROWS, 4);
        assert_eq!(RECOVERY_SELECTION_SCAN_BATCH_ROWS, 64);
        assert_eq!(MAX_CHECKPOINT_SESSION_ID_BYTES, 256);
        assert_eq!(
            RECOVERY_AUTHORITY_RECONCILE_MAX_ADMITTED_BYTES,
            4 * MAX_PERSISTED_CHECKPOINT_TEXT_BYTES
        );
        assert_eq!(
            RECOVERY_AUTHORITY_RECONCILE_WALL_BUDGET,
            Duration::from_millis(40)
        );

        let conn = make_test_db();
        let plan_details = |sql: &str| -> Vec<String> {
            let mut statement = conn.prepare(sql).unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let population = plan_details(
            "EXPLAIN QUERY PLAN
             SELECT rowid FROM mux_sessions
             WHERE rowid > 10 ORDER BY rowid ASC LIMIT 65",
        )
        .join(" ");
        assert!(
            population.contains("INTEGER PRIMARY KEY"),
            "resumed population must seek by rowid: {population}"
        );
        let reconciliation = plan_details(
            "EXPLAIN QUERY PLAN
             SELECT session_id, dirty_generation
             FROM session_recovery_usability
                  INDEXED BY idx_session_recovery_usability_dirty
             WHERE state = 'dirty'
             ORDER BY dirty_generation ASC, session_id ASC
             LIMIT 4",
        )
        .join(" ");
        assert!(
            reconciliation.contains("idx_session_recovery_usability_dirty"),
            "dirty reconciliation must use its bounded-order index: {reconciliation}"
        );
        let selection = plan_details(
            "EXPLAIN QUERY PLAN
             SELECT session_id, validated_checkpoint_id
             FROM session_recovery_usability
                  INDEXED BY idx_session_recovery_usability_state
             WHERE state = 'usable'
               AND validated_checkpoint_id <= 100
               AND (validated_checkpoint_id < 100
                    OR (validated_checkpoint_id = 100 AND session_id > 'cursor'))
             ORDER BY validated_checkpoint_id DESC, session_id ASC
             LIMIT 64",
        )
        .join(" ");
        assert!(
            selection.contains("idx_session_recovery_usability_state"),
            "resumed selection must seek the ordered usability index: {selection}"
        );
    }

    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    #[test]
    #[allow(deprecated)]
    fn bounded_reconciliation_statement_count_is_history_independent() {
        fn first_step_statement_count(session_count: usize) -> usize {
            let mut conn = make_test_db();
            for index in 0..session_count {
                insert_session(
                    &conn,
                    &format!("statement-count-{index:05}"),
                    i64::try_from(index).unwrap(),
                    false,
                );
            }
            RECOVERY_TRACE_STATEMENTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            conn.trace(Some(record_recovery_trace_statement));
            let transaction = begin_retention_transaction(&conn).unwrap();
            let observer = FakeOwnerObserver {
                current_host: Some(test_host("trj", "boot-a")),
                processes: BTreeMap::new(),
            };
            assert_eq!(
                newest_usable_recovery_session(&transaction, &observer).unwrap(),
                ProtectedRecoverySelection::Pending
            );
            transaction.commit().unwrap();
            conn.trace(None);
            RECOVERY_TRACE_STATEMENTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }

        let exact_batch_count = first_step_statement_count(4);
        let long_history_count = first_step_statement_count(2_049);
        assert_eq!(exact_batch_count, long_history_count);
        assert_eq!(
            long_history_count, 12,
            "BEGIN + two bounded authority queries + four canonical probes + four authority updates + COMMIT"
        );
    }

    #[test]
    fn long_corrupt_prefix_defers_then_preserves_the_newest_canonical_point() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        insert_session(&conn, "usable-old", 1, false);
        set_unclean_owner(&conn, "usable-old", &host_id, 40, 940, 1);
        insert_v2_recovery_snapshot(&conn, "usable-old", 1, 1, 1);
        for index in 0_i64..13 {
            let session_id = format!("corrupt-{index:02}");
            insert_session(&conn, &session_id, 10 + index, false);
            set_unclean_owner(
                &conn,
                &session_id,
                &host_id,
                100 + index,
                1_000 + index,
                10 + index,
            );
            let checkpoint_id = insert_v2_recovery_snapshot(
                &conn,
                &session_id,
                10 + index,
                u64::try_from(index + 10).unwrap(),
                u64::try_from(index + 10).unwrap(),
            );
            conn.execute(
                "UPDATE session_checkpoints
                 SET state_hash = 'snp2:corrupt'
                 WHERE id = ?1",
                [checkpoint_id],
            )
            .unwrap();
        }

        let first = delete_excess_closed_sessions_phase_with_observer(&conn, 0, &dead).unwrap();
        assert!(
            first.recovery_reconciliation_pending,
            "the first reconciliation batch must remain explicitly pending"
        );
        assert_eq!(
            first.value, 0,
            "the first reconciliation batch must commit progress without deleting"
        );
        assert_eq!(count_sessions(&conn), 14);
        let deleted = delete_excess_closed_sessions_with_observer(&conn, 0, &dead).unwrap();
        assert_eq!(deleted, 13);
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(
            conn.query_row("SELECT session_id FROM mux_sessions", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "usable-old"
        );
        assert!(
            crate::session_restore::usable_recovery_checkpoint_id_from_conn(&conn, "usable-old")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn interrupted_reconciliation_rolls_back_and_committed_selection_survives_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        insert_session(&conn, "restart-authority", 1, false);
        set_unclean_owner(&conn, "restart-authority", &host_id, 81, 981, 1);
        let restart_checkpoint = insert_v2_recovery_snapshot(&conn, "restart-authority", 1, 81, 81);

        {
            let transaction = begin_retention_transaction(&conn).unwrap();
            assert_eq!(
                newest_usable_recovery_session(&transaction, &dead).unwrap(),
                ProtectedRecoverySelection::Ready(Some(ProtectedRecoveryPoint {
                    session_id: "restart-authority".to_string(),
                    checkpoint_id: restart_checkpoint,
                }))
            );
            // Drop without commit to model interruption after reconciliation
            // and selection but before the transaction receipt is durable.
        }
        let rolled_back_state: String = conn
            .query_row(
                "SELECT state FROM session_recovery_usability
                 WHERE session_id = 'restart-authority'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rolled_back_state, "dirty");
        assert_eq!(
            drive_recovery_selection(&conn, &dead, 4).as_deref(),
            Some("restart-authority")
        );
        drop(conn);

        let reopened = Connection::open(&path).unwrap();
        reopened.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        assert_eq!(
            drive_recovery_selection(&reopened, &dead, 1).as_deref(),
            Some("restart-authority"),
            "restart must recompute and canonically verify the protected identity"
        );
    }

    #[test]
    fn cached_selection_rechecks_host_fence_after_database_move() {
        let conn = make_test_db();
        let host_a =
            serde_json::to_string(&test_host_on_machine("host-a", TEST_MACHINE_A, "boot-a"))
                .unwrap();
        let host_b =
            serde_json::to_string(&test_host_on_machine("host-b", TEST_MACHINE_B, "boot-b"))
                .unwrap();
        let observer_a = FakeOwnerObserver {
            current_host: Some(test_host_on_machine("host-a", TEST_MACHINE_A, "boot-a")),
            processes: BTreeMap::new(),
        };
        let observer_b = FakeOwnerObserver {
            current_host: Some(test_host_on_machine("host-b", TEST_MACHINE_B, "boot-b")),
            processes: BTreeMap::new(),
        };

        insert_session(&conn, "candidate-on-b", 1, false);
        set_unclean_owner(&conn, "candidate-on-b", &host_b, 701, 1_701, 1);
        insert_v2_recovery_snapshot(&conn, "candidate-on-b", 1, 701, 701);
        insert_session(&conn, "candidate-on-a", 2, false);
        set_unclean_owner(&conn, "candidate-on-a", &host_a, 702, 1_702, 2);
        insert_v2_recovery_snapshot(&conn, "candidate-on-a", 2, 702, 702);

        assert_eq!(
            drive_recovery_selection(&conn, &observer_a, 4).as_deref(),
            Some("candidate-on-a")
        );
        assert_eq!(
            drive_recovery_selection(&conn, &observer_b, 2).as_deref(),
            Some("candidate-on-b"),
            "a persisted cache must not carry one machine's owner verdict onto another machine"
        );
    }

    #[test]
    fn newly_dead_candidate_above_durable_cursor_is_retained_until_promoted() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        insert_session(&conn, "cursor-protected-old", 1, false);
        set_unclean_owner(&conn, "cursor-protected-old", &host_id, 800, 1_800, 1);
        insert_v2_recovery_snapshot(&conn, "cursor-protected-old", 1, 800, 800);

        let mut live_processes = BTreeMap::new();
        for index in 0_i64..65 {
            let session_id = format!("cursor-live-{index:03}");
            let pid = 801 + index;
            let process_start = 1_801 + index;
            insert_session(&conn, &session_id, 2 + index, false);
            set_unclean_owner(&conn, &session_id, &host_id, pid, process_start, 2 + index);
            insert_v2_recovery_snapshot(
                &conn,
                &session_id,
                2 + index,
                u64::try_from(pid).unwrap(),
                u64::try_from(pid).unwrap(),
            );
            live_processes.insert(
                u32::try_from(pid).unwrap(),
                procinfo::ProcessStartTimeObservation::Running(
                    u64::try_from(process_start).unwrap(),
                ),
            );
        }
        let live = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: live_processes.clone(),
        };
        assert_eq!(
            drive_recovery_selection(&conn, &live, 96).as_deref(),
            Some("cursor-protected-old")
        );

        let transaction = begin_retention_transaction(&conn).unwrap();
        assert_eq!(
            newest_usable_recovery_session(&transaction, &live).unwrap(),
            ProtectedRecoverySelection::Pending,
            "the first restarted scan must stop at the exact 64-row boundary"
        );
        transaction.commit().unwrap();

        let mut after_death_processes = live_processes;
        after_death_processes.remove(&865);
        let after_death = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: after_death_processes,
        };
        assert_eq!(
            delete_excess_closed_sessions_with_observer(&conn, 0, &after_death).unwrap(),
            0,
            "a newer candidate that died above the durable cursor must not be deleted behind the older protected point"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM mux_sessions WHERE session_id = 'cursor-live-064'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );

        assert_eq!(
            delete_excess_closed_sessions_with_observer(&conn, 0, &after_death).unwrap(),
            1,
            "the next completed scan must promote the newly dead newest candidate and reclaim the older point"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM mux_sessions WHERE session_id = 'cursor-live-064'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM mux_sessions WHERE session_id = 'cursor-protected-old'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn concurrent_checkpoint_commit_cannot_cross_selection_transaction() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        let selector = Connection::open(&path).unwrap();
        selector
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
            .unwrap();
        selector.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        insert_session(&selector, "concurrent-authority", 1, false);
        set_unclean_owner(&selector, "concurrent-authority", &host_id, 401, 1_401, 1);
        let concurrent_checkpoint =
            insert_v2_recovery_snapshot(&selector, "concurrent-authority", 1, 401, 401);
        assert_eq!(
            drive_recovery_selection(&selector, &dead, 4).as_deref(),
            Some("concurrent-authority")
        );

        let writer = Connection::open(&path).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        writer.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let transaction = begin_retention_transaction(&selector).unwrap();
        assert_eq!(
            newest_usable_recovery_session(&transaction, &dead).unwrap(),
            ProtectedRecoverySelection::Ready(Some(ProtectedRecoveryPoint {
                session_id: "concurrent-authority".to_string(),
                checkpoint_id: concurrent_checkpoint,
            }))
        );
        let blocked = writer.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (
                 'concurrent-authority', 2, 'periodic', 'legacy',
                 0, 0, 'snapshot', '{}'
             )",
            [],
        );
        assert!(
            blocked.as_ref().is_err_and(|error| matches!(
                error.sqlite_error_code(),
                Some(
                    rusqlite::ffi::ErrorCode::DatabaseBusy
                        | rusqlite::ffi::ErrorCode::DatabaseLocked
                )
            )),
            "the immediate selection transaction must serialize checkpoint commits: {blocked:?}"
        );
        transaction.commit().unwrap();
        writer
            .execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, checkpoint_role, topology_json
                 ) VALUES (
                     'concurrent-authority', 2, 'periodic', 'legacy',
                     0, 0, 'snapshot', '{}'
                 )",
                [],
            )
            .unwrap();
        let (state, scan_complete): (String, bool) = selector
            .query_row(
                "SELECT usability.state, selection.scan_complete
                 FROM session_recovery_usability AS usability
                 CROSS JOIN session_recovery_selection AS selection
                 WHERE usability.session_id = 'concurrent-authority'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "dirty");
        assert!(!scan_complete);
    }

    #[test]
    fn every_recovery_source_mutation_invalidates_or_removes_authority() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        let cases = [
            "checkpoint-insert",
            "checkpoint-update",
            "checkpoint-delete",
            "pane-insert",
            "pane-update",
            "pane-delete",
        ];
        let mut checkpoints = BTreeMap::new();
        for (index, session_id) in cases.into_iter().enumerate() {
            insert_session(&conn, session_id, i64::try_from(index).unwrap(), false);
            set_unclean_owner(
                &conn,
                session_id,
                &host_id,
                200 + i64::try_from(index).unwrap(),
                1_200 + i64::try_from(index).unwrap(),
                i64::try_from(index).unwrap(),
            );
            let checkpoint_id = insert_v2_recovery_snapshot(
                &conn,
                session_id,
                i64::try_from(index).unwrap(),
                200 + u64::try_from(index).unwrap(),
                200 + u64::try_from(index).unwrap(),
            );
            checkpoints.insert(session_id, checkpoint_id);
        }
        assert!(drive_recovery_selection(&conn, &dead, 8).is_some());

        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES ('checkpoint-insert', 90, 'periodic', 'legacy', 0, 0,
                       'snapshot', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE session_checkpoints SET topology_json = '{\"changed\":true}'
             WHERE id = ?1",
            [checkpoints["checkpoint-update"]],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM session_checkpoints WHERE id = ?1",
            [checkpoints["checkpoint-delete"]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mux_pane_state (
                 checkpoint_id, pane_id, terminal_state_json
             ) VALUES (?1, 999, '{}')",
            [checkpoints["pane-insert"]],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_pane_state SET terminal_state_json = '{\"changed\":true}'
             WHERE checkpoint_id = ?1",
            [checkpoints["pane-update"]],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM mux_pane_state WHERE checkpoint_id = ?1",
            [checkpoints["pane-delete"]],
        )
        .unwrap();

        for session_id in cases {
            let state: String = conn
                .query_row(
                    "SELECT state FROM session_recovery_usability WHERE session_id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(state, "dirty", "{session_id} mutation must invalidate");
        }
        let scan_complete: bool = conn
            .query_row(
                "SELECT scan_complete FROM session_recovery_selection",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!scan_complete);

        insert_session(&conn, "ack-preserve-authority", 99, false);
        set_unclean_owner(&conn, "ack-preserve-authority", &host_id, 299, 1_299, 99);
        insert_v2_recovery_snapshot(&conn, "ack-preserve-authority", 99, 299, 299);
        assert_eq!(
            drive_recovery_selection(&conn, &dead, 8).as_deref(),
            Some("ack-preserve-authority")
        );
        set_recovery_acknowledgement_with_observer(
            &conn,
            "ack-preserve-authority",
            Some(100),
            &dead,
        )
        .unwrap();
        let (ack_state, ack_scan_complete): (String, bool) = conn
            .query_row(
                "SELECT usability.state, selection.scan_complete
                 FROM session_recovery_usability AS usability
                 CROSS JOIN session_recovery_selection AS selection
                 WHERE usability.session_id = 'ack-preserve-authority'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ack_state, "usable");
        assert!(!ack_scan_complete);
        set_recovery_acknowledgement_with_observer(&conn, "ack-preserve-authority", None, &dead)
            .unwrap();
        let preserved_ack: Option<i64> = conn
            .query_row(
                "SELECT recovery_acknowledged_at FROM mux_sessions
                 WHERE session_id = 'ack-preserve-authority'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved_ack, None);

        insert_session(&conn, "lifecycle-authority", 100, false);
        set_unclean_owner(&conn, "lifecycle-authority", &host_id, 301, 1_301, 100);
        let lifecycle_source =
            insert_v2_recovery_snapshot(&conn, "lifecycle-authority", 100, 301, 301);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (
                 'lifecycle-authority', 101, 'startup', 'pending:rsi2',
                 0, 0, '{\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent'
             )",
            [],
        )
        .unwrap();
        let lifecycle_intent = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at, resolved_at
             ) VALUES (?1, 'lifecycle-authority', ?2, 'resolved', 101, 101)",
            rusqlite::params![lifecycle_intent, lifecycle_source],
        )
        .unwrap();
        assert!(drive_recovery_selection(&conn, &dead, 8).is_some());
        conn.execute(
            "UPDATE restore_attempt_lifecycle
             SET resolved_at = 102
             WHERE intent_checkpoint_id = ?1",
            [lifecycle_intent],
        )
        .unwrap();
        let lifecycle_scan_complete: bool = conn
            .query_row(
                "SELECT scan_complete FROM session_recovery_selection",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!lifecycle_scan_complete);
        conn.execute(
            "DELETE FROM restore_attempt_lifecycle WHERE intent_checkpoint_id = ?1",
            [lifecycle_intent],
        )
        .unwrap();

        insert_session(&conn, "cascade-authority", 100, false);
        set_unclean_owner(&conn, "cascade-authority", &host_id, 300, 1_300, 100);
        insert_v2_recovery_snapshot(&conn, "cascade-authority", 100, 300, 300);
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "DELETE FROM mux_sessions WHERE session_id = 'cascade-authority'",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let orphaned_authority: bool = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM session_recovery_usability
                     WHERE session_id = 'cascade-authority'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!orphaned_authority);
    }

    #[test]
    fn checkpoint_pane_burst_coalesces_to_one_authority_generation() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        insert_session(&conn, "pane-burst", 1, false);
        set_unclean_owner(&conn, "pane-burst", &host_id, 501, 1_501, 1);
        insert_v2_recovery_snapshot(&conn, "pane-burst", 1, 501, 501);
        assert_eq!(
            drive_recovery_selection(&conn, &dead, 4).as_deref(),
            Some("pane-burst")
        );
        let generation_before: i64 = conn
            .query_row(
                "SELECT mutation_generation FROM session_recovery_selection",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let transaction = begin_retention_transaction(&conn).unwrap();
        transaction
            .execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, checkpoint_role, topology_json
                 ) VALUES (
                     'pane-burst', 2, 'periodic', 'pending:snp2',
                     100, 200, 'snapshot', '{}'
                 )",
                [],
            )
            .unwrap();
        let checkpoint_id = transaction.last_insert_rowid();
        for pane_id in 0..100_i64 {
            transaction
                .execute(
                    "INSERT INTO mux_pane_state (
                         checkpoint_id, pane_id, terminal_state_json
                     ) VALUES (?1, ?2, '{}')",
                    rusqlite::params![checkpoint_id, pane_id],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        let (generation_after, state): (i64, String) = conn
            .query_row(
                "SELECT selection.mutation_generation, usability.state
                 FROM session_recovery_selection AS selection
                 CROSS JOIN session_recovery_usability AS usability
                 WHERE usability.session_id = 'pane-burst'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(generation_after, generation_before + 1);
        assert_eq!(state, "dirty");
    }

    #[test]
    fn recovery_retention_and_acknowledgement_survive_database_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        for (session_id, timestamp, pid) in [("restart-old", 10, 51), ("restart-new", 20, 52)] {
            insert_session(&conn, session_id, timestamp, false);
            set_unclean_owner(&conn, session_id, &host_id, pid, 1_000 + pid, timestamp);
            insert_v2_recovery_snapshot(
                &conn,
                session_id,
                timestamp,
                u64::try_from(pid).unwrap(),
                u64::try_from(pid).unwrap(),
            );
        }
        drop(conn);

        let reopened = Connection::open(&path).unwrap();
        reopened.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        assert_eq!(
            delete_excess_closed_sessions_with_observer(&reopened, 0, &dead).unwrap(),
            1
        );
        set_recovery_acknowledgement_with_observer(&reopened, "restart-new", Some(30), &dead)
            .unwrap();
        drop(reopened);

        let restarted = Connection::open(&path).unwrap();
        restarted
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        let acknowledgement: Option<i64> = restarted
            .query_row(
                "SELECT recovery_acknowledged_at FROM mux_sessions
                 WHERE session_id = 'restart-new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(acknowledgement, Some(30));
        assert_eq!(
            delete_excess_closed_sessions_with_observer(&restarted, 0, &dead).unwrap(),
            1
        );
        assert_eq!(count_sessions(&restarted), 0);
    }

    #[test]
    fn exact_size_pressure_counts_owner_lifecycle_and_acknowledged_recovery_bytes() {
        let conn = make_test_db();
        let host_id = encoded_test_host("trj", "boot-a");
        let dead = FakeOwnerObserver {
            current_host: Some(test_host("trj", "boot-a")),
            processes: BTreeMap::new(),
        };
        insert_session(&conn, "sized-crash", 1, false);
        let baseline: i64 = conn
            .query_row(
                "SELECT retained_bytes FROM session_retained_size
                 WHERE session_id = 'sized-crash'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        set_unclean_owner(&conn, "sized-crash", &host_id, 61, 1_061, 1);
        set_recovery_acknowledgement_with_observer(&conn, "sized-crash", Some(2), &dead).unwrap();
        let with_lifecycle: i64 = conn
            .query_row(
                "SELECT retained_bytes FROM session_retained_size
                 WHERE session_id = 'sized-crash'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            with_lifecycle - baseline,
            i64::try_from(host_id.len()).unwrap() + 32,
            "host identity plus owner PID/start/heartbeat and acknowledgement must each be charged exactly once"
        );

        let budget = 1_024_i64 * 1_024;
        let padding = usize::try_from(budget - with_lifecycle).unwrap();
        conn.execute(
            "UPDATE mux_sessions SET topology_json = topology_json || ?2
             WHERE session_id = ?1",
            rusqlite::params!["sized-crash", "x".repeat(padding)],
        )
        .unwrap();
        let at_budget = delete_sessions_by_size_with_observer(&conn, 1, &dead).unwrap();
        assert_eq!(at_budget.measured_bytes, u64::try_from(budget).unwrap());
        assert_eq!(at_budget.deleted, 0);

        conn.execute(
            "UPDATE mux_sessions SET topology_json = topology_json || 'x'
             WHERE session_id = 'sized-crash'",
            [],
        )
        .unwrap();
        let over_budget = delete_sessions_by_size_with_observer(&conn, 1, &dead).unwrap();
        assert_eq!(
            over_budget.measured_bytes,
            u64::try_from(budget + 1).unwrap()
        );
        assert_eq!(over_budget.deleted, 1);
        assert_eq!(over_budget.retained_bytes, 0);
    }

    fn count_sessions(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
            .unwrap()
    }

    fn count_checkpoints(conn: &Connection) -> i64 {
        // Count every persisted snapshot, including the verified zero-pane
        // shutdown snapshot that gives a test fixture destructive clean-state
        // authority. Payload-only assertions must account for that authority
        // snapshot.
        conn.query_row(
            "SELECT COUNT(*) FROM session_checkpoints
             WHERE checkpoint_role = 'snapshot'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn count_pane_states(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap()
    }

    // ---- Age-based cleanup ----

    #[test]
    fn delete_old_closed_sessions() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000; // 31 days ago

        insert_session(&conn, "old-closed", old, true);
        insert_session(&conn, "recent-closed", now - 1000, true);
        insert_session(&conn, "old-active", old, false); // active: should NOT be deleted

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 1); // only old-closed
        assert_eq!(count_sessions(&conn), 2);
    }

    #[test]
    fn age_cleanup_preserves_active_sessions() {
        let conn = make_test_db();
        let old = (epoch_ms() as i64) - 90 * 86_400_000; // 90 days ago

        insert_session(&conn, "active-old", old, false);

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn cleanup_candidate_queries_retain_oversized_session_identity() {
        let conn = make_test_db();
        let old =
            i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer") - 90 * 86_400_000;
        let oversized_session_id = "s".repeat(MAX_CHECKPOINT_SESSION_ID_BYTES + 1);
        conn.execute_batch(
            "DROP TRIGGER mux_sessions_recovery_usability_ai;
             PRAGMA ignore_check_constraints = ON;",
        )
        .expect("enable historical-corruption fixture insertion");
        conn.execute(
            "INSERT INTO mux_sessions (
                 session_id, created_at, last_checkpoint_at, shutdown_clean,
                 topology_json, ft_version
             ) VALUES (?1, ?2, ?2, 1, '{}', '0.1.0')",
            rusqlite::params![oversized_session_id, old],
        )
        .expect("insert oversized session identity fixture");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role
             ) VALUES (?1, ?2, 'periodic', 'legacy:oversized-session',
                       0, 1048576, 'snapshot')",
            rusqlite::params![oversized_session_id, old],
        )
        .expect("insert oversized session checkpoint fixture");
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .expect("restore canonical CHECK enforcement");
        conn.execute_batch(crate::storage::SCHEMA_SQL)
            .expect("restore canonical recovery-usability trigger authority");

        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(delete_excess_closed_sessions(&conn, 0).unwrap(), 0);
        delete_sessions_by_size(&conn, 0)
            .expect_err("oversized session identity must fail exact byte accounting closed");
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn age_cleanup_never_deletes_session_with_corrupt_v2_clean_snapshot() {
        let conn = make_test_db();
        let old =
            i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer") - 90 * 86_400_000;
        insert_session(&conn, "corrupt-v2-clean", old, false);
        let snapshot_id = insert_v2_clean_shutdown_snapshot(&conn, "corrupt-v2-clean", old);
        conn.execute(
            "UPDATE session_checkpoints
             SET metadata_json = '{\"old_to_new\":{\"1\":9}}'
             WHERE id = ?1",
            [snapshot_id],
        )
        .expect("tamper clean snapshot without recomputing witness");

        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn unresolved_restore_intent_remains_blocking_after_a_later_snapshot() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;
        insert_session(&conn, "unresolved-restore", old, true);
        let source_id = insert_checkpoint(&conn, "unresolved-restore", old, 2 * 1_024 * 1_024);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 topology_json
             ) VALUES (
                 'unresolved-restore', ?1, 'startup', 'pending:rsi2',
                 0, 0,
                 '{\"old_to_new\":{},\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent', NULL
             )",
            [old + 1],
        )
        .expect("insert explicit unresolved restore intent");
        let intent_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at
             ) VALUES (?1, 'unresolved-restore', ?2, 'intent', ?3)",
            rusqlite::params![intent_id, source_id, old + 1],
        )
        .expect("bind authoritative unresolved lifecycle");

        // The later snapshot and clean authority make the ordinary clean-session
        // authority valid again, but cannot resolve the older external-effect
        // intent merely by displacing it from the latest-row position.
        insert_checkpoint(&conn, "unresolved-restore", old + 2, 2 * 1_024 * 1_024);
        assert!(
            has_unresolved_restore_intent(&conn, "unresolved-restore")
                .expect("query unresolved intent"),
            "intent {intent_id} must remain visible after later rows"
        );

        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(delete_excess_closed_sessions(&conn, 0).unwrap(), 0);
        let size = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(size.deleted, 0);
        assert!(size.ineligible_shortfall_bytes > 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn corrupt_or_oversized_legacy_receipt_metadata_fails_closed() {
        let conn = make_test_db();
        let cases = [
            ("corrupt-legacy-receipt", "{".to_string()),
            (
                "oversized-legacy-receipt",
                "x".repeat(MAX_CHECKPOINT_METADATA_BYTES + 1),
            ),
        ];

        for (session_id, metadata_json) in cases {
            insert_session(&conn, session_id, 1_000, false);
            conn.execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, metadata_json, checkpoint_role
                 ) VALUES (?1, 1001, 'startup', 'corrupt:legacy-receipt',
                           0, 0, ?2, 'restore_receipt')",
                rusqlite::params![session_id, metadata_json],
            )
            .expect("insert corrupt legacy receipt fixture");
            assert!(
                has_unresolved_restore_intent(&conn, session_id)
                    .expect("classify corrupt legacy receipt"),
                "{session_id} must remain unresolved"
            );
        }
    }

    #[test]
    fn finite_invalid_lifecycle_status_fails_closed_as_unresolved() {
        let conn = make_test_db();
        insert_session(&conn, "invalid-lifecycle-status", 1_000, false);
        let source_id = insert_checkpoint(&conn, "invalid-lifecycle-status", 1_001, 0);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (
                 'invalid-lifecycle-status', 1002, 'startup', 'pending:rsi2',
                 0, 0,
                 '{\"old_to_new\":{},\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent'
             )",
            [],
        )
        .expect("insert intent fixture");
        let intent_id = conn.last_insert_rowid();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .expect("permit lifecycle corruption fixture");
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at, resolved_at
             ) VALUES (?1, 'invalid-lifecycle-status', ?2,
                       'corrupt-status', 1002, 1002)",
            rusqlite::params![intent_id, source_id],
        )
        .expect("insert invalid lifecycle status fixture");
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .expect("restore constraint enforcement");

        assert!(
            has_unresolved_restore_intent(&conn, "invalid-lifecycle-status")
                .expect("classify invalid lifecycle status")
        );
    }

    #[test]
    fn linked_outcome_cannot_resolve_an_intent_without_lifecycle_authority() {
        let conn = make_test_db();
        let old =
            i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer") - 90 * 86_400_000;
        insert_session(&conn, "missing-lifecycle", old, true);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (
                 'missing-lifecycle', ?1, 'startup', 'pending:rsi2',
                 0, 0, '{\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent'
             )",
            [old + 1],
        )
        .expect("insert intent without lifecycle authority");
        let intent_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 restore_intent_checkpoint_id
             ) VALUES (
                 'missing-lifecycle', ?1, 'startup', 'pending:rst2',
                 0, 0, '{\"old_to_new\":{},\"restore_attempt\":{\"phase\":\"outcome\"}}',
                 'restore_receipt', ?2
             )",
            rusqlite::params![old + 2, intent_id],
        )
        .expect("insert linked outcome without lifecycle transition");
        insert_session(&conn, "foreign-lifecycle-owner", old, false);
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at, resolved_at
             ) VALUES (?1, 'foreign-lifecycle-owner', ?2,
                       'resolved', ?3, ?3)",
            rusqlite::params![intent_id, intent_id + 100, old],
        )
        .expect("seed cross-session lifecycle corruption");
        insert_v2_clean_shutdown_snapshot(&conn, "missing-lifecycle", old + 3);

        assert!(
            has_unresolved_restore_intent(&conn, "missing-lifecycle")
                .expect("query missing lifecycle authority")
        );
        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM mux_sessions WHERE session_id = 'missing-lifecycle'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count protected missing-lifecycle session"),
            1
        );
    }

    #[test]
    fn age_cleanup_uses_recent_checkpoint_for_long_lived_closed_session() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let created_at = now - 90 * 86_400_000;
        insert_session(&conn, "long-lived-recently-closed", created_at, true);
        insert_checkpoint(&conn, "long-lived-recently-closed", now - 1_000, 1_024);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = ?1 WHERE session_id = ?2",
            rusqlite::params![now - 1_000, "long-lived-recently-closed"],
        )
        .unwrap();

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn age_cleanup_uses_authoritative_recency_when_cached_metadata_is_stale() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;

        insert_session(&conn, "recent-created-corrupt-cache", now - 1_000, true);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = -1 WHERE session_id = ?1",
            ["recent-created-corrupt-cache"],
        )
        .unwrap();

        insert_session(&conn, "recent-checkpoint-stale-cache", old, true);
        insert_checkpoint(&conn, "recent-checkpoint-stale-cache", now - 500, 1_024);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = ?1 WHERE session_id = ?2",
            rusqlite::params![old, "recent-checkpoint-stale-cache"],
        )
        .unwrap();

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 2);
    }

    // ---- Count-based cleanup ----

    #[test]
    fn delete_excess_closed_sessions_keeps_newest() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..5 {
            insert_session(&conn, &format!("sess-{i}"), now + i * 1000, true);
        }
        settle_test_recovery_selection(&conn);

        let deleted = delete_excess_closed_sessions(&conn, 3).unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(count_sessions(&conn), 3);

        // Verify the 3 newest were kept
        let kept: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT session_id FROM mux_sessions ORDER BY created_at DESC")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(kept, vec!["sess-4", "sess-3", "sess-2"]);
    }

    #[test]
    fn count_cleanup_breaks_equal_timestamps_deterministically() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        for id in ["sess-a", "sess-c", "sess-b"] {
            insert_session(&conn, id, now, true);
        }

        let deleted = delete_excess_closed_sessions(&conn, 1).unwrap();
        assert_eq!(deleted, 2);
        let retained: String = conn
            .query_row("SELECT session_id FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, "sess-c");
    }

    #[test]
    fn count_cleanup_orders_by_checkpoint_recency_over_session_creation() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;
        insert_session(&conn, "recent-by-created", now - 10_000, true);
        insert_session(&conn, "recent-by-checkpoint", old, true);
        insert_checkpoint(&conn, "recent-by-checkpoint", now - 1_000, 1_024);
        let deleted = delete_excess_closed_sessions(&conn, 1).unwrap();
        assert_eq!(deleted, 1);
        let retained: String = conn
            .query_row("SELECT session_id FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, "recent-by-checkpoint");
    }

    // ---- Size-based cleanup ----

    #[test]
    fn delete_sessions_by_size_frees_space() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Create 3 sessions, each with 400KB of checkpoint data
        for i in 0..3 {
            let id = format!("sess-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 400 * 1024); // 400KB each
        }

        // Total: 1200KB. Budget: 1MB (1024KB). Need to free 176KB → delete oldest.
        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1); // Deletes oldest (400KB > 176KB needed)
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 2);
    }

    #[test]
    fn size_cleanup_noop_when_under_budget() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "small", now, true);
        insert_checkpoint(&conn, "small", now, 1024); // 1KB

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.deleted_bytes, 0);
        assert_eq!(outcome.measured_bytes, outcome.retained_bytes);
        assert!(outcome.measured_bytes > 0);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
    }

    #[test]
    fn size_cleanup_honors_exact_budget_and_plus_one_boundary() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "boundary", now, true);
        let checkpoint_id = insert_checkpoint(&conn, "boundary", now, 0);
        let baseline: i64 = conn
            .query_row(
                "SELECT retained_bytes FROM session_retained_size
                 WHERE session_id = 'boundary'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let budget = 1_024_u64 * 1_024;
        let padding_len = usize::try_from(budget - u64::try_from(baseline).unwrap()).unwrap();
        let exact_budget_payload = format!("\"{}\"", "x".repeat(padding_len - 2));
        conn.execute(
            "UPDATE session_checkpoints SET topology_json = ?1 WHERE id = ?2",
            rusqlite::params![exact_budget_payload, checkpoint_id],
        )
        .unwrap();

        let at_budget = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(at_budget.measured_bytes, budget);
        assert_eq!(at_budget.deleted, 0);
        assert_eq!(at_budget.deleted_bytes, 0);
        assert_eq!(at_budget.retained_bytes, budget);
        assert_eq!(at_budget.ineligible_shortfall_bytes, 0);

        let plus_one_payload = format!("\"{}\"", "x".repeat(padding_len - 1));
        conn.execute(
            "UPDATE session_checkpoints SET topology_json = ?1 WHERE id = ?2",
            rusqlite::params![plus_one_payload, checkpoint_id],
        )
        .unwrap();
        let over_budget = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(over_budget.measured_bytes, budget + 1);
        assert_eq!(over_budget.deleted, 1);
        assert_eq!(over_budget.deleted_bytes, budget + 1);
        assert_eq!(over_budget.retained_bytes, 0);
        assert_eq!(over_budget.ineligible_shortfall_bytes, 0);
    }

    #[test]
    fn size_cleanup_rejects_summary_drift_before_any_deletion() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "drifted", now, true);
        insert_checkpoint(&conn, "drifted", now, 2 * 1_024 * 1_024);
        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = checkpoint_row_bytes + 1
             WHERE session_id = 'drifted'",
            [],
        )
        .unwrap();

        let error = delete_sessions_by_size(&conn, 1)
            .expect_err("trigger authority drift must fail before eviction");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 2);
    }

    #[test]
    fn retained_size_trigger_overflow_and_underflow_are_atomic() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "arithmetic", now, false);
        let session_bytes: i64 = conn
            .query_row(
                "SELECT session_row_bytes FROM session_retained_size
                 WHERE session_id = 'arithmetic'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = ?1
             WHERE session_id = 'arithmetic'",
            [i64::MAX - session_bytes],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT OR IGNORE INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes
                 ) VALUES ('arithmetic', ?1, 'periodic', 'hash', 0, 0)",
                [now],
            )
            .is_err(),
            "RAISE(ABORT) must defeat outer OR IGNORE at the i64 boundary"
        );
        assert_eq!(count_checkpoints(&conn), 0);

        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = 0
             WHERE session_id = 'arithmetic'",
            [],
        )
        .unwrap();
        let checkpoint_id = insert_checkpoint(&conn, "arithmetic", now, 0);
        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = 0
             WHERE session_id = 'arithmetic'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "DELETE FROM session_checkpoints WHERE id = ?1",
                [checkpoint_id],
            )
            .is_err(),
            "trigger subtraction below zero must roll back the deletion"
        );
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn pane_and_lifecycle_overflow_guards_defeat_outer_or_ignore() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "additive-guards", now, false);
        let source_id = insert_checkpoint(&conn, "additive-guards", now, 0);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role
             ) VALUES ('additive-guards', ?1, 'startup', 'intent', 0, 0,
                       'restore_intent')",
            [now + 1],
        )
        .unwrap();
        let intent_id = conn.last_insert_rowid();

        conn.execute(
            "UPDATE session_retained_size
             SET pane_state_row_bytes = 9223372036854775807 - retained_bytes
             WHERE session_id = 'additive-guards'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT OR IGNORE INTO mux_pane_state (
                     checkpoint_id, pane_id, terminal_state_json
                 ) VALUES (?1, 7, '{}')",
                [source_id],
            )
            .is_err()
        );
        assert_eq!(count_pane_states(&conn), 0);

        conn.execute(
            "UPDATE session_retained_size
             SET pane_state_row_bytes = 0,
                 restore_lifecycle_row_bytes =
                     9223372036854775807
                     - (session_row_bytes + checkpoint_row_bytes)
             WHERE session_id = 'additive-guards'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT OR IGNORE INTO restore_attempt_lifecycle (
                     intent_checkpoint_id, session_id, source_checkpoint_id,
                     status, created_at
                 ) VALUES (?1, 'additive-guards', ?2, 'intent', ?3)",
                rusqlite::params![intent_id, source_id, now + 2],
            )
            .is_err()
        );
        let lifecycle_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM restore_attempt_lifecycle",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lifecycle_rows, 0);
    }

    #[test]
    fn every_retained_size_update_guard_defeats_outer_or_ignore() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "session-update-guard", now, false);
        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = 9223372036854775807 - retained_bytes
             WHERE session_id = 'session-update-guard'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE OR IGNORE mux_sessions
                 SET topology_json = '{\"expanded\":true}'
                 WHERE session_id = 'session-update-guard'",
                [],
            )
            .is_err()
        );
        let topology_json: String = conn
            .query_row(
                "SELECT topology_json FROM mux_sessions
                 WHERE session_id = 'session-update-guard'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(topology_json, "{}");

        insert_session(&conn, "checkpoint-update-guard", now + 1, false);
        let checkpoint_id = insert_checkpoint(&conn, "checkpoint-update-guard", now + 1, 0);
        conn.execute(
            "UPDATE session_retained_size
             SET pane_state_row_bytes = 9223372036854775807 - retained_bytes
             WHERE session_id = 'checkpoint-update-guard'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE OR IGNORE session_checkpoints
                 SET state_hash = state_hash || 'x' WHERE id = ?1",
                [checkpoint_id],
            )
            .is_err()
        );
        let state_hash: String = conn
            .query_row(
                "SELECT state_hash FROM session_checkpoints WHERE id = ?1",
                [checkpoint_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state_hash, "0123456789abcdef");

        insert_session(&conn, "pane-update-guard", now + 2, false);
        let pane_checkpoint_id = insert_checkpoint(&conn, "pane-update-guard", now + 2, 0);
        conn.execute(
            "INSERT INTO mux_pane_state (
                 checkpoint_id, pane_id, terminal_state_json
             ) VALUES (?1, 7, '{}')",
            [pane_checkpoint_id],
        )
        .unwrap();
        let pane_state_id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE session_retained_size
             SET restore_lifecycle_row_bytes = 9223372036854775807 - retained_bytes
             WHERE session_id = 'pane-update-guard'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE OR IGNORE mux_pane_state
                 SET terminal_state_json = '{\"expanded\":true}' WHERE id = ?1",
                [pane_state_id],
            )
            .is_err()
        );
        let terminal_state_json: String = conn
            .query_row(
                "SELECT terminal_state_json FROM mux_pane_state WHERE id = ?1",
                [pane_state_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminal_state_json, "{}");

        insert_session(&conn, "lifecycle-update-guard", now + 3, false);
        let source_id = insert_checkpoint(&conn, "lifecycle-update-guard", now + 3, 0);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role
             ) VALUES (
                 'lifecycle-update-guard', ?1, 'startup', 'intent', 0, 0,
                 'restore_intent'
             )",
            [now + 4],
        )
        .unwrap();
        let intent_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at
             ) VALUES (?1, 'lifecycle-update-guard', ?2, 'intent', ?3)",
            rusqlite::params![intent_id, source_id, now + 5],
        )
        .unwrap();
        conn.execute(
            "UPDATE session_retained_size
             SET pane_state_row_bytes = 9223372036854775807 - retained_bytes
             WHERE session_id = 'lifecycle-update-guard'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE OR IGNORE restore_attempt_lifecycle
                 SET status = 'reconciliation_required'
                 WHERE intent_checkpoint_id = ?1",
                [intent_id],
            )
            .is_err()
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1",
                [intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "intent");
    }

    #[test]
    fn size_cleanup_evicts_by_checkpoint_recency_over_session_creation() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;
        insert_session(&conn, "genuinely-old", old, true);
        insert_checkpoint(&conn, "genuinely-old", old, 700 * 1_024);
        insert_session(&conn, "recent-by-checkpoint", old - 1, true);
        insert_checkpoint(&conn, "recent-by-checkpoint", now - 1_000, 700 * 1_024);
        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        let retained: String = conn
            .query_row("SELECT session_id FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, "recent-by-checkpoint");
    }

    #[test]
    fn size_cleanup_rejects_negative_checkpoint_accounting_without_deleting() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "corrupt-size", now, true);
        insert_checkpoint(&conn, "corrupt-size", now, -1);

        let error = delete_sessions_by_size(&conn, 1)
            .expect_err("negative byte accounting must fail closed");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn sqlite_timestamp_conversion_rejects_values_that_would_wrap_or_overdelete() {
        assert_eq!(
            u64_to_sqlite_integer(u64::try_from(i64::MAX).unwrap()).unwrap(),
            i64::MAX
        );
        assert!(u64_to_sqlite_integer(u64::MAX).is_err());
    }

    #[test]
    fn size_cleanup_rolls_back_partial_deletes_when_a_later_delete_fails() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        for i in 0..3 {
            let id = format!("sess-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 700 * 1024);
        }
        conn.execute_batch(
            "CREATE TABLE synthetic_delete_guard (
                 session_id TEXT PRIMARY KEY
                     REFERENCES mux_sessions(session_id) ON DELETE RESTRICT
             );
             INSERT INTO synthetic_delete_guard(session_id) VALUES ('sess-1');",
        )
        .unwrap();

        delete_sessions_by_size(&conn, 1)
            .expect_err("the second candidate must abort the size-cleanup transaction");
        assert_eq!(count_sessions(&conn), 3);
        assert_eq!(count_checkpoints(&conn), 6);
    }

    #[test]
    fn size_cleanup_rejects_unaudited_ignore_trigger_before_mutation() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        for i in 0..2 {
            let id = format!("sess-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 700 * 1024);
        }
        conn.execute_batch(
            "CREATE TRIGGER ignore_first_size_delete
             BEFORE DELETE ON mux_sessions
             WHEN OLD.session_id = 'sess-0'
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();

        let error = delete_sessions_by_size(&conn, 1)
            .expect_err("an unaudited ignore trigger must fail before any deletion");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 2);
        assert_eq!(count_checkpoints(&conn), 4);
    }

    #[test]
    fn size_cleanup_rejects_temp_schema_triggers_before_mutation() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "sess-temp", now, true);
        insert_checkpoint(&conn, "sess-temp", now, 2 * 1024 * 1024);
        conn.execute_batch(
            "CREATE TEMP TRIGGER ignore_temp_size_delete
             BEFORE DELETE ON mux_sessions
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();

        delete_sessions_by_size(&conn, 1)
            .expect_err("TEMP triggers on authoritative tables must fail closed");
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 2);
    }

    #[test]
    fn retention_trigger_guard_is_identifier_case_insensitive() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "sess-case", now, true);
        insert_checkpoint(&conn, "sess-case", now, 2 * 1024 * 1024);
        conn.execute_batch(
            "CREATE TRIGGER ignore_uppercase_size_delete
             BEFORE DELETE ON MUX_SESSIONS
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();

        delete_sessions_by_size(&conn, 1)
            .expect_err("identifier casing must not bypass the persistent trigger guard");
        assert_eq!(count_sessions(&conn), 1);

        conn.execute_batch(
            "DROP TRIGGER ignore_uppercase_size_delete;
             CREATE TEMP TRIGGER ignore_mixed_case_size_delete
             BEFORE DELETE ON MuX_sEsSiOnS
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();
        delete_sessions_by_size(&conn, 1)
            .expect_err("identifier casing must not bypass the TEMP trigger guard");
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 2);
    }

    #[test]
    fn retention_rejects_same_name_trigger_body_drift_before_deletion() {
        let conn = make_test_db();
        let old = i64::try_from(epoch_ms()).unwrap() - 90 * 86_400_000;
        insert_session(&conn, "same-name-drift", old, true);
        conn.execute_batch(
            "DROP TRIGGER mux_sessions_retained_size_ad;
             CREATE TRIGGER mux_sessions_retained_size_ad
             AFTER DELETE ON mux_sessions BEGIN SELECT 1; END;",
        )
        .expect("replace canonical delete trigger with a same-name no-op");

        let error = delete_sessions_by_age(&conn, 30)
            .expect_err("same-name trigger drift must fail before age deletion");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn retention_rejects_extra_summary_table_trigger_before_deletion() {
        let conn = make_test_db();
        let old = i64::try_from(epoch_ms()).unwrap() - 90 * 86_400_000;
        insert_session(&conn, "summary-trigger-drift", old, true);
        conn.execute_batch(
            "CREATE TRIGGER unaudited_summary_trigger
             AFTER UPDATE ON session_retained_size BEGIN SELECT 1; END;",
        )
        .expect("install unaudited retained-size summary trigger");

        let error = delete_sessions_by_age(&conn, 30)
            .expect_err("a summary-table trigger must fail before age deletion");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn size_cleanup_reports_shortfall_when_active_data_alone_exceeds_budget() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "active-large", now, false);
        insert_checkpoint(&conn, "active-large", now, 2 * 1024 * 1024);
        insert_session(&conn, "closed-small", now - 1, true);
        insert_checkpoint(&conn, "closed-small", now - 1, 400 * 1024);

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1024 * 1024
        );
        assert_eq!(
            outcome.measured_bytes,
            outcome.deleted_bytes + outcome.retained_bytes
        );
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn size_receipt_and_active_shortfall_remain_consistent_across_restart() {
        let file = tempfile::NamedTempFile::new().expect("temporary retention database");
        let path = file.path().to_path_buf();
        let conn = Connection::open(&path).expect("open retention database");
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        let now = epoch_ms() as i64;
        insert_session(&conn, "restart-active", now, false);
        insert_checkpoint(&conn, "restart-active", now, 2 * 1_024 * 1_024);
        insert_session(&conn, "restart-closed", now - 1, true);
        insert_checkpoint(&conn, "restart-closed", now - 1, 400 * 1_024);
        drop(conn);

        let reopened = Connection::open(&path).expect("reopen before cleanup");
        reopened.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let outcome = delete_sessions_by_size(&reopened, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(
            outcome.measured_bytes,
            outcome.deleted_bytes + outcome.retained_bytes
        );
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1_024 * 1_024
        );
        drop(reopened);

        let restarted = Connection::open(&path).expect("reopen after cleanup");
        let retained: i64 = restarted
            .query_row(
                "SELECT COALESCE(SUM(retained_bytes), 0)
                 FROM session_retained_size",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(u64::try_from(retained).unwrap(), outcome.retained_bytes);
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1_024 * 1_024
        );
    }

    #[test]
    fn size_cleanup_charges_and_deletes_closed_session_without_payload_checkpoint() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "active-large", now, false);
        insert_checkpoint(&conn, "active-large", now, 2 * 1024 * 1024);
        insert_session(&conn, "closed-without-checkpoint", now - 1, true);

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        assert!(outcome.deleted_bytes > 0);
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1024 * 1024
        );
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn size_cleanup_excludes_orphan_bytes_from_eviction_and_shortfall() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "valid-closed", now, true);
        insert_checkpoint(&conn, "valid-closed", now, 400 * 1024);

        // Simulate legacy/corrupt orphan data. Its 2 MiB is reclaimed by the
        // orphan phase, not by evicting an unrelated valid session.
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::Checkpoint, |conn| {
            conn.execute(
                "INSERT INTO session_checkpoints
                 (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
                 VALUES ('orphan-session', ?1, 'periodic', 'orphan', 0, ?2)",
                rusqlite::params![now, 2 * 1024 * 1024],
            )
            .unwrap();
        });

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.deleted_bytes, 0);
        assert_eq!(outcome.measured_bytes, outcome.retained_bytes);
        assert!(outcome.measured_bytes > 0);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 3);

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_restore_lifecycle_rows, 0);
        assert_eq!(orphaned.orphaned_checkpoints, 1);
        assert_eq!(orphaned.orphaned_pane_states, 0);
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 2);
    }

    // ---- Cascade delete ----

    #[test]
    fn session_delete_cascades_to_checkpoints_and_pane_state() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000;

        insert_session(&conn, "old-sess", old, true);
        let cp_id = insert_checkpoint(&conn, "old-sess", old, 1024);
        insert_pane_state(&conn, cp_id, 1);
        insert_pane_state(&conn, cp_id, 2);

        assert_eq!(count_checkpoints(&conn), 2);
        assert_eq!(count_pane_states(&conn), 2);

        delete_sessions_by_age(&conn, 30).unwrap();

        assert_eq!(count_sessions(&conn), 0);
        assert_eq!(count_checkpoints(&conn), 0);
        assert_eq!(count_pane_states(&conn), 0);
    }

    // ---- Orphaned data cleanup ----

    #[test]
    fn cleanup_orphaned_checkpoints() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Create a session and checkpoint normally
        insert_session(&conn, "valid", now, true);
        insert_checkpoint(&conn, "valid", now, 1024);

        // Temporarily disable FK to insert orphaned checkpoint (simulates corruption)
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::Checkpoint, |conn| {
            conn.execute(
                "INSERT INTO session_checkpoints
                 (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
                 VALUES ('orphan-sess', ?1, 'periodic', '0123456789abcdef', 0, 0)",
                [now],
            )
            .unwrap();
        });

        assert_eq!(count_checkpoints(&conn), 3);

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_checkpoints, 1);
        assert_eq!(count_checkpoints(&conn), 2);
    }

    #[test]
    fn cleanup_orphaned_pane_states() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "valid", now, true);
        let cp_id = insert_checkpoint(&conn, "valid", now, 1024);
        insert_pane_state(&conn, cp_id, 1);

        // Temporarily disable FK to insert orphaned pane_state (simulates corruption)
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::PaneState, |conn| {
            conn.execute(
                "INSERT INTO mux_pane_state
                 (checkpoint_id, pane_id, terminal_state_json)
                 VALUES (99999, 42, '{}')",
                [],
            )
            .unwrap();
        });

        assert_eq!(count_pane_states(&conn), 2);

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_pane_states, 1);
        assert_eq!(count_pane_states(&conn), 1);
    }

    #[test]
    fn cleanup_collects_pane_state_children_of_orphan_checkpoint() {
        // ft-rt6ol: a checkpoint orphaned from mux_sessions that STILL has
        // mux_pane_state children must be fully collected in ONE pass. The old
        // pane_state-first ordering left the children behind (the child's
        // checkpoint still existed at the first DELETE, then was removed by the
        // second DELETE, orphaning the child).
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Orphan checkpoint (ghost session) WITH a pane_state child, inserted
        // with FK enforcement off to simulate an older/corrupt DB.
        let orphan_cp_id = seed_legacy_orphan(
            &conn,
            LegacyOrphanInsertSurface::Checkpoint,
            |conn| {
                conn.execute(
                    "INSERT INTO session_checkpoints
                     (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
                     VALUES ('orphan-sess', ?1, 'periodic', '0123456789abcdef', 1, 0)",
                    [now],
                )
                .unwrap();
                conn.last_insert_rowid()
            },
        );
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::PaneState, |conn| {
            conn.execute(
                "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
                 VALUES (?1, 7, '{}')",
                [orphan_cp_id],
            )
            .unwrap();
        });

        assert_eq!(count_checkpoints(&conn), 1);
        assert_eq!(count_pane_states(&conn), 1);

        // One pass must remove BOTH the orphan checkpoint and its child.
        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(
            orphaned.orphaned_checkpoints, 1,
            "orphan checkpoint removed"
        );
        assert_eq!(
            orphaned.orphaned_pane_states, 1,
            "its pane_state child collected in the same pass"
        );
        assert_eq!(count_checkpoints(&conn), 0);
        assert_eq!(
            count_pane_states(&conn),
            0,
            "no orphan pane_state may remain after one cleanup pass (ft-rt6ol)"
        );
    }

    #[test]
    fn cleanup_removes_orphan_lifecycle_before_its_intent_checkpoint() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let intent_id = seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::Checkpoint, |conn| {
            conn.execute(
                "INSERT INTO session_checkpoints (
                         session_id, checkpoint_at, checkpoint_type, state_hash,
                         pane_count, total_bytes, metadata_json, checkpoint_role
                     ) VALUES (
                         'orphan-attempt', ?1, 'startup', 'pending:rsi2',
                         0, 0, '{\"restore_attempt\":{\"phase\":\"intent\"}}',
                         'restore_intent'
                     )",
                [now],
            )
            .expect("seed orphan restore intent");
            conn.last_insert_rowid()
        });
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::RestoreLifecycle, |conn| {
            conn.execute(
                "INSERT INTO restore_attempt_lifecycle (
                         intent_checkpoint_id, session_id, source_checkpoint_id,
                         status, created_at
                     ) VALUES (?1, 'orphan-attempt', ?2,
                               'reconciliation_required', ?3)",
                rusqlite::params![intent_id, intent_id + 1, now],
            )
            .expect("seed orphan restore lifecycle");
        });

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_restore_lifecycle_rows, 1);
        assert_eq!(orphaned.orphaned_checkpoints, 1);
        assert_eq!(orphaned.orphaned_pane_states, 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM restore_attempt_lifecycle",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count retained lifecycle rows"),
            0
        );
    }

    #[test]
    fn full_cleanup_reports_lifecycle_only_orphan_as_completed_work() {
        let conn = make_test_db();
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::RestoreLifecycle, |conn| {
            conn.execute(
                "INSERT INTO restore_attempt_lifecycle (
                         intent_checkpoint_id, session_id, source_checkpoint_id,
                         status, created_at
                     ) VALUES (999, 'missing-session', 998, 'intent', 1000)",
                [],
            )
            .expect("seed lifecycle-only orphan");
        });

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };
        let result = cleanup_sessions(&conn, &config).expect("clean lifecycle-only orphan");

        assert_eq!(result.total_sessions_deleted(), 0);
        assert_eq!(result.orphaned_restore_lifecycle_rows, 1);
        assert_eq!(result.orphaned_checkpoints, 0);
        assert_eq!(result.orphaned_pane_states, 0);
        assert!(result.any_work_done());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM restore_attempt_lifecycle",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count remaining lifecycle rows"),
            0
        );
    }

    // ---- Full cleanup pipeline ----

    #[test]
    fn full_cleanup_with_defaults() {
        let conn = make_test_db();
        let config = SessionRetentionConfig::default();
        let now = epoch_ms() as i64;

        // Insert some sessions within retention period
        insert_session(&conn, "recent", now, true);
        insert_checkpoint(&conn, "recent", now, 1024);

        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.total_sessions_deleted(), 0);
    }

    #[test]
    fn full_cleanup_disabled_policies() {
        let conn = make_test_db();
        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.total_sessions_deleted(), 0);
    }

    #[test]
    fn cleanup_receipt_requires_prompt_retry_after_bounded_reconciliation() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        for index in 0..5 {
            insert_session(
                &conn,
                &format!("pending-active-{index}"),
                now + i64::from(index),
                false,
            );
        }
        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };

        let first = cleanup_sessions(&conn, &config).expect("bounded reconciliation step");
        assert!(first.recovery_reconciliation_pending);
        assert!(first.any_work_done());
        assert_eq!(first.total_sessions_deleted(), 0);

        let second = cleanup_sessions(&conn, &config).expect("reconciled cleanup step");
        assert!(!second.recovery_reconciliation_pending);
        assert_eq!(second.total_sessions_deleted(), 0);
        assert_eq!(count_sessions(&conn), 5);
    }

    // ---- Config defaults ----

    #[test]
    fn config_defaults_sensible() {
        let config = SessionRetentionConfig::default();
        assert_eq!(config.max_age_days, 30);
        assert_eq!(config.max_closed_sessions, 50);
        assert_eq!(config.max_total_size_mb, 500);
        assert_eq!(config.cleanup_interval_hours, 24);
    }

    // ---- CleanupResult helpers ----

    #[test]
    fn cleanup_result_total() {
        let result = CleanupResult {
            deleted_by_age: 3,
            deleted_by_count: 2,
            deleted_by_size: 1,
            ..Default::default()
        };
        assert_eq!(result.total_sessions_deleted(), 6);
        assert!(result.any_work_done());
    }

    #[test]
    fn cleanup_result_empty() {
        let result = CleanupResult::default();
        assert!(!result.recovery_reconciliation_pending);
        assert_eq!(result.total_sessions_deleted(), 0);
        assert!(!result.any_work_done());
    }

    // ====================================================================
    // CleanupResult additional tests
    // ====================================================================

    #[test]
    fn cleanup_result_any_work_done_orphan_checkpoints_only() {
        let result = CleanupResult {
            orphaned_checkpoints: 5,
            ..Default::default()
        };
        assert!(result.any_work_done());
        assert_eq!(result.total_sessions_deleted(), 0);
    }

    #[test]
    fn cleanup_result_any_work_done_orphan_pane_states_only() {
        let result = CleanupResult {
            orphaned_pane_states: 3,
            ..Default::default()
        };
        assert!(result.any_work_done());
    }

    #[test]
    fn cleanup_result_any_work_done_orphan_restore_lifecycle_only() {
        let result = CleanupResult {
            orphaned_restore_lifecycle_rows: 2,
            ..Default::default()
        };
        assert!(result.any_work_done());
        assert_eq!(result.total_sessions_deleted(), 0);
    }

    #[test]
    fn cleanup_result_debug() {
        let result = CleanupResult {
            deleted_by_age: 1,
            deleted_by_count: 2,
            deleted_by_size: 3,
            orphaned_restore_lifecycle_rows: 6,
            orphaned_checkpoints: 4,
            orphaned_pane_states: 5,
            ..Default::default()
        };
        let dbg = format!("{result:?}");
        assert!(dbg.contains("CleanupResult"));
        assert!(dbg.contains("orphaned_restore_lifecycle_rows"));
    }

    #[test]
    fn cleanup_result_clone() {
        let result = CleanupResult {
            deleted_by_age: 10,
            ..Default::default()
        };
        let result2 = result.clone();
        assert_eq!(result2.deleted_by_age, 10);
        assert_eq!(result2.total_sessions_deleted(), 10);
    }

    #[test]
    fn cleanup_result_total_combines_all_sources() {
        let result = CleanupResult {
            deleted_by_age: 5,
            deleted_by_count: 3,
            deleted_by_size: 2,
            ..Default::default()
        };
        assert_eq!(result.total_sessions_deleted(), 10);
    }

    #[test]
    fn cleanup_result_any_work_done_all_zero_sessions_but_orphans() {
        let result = CleanupResult {
            deleted_by_age: 0,
            deleted_by_count: 0,
            deleted_by_size: 0,
            orphaned_restore_lifecycle_rows: 0,
            orphaned_checkpoints: 1,
            orphaned_pane_states: 0,
            ..Default::default()
        };
        assert!(result.any_work_done());
    }

    // ====================================================================
    // epoch_ms test
    // ====================================================================

    #[test]
    fn epoch_ms_returns_positive() {
        let ms = epoch_ms();
        assert!(ms > 0);
    }

    #[test]
    fn epoch_ms_reasonable_range() {
        // Should be after 2024-01-01 (1704067200000ms)
        let ms = epoch_ms();
        assert!(ms > 1_704_067_200_000);
    }

    // ====================================================================
    // Config serde
    // ====================================================================

    #[test]
    fn config_serde_roundtrip() {
        let config = SessionRetentionConfig {
            max_age_days: 60,
            max_closed_sessions: 100,
            max_total_size_mb: 1000,
            cleanup_interval_hours: 12,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRetentionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_age_days, 60);
        assert_eq!(back.max_closed_sessions, 100);
        assert_eq!(back.max_total_size_mb, 1000);
        assert_eq!(back.cleanup_interval_hours, 12);
    }

    // ====================================================================
    // Edge case: empty database cleanup
    // ====================================================================

    #[test]
    fn cleanup_empty_database() {
        let conn = make_test_db();
        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 50,
            max_total_size_mb: 500,
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.total_sessions_deleted(), 0);
        assert!(!result.any_work_done());
    }

    // ====================================================================
    // Edge case: only active sessions
    // ====================================================================

    #[test]
    fn cleanup_only_active_sessions_deletes_nothing() {
        let conn = make_test_db();
        let old = (epoch_ms() as i64) - 90 * 86_400_000; // 90 days ago

        // All sessions are active (shutdown_clean = false)
        for i in 0..5 {
            insert_session(&conn, &format!("active-{i}"), old, false);
        }

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 2,
            max_total_size_mb: 0, // disabled
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.deleted_by_age, 0);
        // excess closed sessions check also skips active
        assert_eq!(count_sessions(&conn), 5);
    }

    // ====================================================================
    // Online cleanup issues no explicit compaction; this fixture pins NONE
    // ====================================================================

    #[test]
    fn cleanup_leaves_reclaimable_pages_for_sqlite_reuse_after_many_deletions() {
        let conn = make_test_db();
        let auto_vacuum: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            auto_vacuum, 0,
            "freelist-reuse proof requires auto_vacuum=NONE"
        );
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000;

        // Large inline topology values make the deleted rows occupy enough
        // pages for freelist_count to be a deterministic physical-compaction
        // witness. Full VACUUM would drive this count back to zero.
        let payload = "x".repeat(32 * 1024);
        for i in 0..12 {
            insert_session_with_topology(&conn, &format!("old-{i}"), old, true, &payload);
        }
        settle_test_recovery_selection(&conn);

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.deleted_by_age, 12);
        let reusable_pages: i64 = conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert!(
            reusable_pages > 0,
            "retention cleanup must preserve freed pages for SQLite reuse"
        );

        // A sentinel interactive write must remain available immediately after
        // cleanup; under this pinned NONE policy, cleanup does not compact.
        insert_session(&conn, "interactive-sentinel", now, false);
        assert_eq!(count_sessions(&conn), 1);
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ───────────────────────────────────

    // ── CleanupResult trait coverage ──

    #[test]
    fn cleanup_result_default_all_zeros() {
        let r = CleanupResult::default();
        assert_eq!(r.deleted_by_age, 0);
        assert_eq!(r.deleted_by_count, 0);
        assert_eq!(r.deleted_by_size, 0);
        assert_eq!(r.size_measured_bytes, 0);
        assert_eq!(r.size_deleted_bytes, 0);
        assert_eq!(r.size_retained_bytes, 0);
        assert_eq!(r.size_ineligible_shortfall_bytes, 0);
        assert_eq!(r.orphaned_restore_lifecycle_rows, 0);
        assert_eq!(r.orphaned_checkpoints, 0);
        assert_eq!(r.orphaned_pane_states, 0);
        assert_eq!(r.total_sessions_deleted(), 0);
        assert!(!r.any_work_done());
    }

    #[test]
    fn cleanup_result_total_single_source_age() {
        let r = CleanupResult {
            deleted_by_age: 7,
            ..Default::default()
        };
        assert_eq!(r.total_sessions_deleted(), 7);
        assert!(r.any_work_done());
    }

    #[test]
    fn cleanup_result_total_single_source_count() {
        let r = CleanupResult {
            deleted_by_count: 4,
            ..Default::default()
        };
        assert_eq!(r.total_sessions_deleted(), 4);
        assert!(r.any_work_done());
    }

    #[test]
    fn cleanup_result_total_single_source_size() {
        let r = CleanupResult {
            deleted_by_size: 2,
            ..Default::default()
        };
        assert_eq!(r.total_sessions_deleted(), 2);
        assert!(r.any_work_done());
    }

    // ── Multi-phase cleanup ──

    #[test]
    fn full_cleanup_all_phases_fire() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000; // 31 days ago

        // 1 old closed session → age cleanup
        insert_session(&conn, "age-victim", old, true);

        // 5 recent closed sessions → count cleanup (keep only 2)
        for i in 0..5 {
            let id = format!("recent-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            // give each 600KB of checkpoint data → total 3000KB > 2MB budget for last 2
            insert_checkpoint(&conn, &id, now + i * 1000, 600 * 1024);
        }
        settle_test_recovery_selection(&conn);

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 2,
            max_total_size_mb: 2,
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();

        // age should delete "age-victim"
        assert!(result.deleted_by_age >= 1);
        // count should delete excess beyond 2
        // After age delete: 5 remain, keep 2 → delete 3
        assert!(result.deleted_by_count >= 1);
    }

    // ── Size cleanup: multiple deletions needed ──

    #[test]
    fn size_cleanup_deletes_multiple_sessions() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // 4 sessions × 400 KiB = 1600 KiB.
        for i in 0..4 {
            let id = format!("big-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 400 * 1024);
        }
        settle_test_recovery_selection(&conn);

        // Budget: 1 MiB = 1024 KiB. Total is 1600 KiB, so freeing 576 KiB
        // deterministically deletes the two oldest 400 KiB sessions.
        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 2);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 2);
    }

    // ── Excess closed: mixed active and closed ──

    #[test]
    fn excess_closed_ignores_active_sessions() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // 3 active sessions (should NOT be counted or deleted)
        for i in 0..3 {
            insert_session(&conn, &format!("active-{i}"), now + i * 1000, false);
        }

        // 4 closed sessions → keep 2 → delete 2
        for i in 0..4 {
            insert_session(&conn, &format!("closed-{i}"), now + (i + 3) * 1000, true);
        }
        settle_test_recovery_selection(&conn);

        let deleted = delete_excess_closed_sessions(&conn, 2).unwrap();
        assert_eq!(deleted, 2);
        // All 3 active remain + 2 closed = 5
        assert_eq!(count_sessions(&conn), 5);
    }

    #[test]
    fn excess_closed_noop_when_under_limit() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "s1", now, true);
        insert_session(&conn, "s2", now + 1000, true);

        let deleted = delete_excess_closed_sessions(&conn, 10).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 2);
    }

    // ── Age boundary ──

    #[test]
    fn age_cleanup_boundary_exactly_at_cutoff() {
        let conn = make_test_db();
        let now = 1_800_000_000_000_u64;
        let exactly_at_cutoff = i64::try_from(now - 30 * 86_400_000).unwrap();

        insert_session(&conn, "boundary", exactly_at_cutoff, true);

        let observer = SystemSessionOwnerObserver::new();
        let deleted = delete_sessions_by_age_phase_at_with_observer(&conn, 30, &observer, now)
            .unwrap()
            .value;
        assert_eq!(deleted, 0);
    }

    #[test]
    fn age_cleanup_one_ms_past_cutoff() {
        let conn = make_test_db();
        let now = 1_800_000_000_000_u64;
        let just_past = i64::try_from(now - 30 * 86_400_000 - 1).unwrap();

        insert_session(&conn, "just-past", just_past, true);

        let observer = SystemSessionOwnerObserver::new();
        let deleted = delete_sessions_by_age_phase_at_with_observer(&conn, 30, &observer, now)
            .unwrap()
            .value;
        assert_eq!(deleted, 1);
    }

    // ── Cascade: multiple checkpoints per session ──

    #[test]
    fn cascade_deletes_multiple_checkpoints_and_pane_states() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000;

        insert_session(&conn, "multi-cp", old, true);
        let cp1 = insert_checkpoint(&conn, "multi-cp", old, 1024);
        let cp2 = insert_checkpoint(&conn, "multi-cp", old + 1000, 2048);
        let cp3 = insert_checkpoint(&conn, "multi-cp", old + 2000, 512);
        insert_pane_state(&conn, cp1, 1);
        insert_pane_state(&conn, cp1, 2);
        insert_pane_state(&conn, cp2, 3);
        insert_pane_state(&conn, cp3, 4);
        insert_pane_state(&conn, cp3, 5);

        assert_eq!(count_checkpoints(&conn), 4);
        assert_eq!(count_pane_states(&conn), 5);

        delete_sessions_by_age(&conn, 30).unwrap();

        assert_eq!(count_sessions(&conn), 0);
        assert_eq!(count_checkpoints(&conn), 0);
        assert_eq!(count_pane_states(&conn), 0);
    }

    // ── Orphan cleanup noop ──

    #[test]
    fn orphan_cleanup_noop_when_no_orphans() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "valid", now, true);
        let cp = insert_checkpoint(&conn, "valid", now, 1024);
        insert_pane_state(&conn, cp, 1);

        assert_eq!(
            cleanup_orphaned_data(&conn).unwrap(),
            OrphanCleanupOutcome::default()
        );
    }

    #[test]
    fn orphan_cleanup_on_empty_db() {
        let conn = make_test_db();
        assert_eq!(
            cleanup_orphaned_data(&conn).unwrap(),
            OrphanCleanupOutcome::default()
        );
    }

    // ── Size cleanup: sessions with only clean-authority snapshots ──

    #[test]
    fn size_cleanup_with_only_session_and_clean_snapshot_payloads() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Session rows and their zero-pane clean snapshots are charged even
        // without captured pane payloads.
        for i in 0..5 {
            insert_session(&conn, &format!("nochk-{i}"), now + i * 1000, true);
        }
        settle_test_recovery_selection(&conn);

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.deleted_bytes, 0);
        assert_eq!(outcome.measured_bytes, outcome.retained_bytes);
        assert!(outcome.measured_bytes > 0);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 5);
    }

    // ── Config edge cases ──

    #[test]
    fn overflowing_size_budget_fails_closed_even_on_an_empty_database() {
        let config = SessionRetentionConfig {
            max_age_days: u64::MAX,
            max_closed_sessions: usize::MAX,
            max_total_size_mb: u64::MAX,
            cleanup_interval_hours: u64::MAX,
        };
        let conn = make_test_db();
        let error = cleanup_sessions(&conn, &config)
            .expect_err("MiB-to-byte overflow must not silently become an unlimited budget");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 0);
    }

    #[test]
    fn config_serde_preserves_zero_values() {
        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRetentionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_age_days, 0);
        assert_eq!(back.max_closed_sessions, 0);
        assert_eq!(back.max_total_size_mb, 0);
        assert_eq!(back.cleanup_interval_hours, 0);
    }

    #[test]
    fn config_debug_impl() {
        let config = SessionRetentionConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("SessionRetentionConfig"));
    }

    #[test]
    fn config_clone_preserves_all_fields() {
        let config = SessionRetentionConfig {
            max_age_days: 7,
            max_closed_sessions: 10,
            max_total_size_mb: 100,
            cleanup_interval_hours: 6,
        };
        let cloned = config.clone();
        assert_eq!(cloned.max_age_days, 7);
        assert_eq!(cloned.max_closed_sessions, 10);
        assert_eq!(cloned.max_total_size_mb, 100);
        assert_eq!(cloned.cleanup_interval_hours, 6);
    }

    // ── epoch_ms additional ──

    #[test]
    fn epoch_ms_is_a_saturating_system_time_conversion() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let observed = u128::from(epoch_ms());
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        // Wall time is allowed to step in either direction. Only assert an
        // ordinary-range conversion when both surrounding observations form
        // an ordered interval; otherwise the clock moved and there is no
        // monotonic contract to test.
        if before <= after && after <= u128::from(u64::MAX) {
            assert!((before..=after).contains(&observed));
        } else {
            assert!(observed <= u128::from(u64::MAX));
        }
    }
}
