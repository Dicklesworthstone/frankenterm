//! Session restore engine — detect and recover from unclean shutdowns.
//!
//! On `ft watch` startup, this module checks the database for sessions
//! that did not shut down cleanly (`shutdown_clean = 0`). If found, it
//! loads the latest checkpoint, reconstructs the mux topology via
//! [`LayoutRestorer`], and optionally displays restore banners.
//!
//! # Data flow
//!
//! ```text
//! Database → SessionCandidate → RestoreDecision → LayoutRestorer → RestoreSummary
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::checkpoint_witness::{
    CHECKPOINT_ROLE_RESTORE_RECEIPT, CHECKPOINT_ROLE_SNAPSHOT, RESTORE_RECEIPT_WITNESS_PREFIX,
    SNAPSHOT_WITNESS_PREFIX, PersistedPaneState, canonical_json_string, checkpoint_witness,
};
use crate::restore_layout::{LayoutRestorer, RestoreConfig, RestoreResult};
use crate::restore_process::{LaunchConfig, ProcessLauncher};
use crate::restore_scrollback::{
    InjectionConfig, InjectionReport, ScrollbackData, ScrollbackInjector,
};
use crate::session_pane_state::{AgentMetadata, PaneStateSnapshot, ProcessInfo, TerminalState};
use crate::session_topology::{PaneNode, TopologySnapshot};
use crate::wezterm::WeztermHandle;

// =============================================================================
// Error type
// =============================================================================

/// Errors during session restore.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("database error: {0}")]
    Database(String),

    #[error("invalid persisted value for {field}: {value}")]
    InvalidPersistedValue { field: &'static str, value: i64 },

    #[error("no restorable sessions found")]
    NoSessions,

    #[error("checkpoint data corrupt: {0}")]
    CorruptCheckpoint(String),

    #[error("topology deserialization failed: {0}")]
    TopologyParse(String),

    #[error("layout restoration failed: {0}")]
    LayoutRestore(String),

    #[error("wezterm command failed: {0}")]
    Wezterm(String),

    #[error("restore bookkeeping failed: {0}")]
    Bookkeeping(String),

    /// Stored consistency witness does not match the exact persisted
    /// checkpoint projection. This detects corruption and partial mutation;
    /// it is not authentication against a writer that can recompute SHA-256.
    #[error(
        "state_hash mismatch on checkpoint {checkpoint_id} \
         (session {session_id}): stored={stored}, recomputed={recomputed}"
    )]
    StateHashMismatch {
        checkpoint_id: i64,
        session_id: String,
        stored: String,
        recomputed: String,
    },

    #[error("invalid checkpoint role on checkpoint {checkpoint_id}: {role}")]
    InvalidCheckpointRole { checkpoint_id: i64, role: String },

    #[error(
        "checkpoint {checkpoint_id} has role {role} and is not a restorable session snapshot"
    )]
    CheckpointNotRestorable { checkpoint_id: i64, role: String },

    #[error("checkpoint {checkpoint_id} has no checkpoint-local topology")]
    CheckpointTopologyUnavailable { checkpoint_id: i64 },

    #[error(
        "legacy-unverified checkpoint {checkpoint_id} requires explicit manual recovery and cannot be auto-restored"
    )]
    LegacyCheckpointRequiresManualRestore { checkpoint_id: i64 },
}

impl From<rusqlite::Error> for RestoreError {
    fn from(e: rusqlite::Error) -> Self {
        RestoreError::Database(e.to_string())
    }
}

fn decode_u64(value: i64, field: &'static str) -> Result<u64, RestoreError> {
    u64::try_from(value).map_err(|_| RestoreError::InvalidPersistedValue { field, value })
}

fn decode_opt_u64(value: Option<i64>, field: &'static str) -> Result<Option<u64>, RestoreError> {
    value.map(|v| decode_u64(v, field)).transpose()
}

fn decode_usize(value: i64, field: &'static str) -> Result<usize, RestoreError> {
    usize::try_from(value).map_err(|_| RestoreError::InvalidPersistedValue { field, value })
}

fn decode_opt_usize(
    value: Option<i64>,
    field: &'static str,
) -> Result<Option<usize>, RestoreError> {
    value.map(|v| decode_usize(v, field)).transpose()
}

fn decode_bool(value: i64, field: &'static str) -> Result<bool, RestoreError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(RestoreError::InvalidPersistedValue { field, value }),
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Session restore behavior configuration.
///
/// ```toml
/// [session]
/// auto_restore = false
/// restore_scrollback = false
/// restore_max_lines = 5000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRestoreConfig {
    /// Skip the restore prompt and always restore automatically.
    pub auto_restore: bool,
    /// Attempt to replay scrollback content (requires native-wezterm feature).
    pub restore_scrollback: bool,
    /// Maximum scrollback lines to replay per pane.
    pub restore_max_lines: usize,
    /// Process re-launch behavior after a fully successful restore.
    pub process_relaunch: LaunchConfig,
}

impl Default for SessionRestoreConfig {
    fn default() -> Self {
        Self {
            auto_restore: false,
            restore_scrollback: false,
            restore_max_lines: 5000,
            process_relaunch: LaunchConfig::default(),
        }
    }
}

// =============================================================================
// Data types
// =============================================================================

/// A session candidate for restore.
#[derive(Debug, Clone)]
pub struct SessionCandidate {
    pub session_id: String,
    pub created_at: u64,
    pub last_checkpoint_at: Option<u64>,
    /// Mutable latest-session summary retained for CLI compatibility. Restore
    /// authority comes exclusively from `CheckpointData::topology_json`.
    pub topology_json: String,
    pub ft_version: String,
    pub host_id: Option<String>,
}

/// Durable purpose of a checkpoint row.
///
/// Snapshot rows carry restorable topology and pane state. Restore receipts
/// only attest that a prior restore attempt recorded its old-to-new pane map;
/// they must never be selected as layout restore points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointRole {
    Snapshot,
    RestoreReceipt,
}

impl CheckpointRole {
    fn from_db(checkpoint_id: i64, role: &str) -> Result<Self, RestoreError> {
        match role {
            CHECKPOINT_ROLE_SNAPSHOT => Ok(Self::Snapshot),
            CHECKPOINT_ROLE_RESTORE_RECEIPT => Ok(Self::RestoreReceipt),
            _ => Err(RestoreError::InvalidCheckpointRole {
                checkpoint_id,
                role: role.to_string(),
            }),
        }
    }

    const fn as_db_str(self) -> &'static str {
        match self {
            Self::Snapshot => CHECKPOINT_ROLE_SNAPSHOT,
            Self::RestoreReceipt => CHECKPOINT_ROLE_RESTORE_RECEIPT,
        }
    }
}

impl std::fmt::Display for CheckpointRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_db_str())
    }
}

/// Strength of the consistency evidence carried by a loaded row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointVerification {
    /// Versioned SHA-256 witness recomputed over the exact stored projection.
    VerifiedV2,
    /// Historical 16-hex witness or literal `restore`. It is retained for
    /// explicit manual recovery only and is never eligible for auto-restore.
    LegacyUnverified,
}

/// Loaded checkpoint with per-pane state.
#[derive(Debug, Clone)]
pub struct CheckpointData {
    pub checkpoint_id: i64,
    pub session_id: String,
    pub checkpoint_at: u64,
    pub checkpoint_type: String,
    pub checkpoint_role: CheckpointRole,
    pub verification: CheckpointVerification,
    /// Topology owned by this exact checkpoint. Receipts have no topology.
    pub topology_json: Option<String>,
    pub pane_count: usize,
    pub total_bytes: usize,
    pub pane_states: Vec<RestoredPaneState>,
}

/// Per-pane state loaded from the database.
#[derive(Debug, Clone)]
pub struct RestoredPaneState {
    pub pane_id: u64,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub terminal_state: Option<TerminalState>,
    pub agent_metadata: Option<AgentMetadata>,
    pub scrollback_checkpoint_seq: Option<u64>,
    pub last_output_at: Option<u64>,
}

/// Complete result of a session restore.
#[derive(Debug)]
pub struct RestoreSummary {
    /// The session that was restored.
    pub session_id: String,
    /// Checkpoint that was loaded.
    pub checkpoint_id: i64,
    /// Layout restoration result.
    pub layout_result: RestoreResult,
    /// Pane states that were loaded.
    pub pane_states: Vec<RestoredPaneState>,
    /// Scrollback injection report when replay was enabled.
    pub scrollback_result: Option<InjectionReport>,
    /// Global scrollback replay error when capture data could not be loaded.
    pub scrollback_error: Option<String>,
    /// Total time for the restore in milliseconds.
    pub elapsed_ms: u64,
}

impl RestoreSummary {
    /// Number of panes successfully restored.
    pub fn restored_count(&self) -> usize {
        self.layout_result.pane_id_map.len()
    }

    /// Number of panes that failed to restore.
    pub fn failed_count(&self) -> usize {
        self.layout_result.failed_panes.len()
    }

    /// Total panes attempted.
    pub fn total_count(&self) -> usize {
        self.restored_count() + self.failed_count()
    }

    /// Number of panes whose scrollback was replayed.
    pub fn scrollback_restored_count(&self) -> usize {
        self.scrollback_result
            .as_ref()
            .map_or(0, InjectionReport::success_count)
    }

    /// Number of panes whose scrollback replay failed.
    pub fn scrollback_failed_count(&self) -> usize {
        self.scrollback_result
            .as_ref()
            .map_or(0, InjectionReport::failure_count)
    }

    /// Number of panes skipped during scrollback replay.
    pub fn scrollback_skipped_count(&self) -> usize {
        self.scrollback_result
            .as_ref()
            .map_or(0, |report| report.skipped.len())
    }

    /// Total bytes written during scrollback replay.
    pub fn scrollback_bytes_written(&self) -> usize {
        self.scrollback_result
            .as_ref()
            .map_or(0, InjectionReport::total_bytes)
    }
}

// =============================================================================
// Database queries
// =============================================================================

fn open_conn(db_path: &str) -> Result<Connection, RestoreError> {
    let conn = Connection::open(db_path)?;
    // [ft-rfpk6] `PRAGMA foreign_keys` is per-connection. The schema's
    // ON DELETE CASCADE chain (mux_pane_state → session_checkpoints →
    // mux_sessions) only fires when FKs are ON. Without this, any
    // future DELETE that relies on CASCADE leaks orphan rows. See
    // ft-s4myu for the matching fix on StorageHandle::with_config.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
    )?;
    Ok(conn)
}

/// Find sessions that did not shut down cleanly.
pub fn find_unclean_sessions(db_path: &str) -> Result<Vec<SessionCandidate>, RestoreError> {
    let conn = open_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT session.session_id,
                session.created_at,
                session.last_checkpoint_at,
                session.topology_json,
                session.ft_version,
                session.host_id,
                CASE
                    WHEN session.shutdown_clean = 1
                     AND EXISTS (
                         SELECT 1
                         FROM session_checkpoints AS clean
                         WHERE clean.id = session.clean_checkpoint_id
                           AND clean.session_id = session.session_id
                           AND clean.id = (
                               SELECT latest.id
                               FROM session_checkpoints AS latest
                               WHERE latest.session_id = session.session_id
                               ORDER BY latest.checkpoint_at DESC, latest.id DESC
                               LIMIT 1
                           )
                     )
                    THEN 1
                    ELSE 0
                END AS authoritative_shutdown_clean
         FROM mux_sessions AS session
         ORDER BY COALESCE(session.last_checkpoint_at, session.created_at) DESC,
                  session.session_id ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (
            session_id,
            created_at,
            last_checkpoint_at,
            topology_json,
            ft_version,
            host_id,
            shutdown_clean,
        ) = row?;
        if decode_bool(shutdown_clean, "mux_sessions.shutdown_clean")? {
            continue;
        }
        candidates.push(SessionCandidate {
            session_id,
            created_at: decode_u64(created_at, "mux_sessions.created_at")?,
            last_checkpoint_at: decode_opt_u64(
                last_checkpoint_at,
                "mux_sessions.last_checkpoint_at",
            )?,
            topology_json,
            ft_version,
            host_id,
        });
    }

    Ok(candidates)
}

/// Load the latest checkpoint for a session, including pane states.
pub fn load_latest_checkpoint(
    db_path: &str,
    session_id: &str,
) -> Result<Option<CheckpointData>, RestoreError> {
    let conn = open_conn(db_path)?;

    // Purpose is explicit in schema v36. Do not infer it from pane-row
    // presence or the overloaded `startup` checkpoint type: a genuine empty
    // snapshot is still a snapshot, while a restore receipt is never one.
    let checkpoint_id = conn.query_row(
        "SELECT c.id
         FROM session_checkpoints c
         WHERE c.session_id = ?1
           AND c.checkpoint_role = 'snapshot'
         ORDER BY c.checkpoint_at DESC, c.id DESC
         LIMIT 1",
        [session_id],
        |row| row.get::<_, i64>(0),
    );

    match checkpoint_id {
        Ok(id) => load_checkpoint_by_id(db_path, id),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(RestoreError::Database(e.to_string())),
    }
}

/// Load a specific checkpoint by row ID, including pane states.
pub fn load_checkpoint_by_id(
    db_path: &str,
    checkpoint_id: i64,
) -> Result<Option<CheckpointData>, RestoreError> {
    let conn = open_conn(db_path)?;

    let checkpoint = conn.query_row(
        "SELECT session_id, checkpoint_at, checkpoint_type, checkpoint_role,
                state_hash, pane_count, total_bytes, metadata_json, topology_json
         FROM session_checkpoints
         WHERE id = ?1",
        [checkpoint_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        },
    );

    let (
        session_id,
        checkpoint_at_raw,
        checkpoint_type,
        checkpoint_role_raw,
        stored_state_hash,
        pane_count_raw,
        total_bytes_raw,
        metadata_json,
        topology_json,
    ) = match checkpoint {
        Ok(c) => c,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(RestoreError::Database(e.to_string())),
    };
    let checkpoint_at = decode_u64(checkpoint_at_raw, "session_checkpoints.checkpoint_at")?;
    let pane_count = decode_usize(pane_count_raw, "session_checkpoints.pane_count")?;
    let total_bytes = decode_usize(total_bytes_raw, "session_checkpoints.total_bytes")?;
    let checkpoint_role = CheckpointRole::from_db(checkpoint_id, &checkpoint_role_raw)?;

    // Load the exact persisted pane projection. `env_json` is deliberately
    // included even though restore does not re-inject environment variables:
    // it participates in the row-local v2 consistency witness.
    let mut stmt = conn.prepare(
        "SELECT pane_id, cwd, command, env_json, terminal_state_json,
                agent_metadata_json, scrollback_checkpoint_seq, last_output_at
         FROM mux_pane_state
         WHERE checkpoint_id = ?1
         ORDER BY pane_id ASC, id ASC",
    )?;

    let persisted_panes = stmt
        .query_map([checkpoint_id], |row| {
            Ok(PersistedPaneState {
                pane_id: row.get(0)?,
                cwd: row.get(1)?,
                command: row.get(2)?,
                env_json: row.get(3)?,
                terminal_state_json: row.get(4)?,
                agent_metadata_json: row.get(5)?,
                scrollback_checkpoint_seq: row.get(6)?,
                last_output_at: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    validate_checkpoint_structure(
        checkpoint_id,
        checkpoint_role,
        &checkpoint_type,
        pane_count,
        total_bytes,
        metadata_json.as_deref(),
        topology_json.as_deref(),
        &persisted_panes,
    )?;
    let verification = verify_checkpoint_witness(
        checkpoint_id,
        &session_id,
        checkpoint_at_raw,
        &checkpoint_type,
        checkpoint_role,
        &stored_state_hash,
        pane_count_raw,
        total_bytes_raw,
        metadata_json.as_deref(),
        topology_json.as_deref(),
        &persisted_panes,
    )?;

    let mut pane_states = Vec::with_capacity(persisted_panes.len());
    for persisted in persisted_panes {
        let pane_id = decode_u64(persisted.pane_id, "mux_pane_state.pane_id")?;
        let terminal_state = serde_json::from_str::<TerminalState>(&persisted.terminal_state_json)
            .map_err(|error| {
                RestoreError::CorruptCheckpoint(format!(
                    "pane {pane_id} has invalid terminal_state_json: {error}"
                ))
            })?;
        let agent_metadata = match persisted.agent_metadata_json.as_deref() {
            Some(agent_json) => Some(serde_json::from_str::<AgentMetadata>(agent_json).map_err(
                |error| {
                    RestoreError::CorruptCheckpoint(format!(
                        "pane {pane_id} has invalid agent_metadata_json: {error}"
                    ))
                },
            )?),
            None => None,
        };

        pane_states.push(RestoredPaneState {
            pane_id,
            cwd: persisted.cwd,
            command: persisted.command,
            terminal_state: Some(terminal_state),
            agent_metadata,
            scrollback_checkpoint_seq: decode_opt_u64(
                persisted.scrollback_checkpoint_seq,
                "mux_pane_state.scrollback_checkpoint_seq",
            )?,
            last_output_at: decode_opt_u64(
                persisted.last_output_at,
                "mux_pane_state.last_output_at",
            )?,
        });
    }

    Ok(Some(CheckpointData {
        checkpoint_id,
        session_id,
        checkpoint_at,
        checkpoint_type,
        checkpoint_role,
        verification,
        topology_json,
        pane_count,
        total_bytes,
        pane_states,
    }))
}

#[allow(clippy::too_many_arguments)]
fn validate_checkpoint_structure(
    checkpoint_id: i64,
    role: CheckpointRole,
    checkpoint_type: &str,
    pane_count: usize,
    total_bytes: usize,
    metadata_json: Option<&str>,
    topology_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Result<(), RestoreError> {
    match role {
        CheckpointRole::Snapshot => {
            if topology_json.is_none() {
                return Err(RestoreError::CheckpointTopologyUnavailable { checkpoint_id });
            }
            if panes.len() != pane_count {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "checkpoint {checkpoint_id} declares {pane_count} panes but stores {} pane rows",
                    panes.len()
                )));
            }
            // The loader query orders by pane_id then row id, so duplicates
            // are adjacent and need no per-checkpoint hash-set allocation.
            let mut previous_pane_id = None;
            for pane in panes {
                decode_u64(pane.pane_id, "mux_pane_state.pane_id")?;
                if previous_pane_id == Some(pane.pane_id) {
                    return Err(RestoreError::CorruptCheckpoint(format!(
                        "checkpoint {checkpoint_id} stores duplicate pane id {}",
                        pane.pane_id
                    )));
                }
                previous_pane_id = Some(pane.pane_id);
            }
        }
        CheckpointRole::RestoreReceipt => {
            if checkpoint_type != "startup" {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore receipt {checkpoint_id} has checkpoint type {checkpoint_type}, expected startup"
                )));
            }
            if total_bytes != 0 {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore receipt {checkpoint_id} declares {total_bytes} payload bytes"
                )));
            }
            if topology_json.is_some() {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore receipt {checkpoint_id} unexpectedly carries topology"
                )));
            }
            if !panes.is_empty() {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore receipt {checkpoint_id} unexpectedly stores pane-state rows"
                )));
            }
            let mapping_count = restore_receipt_mapping_count(checkpoint_id, metadata_json)?;
            if mapping_count != pane_count {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore receipt {checkpoint_id} declares {pane_count} mappings but metadata stores {mapping_count}"
                )));
            }
        }
    }
    Ok(())
}

fn restore_receipt_mapping_count(
    checkpoint_id: i64,
    metadata_json: Option<&str>,
) -> Result<usize, RestoreError> {
    let metadata_json = metadata_json.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "restore receipt {checkpoint_id} has NULL metadata_json"
        ))
    })?;
    let metadata: serde_json::Value = serde_json::from_str(metadata_json).map_err(|error| {
        RestoreError::CorruptCheckpoint(format!(
            "restore receipt {checkpoint_id} has invalid metadata_json: {error}"
        ))
    })?;
    let mapping = metadata
        .get("old_to_new")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "restore receipt {checkpoint_id} metadata has no old_to_new object"
            ))
        })?;
    let mut old_ids = HashSet::with_capacity(mapping.len());
    let mut new_ids = HashSet::with_capacity(mapping.len());
    for (old_id, new_id) in mapping {
        let parsed_old_id = old_id.parse::<u64>().map_err(|_| {
            RestoreError::CorruptCheckpoint(format!(
                "restore receipt {checkpoint_id} has non-u64 old pane id {old_id}"
            ))
        })?;
        if !old_ids.insert(parsed_old_id) {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "restore receipt {checkpoint_id} contains duplicate encodings of old pane {parsed_old_id}"
            )));
        }
        let new_id = new_id.as_u64().ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "restore receipt {checkpoint_id} has non-u64 new pane id for {old_id}"
            ))
        })?;
        if !new_ids.insert(new_id) {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "restore receipt {checkpoint_id} maps more than one old pane to new pane {new_id}"
            )));
        }
    }
    Ok(mapping.len())
}

#[allow(clippy::too_many_arguments)]
fn verify_checkpoint_witness(
    checkpoint_id: i64,
    session_id: &str,
    checkpoint_at: i64,
    checkpoint_type: &str,
    role: CheckpointRole,
    stored_state_hash: &str,
    pane_count: i64,
    total_bytes: i64,
    metadata_json: Option<&str>,
    topology_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Result<CheckpointVerification, RestoreError> {
    let is_v2 = stored_state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX)
        || stored_state_hash.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX);
    if is_v2 {
        let expected_prefix = match role {
            CheckpointRole::Snapshot => SNAPSHOT_WITNESS_PREFIX,
            CheckpointRole::RestoreReceipt => RESTORE_RECEIPT_WITNESS_PREFIX,
        };
        if !stored_state_hash.starts_with(expected_prefix) {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} uses a witness prefix for the wrong checkpoint role"
            )));
        }
        let recomputed = checkpoint_witness(
            role.as_db_str(),
            session_id,
            checkpoint_id,
            checkpoint_at,
            checkpoint_type,
            pane_count,
            total_bytes,
            metadata_json,
            topology_json,
            panes,
        )
        .map_err(|error| {
            RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} witness projection is invalid: {error}"
            ))
        })?;
        if recomputed != stored_state_hash {
            return Err(RestoreError::StateHashMismatch {
                checkpoint_id,
                session_id: session_id.to_string(),
                stored: stored_state_hash.to_string(),
                recomputed,
            });
        }
        return Ok(CheckpointVerification::VerifiedV2);
    }

    let is_legacy = match role {
        CheckpointRole::Snapshot => {
            stored_state_hash.len() == 16
                && stored_state_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
        }
        CheckpointRole::RestoreReceipt => stored_state_hash == "restore",
    };
    if is_legacy {
        return Ok(CheckpointVerification::LegacyUnverified);
    }

    Err(RestoreError::CorruptCheckpoint(format!(
        "checkpoint {checkpoint_id} has an unknown state_hash encoding"
    )))
}

/// Test-only exact-receipt clean transition. Production restore bookkeeping
/// uses `finalize_restore`, which creates and binds its receipt atomically.
#[cfg(test)]
fn mark_session_restored(db_path: &str, session_id: &str) -> Result<(), RestoreError> {
    let conn = open_conn(db_path)?;
    let checkpoint_id = conn
        .query_row(
            "SELECT id
             FROM session_checkpoints
             WHERE session_id = ?1
             ORDER BY checkpoint_at DESC, id DESC
             LIMIT 1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| {
            RestoreError::Bookkeeping(format!(
                "cannot mark session {session_id} restored without an exact checkpoint receipt: {error}"
            ))
        })?;
    let updated = conn.execute(
        "UPDATE mux_sessions
         SET shutdown_clean = 1, clean_checkpoint_id = ?2
         WHERE session_id = ?1",
        rusqlite::params![session_id, checkpoint_id],
    )?;
    if updated != 1 {
        return Err(RestoreError::Bookkeeping(format!(
            "cannot bind clean receipt to missing session {session_id}"
        )));
    }
    Ok(())
}

fn prepare_restore_receipt_metadata(
    pane_id_map: &HashMap<u64, u64>,
) -> Result<(String, i64), RestoreError> {
    let pane_count = i64::try_from(pane_id_map.len()).map_err(|_| {
        RestoreError::Bookkeeping("restore pane mapping count exceeds SQLite INTEGER".to_string())
    })?;
    let mut ordered_mapping = BTreeMap::new();
    let mut new_ids = HashSet::with_capacity(pane_id_map.len());
    for (&old_id, &new_id) in pane_id_map {
        if !new_ids.insert(new_id) {
            return Err(RestoreError::Bookkeeping(format!(
                "restore mapping assigns new pane {new_id} more than once"
            )));
        }
        ordered_mapping.insert(old_id.to_string(), new_id);
    }
    let metadata = serde_json::json!({ "old_to_new": ordered_mapping });
    let metadata_json = canonical_json_string(&metadata).map_err(|error| {
        RestoreError::Bookkeeping(format!(
            "failed to canonicalize restore receipt metadata: {error}"
        ))
    })?;
    Ok((metadata_json, pane_count))
}

/// Insert one restore receipt and replace its transaction-local placeholder
/// only after SQLite assigns the row ID that the v2 witness binds.
fn insert_restore_receipt(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    now_ms: i64,
    metadata_json: &str,
    pane_count: i64,
) -> Result<i64, RestoreError> {
    tx.execute(
        "INSERT INTO session_checkpoints
         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
          total_bytes, metadata_json, checkpoint_role, topology_json)
         VALUES (?1, ?2, 'startup', 'pending:rst2', ?3, 0, ?4,
                 'restore_receipt', NULL)",
        rusqlite::params![session_id, now_ms, pane_count, metadata_json],
    )?;
    let checkpoint_id = tx.last_insert_rowid();
    let state_hash = checkpoint_witness(
        CHECKPOINT_ROLE_RESTORE_RECEIPT,
        session_id,
        checkpoint_id,
        now_ms,
        "startup",
        pane_count,
        0,
        Some(metadata_json),
        None,
        &[],
    )
    .map_err(|error| {
        RestoreError::Bookkeeping(format!(
            "failed to compute restore receipt witness: {error}"
        ))
    })?;
    let updated = tx.execute(
        "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
        rusqlite::params![state_hash, checkpoint_id],
    )?;
    if updated != 1 {
        return Err(RestoreError::Bookkeeping(format!(
            "restore receipt witness update changed {updated} rows for checkpoint {checkpoint_id}"
        )));
    }
    Ok(checkpoint_id)
}

/// Record the pane ID mapping from restore in a new startup checkpoint.
#[cfg(test)]
fn save_restore_checkpoint(
    db_path: &str,
    session_id: &str,
    pane_id_map: &HashMap<u64, u64>,
) -> Result<i64, RestoreError> {
    let mut conn = open_conn(db_path)?;
    // br-ft-0n4nx: route through the shared clock-anomaly helper so
    // a pre-epoch host clock can't silently produce checkpoint_at=0
    // for every persisted record (collision on the persisted column).
    let now_ms = crate::clock_anomaly::epoch_ms_i64("ft.session_restore.clock");

    let (metadata_json, pane_count) = prepare_restore_receipt_metadata(pane_id_map)?;

    let tx = conn.transaction()?;
    let checkpoint_id =
        insert_restore_receipt(&tx, session_id, now_ms, &metadata_json, pane_count)?;
    let updated = tx.execute(
        "UPDATE mux_sessions
         SET last_checkpoint_at = ?2,
             shutdown_clean = 0,
             clean_checkpoint_id = NULL
         WHERE session_id = ?1",
        rusqlite::params![session_id, now_ms],
    )?;
    if updated != 1 {
        return Err(RestoreError::Bookkeeping(format!(
            "restore receipt references missing session {session_id}"
        )));
    }
    tx.commit()?;

    Ok(checkpoint_id)
}

/// Persist restore bookkeeping atomically so a successful restore never leaves
/// a half-written startup checkpoint or clean/unclean transition behind.
fn finalize_restore(
    db_path: &str,
    session_id: &str,
    pane_id_map: &HashMap<u64, u64>,
    mark_clean: bool,
) -> Result<i64, RestoreError> {
    let mut conn = open_conn(db_path)?;
    // br-ft-0n4nx: route through the shared clock-anomaly helper so
    // a pre-epoch host clock can't silently produce checkpoint_at=0
    // for every persisted record (collision on the persisted column).
    let now_ms = crate::clock_anomaly::epoch_ms_i64("ft.session_restore.clock");

    let (metadata_json, pane_count) = prepare_restore_receipt_metadata(pane_id_map)?;

    let tx = conn.transaction()?;
    let checkpoint_id =
        insert_restore_receipt(&tx, session_id, now_ms, &metadata_json, pane_count)?;
    let updated = if mark_clean {
        tx.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 shutdown_clean = 1,
                 clean_checkpoint_id = ?3
             WHERE session_id = ?1",
            rusqlite::params![session_id, now_ms, checkpoint_id],
        )?
    } else {
        tx.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 shutdown_clean = 0,
                 clean_checkpoint_id = NULL
             WHERE session_id = ?1",
            rusqlite::params![session_id, now_ms],
        )?
    };
    if updated != 1 {
        return Err(RestoreError::Bookkeeping(format!(
            "restore receipt references missing session {session_id}"
        )));
    }
    tx.commit()?;

    Ok(checkpoint_id)
}

fn load_scrollback_data(
    db_path: &str,
    pane_states: &[RestoredPaneState],
    max_lines: usize,
) -> Result<HashMap<u64, ScrollbackData>, RestoreError> {
    let conn = open_conn(db_path)?;
    // [ft-8ihuz] Pull at most `max_lines` segments by ORDER BY seq DESC
    // + LIMIT, then reverse in Rust to restore chronological order.
    // The old query had no LIMIT and relied on an in-memory truncate
    // after loading all segments up to max_seq — for a pane with
    // hundreds of MB / multi-GB of captured scrollback (realistic for
    // long-running agent sessions), the full read allocates the whole
    // dataset and throws ~99.5% away once scrollback.truncate runs.
    // That allocation spike could OOM frankenterm on restore of a
    // heavy session. DESC + LIMIT bounds the allocation at
    // max_lines × avg_segment_size, and the final `.reverse()` keeps
    // the resulting ScrollbackData byte-for-byte identical to the
    // pre-fix output for any input where the old code would NOT have
    // truncated.
    let mut stmt = conn.prepare(
        "SELECT content
         FROM output_segments
         WHERE pane_id = ?1 AND seq <= ?2
         ORDER BY seq DESC
         LIMIT ?3",
    )?;

    let max_lines_i64 = i64::try_from(max_lines).unwrap_or(i64::MAX);

    let mut scrollbacks = HashMap::new();
    for pane_state in pane_states {
        let Some(max_seq) = pane_state.scrollback_checkpoint_seq else {
            continue;
        };

        let pane_id_i64 = i64::try_from(pane_state.pane_id).map_err(|_| {
            RestoreError::CorruptCheckpoint(format!(
                "pane {} exceeds sqlite integer range",
                pane_state.pane_id
            ))
        })?;
        let max_seq_i64 = i64::try_from(max_seq).map_err(|_| {
            RestoreError::CorruptCheckpoint(format!(
                "pane {} scrollback seq {} exceeds sqlite integer range",
                pane_state.pane_id, max_seq
            ))
        })?;

        let mut segments = stmt
            .query_map([pane_id_i64, max_seq_i64, max_lines_i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if segments.is_empty() {
            continue;
        }

        // Query returned segments in DESC order; ScrollbackData needs
        // chronological (ASC). Single in-place reverse, no re-alloc.
        segments.reverse();

        let mut scrollback = ScrollbackData::from_segments(segments);
        // Second-line-of-defense truncate: the SQL LIMIT already caps
        // segments at max_lines, but truncate also enforces the
        // contract at the ScrollbackData level so any future caller
        // that passes a pre-built Vec doesn't accidentally exceed
        // the cap.
        scrollback.truncate(max_lines);
        scrollbacks.insert(pane_state.pane_id, scrollback);
    }

    Ok(scrollbacks)
}

fn launch_snapshot_from_restored_state(
    state: &RestoredPaneState,
    checkpoint_at: u64,
) -> PaneStateSnapshot {
    let terminal = state
        .terminal_state
        .clone()
        .unwrap_or_else(|| TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: 0,
            cursor_col: 0,
            is_alt_screen: false,
            title: String::new(),
        });

    let mut snapshot = PaneStateSnapshot::new(state.pane_id, checkpoint_at, terminal);
    if let Some(cwd) = &state.cwd {
        snapshot = snapshot.with_cwd(cwd.clone());
    }
    if let Some(command) = &state.command
        && !command.is_empty()
    {
        snapshot = snapshot.with_process(ProcessInfo {
            name: command.clone(),
            pid: None,
            argv: None,
        });
    }
    if let Some(agent) = &state.agent_metadata {
        snapshot = snapshot.with_agent(agent.clone());
    }
    snapshot
}

fn validate_restore_topology(
    checkpoint: &CheckpointData,
    topology: &TopologySnapshot,
) -> Result<(), RestoreError> {
    let mut topology_pane_ids = topology.pane_ids();
    if topology_pane_ids.len() != checkpoint.pane_count {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {} declares {} panes but its topology contains {} pane leaves",
            checkpoint.checkpoint_id,
            checkpoint.pane_count,
            topology_pane_ids.len()
        )));
    }
    topology_pane_ids.sort_unstable();
    if let Some(duplicate) = topology_pane_ids
        .windows(2)
        .find(|ids| ids[0] == ids[1])
        .map(|ids| ids[0])
    {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {} topology contains duplicate pane id {duplicate}",
            checkpoint.checkpoint_id
        )));
    }

    let mut persisted_pane_ids: Vec<u64> = checkpoint
        .pane_states
        .iter()
        .map(|pane| pane.pane_id)
        .collect();
    persisted_pane_ids.sort_unstable();
    if topology_pane_ids != persisted_pane_ids {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {} topology pane IDs do not match its persisted pane-state IDs",
            checkpoint.checkpoint_id
        )));
    }

    for window in &topology.windows {
        if window
            .active_tab_index
            .is_some_and(|active_index| active_index >= window.tabs.len())
        {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "checkpoint {} window {} has an out-of-range active tab index",
                checkpoint.checkpoint_id, window.window_id
            )));
        }
        for tab in &window.tabs {
            validate_restore_pane_tree(checkpoint.checkpoint_id, &tab.pane_tree)?;
            if let Some(active_pane_id) = tab.active_pane_id {
                let mut tab_pane_ids = Vec::new();
                tab.pane_tree.collect_pane_ids(&mut tab_pane_ids);
                if !tab_pane_ids.contains(&active_pane_id) {
                    return Err(RestoreError::CorruptCheckpoint(format!(
                        "checkpoint {} tab {} names active pane {} outside its pane tree",
                        checkpoint.checkpoint_id, tab.tab_id, active_pane_id
                    )));
                }
            }
        }
    }

    Ok(())
}

fn validate_restore_pane_tree(
    checkpoint_id: i64,
    pane_tree: &PaneNode,
) -> Result<(), RestoreError> {
    match pane_tree {
        PaneNode::Leaf { .. } => Ok(()),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            if children.len() < 2 {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "checkpoint {checkpoint_id} topology contains a split with fewer than two children"
                )));
            }
            for (ratio, child) in children {
                if !ratio.is_finite() || *ratio <= 0.0 {
                    return Err(RestoreError::CorruptCheckpoint(format!(
                        "checkpoint {checkpoint_id} topology contains a non-positive split ratio"
                    )));
                }
                validate_restore_pane_tree(checkpoint_id, child)?;
            }
            let ratio_sum: f64 = children.iter().map(|(ratio, _)| *ratio).sum();
            if !ratio_sum.is_finite() {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "checkpoint {checkpoint_id} topology split ratios overflow"
                )));
            }
            Ok(())
        }
    }
}

// =============================================================================
// CLI query functions
// =============================================================================

/// Session summary for CLI display.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub created_at: u64,
    pub last_checkpoint_at: Option<u64>,
    pub shutdown_clean: bool,
    pub ft_version: String,
    pub host_id: Option<String>,
    pub checkpoint_count: usize,
    pub pane_count: Option<usize>,
}

/// List all sessions with their checkpoint counts.
pub fn list_sessions(db_path: &str) -> Result<Vec<SessionInfo>, RestoreError> {
    let conn = open_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT s.session_id, s.created_at, s.last_checkpoint_at,
                CASE
                    WHEN s.shutdown_clean = 1
                     AND EXISTS (
                         SELECT 1
                         FROM session_checkpoints AS clean
                         WHERE clean.id = s.clean_checkpoint_id
                           AND clean.session_id = s.session_id
                           AND clean.id = (
                               SELECT latest.id
                               FROM session_checkpoints AS latest
                               WHERE latest.session_id = s.session_id
                               ORDER BY latest.checkpoint_at DESC, latest.id DESC
                               LIMIT 1
                           )
                     )
                    THEN 1
                    ELSE 0
                END AS authoritative_shutdown_clean,
                s.ft_version, s.host_id,
                (SELECT COUNT(*) FROM session_checkpoints c WHERE c.session_id = s.session_id),
                (SELECT c.pane_count FROM session_checkpoints c
                 WHERE c.session_id = s.session_id
                   AND c.checkpoint_role = 'snapshot'
                 ORDER BY c.checkpoint_at DESC, c.id DESC LIMIT 1)
         FROM mux_sessions s
         ORDER BY COALESCE(s.last_checkpoint_at, s.created_at) DESC, s.session_id ASC",
    )?;

    let raw_sessions = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<i64>>(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    raw_sessions
        .into_iter()
        .map(
            |(
                session_id,
                created_at,
                last_checkpoint_at,
                shutdown_clean,
                ft_version,
                host_id,
                checkpoint_count,
                pane_count,
            )| {
                Ok(SessionInfo {
                    session_id,
                    created_at: decode_u64(created_at, "mux_sessions.created_at")?,
                    last_checkpoint_at: decode_opt_u64(
                        last_checkpoint_at,
                        "mux_sessions.last_checkpoint_at",
                    )?,
                    shutdown_clean: decode_bool(shutdown_clean, "mux_sessions.shutdown_clean")?,
                    ft_version,
                    host_id,
                    checkpoint_count: decode_usize(checkpoint_count, "session_checkpoints.count")?,
                    pane_count: decode_opt_usize(pane_count, "session_checkpoints.pane_count")?,
                })
            },
        )
        .collect()
}

/// Checkpoint summary for show command.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointInfo {
    pub id: i64,
    pub checkpoint_at: u64,
    pub checkpoint_type: String,
    pub checkpoint_role: CheckpointRole,
    pub pane_count: usize,
    pub total_bytes: usize,
}

/// Show detailed session info including checkpoints.
pub fn show_session(
    db_path: &str,
    session_id: &str,
) -> Result<(SessionCandidate, Vec<CheckpointInfo>), RestoreError> {
    let conn = open_conn(db_path)?;

    // Get session
    let session = conn
        .query_row(
            "SELECT session_id, created_at, last_checkpoint_at, topology_json, ft_version, host_id
             FROM mux_sessions WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => RestoreError::NoSessions,
            other => RestoreError::Database(other.to_string()),
        })?;
    let session = SessionCandidate {
        session_id: session.0,
        created_at: decode_u64(session.1, "mux_sessions.created_at")?,
        last_checkpoint_at: decode_opt_u64(session.2, "mux_sessions.last_checkpoint_at")?,
        topology_json: session.3,
        ft_version: session.4,
        host_id: session.5,
    };

    // Get checkpoints
    let mut stmt = conn.prepare(
        "SELECT id, checkpoint_at, checkpoint_type, checkpoint_role, pane_count, total_bytes
         FROM session_checkpoints
         WHERE session_id = ?1
         ORDER BY checkpoint_at DESC, id DESC",
    )?;

    let raw_checkpoints = stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let checkpoints = raw_checkpoints
        .into_iter()
        .map(
            |(id, checkpoint_at, checkpoint_type, checkpoint_role, pane_count, total_bytes)| {
                Ok(CheckpointInfo {
                    id,
                    checkpoint_at: decode_u64(checkpoint_at, "session_checkpoints.checkpoint_at")?,
                    checkpoint_type,
                    checkpoint_role: CheckpointRole::from_db(id, &checkpoint_role)?,
                    pane_count: decode_usize(pane_count, "session_checkpoints.pane_count")?,
                    total_bytes: decode_usize(total_bytes, "session_checkpoints.total_bytes")?,
                })
            },
        )
        .collect::<Result<Vec<_>, RestoreError>>()?;

    Ok((session, checkpoints))
}

/// Session health check result.
#[derive(Debug, Clone, Serialize)]
pub struct SessionDoctorReport {
    pub total_sessions: usize,
    pub unclean_sessions: usize,
    pub total_checkpoints: usize,
    pub orphaned_pane_states: usize,
    pub total_data_bytes: usize,
}

/// Run health check on session data.
pub fn session_doctor(db_path: &str) -> Result<SessionDoctorReport, RestoreError> {
    let conn = open_conn(db_path)?;

    let total_sessions: i64 =
        conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))?;

    let mut shutdown_stmt = conn.prepare(
        "SELECT CASE
                    WHEN session.shutdown_clean = 1
                     AND EXISTS (
                         SELECT 1
                         FROM session_checkpoints AS clean
                         WHERE clean.id = session.clean_checkpoint_id
                           AND clean.session_id = session.session_id
                           AND clean.id = (
                               SELECT latest.id
                               FROM session_checkpoints AS latest
                               WHERE latest.session_id = session.session_id
                               ORDER BY latest.checkpoint_at DESC, latest.id DESC
                               LIMIT 1
                           )
                     )
                    THEN 1
                    ELSE 0
                END
         FROM mux_sessions AS session",
    )?;
    let shutdown_rows = shutdown_stmt.query_map([], |row| row.get::<_, i64>(0))?;
    let mut unclean_sessions = 0usize;
    for row in shutdown_rows {
        if !decode_bool(row?, "mux_sessions.shutdown_clean")? {
            unclean_sessions += 1;
        }
    }

    let total_checkpoints: i64 =
        conn.query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })?;

    let orphaned_pane_states: i64 = conn.query_row(
        "SELECT COUNT(*) FROM mux_pane_state
         WHERE checkpoint_id NOT IN (SELECT id FROM session_checkpoints)",
        [],
        |row| row.get(0),
    )?;

    let total_data_bytes: i64 = conn.query_row(
        "SELECT COALESCE(SUM(total_bytes), 0) FROM session_checkpoints",
        [],
        |row| row.get(0),
    )?;

    Ok(SessionDoctorReport {
        total_sessions: decode_usize(total_sessions, "mux_sessions.count")?,
        unclean_sessions,
        total_checkpoints: decode_usize(total_checkpoints, "session_checkpoints.count")?,
        orphaned_pane_states: decode_usize(orphaned_pane_states, "mux_pane_state.orphaned_count")?,
        total_data_bytes: decode_usize(total_data_bytes, "session_checkpoints.total_bytes_sum")?,
    })
}

/// Delete a session and all its checkpoints (cascading via SQL).
pub fn delete_session(db_path: &str, session_id: &str) -> Result<bool, RestoreError> {
    let mut conn = open_conn(db_path)?;

    // [ft-dqsev] Wrap the three DELETEs in one transaction so a
    // mid-sequence failure (lock contention, disk full, cancel) can't
    // leave pane_state orphaned from its checkpoints or checkpoints
    // orphaned from their session row. Without this, a partial failure
    // was silent data corruption — subsequent `load_checkpoint_by_id`
    // calls would return Some(checkpoint) with an empty `pane_states`
    // vec, producing a restored session that looks clean but carries
    // no terminal state.
    let tx = conn.transaction()?;

    // Delete pane states via checkpoint cascade
    let deleted_pane_states = tx.execute(
        "DELETE FROM mux_pane_state WHERE checkpoint_id IN
         (SELECT id FROM session_checkpoints WHERE session_id = ?1)",
        [session_id],
    )?;

    // Delete checkpoints
    let deleted_checkpoints = tx.execute(
        "DELETE FROM session_checkpoints WHERE session_id = ?1",
        [session_id],
    )?;

    // Delete session
    let deleted_sessions = tx.execute(
        "DELETE FROM mux_sessions WHERE session_id = ?1",
        [session_id],
    )?;

    tx.commit()?;
    Ok(deleted_sessions > 0 || deleted_checkpoints > 0 || deleted_pane_states > 0)
}

// =============================================================================
// Restore banner
// =============================================================================

/// Generate a restore banner for a pane.
fn restore_banner(
    old_pane_id: u64,
    session_id: &str,
    checkpoint_at: u64,
    pane_state: Option<&RestoredPaneState>,
) -> String {
    let time_str = format_epoch_ms(checkpoint_at);
    let mut lines = Vec::new();

    lines.push(format!(
        "\x1b[1;36m═══ Session restored from checkpoint at {time_str} ═══\x1b[0m"
    ));

    // Show agent context if available
    if let Some(state) = pane_state {
        if let Some(ref agent) = state.agent_metadata {
            let agent_info = match (&agent.state, &agent.session_id) {
                (Some(st), Some(sid)) => {
                    format!("{} (session {}, state: {})", agent.agent_type, sid, st)
                }
                (Some(st), None) => format!("{} (state: {})", agent.agent_type, st),
                _ => agent.agent_type.clone(),
            };
            lines.push(format!(
                "\x1b[1;33m═══ Previously running: {agent_info} ═══\x1b[0m"
            ));
        }
        if let Some(ref cmd) = state.command {
            lines.push(format!("\x1b[90m═══ Process: {cmd} ═══\x1b[0m"));
        }
    }

    lines.push(format!(
        "\x1b[90m═══ Previous output: ft session show {session_id} --pane {old_pane_id} ═══\x1b[0m"
    ));

    lines.join("\r\n") + "\r\n"
}

/// Format epoch ms to human-readable string.
fn format_epoch_ms(epoch_ms: u64) -> String {
    let secs = epoch_ms / 1000;
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let s = secs % 60;
    // Use UTC for simplicity; downstream could localize
    format!("{hours:02}:{mins:02}:{s:02} UTC")
}

// =============================================================================
// Session restore engine
// =============================================================================

/// The session restore engine orchestrates the full restore flow.
pub struct SessionRestorer {
    db_path: Arc<String>,
    config: SessionRestoreConfig,
}

impl SessionRestorer {
    /// Create a new session restorer.
    pub fn new(db_path: Arc<String>, config: SessionRestoreConfig) -> Self {
        Self { db_path, config }
    }

    /// Whether auto-restore is enabled (skip user prompt).
    pub fn auto_restore(&self) -> bool {
        self.config.auto_restore
    }

    /// Detect unclean sessions that can be restored.
    ///
    /// Returns the best candidate with the most recent restorable checkpoint,
    /// or `None` if no unclean session has usable pane-state data.
    pub fn detect(&self) -> Result<Option<SessionCandidate>, RestoreError> {
        let candidates = find_unclean_sessions(&self.db_path)?;

        if candidates.is_empty() {
            debug!("No unclean sessions found — clean startup");
            return Ok(None);
        }

        info!(
            count = candidates.len(),
            "Detected unclean session(s) from previous run"
        );

        let mut best: Option<(SessionCandidate, u64, i64)> = None;
        let mut first_error: Option<RestoreError> = None;

        for candidate in candidates {
            match load_latest_checkpoint(&self.db_path, &candidate.session_id) {
                Ok(Some(checkpoint))
                    if self.config.auto_restore
                        && checkpoint.verification == CheckpointVerification::LegacyUnverified =>
                {
                    warn!(
                        session_id = %candidate.session_id,
                        checkpoint_id = checkpoint.checkpoint_id,
                        checkpoint_at = checkpoint.checkpoint_at,
                        "Skipping legacy-unverified checkpoint in auto-restore mode"
                    );
                }
                Ok(Some(checkpoint)) if !checkpoint.pane_states.is_empty() => {
                    let checkpoint_at = checkpoint.checkpoint_at;
                    let checkpoint_id = checkpoint.checkpoint_id;
                    if best
                        .as_ref()
                        .is_none_or(|(_, best_checkpoint_at, best_checkpoint_id)| {
                            (checkpoint_at, checkpoint_id)
                                > (*best_checkpoint_at, *best_checkpoint_id)
                        })
                    {
                        best = Some((candidate, checkpoint_at, checkpoint_id));
                    }
                }
                Ok(Some(checkpoint)) => {
                    warn!(
                        session_id = %candidate.session_id,
                        checkpoint_id = checkpoint.checkpoint_id,
                        checkpoint_at = checkpoint.checkpoint_at,
                        "Skipping unclean session with empty checkpoint data"
                    );
                }
                Ok(None) => {
                    warn!(
                        session_id = %candidate.session_id,
                        "Skipping unclean session with no checkpoints"
                    );
                }
                Err(error) => {
                    warn!(
                        session_id = %candidate.session_id,
                        error = %error,
                        "Skipping unclean session with unreadable checkpoint data"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        let Some((best, checkpoint_at, checkpoint_id)) = best else {
            if let Some(error) = first_error {
                return Err(error);
            }
            debug!("No restorable unclean sessions found");
            return Ok(None);
        };

        info!(
            session_id = %best.session_id,
            checkpoint_id,
            checkpoint_at,
            ft_version = %best.ft_version,
            "Best restore candidate identified"
        );

        Ok(Some(best))
    }

    /// Load checkpoint data for a session candidate.
    pub fn load_checkpoint(
        &self,
        session: &SessionCandidate,
    ) -> Result<CheckpointData, RestoreError> {
        let checkpoint =
            load_latest_checkpoint(&self.db_path, &session.session_id)?.ok_or_else(|| {
                RestoreError::CorruptCheckpoint("no checkpoints found for session".to_string())
            })?;

        info!(
            session_id = %session.session_id,
            checkpoint_id = checkpoint.checkpoint_id,
            checkpoint_at = checkpoint.checkpoint_at,
            checkpoint_role = %checkpoint.checkpoint_role,
            verification = ?checkpoint.verification,
            pane_count = checkpoint.pane_count,
            loaded_panes = checkpoint.pane_states.len(),
            "Loaded checkpoint for restore"
        );

        Ok(checkpoint)
    }

    /// Execute the full restore: recreate layout, send banners, record mapping.
    ///
    /// The `wezterm` handle must be connected to a running WezTerm instance.
    pub async fn restore(
        &self,
        session: &SessionCandidate,
        checkpoint: &CheckpointData,
        wezterm: WeztermHandle,
    ) -> Result<RestoreSummary, RestoreError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.restore_with_cx(&cx, session, checkpoint, wezterm).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`restore`].
    ///
    /// Tick 129 upgraded this from a pre-flight delegate to a
    /// fully cx-threaded multi-step pipeline. Caller cancellation
    /// now propagates into every subsystem:
    /// - `LayoutRestorer::restore_with_cx` (tick 93)
    /// - `ScrollbackInjector::inject_with_cx` (tick 94)
    /// - per-pane `wezterm.send_text_with_cx` for banner delivery
    /// - `ProcessLauncher::execute_cx` for process relaunch
    ///
    /// Per-step `cx.checkpoint()` seams also gate each stage so a
    /// cancelled caller can bail between topology parse, layout,
    /// scrollback, banners, bookkeeping, and relaunch.
    pub async fn restore_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session: &SessionCandidate,
        checkpoint: &CheckpointData,
        wezterm: WeztermHandle,
    ) -> Result<RestoreSummary, RestoreError> {
        cx.checkpoint()
            .map_err(|err| RestoreError::Wezterm(format!("restore cancelled: {err}")))?;

        if checkpoint.session_id != session.session_id {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "checkpoint {} belongs to session {}, not {}",
                checkpoint.checkpoint_id, checkpoint.session_id, session.session_id
            )));
        }
        if checkpoint.checkpoint_role != CheckpointRole::Snapshot {
            return Err(RestoreError::CheckpointNotRestorable {
                checkpoint_id: checkpoint.checkpoint_id,
                role: checkpoint.checkpoint_role.to_string(),
            });
        }
        if self.config.auto_restore
            && checkpoint.verification == CheckpointVerification::LegacyUnverified
        {
            return Err(RestoreError::LegacyCheckpointRequiresManualRestore {
                checkpoint_id: checkpoint.checkpoint_id,
            });
        }

        let start = std::time::Instant::now();

        let topology_json = checkpoint.topology_json.as_deref().ok_or(
            RestoreError::CheckpointTopologyUnavailable {
                checkpoint_id: checkpoint.checkpoint_id,
            },
        )?;
        let topology = TopologySnapshot::from_json(topology_json)
            .map_err(|e| RestoreError::TopologyParse(e.to_string()))?;
        validate_restore_topology(checkpoint, &topology)?;

        let total_panes = topology.pane_count();

        info!(
            session_id = %session.session_id,
            windows = topology.windows.len(),
            panes = total_panes,
            "Restoring session topology (cx-first)"
        );

        cx.checkpoint().map_err(|err| {
            RestoreError::Wezterm(format!("restore cancelled before layout: {err}"))
        })?;

        let restore_config = RestoreConfig {
            restore_working_dirs: true,
            restore_split_ratios: true,
            continue_on_error: true,
        };
        let restorer = LayoutRestorer::new(wezterm.clone(), restore_config);
        let layout_result = restorer
            .restore_with_cx(cx, &topology)
            .await
            .map_err(|e| RestoreError::LayoutRestore(e.to_string()))?;

        info!(
            panes_created = layout_result.panes_created,
            windows_created = layout_result.windows_created,
            tabs_created = layout_result.tabs_created,
            failed = layout_result.failed_panes.len(),
            "Layout restoration complete"
        );

        for (old_id, error) in &layout_result.failed_panes {
            warn!(old_pane_id = old_id, error = %error, "Failed to restore pane");
        }

        let pane_state_map: HashMap<u64, &RestoredPaneState> = checkpoint
            .pane_states
            .iter()
            .map(|ps| (ps.pane_id, ps))
            .collect();

        cx.checkpoint().map_err(|err| {
            RestoreError::Wezterm(format!("restore cancelled before scrollback: {err}"))
        })?;

        let (scrollback_result, scrollback_error) = if self.config.restore_scrollback {
            match load_scrollback_data(
                &self.db_path,
                &checkpoint.pane_states,
                self.config.restore_max_lines,
            ) {
                Ok(scrollback_data) => {
                    let injector = ScrollbackInjector::new(
                        wezterm.clone(),
                        InjectionConfig {
                            max_lines: self.config.restore_max_lines,
                            ..InjectionConfig::default()
                        },
                    );
                    match injector
                        .inject_with_cx(cx, &layout_result.pane_id_map, &scrollback_data)
                        .await
                    {
                        Ok(report) => {
                            if report.failure_count() > 0 || !report.skipped.is_empty() {
                                warn!(
                                    restored = report.success_count(),
                                    failed = report.failure_count(),
                                    skipped = report.skipped.len(),
                                    "Scrollback replay completed with warnings"
                                );
                            } else {
                                info!(
                                    restored = report.success_count(),
                                    bytes = report.total_bytes(),
                                    "Scrollback replay complete"
                                );
                            }
                            (Some(report), None)
                        }
                        Err(error) => {
                            warn!(
                                session_id = %session.session_id,
                                checkpoint_id = checkpoint.checkpoint_id,
                                error = %error,
                                "Scrollback injection returned error"
                            );
                            (None, Some(error.to_string()))
                        }
                    }
                }
                Err(error) => {
                    warn!(
                        session_id = %session.session_id,
                        checkpoint_id = checkpoint.checkpoint_id,
                        error = %error,
                        "Skipping scrollback replay because persisted output could not be loaded"
                    );
                    (None, Some(error.to_string()))
                }
            }
        } else {
            (None, None)
        };

        cx.checkpoint().map_err(|err| {
            RestoreError::Wezterm(format!("restore cancelled before banner send: {err}"))
        })?;

        for (&old_id, &new_id) in &layout_result.pane_id_map {
            let pane_state = pane_state_map.get(&old_id).copied();
            let banner = restore_banner(
                old_id,
                &session.session_id,
                checkpoint.checkpoint_at,
                pane_state,
            );

            if let Err(e) = wezterm.send_text_with_cx(cx, new_id, &banner).await {
                warn!(
                    old_pane_id = old_id,
                    new_pane_id = new_id,
                    error = %e,
                    "Failed to send restore banner"
                );
            } else {
                debug!(
                    old_pane_id = old_id,
                    new_pane_id = new_id,
                    agent = ?pane_state.and_then(|ps| ps.agent_metadata.as_ref().map(|a| &a.agent_type)),
                    "Restore banner sent"
                );
            }
        }

        cx.checkpoint().map_err(|err| {
            RestoreError::Wezterm(format!("restore cancelled before bookkeeping: {err}"))
        })?;

        let mark_clean = layout_result.failed_panes.is_empty();
        let bookkeeping_checkpoint = finalize_restore(
            &self.db_path,
            &session.session_id,
            &layout_result.pane_id_map,
            mark_clean,
        )
        .map_err(|error| {
            RestoreError::Bookkeeping(format!(
                "failed to persist restore bookkeeping for session {}: {error}",
                session.session_id
            ))
        })?;
        debug!(
            checkpoint_id = bookkeeping_checkpoint,
            mark_clean, "Persisted restore bookkeeping"
        );

        if !mark_clean {
            warn!(
                session_id = %session.session_id,
                restored_panes = layout_result.pane_id_map.len(),
                failed_panes = layout_result.failed_panes.len(),
                "Session restore incomplete; leaving source session unclean for retry"
            );
        } else {
            let launch_snapshots: Vec<_> = checkpoint
                .pane_states
                .iter()
                .map(|state| launch_snapshot_from_restored_state(state, checkpoint.checkpoint_at))
                .collect();
            let launcher =
                ProcessLauncher::new(wezterm.clone(), self.config.process_relaunch.clone());
            let plans = launcher.plan(&layout_result.pane_id_map, &launch_snapshots);
            if !plans.is_empty() {
                let report = launcher.execute_cx(cx, &plans).await;
                if report.failed > 0 || report.manual > 0 {
                    warn!(
                        session_id = %session.session_id,
                        shells_launched = report.shells_launched,
                        agents_launched = report.agents_launched,
                        manual = report.manual,
                        skipped = report.skipped,
                        failed = report.failed,
                        "Process relaunch completed with follow-up required"
                    );
                } else {
                    info!(
                        session_id = %session.session_id,
                        shells_launched = report.shells_launched,
                        agents_launched = report.agents_launched,
                        skipped = report.skipped,
                        "Process relaunch complete"
                    );
                }
            }
        }

        let elapsed = start.elapsed().as_millis() as u64;
        let failed_panes = layout_result.failed_panes.len();
        let restore_status = if failed_panes == 0 {
            "complete"
        } else {
            "partial"
        };

        info!(
            session_id = %session.session_id,
            restored = layout_result.pane_id_map.len(),
            failed = failed_panes,
            total = total_panes,
            status = restore_status,
            elapsed_ms = elapsed,
            "Session restore finished (cx-first)"
        );

        Ok(RestoreSummary {
            session_id: session.session_id.clone(),
            checkpoint_id: checkpoint.checkpoint_id,
            layout_result,
            pane_states: checkpoint.pane_states.clone(),
            scrollback_result,
            scrollback_error,
            elapsed_ms: elapsed,
        })
    }

    /// Run the full detection + restore flow.
    ///
    /// Returns `None` if no restore is needed (clean startup).
    pub async fn detect_and_restore(
        &self,
        wezterm: WeztermHandle,
    ) -> Result<Option<RestoreSummary>, RestoreError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.detect_and_restore_with_cx(&cx, wezterm).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`detect_and_restore`].
    ///
    /// Checkpoint seams at each step boundary make cancellation
    /// responsive between (1) pre-flight, (2) detect, (3)
    /// checkpoint load, (4) WezTerm reachability check, and
    /// (5) before the actual restore fires. The inner restore is
    /// routed through [`restore_with_cx`] so cancellation
    /// propagates into the expensive multi-step pipeline.
    pub async fn detect_and_restore_with_cx(
        &self,
        cx: &crate::cx::Cx,
        wezterm: WeztermHandle,
    ) -> Result<Option<RestoreSummary>, RestoreError> {
        cx.checkpoint()
            .map_err(|err| RestoreError::Wezterm(format!("detect_and_restore cancelled: {err}")))?;

        let session = match self.detect()? {
            Some(s) => s,
            None => return Ok(None),
        };

        cx.checkpoint().map_err(|err| {
            RestoreError::Wezterm(format!(
                "detect_and_restore cancelled before checkpoint load: {err}"
            ))
        })?;

        let checkpoint = self.load_checkpoint(&session)?;

        if checkpoint.pane_states.is_empty() {
            warn!(
                session_id = %session.session_id,
                checkpoint_id = checkpoint.checkpoint_id,
                "Checkpoint became empty after detection; skipping restore and leaving the session unclean"
            );
            return Ok(None);
        }

        cx.checkpoint().map_err(|err| {
            RestoreError::Wezterm(format!(
                "detect_and_restore cancelled before wezterm check: {err}"
            ))
        })?;

        // ft-xbnl0.2.3 tick 129: route list_panes through cx-first.
        match wezterm.list_panes_with_cx(cx).await {
            Ok(panes) if !panes.is_empty() => {
                info!(
                    existing_panes = panes.len(),
                    "WezTerm has existing panes; restore will create new panes alongside them"
                );
            }
            Ok(_) => {
                debug!("WezTerm running with no panes — clean slate for restore");
            }
            Err(e) => {
                return Err(RestoreError::Wezterm(format!("cannot reach WezTerm: {e}")));
            }
        }

        let summary = self
            .restore_with_cx(cx, &session, &checkpoint, wezterm)
            .await?;

        Ok(Some(summary))
    }
}

// =============================================================================
// Display helpers
// =============================================================================

/// Format a restore summary for human display.
pub fn format_restore_summary(summary: &RestoreSummary) -> String {
    let mut out = String::new();
    let status = if summary.layout_result.failed_panes.is_empty() {
        "restored"
    } else {
        "partially restored"
    };
    out.push_str(&format!(
        "Session {} {}: {}/{} panes in {}ms\n",
        summary.session_id,
        status,
        summary.restored_count(),
        summary.total_count(),
        summary.elapsed_ms,
    ));

    if !summary.layout_result.failed_panes.is_empty() {
        out.push_str("Failed panes:\n");
        for (id, err) in &summary.layout_result.failed_panes {
            out.push_str(&format!("  pane {id}: {err}\n"));
        }
    }

    if let Some(report) = &summary.scrollback_result {
        out.push_str(&format!(
            "Scrollback replay: {} restored, {} failed, {} skipped, {} bytes\n",
            report.success_count(),
            report.failure_count(),
            report.skipped.len(),
            report.total_bytes(),
        ));
        for (pane_id, err) in &report.failures {
            out.push_str(&format!("  scrollback pane {pane_id}: {err}\n"));
        }
    }

    if let Some(error) = &summary.scrollback_error {
        out.push_str(&format!("Scrollback replay unavailable: {error}\n"));
    }

    out
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::session_pane_state::AgentMetadata;
    use crate::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
    use crate::wezterm::{
        MockWezterm, MoveDirection, SplitDirection, WeztermFuture, WeztermInterface,
    };
    use rusqlite::params;

    fn test_runtime_error(operation: &'static str, detail: impl Into<String>) -> crate::Error {
        crate::Error::RuntimeOperation {
            operation,
            source: crate::error::RuntimeOperationSource::Backend(detail.into()),
        }
    }

    // ── Schema-v36 role and witness regressions ──────────────────────

    #[test]
    fn load_checkpoint_by_id_verifies_unmodified_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-ok", false);

        let mut mapping = HashMap::new();
        mapping.insert(11u64, 42u64);
        mapping.insert(22, 43);

        let cp_id =
            finalize_restore(&db_path, "sess-ok", &mapping, true).expect("finalize restore");

        let loaded = load_checkpoint_by_id(&db_path, cp_id)
            .expect("load should not error")
            .expect("receipt row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::RestoreReceipt);
        assert_eq!(loaded.verification, CheckpointVerification::VerifiedV2);
        assert_eq!(loaded.pane_count, 2);
        assert!(loaded.pane_states.is_empty());
        assert!(loaded.topology_json.is_none());
    }

    #[test]
    fn restore_receipt_cannot_be_submitted_as_layout_snapshot() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-receipt-role", false);
        let checkpoint_id = finalize_restore(
            &db_path,
            "sess-receipt-role",
            &HashMap::from([(1_u64, 10_u64)]),
            false,
        )
        .unwrap();
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .unwrap()
            .unwrap();
        let (session, _) = show_session(&db_path, "sess-receipt-role").unwrap();
        let restorer = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig::default(),
        );

        let error = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            Arc::new(MockWezterm::new()),
        ))
        .expect_err("restore receipts are bookkeeping, not topology snapshots");
        assert!(matches!(
            error,
            RestoreError::CheckpointNotRestorable {
                checkpoint_id: id,
                ..
            } if id == checkpoint_id
        ));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_tampered_v2_state_hash() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-tampered", false);

        let mut mapping = HashMap::new();
        mapping.insert(1u64, 100u64);

        let cp_id =
            finalize_restore(&db_path, "sess-tampered", &mapping, true).expect("finalize restore");

        conn.execute(
            "UPDATE session_checkpoints
             SET state_hash = 'rst2:0000000000000000000000000000000000000000000000000000000000000000'
             WHERE id = ?1",
            [cp_id],
        )
        .unwrap();

        let err = load_checkpoint_by_id(&db_path, cp_id).expect_err("tampered hash must error");
        match err {
            RestoreError::StateHashMismatch {
                checkpoint_id,
                session_id,
                stored,
                ..
            } => {
                assert_eq!(checkpoint_id, cp_id);
                assert_eq!(session_id, "sess-tampered");
                assert!(stored.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
            }
            other => panic!("expected StateHashMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn load_checkpoint_by_id_rejects_tampered_pane_id_mapping() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-map-tamper", false);

        let mut mapping = HashMap::new();
        mapping.insert(1u64, 100u64);

        let cp_id = finalize_restore(&db_path, "sess-map-tamper", &mapping, true)
            .expect("finalize restore");

        // Swap in a different old_to_new without touching state_hash.
        conn.execute(
            "UPDATE session_checkpoints
             SET metadata_json = '{\"old_to_new\":{\"1\":999}}'
             WHERE id = ?1",
            [cp_id],
        )
        .unwrap();

        let err = load_checkpoint_by_id(&db_path, cp_id).expect_err("tampered pane map must error");
        assert!(
            matches!(err, RestoreError::StateHashMismatch { .. }),
            "expected StateHashMismatch, got: {err:?}"
        );
    }

    #[test]
    fn load_checkpoint_by_id_labels_legacy_restore_literal_unverified() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-legacy", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 0, 0, ?3, 'restore_receipt')",
            params!["sess-legacy", 1_700_000_000_000i64, "{\"old_to_new\":{}}"],
        )
        .unwrap();
        let cp_id = conn.last_insert_rowid();

        let loaded = load_checkpoint_by_id(&db_path, cp_id)
            .expect("legacy receipt remains inspectable")
            .expect("legacy receipt row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::RestoreReceipt);
        assert_eq!(
            loaded.verification,
            CheckpointVerification::LegacyUnverified
        );
    }

    #[test]
    fn load_checkpoint_by_id_rejects_missing_metadata_on_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-nometa", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 0, 0, NULL, 'restore_receipt')",
            params!["sess-nometa", 1_700_000_000_000i64],
        )
        .unwrap();
        let cp_id = conn.last_insert_rowid();

        let err = load_checkpoint_by_id(&db_path, cp_id).expect_err("null metadata must error");
        assert!(
            matches!(err, RestoreError::CorruptCheckpoint(_)),
            "expected structural corruption, got: {err:?}"
        );
    }

    #[test]
    fn load_checkpoint_by_id_rejects_duplicate_receipt_target_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-duplicate-target", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 2, 0, ?3, 'restore_receipt')",
            params![
                "sess-duplicate-target",
                1_700_000_000_000i64,
                r#"{"old_to_new":{"1":99,"2":99}}"#,
            ],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("a receipt mapping must be one-to-one");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_aliased_receipt_source_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-aliased-source", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 2, 0, ?3, 'restore_receipt')",
            params![
                "sess-aliased-source",
                1_700_000_000_000i64,
                r#"{"old_to_new":{"01":98,"1":99}}"#,
            ],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("numeric aliases must not name the same source pane twice");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn startup_snapshot_is_not_confused_with_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-capture", false);
        let cp_id = insert_checkpoint(&conn, "sess-capture", 1000, 0);
        conn.execute(
            "UPDATE session_checkpoints SET checkpoint_type = 'startup' WHERE id = ?1",
            [cp_id],
        )
        .unwrap();

        let loaded = load_checkpoint_by_id(&db_path, cp_id)
            .expect("startup snapshot must load")
            .expect("startup snapshot row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::Snapshot);
        assert_eq!(loaded.checkpoint_type, "startup");
        assert_eq!(
            loaded.verification,
            CheckpointVerification::LegacyUnverified
        );
        assert!(loaded.topology_json.is_some());
    }

    fn run_async_test<F, T>(future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        use crate::runtime_async::CompatRuntime;

        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build session_restore test runtime");
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(future)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));

        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    struct SplitFailOnceWezterm {
        inner: MockWezterm,
        split_calls: AtomicUsize,
    }

    impl SplitFailOnceWezterm {
        fn new() -> Self {
            Self {
                inner: MockWezterm::new(),
                split_calls: AtomicUsize::new(0),
            }
        }
    }

    impl WeztermInterface for SplitFailOnceWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            self.inner.list_panes()
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
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

        fn spawn_targeted(
            &self,
            cwd: Option<&str>,
            domain_name: Option<&str>,
            target: crate::wezterm::SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            self.inner.spawn_targeted(cwd, domain_name, target)
        }

        fn split_pane(
            &self,
            pane_id: u64,
            direction: SplitDirection,
            cwd: Option<&str>,
            percent: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            if self.split_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(async {
                    Err(test_runtime_error(
                        "session_restore.test.split_pane",
                        "simulated split failure",
                    ))
                });
            }

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

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            self.inner.circuit_status()
        }
    }

    struct SpawnFailSecondTabWezterm {
        inner: MockWezterm,
        spawn_calls: AtomicUsize,
    }

    impl SpawnFailSecondTabWezterm {
        fn new() -> Self {
            Self {
                inner: MockWezterm::new(),
                spawn_calls: AtomicUsize::new(0),
            }
        }
    }

    impl WeztermInterface for SpawnFailSecondTabWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            self.inner.list_panes()
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
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
            if self.spawn_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Box::pin(async {
                    Err(test_runtime_error(
                        "session_restore.test.spawn",
                        "simulated second-tab spawn failure",
                    ))
                });
            }

            self.inner.spawn(cwd, domain_name)
        }

        fn spawn_targeted(
            &self,
            cwd: Option<&str>,
            domain_name: Option<&str>,
            target: crate::wezterm::SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            if self.spawn_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Box::pin(async {
                    Err(test_runtime_error(
                        "session_restore.test.spawn_targeted",
                        "simulated second-tab spawn failure",
                    ))
                });
            }

            self.inner.spawn_targeted(cwd, domain_name, target)
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

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            self.inner.circuit_status()
        }
    }

    fn setup_test_db() -> (String, Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();

        // Create schema tables
        conn.execute_batch(
            "CREATE TABLE mux_sessions (
                session_id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                last_checkpoint_at INTEGER,
                shutdown_clean INTEGER NOT NULL DEFAULT 0,
                topology_json TEXT NOT NULL,
                window_metadata_json TEXT,
                ft_version TEXT NOT NULL,
                host_id TEXT,
                clean_checkpoint_id INTEGER
                    REFERENCES session_checkpoints(id) ON DELETE SET NULL
            );

            CREATE TABLE session_checkpoints (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                checkpoint_at INTEGER NOT NULL,
                checkpoint_type TEXT NOT NULL,
                state_hash TEXT NOT NULL,
                pane_count INTEGER NOT NULL,
                total_bytes INTEGER NOT NULL,
                metadata_json TEXT,
                checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
                    CHECK(checkpoint_role IN ('snapshot','restore_receipt')),
                topology_json TEXT
            );

            CREATE TABLE mux_pane_state (
                id INTEGER PRIMARY KEY,
                checkpoint_id INTEGER NOT NULL,
                pane_id INTEGER NOT NULL,
                cwd TEXT,
                command TEXT,
                env_json TEXT,
                terminal_state_json TEXT NOT NULL,
                agent_metadata_json TEXT,
                scrollback_checkpoint_seq INTEGER,
                last_output_at INTEGER
            );

            CREATE TABLE output_segments (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_len INTEGER NOT NULL,
                content_hash TEXT,
                captured_at INTEGER NOT NULL,
                UNIQUE(pane_id, seq)
            );

            CREATE INDEX idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
            CREATE INDEX idx_checkpoints_session_role_latest
                ON session_checkpoints(session_id, checkpoint_role, checkpoint_at DESC, id DESC);
            CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
            CREATE INDEX idx_output_segments_pane_seq ON output_segments(pane_id, seq);",
        )
        .unwrap();

        (db_path, conn, dir)
    }

    fn insert_session(conn: &Connection, session_id: &str, shutdown_clean: bool) {
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, 1000i64, topology, "0.1.0", shutdown_clean as i64],
        )
        .unwrap();
        if shutdown_clean {
            insert_clean_receipt(conn, session_id, 1000);
        }
    }

    fn insert_clean_receipt(conn: &Connection, session_id: &str, checkpoint_at: i64) {
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role,
              topology_json)
             VALUES (?1, ?2, 'startup', 'restore', 0, 0,
                     '{\"old_to_new\":{}}', 'restore_receipt', NULL)",
            params![session_id, checkpoint_at],
        )
        .unwrap();
        let receipt_id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 last_checkpoint_at = ?2,
                 clean_checkpoint_id = ?3
             WHERE session_id = ?1",
            params![session_id, checkpoint_at, receipt_id],
        )
        .unwrap();
    }

    fn insert_checkpoint(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
        pane_count: usize,
    ) -> i64 {
        let topology_json: String = conn
            .query_row(
                "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, checkpoint_role, topology_json)
             VALUES (?1, ?2, 'periodic', '0123456789abcdef', ?3, 1024,
                     'snapshot', ?4)",
            params![session_id, checkpoint_at, pane_count as i64, topology_json],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_pane_state(
        conn: &Connection,
        checkpoint_id: i64,
        pane_id: u64,
        cwd: Option<&str>,
        command: Option<&str>,
    ) {
        insert_pane_state_with_scrollback(conn, checkpoint_id, pane_id, cwd, command, None, None);
    }

    fn insert_pane_state_with_scrollback(
        conn: &Connection,
        checkpoint_id: i64,
        pane_id: u64,
        cwd: Option<&str>,
        command: Option<&str>,
        scrollback_checkpoint_seq: Option<i64>,
        last_output_at: Option<i64>,
    ) {
        let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, cwd, command, terminal_state_json, scrollback_checkpoint_seq, last_output_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                checkpoint_id,
                pane_id as i64,
                cwd,
                command,
                terminal_json,
                scrollback_checkpoint_seq,
                last_output_at
            ],
        )
        .unwrap();
    }

    fn seal_checkpoint_v2(conn: &Connection, checkpoint_id: i64) -> String {
        let (
            session_id,
            checkpoint_at,
            checkpoint_type,
            checkpoint_role,
            pane_count,
            total_bytes,
            metadata_json,
            topology_json,
        ): (
            String,
            i64,
            String,
            String,
            i64,
            i64,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT session_id, checkpoint_at, checkpoint_type, checkpoint_role,
                        pane_count, total_bytes, metadata_json, topology_json
                 FROM session_checkpoints WHERE id = ?1",
                [checkpoint_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT pane_id, cwd, command, env_json, terminal_state_json,
                        agent_metadata_json, scrollback_checkpoint_seq, last_output_at
                 FROM mux_pane_state WHERE checkpoint_id = ?1
                 ORDER BY pane_id ASC, id ASC",
            )
            .unwrap();
        let panes = stmt
            .query_map([checkpoint_id], |row| {
                Ok(PersistedPaneState {
                    pane_id: row.get(0)?,
                    cwd: row.get(1)?,
                    command: row.get(2)?,
                    env_json: row.get(3)?,
                    terminal_state_json: row.get(4)?,
                    agent_metadata_json: row.get(5)?,
                    scrollback_checkpoint_seq: row.get(6)?,
                    last_output_at: row.get(7)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let state_hash = checkpoint_witness(
            &checkpoint_role,
            &session_id,
            checkpoint_id,
            checkpoint_at,
            &checkpoint_type,
            pane_count,
            total_bytes,
            metadata_json.as_deref(),
            topology_json.as_deref(),
            &panes,
        )
        .unwrap();
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            params![state_hash, checkpoint_id],
        )
        .unwrap();
        state_hash
    }

    fn insert_output_segment(
        conn: &Connection,
        pane_id: u64,
        seq: i64,
        content: &str,
        captured_at: i64,
    ) {
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                pane_id as i64,
                seq,
                content,
                content.len() as i64,
                captured_at
            ],
        )
        .unwrap();
    }

    fn set_single_pane_topology(conn: &Connection, session_id: &str, pane_id: u64, cwd: &str) {
        let topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    active_pane_id: Some(pane_id),
                    pane_tree: PaneNode::Leaf {
                        pane_id,
                        rows: 24,
                        cols: 80,
                        cwd: Some(cwd.to_string()),
                        title: None,
                        is_active: true,
                    },
                }],
                active_tab_index: Some(0),
            }],
        };

        let topology_json = topology.to_json().expect("serialize single-pane topology");
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params![session_id, topology_json],
        )
        .unwrap();
        conn.execute(
            "UPDATE session_checkpoints
             SET topology_json = ?2
             WHERE session_id = ?1 AND checkpoint_role = 'snapshot'",
            params![session_id, topology_json],
        )
        .unwrap();
    }

    #[test]
    fn detect_no_unclean_sessions() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-abc", true);

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn detect_unclean_session() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-crash", false);

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "sess-crash");
    }

    #[test]
    fn detect_picks_most_recent() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-old", false);
        insert_session(&conn, "sess-new", false);

        // Give sess-new a more recent checkpoint
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 2000 WHERE session_id = 'sess-new'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 1000 WHERE session_id = 'sess-old'",
            [],
        )
        .unwrap();

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert_eq!(candidates[0].session_id, "sess-new");
    }

    #[test]
    fn find_unclean_sessions_rejects_invalid_shutdown_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-bad-clean", 1000i64, topology, "0.1.0", 2i64],
        )
        .unwrap();

        let err = find_unclean_sessions(&db_path).expect_err("invalid shutdown_clean");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.shutdown_clean",
                value: 2
            }
        ));
    }

    // [ft-8ihuz] Pin the DESC+LIMIT behavior in load_scrollback_data:
    //   1. When more segments exist than max_lines, only the last
    //      max_lines are loaded and they appear in chronological
    //      order (the SQL gives DESC, the code reverses in-place).
    //   2. Total loaded content is bounded at max_lines segments —
    //      what would have been an OOM hazard on the old
    //      no-LIMIT path is now capped.
    // Structured as a direct call to load_scrollback_data via the
    // setup_test_db helper rather than going through the full
    // SessionRestorer to keep the test focused on the query shape.
    #[test]
    fn load_scrollback_data_respects_max_lines_limit_ft_8ihuz() {
        let (db_path, conn, _dir) = setup_test_db();
        let pane_id = 42u64;

        // Seed 50 segments, each with distinct content. The
        // scrollback_checkpoint_seq in pane state caps at 49 so
        // all segments are eligible.
        for seq in 0i64..50 {
            insert_output_segment(
                &conn,
                pane_id,
                seq,
                &format!("segment-content-{seq}"),
                1000 + seq,
            );
        }

        let pane_state = RestoredPaneState {
            pane_id,
            cwd: None,
            command: None,
            terminal_state: None,
            agent_metadata: None,
            scrollback_checkpoint_seq: Some(49),
            last_output_at: None,
        };

        // Ask for at most 5 lines; the SQL LIMIT should bound
        // the read to 5 rows even though 50 are available.
        let scrollbacks =
            load_scrollback_data(&db_path, std::slice::from_ref(&pane_state), 5).unwrap();
        let sb = scrollbacks.get(&pane_id).expect("pane has a scrollback");

        assert_eq!(
            sb.lines.len(),
            5,
            "ft-8ihuz: SQL LIMIT must cap segments at max_lines, not load all 50"
        );
        // DESC + reverse must produce chronological (ascending seq)
        // order. The last 5 segments by seq are 45..=49; content
        // reflects that.
        assert_eq!(sb.lines[0], "segment-content-45");
        assert_eq!(sb.lines[1], "segment-content-46");
        assert_eq!(sb.lines[2], "segment-content-47");
        assert_eq!(sb.lines[3], "segment-content-48");
        assert_eq!(sb.lines[4], "segment-content-49");

        // When max_lines is larger than the total, we get
        // everything in chronological order (no LIMIT truncation,
        // just the natural tail).
        let all = load_scrollback_data(&db_path, std::slice::from_ref(&pane_state), 100).unwrap();
        let all_sb = all.get(&pane_id).expect("pane has scrollback");
        assert_eq!(all_sb.lines.len(), 50);
        assert_eq!(all_sb.lines[0], "segment-content-0");
        assert_eq!(all_sb.lines[49], "segment-content-49");
    }

    #[test]
    fn load_checkpoint_returns_none_for_no_checkpoints() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-no-cp", false);

        let result = load_latest_checkpoint(&db_path, "sess-no-cp").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_checkpoint_with_pane_states() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-ok", false);
        let cp_id = insert_checkpoint(&conn, "sess-ok", 5000, 2);
        insert_pane_state(&conn, cp_id, 0, Some("/home/user"), Some("bash"));
        insert_pane_state(&conn, cp_id, 1, Some("/tmp"), Some("vim"));

        let data = load_latest_checkpoint(&db_path, "sess-ok")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, cp_id);
        assert_eq!(data.pane_states.len(), 2);
        assert_eq!(data.pane_states[0].cwd.as_deref(), Some("/home/user"));
        assert_eq!(data.pane_states[1].command.as_deref(), Some("vim"));
    }

    #[test]
    fn load_checkpoint_with_scrollback_metadata() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-scrollback-meta", false);
        let cp_id = insert_checkpoint(&conn, "sess-scrollback-meta", 5000, 1);
        insert_pane_state_with_scrollback(
            &conn,
            cp_id,
            5,
            Some("/tmp"),
            Some("bash"),
            Some(12),
            Some(5_100),
        );

        let data = load_checkpoint_by_id(&db_path, cp_id).unwrap().unwrap();
        let pane = &data.pane_states[0];
        assert_eq!(pane.scrollback_checkpoint_seq, Some(12));
        assert_eq!(pane.last_output_at, Some(5_100));
    }

    #[test]
    fn load_checkpoint_by_id_verifies_exact_v2_snapshot_projection() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/work"), Some("zsh"));
        conn.execute(
            "UPDATE mux_pane_state
             SET env_json = '{\"redacted_count\":0,\"vars\":{\"LANG\":\"C\"}}'
             WHERE checkpoint_id = ?1",
            [checkpoint_id],
        )
        .unwrap();
        let state_hash = seal_checkpoint_v2(&conn, checkpoint_id);
        assert!(state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX));

        let loaded = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect("v2 snapshot load")
            .expect("v2 snapshot row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::Snapshot);
        assert_eq!(loaded.verification, CheckpointVerification::VerifiedV2);
        assert_eq!(loaded.pane_states.len(), 1);
    }

    #[test]
    fn load_checkpoint_by_id_rejects_v2_snapshot_env_mutation() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2-env", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2-env", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/work"), Some("zsh"));
        seal_checkpoint_v2(&conn, checkpoint_id);
        conn.execute(
            "UPDATE mux_pane_state SET env_json = '{\"vars\":{\"LANG\":\"changed\"}}'
             WHERE checkpoint_id = ?1",
            [checkpoint_id],
        )
        .unwrap();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("env_json participates in the exact snapshot witness");
        assert!(matches!(error, RestoreError::StateHashMismatch { .. }));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_v2_snapshot_topology_mutation() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2-topology", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2-topology", 5000, 0);
        seal_checkpoint_v2(&conn, checkpoint_id);
        conn.execute(
            "UPDATE session_checkpoints
             SET topology_json = '{\"schema_version\":1,\"captured_at\":9999,\"windows\":[]}'
             WHERE id = ?1",
            [checkpoint_id],
        )
        .unwrap();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("checkpoint-local topology participates in the exact snapshot witness");
        assert!(matches!(error, RestoreError::StateHashMismatch { .. }));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_snapshot_pane_count_mismatch() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-count", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-count", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 7, None, None);

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("declared pane count must match stored rows");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_duplicate_snapshot_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-duplicate", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-duplicate", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 7, None, None);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/duplicate"), None);

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("duplicate pane IDs make restore ambiguous");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_latest_checkpoint_picks_newest() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-multi", false);
        let old_cp = insert_checkpoint(&conn, "sess-multi", 1000, 1);
        let new_cp = insert_checkpoint(&conn, "sess-multi", 2000, 1);
        insert_pane_state(&conn, old_cp, 9, Some("/old"), Some("bash"));
        insert_pane_state(&conn, new_cp, 10, Some("/new"), None);

        let data = load_latest_checkpoint(&db_path, "sess-multi")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, new_cp);
        assert_eq!(data.checkpoint_at, 2000);
    }

    #[test]
    fn load_latest_checkpoint_breaks_timestamp_ties_by_row_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-tied", false);
        let older_id = insert_checkpoint(&conn, "sess-tied", 2000, 0);
        let newer_id = insert_checkpoint(&conn, "sess-tied", 2000, 0);
        assert!(newer_id > older_id);

        let loaded = load_latest_checkpoint(&db_path, "sess-tied")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.checkpoint_id, newer_id);
    }

    #[test]
    fn load_latest_checkpoint_does_not_fall_back_past_missing_topology() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-topology-gap", false);
        let older_id = insert_checkpoint(&conn, "sess-topology-gap", 1000, 0);
        let newer_id = insert_checkpoint(&conn, "sess-topology-gap", 2000, 0);
        conn.execute(
            "UPDATE session_checkpoints SET topology_json = NULL WHERE id = ?1",
            [newer_id],
        )
        .unwrap();

        let error = load_latest_checkpoint(&db_path, "sess-topology-gap")
            .expect_err("an incomplete latest snapshot must fail closed");
        assert!(matches!(
            error,
            RestoreError::CheckpointTopologyUnavailable { checkpoint_id }
                if checkpoint_id == newer_id
        ));
        assert_ne!(older_id, newer_id);
    }

    #[test]
    fn checkpoint_load_uses_row_local_topology_not_mutable_session_topology() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-topology", false);
        let first_id = insert_checkpoint(&conn, "sess-topology", 1000, 0);
        let first_topology = load_checkpoint_by_id(&db_path, first_id)
            .unwrap()
            .unwrap()
            .topology_json
            .unwrap();

        let second_topology =
            r#"{"schema_version":1,"captured_at":2000,"workspace_id":"second","windows":[]}"#;
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params!["sess-topology", second_topology],
        )
        .unwrap();
        let second_id = insert_checkpoint(&conn, "sess-topology", 2000, 0);
        conn.execute(
            "UPDATE mux_sessions SET topology_json =
             '{\"schema_version\":1,\"captured_at\":3000,\"workspace_id\":\"latest-session-only\",\"windows\":[]}'
             WHERE session_id = 'sess-topology'",
            [],
        )
        .unwrap();

        let first = load_checkpoint_by_id(&db_path, first_id)
            .unwrap()
            .unwrap();
        let second = load_checkpoint_by_id(&db_path, second_id)
            .unwrap()
            .unwrap();
        assert_eq!(first.topology_json.as_deref(), Some(first_topology.as_str()));
        assert_eq!(second.topology_json.as_deref(), Some(second_topology));
        assert_eq!(
            load_latest_checkpoint(&db_path, "sess-topology")
                .unwrap()
                .unwrap()
                .checkpoint_id,
            second_id
        );
    }

    #[test]
    fn load_latest_checkpoint_ignores_newer_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-shadowed", false);
        let capture_cp = insert_checkpoint(&conn, "sess-shadowed", 1000, 1);
        insert_pane_state(&conn, capture_cp, 42, Some("/real"), Some("bash"));

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', ?3, 0, ?4, 'restore_receipt')",
            params![
                "sess-shadowed",
                2000i64,
                1i64,
                r#"{"old_to_new":{"42":99}}"#,
            ],
        )
        .unwrap();
        let receipt_id = conn.last_insert_rowid();

        let data = load_latest_checkpoint(&db_path, "sess-shadowed")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, capture_cp);
        assert_eq!(data.checkpoint_at, 1000);
        assert_eq!(data.pane_states.len(), 1);
        assert_eq!(data.pane_states[0].pane_id, 42);
        assert_eq!(data.pane_states[0].cwd.as_deref(), Some("/real"));

        let sessions = list_sessions(&db_path).unwrap();
        assert_eq!(sessions[0].checkpoint_count, 2);
        assert_eq!(sessions[0].pane_count, Some(1));
        let (_, checkpoints) = show_session(&db_path, "sess-shadowed").unwrap();
        assert_eq!(checkpoints[0].id, receipt_id);
        assert_eq!(
            checkpoints[0].checkpoint_role,
            CheckpointRole::RestoreReceipt
        );
        assert_eq!(checkpoints[1].id, capture_cp);
        assert_eq!(checkpoints[1].checkpoint_role, CheckpointRole::Snapshot);
    }

    #[test]
    fn load_latest_checkpoint_falls_back_to_newest_empty_checkpoint_when_needed() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-empty-only", false);
        let old_cp = insert_checkpoint(&conn, "sess-empty-only", 1000, 0);
        let new_cp = insert_checkpoint(&conn, "sess-empty-only", 2000, 0);

        let data = load_latest_checkpoint(&db_path, "sess-empty-only")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, new_cp);
        assert_eq!(data.checkpoint_at, 2000);
        assert!(data.pane_states.is_empty());

        // The older empty checkpoint should not be preferred over the newest
        // one when neither is actually restorable.
        assert_ne!(data.checkpoint_id, old_cp);
    }

    #[test]
    fn load_checkpoint_by_id_returns_none_for_missing_checkpoint() {
        let (db_path, _conn, _dir) = setup_test_db();
        let result = load_checkpoint_by_id(&db_path, 999).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_checkpoint_by_id_can_load_non_latest_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-multi-id", false);
        let old_cp = insert_checkpoint(&conn, "sess-multi-id", 1000, 1);
        let _new_cp = insert_checkpoint(&conn, "sess-multi-id", 2000, 2);
        insert_pane_state(&conn, old_cp, 42, Some("/old"), Some("bash"));

        let data = load_checkpoint_by_id(&db_path, old_cp).unwrap().unwrap();
        assert_eq!(data.checkpoint_id, old_cp);
        assert_eq!(data.session_id, "sess-multi-id");
        assert_eq!(data.checkpoint_at, 1000);
        assert_eq!(data.pane_states.len(), 1);
        assert_eq!(data.pane_states[0].pane_id, 42);
        assert_eq!(data.pane_states[0].cwd.as_deref(), Some("/old"));
    }

    #[test]
    fn mark_session_restored_sets_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-restore", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-restore", 1_000, 0);

        mark_session_restored(&db_path, "sess-restore").unwrap();

        let (clean, clean_checkpoint_id): (bool, Option<i64>) = conn
            .query_row(
                "SELECT shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'sess-restore'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(clean);
        assert_eq!(clean_checkpoint_id, Some(checkpoint_id));
    }

    #[test]
    fn save_restore_checkpoint_records_mapping() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-map", false);

        let mut mapping = HashMap::new();
        mapping.insert(0u64, 5u64);
        mapping.insert(1, 6);

        let cp_id = save_restore_checkpoint(&db_path, "sess-map", &mapping).unwrap();
        assert!(cp_id > 0);

        let (cp_type, cp_role, state_hash, metadata_json, topology_json): (
            String,
            String,
            String,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT checkpoint_type, checkpoint_role, state_hash, metadata_json, topology_json
                 FROM session_checkpoints WHERE id = ?1",
                [cp_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(cp_type, "startup");
        assert_eq!(cp_role, CHECKPOINT_ROLE_RESTORE_RECEIPT);
        assert!(state_hash.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
        assert_eq!(metadata_json, r#"{"old_to_new":{"0":5,"1":6}}"#);
        assert!(topology_json.is_none());
    }

    #[test]
    fn save_restore_checkpoint_rejects_duplicate_target_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-duplicate-map", false);
        let mapping = HashMap::from([(1_u64, 99_u64), (2_u64, 99_u64)]);

        let error = save_restore_checkpoint(&db_path, "sess-duplicate-map", &mapping)
            .expect_err("receipt writer must reject a non-injective pane mapping");
        assert!(matches!(error, RestoreError::Bookkeeping(_)));
        let checkpoint_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-duplicate-map'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);
    }

    #[test]
    fn save_restore_checkpoint_updates_session_last_checkpoint_at() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-last-cp", false);

        let checkpoint_id = save_restore_checkpoint(&db_path, "sess-last-cp", &HashMap::new())
            .expect("restore checkpoint");

        let (checkpoint_at, last_checkpoint_at): (i64, Option<i64>) = conn
            .query_row(
                "SELECT c.checkpoint_at, s.last_checkpoint_at
                 FROM session_checkpoints c
                 JOIN mux_sessions s ON s.session_id = c.session_id
                 WHERE c.id = ?1",
                [checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(last_checkpoint_at, Some(checkpoint_at));
    }

    #[test]
    fn finalize_restore_commits_checkpoint_and_clean_transition_together() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-finalize", false);

        let mut mapping = HashMap::new();
        mapping.insert(7u64, 70u64);

        let checkpoint_id =
            finalize_restore(&db_path, "sess-finalize", &mapping, true).expect("finalize restore");

        let (checkpoint_type, checkpoint_role, state_hash, metadata_json, last_checkpoint_at, shutdown_clean): (
            String,
            String,
            String,
            String,
            Option<i64>,
            bool,
        ) = conn
            .query_row(
                "SELECT c.checkpoint_type, c.checkpoint_role, c.state_hash, c.metadata_json,
                        s.last_checkpoint_at, s.shutdown_clean
                 FROM session_checkpoints c
                 JOIN mux_sessions s ON s.session_id = c.session_id
                 WHERE c.id = ?1",
                [checkpoint_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(checkpoint_type, "startup");
        assert_eq!(checkpoint_role, CHECKPOINT_ROLE_RESTORE_RECEIPT);
        assert!(state_hash.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
        assert!(metadata_json.contains("\"old_to_new\":{\"7\":70}"));
        assert!(last_checkpoint_at.is_some());
        assert!(shutdown_clean);
    }

    #[test]
    fn finalize_restore_rejects_missing_session_and_rolls_back_receipt() {
        let (db_path, conn, _dir) = setup_test_db();

        let error = finalize_restore(&db_path, "sess-missing", &HashMap::new(), true)
            .expect_err("bookkeeping must not create a receipt without its session");
        assert!(matches!(error, RestoreError::Bookkeeping(_)));
        let checkpoint_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-missing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);
    }

    #[test]
    fn finalize_restore_rolls_back_startup_checkpoint_when_clean_update_fails() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-finalize-rollback", false);
        conn.execute_batch(
            "CREATE TRIGGER abort_clean_restore
             BEFORE UPDATE OF shutdown_clean ON mux_sessions
             WHEN NEW.shutdown_clean = 1
             BEGIN
                 SELECT RAISE(ABORT, 'simulated clean transition failure');
             END;",
        )
        .unwrap();

        let mut mapping = HashMap::new();
        mapping.insert(8u64, 80u64);

        let err = finalize_restore(&db_path, "sess-finalize-rollback", &mapping, true)
            .expect_err("finalize restore should fail");
        assert!(
            err.to_string()
                .contains("simulated clean transition failure"),
            "unexpected finalize error: {err}"
        );

        let startup_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-finalize-rollback' AND checkpoint_type = 'startup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            startup_count, 0,
            "startup checkpoint insert should roll back when the clean transition aborts"
        );

        let (last_checkpoint_at, shutdown_clean): (Option<i64>, bool) = conn
            .query_row(
                "SELECT last_checkpoint_at, shutdown_clean
                 FROM mux_sessions
                 WHERE session_id = 'sess-finalize-rollback'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(last_checkpoint_at.is_none());
        assert!(!shutdown_clean);
    }

    #[test]
    fn restore_banner_basic() {
        let banner = restore_banner(42, "sess-test", 1700000000000, None);
        assert!(banner.contains("Session restored"));
        assert!(banner.contains("sess-test"));
        assert!(banner.contains("42"));
    }

    #[test]
    fn restore_banner_with_agent_context() {
        let state = RestoredPaneState {
            pane_id: 1,
            cwd: Some("/home".to_string()),
            command: Some("claude-code".to_string()),
            terminal_state: None,
            agent_metadata: Some(AgentMetadata {
                agent_type: "claude_code".to_string(),
                session_id: Some("abc123".to_string()),
                state: Some("working".to_string()),
            }),
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };

        let banner = restore_banner(1, "sess-agent", 1700000000000, Some(&state));
        assert!(banner.contains("claude_code"));
        assert!(banner.contains("abc123"));
        assert!(banner.contains("working"));
        assert!(banner.contains("claude-code")); // process name
    }

    #[test]
    fn format_epoch_ms_produces_utc() {
        // 2023-11-14 22:13:20 UTC = 1700000000 seconds
        let s = format_epoch_ms(1700000000000);
        assert_eq!(s, "22:13:20 UTC");
    }

    #[test]
    fn session_restorer_detect_empty_db() {
        let (db_path, _conn, _dir) = setup_test_db();
        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_restorer_detect_clean_sessions_only() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-clean-1", true);
        insert_session(&conn, "sess-clean-2", true);

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_restorer_detect_finds_crash() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-clean", true);
        insert_session(&conn, "sess-crash", false);
        let cp_id = insert_checkpoint(&conn, "sess-crash", 5000, 1);
        insert_pane_state(&conn, cp_id, 7, Some("/restore"), Some("bash"));

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "sess-crash");
    }

    #[test]
    fn auto_restore_never_selects_legacy_unverified_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-legacy-auto", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-legacy-auto", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));

        let manual = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );
        assert!(manual.detect().unwrap().is_some());

        let automatic = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig {
                auto_restore: true,
                ..SessionRestoreConfig::default()
            },
        );
        assert!(
            automatic.detect().unwrap().is_none(),
            "legacy evidence may be offered manually but never auto-restored"
        );
    }

    #[test]
    fn auto_restore_selects_verified_v2_snapshot() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2-auto", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2-auto", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));
        seal_checkpoint_v2(&conn, checkpoint_id);

        let automatic = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig {
                auto_restore: true,
                ..SessionRestoreConfig::default()
            },
        );
        let candidate = automatic
            .detect()
            .expect("verified auto-detect")
            .expect("verified v2 snapshot should be eligible for auto-restore");
        assert_eq!(candidate.session_id, "sess-v2-auto");
    }

    #[test]
    fn auto_restore_rejects_direct_legacy_checkpoint_submission() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-direct-legacy", false);
        set_single_pane_topology(&conn, "sess-direct-legacy", 7, "/restore");
        let checkpoint_id = insert_checkpoint(&conn, "sess-direct-legacy", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));

        let automatic = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig {
                auto_restore: true,
                ..SessionRestoreConfig::default()
            },
        );
        let mut sessions = find_unclean_sessions(&db_path).unwrap();
        let session = sessions.remove(0);
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .unwrap()
            .unwrap();
        let error = run_async_test(automatic.restore(
            &session,
            &checkpoint,
            Arc::new(MockWezterm::new()),
        ))
        .expect_err("direct submission must not bypass the auto-restore legacy gate");
        assert!(matches!(
            error,
            RestoreError::LegacyCheckpointRequiresManualRestore {
                checkpoint_id: id
            } if id == checkpoint_id
        ));
    }

    #[test]
    fn restore_rejects_topology_and_pane_state_id_mismatch_before_mux_calls() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-id-mismatch", false);
        set_single_pane_topology(&conn, "sess-id-mismatch", 8, "/topology");
        let checkpoint_id = insert_checkpoint(&conn, "sess-id-mismatch", 5000, 1);
        insert_pane_state(
            &conn,
            checkpoint_id,
            7,
            Some("/pane-state"),
            Some("bash"),
        );

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let mut sessions = find_unclean_sessions(&db_path).unwrap();
        let session = sessions.remove(0);
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .unwrap()
            .unwrap();
        let wezterm = Arc::new(MockWezterm::new());
        let error = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            wezterm.clone(),
        ))
        .expect_err("topology and pane-state IDs must agree before restore starts");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
        assert!(run_async_test(wezterm.list_panes()).unwrap().is_empty());
    }

    #[test]
    fn session_restorer_detect_skips_unclean_sessions_without_checkpoints() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-empty", false);
        insert_session(&conn, "sess-restorable", false);

        conn.execute(
            "UPDATE mux_sessions SET created_at = 3000 WHERE session_id = 'sess-empty'",
            [],
        )
        .unwrap();

        let cp_id = insert_checkpoint(&conn, "sess-restorable", 2000, 1);
        insert_pane_state(&conn, cp_id, 9, Some("/good"), Some("zsh"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 2000 WHERE session_id = 'sess-restorable'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert_eq!(
            result
                .as_ref()
                .map(|candidate| candidate.session_id.as_str()),
            Some("sess-restorable")
        );
    }

    #[test]
    fn session_restorer_detect_prefers_most_recent_usable_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-shadowed", false);
        insert_session(&conn, "sess-usable", false);

        let shadowed_capture = insert_checkpoint(&conn, "sess-shadowed", 1000, 1);
        insert_pane_state(&conn, shadowed_capture, 1, Some("/older"), Some("bash"));
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', ?3, 0, ?4, 'restore_receipt')",
            params![
                "sess-shadowed",
                4000i64,
                1i64,
                r#"{"old_to_new":{"1":11}}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 4000 WHERE session_id = 'sess-shadowed'",
            [],
        )
        .unwrap();

        let usable_capture = insert_checkpoint(&conn, "sess-usable", 2500, 1);
        insert_pane_state(&conn, usable_capture, 2, Some("/newer"), Some("fish"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 2500 WHERE session_id = 'sess-usable'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert_eq!(
            result
                .as_ref()
                .map(|candidate| candidate.session_id.as_str()),
            Some("sess-usable")
        );
    }

    #[test]
    fn session_restorer_restore_partial_failure_leaves_session_unclean_for_retry() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-partial", false);

        let split_topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    active_pane_id: Some(1),
                    pane_tree: PaneNode::HSplit {
                        children: vec![
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 1,
                                    rows: 24,
                                    cols: 80,
                                    cwd: Some("/left".to_string()),
                                    title: None,
                                    is_active: true,
                                },
                            ),
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 2,
                                    rows: 24,
                                    cols: 80,
                                    cwd: Some("/right".to_string()),
                                    title: None,
                                    is_active: false,
                                },
                            ),
                        ],
                    },
                }],
                active_tab_index: Some(0),
            }],
        };
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params![
                "sess-partial",
                split_topology.to_json().expect("serialize split topology"),
            ],
        )
        .unwrap();

        let checkpoint_id = insert_checkpoint(&conn, "sess-partial", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 1, Some("/left"), Some("bash"));
        insert_pane_state(&conn, checkpoint_id, 2, Some("/right"), Some("vim"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-partial'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(SplitFailOnceWezterm::new());
        let summary = run_async_test(restorer.restore(&session, &checkpoint, wezterm)).unwrap();

        assert_eq!(summary.restored_count(), 1);
        assert_eq!(summary.failed_count(), 1);

        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-partial'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !shutdown_clean,
            "partial restore should leave the session unclean so it can be retried"
        );

        let retry_candidate = restorer.detect().unwrap().expect("retryable session");
        assert_eq!(retry_candidate.session_id, "sess-partial");
    }

    #[test]
    fn session_restorer_root_tab_failure_leaves_session_unclean_for_retry() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-tab-fail", false);

        let multi_tab_topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![
                    TabSnapshot {
                        tab_id: 0,
                        title: None,
                        active_pane_id: Some(1),
                        pane_tree: PaneNode::Leaf {
                            pane_id: 1,
                            rows: 24,
                            cols: 80,
                            cwd: Some("/ok".to_string()),
                            title: None,
                            is_active: true,
                        },
                    },
                    TabSnapshot {
                        tab_id: 1,
                        title: None,
                        active_pane_id: Some(2),
                        pane_tree: PaneNode::Leaf {
                            pane_id: 2,
                            rows: 24,
                            cols: 80,
                            cwd: Some("/fails".to_string()),
                            title: None,
                            is_active: false,
                        },
                    },
                ],
                active_tab_index: Some(0),
            }],
        };
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params![
                "sess-tab-fail",
                multi_tab_topology
                    .to_json()
                    .expect("serialize multi-tab topology"),
            ],
        )
        .unwrap();

        let checkpoint_id = insert_checkpoint(&conn, "sess-tab-fail", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 1, Some("/ok"), Some("bash"));
        insert_pane_state(&conn, checkpoint_id, 2, Some("/fails"), Some("python"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-tab-fail'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(SpawnFailSecondTabWezterm::new());
        let summary = run_async_test(restorer.restore(&session, &checkpoint, wezterm)).unwrap();

        assert_eq!(summary.restored_count(), 1);
        assert_eq!(summary.failed_count(), 1);
        // [ft-nam3s] Derive the expected text from the same error the mock
        // returns (the ft-kccj8 propagation of the restore_layout.rs fix) —
        // a hardcoded "Runtime error: …" literal staled the moment
        // Error::RuntimeOperation gained its operation field.
        assert_eq!(
            summary.layout_result.failed_panes,
            vec![(
                2,
                test_runtime_error(
                    "session_restore.test.spawn_targeted",
                    "simulated second-tab spawn failure"
                )
                .to_string()
            )]
        );

        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-tab-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !shutdown_clean,
            "failed tab restore should leave the session unclean so it can be retried"
        );

        let retry_candidate = restorer.detect().unwrap().expect("retryable session");
        assert_eq!(retry_candidate.session_id, "sess-tab-fail");
    }

    #[test]
    fn session_restorer_successful_restore_marks_clean_and_keeps_capture_checkpoint_usable() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-success", false);
        set_single_pane_topology(&conn, "sess-success", 7, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-success", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-success'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let mut session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();
        session.topology_json = "{not authoritative topology}".to_string();

        let wezterm = Arc::new(MockWezterm::new());
        let summary =
            run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone())).unwrap();

        assert_eq!(summary.restored_count(), 1);
        assert_eq!(summary.failed_count(), 0);
        assert_eq!(summary.checkpoint_id, checkpoint_id);
        assert!(summary.scrollback_result.is_none());
        assert!(summary.scrollback_error.is_none());
        assert_eq!(summary.layout_result.pane_id_map.len(), 1);

        let verify_conn = Connection::open(&db_path).expect("open verification db");
        let shutdown_clean: bool = verify_conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-success'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            shutdown_clean,
            "successful restore should mark the source session clean"
        );

        let startup_row: (i64, String) = verify_conn
            .query_row(
                "SELECT id, metadata_json
                 FROM session_checkpoints
                 WHERE session_id = ?1 AND checkpoint_role = 'restore_receipt'
                 ORDER BY checkpoint_at DESC, id DESC
                 LIMIT 1",
                ["sess-success"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("startup checkpoint recorded after restore");
        assert_ne!(
            startup_row.0, checkpoint_id,
            "restore should create a distinct startup checkpoint"
        );

        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&7)
            .expect("old pane mapped to restored pane");
        let metadata: serde_json::Value =
            serde_json::from_str(&startup_row.1).expect("parse startup checkpoint metadata");
        assert_eq!(metadata["old_to_new"]["7"].as_u64(), Some(new_pane_id));

        let latest = load_latest_checkpoint(&db_path, "sess-success")
            .unwrap()
            .unwrap();
        assert_eq!(
            latest.checkpoint_id, checkpoint_id,
            "the original capture checkpoint should remain the preferred manual-restore snapshot"
        );
        assert_eq!(latest.pane_states.len(), 1);
        assert_eq!(latest.pane_states[0].pane_id, 7);

        let banner_text = run_async_test(wezterm.get_text(new_pane_id, false)).unwrap();
        assert!(
            banner_text.contains("Session restored"),
            "restored pane should receive the restore banner"
        );

        assert!(
            restorer.detect().unwrap().is_none(),
            "clean session should not be re-detected after a successful restore"
        );
    }

    #[test]
    fn session_restorer_successful_restore_relaunches_agent_command_when_enabled() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-agent-launch", false);
        set_single_pane_topology(&conn, "sess-agent-launch", 7, "/agents");

        let checkpoint_id = insert_checkpoint(&conn, "sess-agent-launch", 5000, 1);
        let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"agent"}"#;
        let agent_json = r#"{"agent_type":"codex","session_id":"sess-42","state":"running"}"#;
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, cwd, command, terminal_state_json, agent_metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                checkpoint_id,
                7i64,
                "/agents",
                "codex",
                terminal_json,
                agent_json
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-agent-launch'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig {
                process_relaunch: LaunchConfig {
                    launch_agents: true,
                    agent_commands: HashMap::from([(
                        "codex".to_string(),
                        "codex --resume".to_string(),
                    )]),
                    ..LaunchConfig::default()
                },
                ..SessionRestoreConfig::default()
            },
        );
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let summary =
            run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone())).unwrap();

        assert_eq!(summary.restored_count(), 1);
        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&7)
            .expect("old pane mapped to restored pane");
        let content = run_async_test(wezterm.get_text(new_pane_id, false)).unwrap();
        assert!(content.contains("Session restored"));
        assert!(content.contains("cd /agents\r"));
        assert!(content.contains("codex --resume\r"));
    }

    #[test]
    fn session_restorer_restore_errors_if_atomic_bookkeeping_fails() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-bookkeeping-fail", false);
        set_single_pane_topology(&conn, "sess-bookkeeping-fail", 9, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-bookkeeping-fail", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 9, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-bookkeeping-fail'",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER abort_bookkeeping_clean
             BEFORE UPDATE OF shutdown_clean ON mux_sessions
             WHEN NEW.shutdown_clean = 1
             BEGIN
                 SELECT RAISE(ABORT, 'simulated restore bookkeeping failure');
             END;",
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let err = run_async_test(restorer.restore(&session, &checkpoint, wezterm))
            .expect_err("restore should fail when bookkeeping cannot be committed");
        assert!(
            matches!(err, RestoreError::Bookkeeping(_)),
            "unexpected restore error: {err}"
        );
        assert!(
            err.to_string()
                .contains("simulated restore bookkeeping failure"),
            "unexpected restore error: {err}"
        );

        let startup_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-bookkeeping-fail' AND checkpoint_type = 'startup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            startup_count, 0,
            "failed restore bookkeeping should not leave a startup checkpoint behind"
        );

        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-bookkeeping-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !shutdown_clean,
            "failed restore bookkeeping should leave the session unclean for retry"
        );
    }

    #[test]
    fn session_restorer_restore_replays_scrollback_before_banner() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-scrollback", false);
        set_single_pane_topology(&conn, "sess-scrollback", 1, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-scrollback", 5000, 1);
        insert_pane_state_with_scrollback(
            &conn,
            checkpoint_id,
            1,
            Some("/restore"),
            Some("bash"),
            Some(1),
            Some(5_200),
        );
        insert_output_segment(&conn, 1, 0, "first line", 5_100);
        insert_output_segment(&conn, 1, 1, "second line", 5_200);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-scrollback'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig {
                restore_scrollback: true,
                ..SessionRestoreConfig::default()
            },
        );
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let summary =
            run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone())).unwrap();

        assert_eq!(summary.restored_count(), 1);
        assert_eq!(summary.scrollback_restored_count(), 1);
        assert_eq!(summary.scrollback_failed_count(), 0);
        assert_eq!(summary.scrollback_skipped_count(), 0);
        assert!(summary.scrollback_error.is_none());

        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&1)
            .expect("pane mapping for replayed pane");
        let content = run_async_test(wezterm.get_text(new_pane_id, false)).unwrap();
        let first_line_offset = content.find("first line").expect("replayed first line");
        let banner_offset = content
            .find("Session restored")
            .expect("restore banner after replay");

        assert!(content.contains("second line"));
        assert!(
            first_line_offset < banner_offset,
            "replayed scrollback should be written before the restore banner"
        );
    }

    #[test]
    fn session_restorer_restore_layout_only_skips_scrollback_replay() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-layout-only", false);
        set_single_pane_topology(&conn, "sess-layout-only", 7, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-layout-only", 5000, 1);
        insert_pane_state_with_scrollback(
            &conn,
            checkpoint_id,
            7,
            Some("/restore"),
            Some("bash"),
            Some(1),
            Some(5_200),
        );
        insert_output_segment(&conn, 7, 0, "first line", 5_100);
        insert_output_segment(&conn, 7, 1, "second line", 5_200);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-layout-only'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let summary =
            run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone())).unwrap();

        assert!(summary.scrollback_result.is_none());
        assert!(summary.scrollback_error.is_none());

        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&7)
            .expect("pane mapping for layout-only restore");
        let content = run_async_test(wezterm.get_text(new_pane_id, false)).unwrap();

        assert!(content.contains("Session restored"));
        assert!(!content.contains("first line"));
        assert!(!content.contains("second line"));
    }

    #[test]
    fn pane_state_parses_terminal_state_json() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-ts", false);
        let cp_id = insert_checkpoint(&conn, "sess-ts", 5000, 1);
        insert_pane_state(&conn, cp_id, 0, Some("/home"), None);

        let data = load_latest_checkpoint(&db_path, "sess-ts")
            .unwrap()
            .unwrap();
        let ts = data.pane_states[0].terminal_state.as_ref().unwrap();
        assert_eq!(ts.rows, 24);
        assert_eq!(ts.cols, 80);
        assert!(!ts.is_alt_screen);
    }

    #[test]
    fn load_latest_checkpoint_reports_corrupt_terminal_state_json() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-bad-ts", false);
        let cp_id = insert_checkpoint(&conn, "sess-bad-ts", 5000, 1);

        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, ?3)",
            params![cp_id, 0i64, "{not-json"],
        )
        .unwrap();

        let err = load_latest_checkpoint(&db_path, "sess-bad-ts").expect_err("corrupt checkpoint");
        let is_corrupt = matches!(err, RestoreError::CorruptCheckpoint(_));
        assert!(is_corrupt, "expected corrupt checkpoint error, got {err:?}");
    }

    #[test]
    fn load_latest_checkpoint_rejects_negative_pane_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-neg-pane", false);
        let cp_id = insert_checkpoint(&conn, "sess-neg-pane", 5000, 1);

        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, ?3)",
            params![
                cp_id,
                -1i64,
                r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"neg"}"#
            ],
        )
        .unwrap();

        let err = load_latest_checkpoint(&db_path, "sess-neg-pane").expect_err("negative pane id");
        assert!(
            matches!(
                err,
                RestoreError::InvalidPersistedValue {
                    field: "mux_pane_state.pane_id",
                    value: -1
                }
            ),
            "expected InvalidPersistedValue for negative pane_id, got {err:?}"
        );
    }

    #[test]
    fn restore_summary_counts() {
        let mut layout_result = RestoreResult {
            pane_id_map: HashMap::new(),
            failed_panes: Vec::new(),
            windows_created: 1,
            tabs_created: 2,
            panes_created: 3,
        };
        layout_result.pane_id_map.insert(0, 5);
        layout_result.pane_id_map.insert(1, 6);
        layout_result
            .failed_panes
            .push((2, "split failed".to_string()));

        let summary = RestoreSummary {
            session_id: "sess-test".to_string(),
            checkpoint_id: 1,
            layout_result,
            pane_states: vec![],
            scrollback_result: None,
            scrollback_error: None,
            elapsed_ms: 100,
        };

        assert_eq!(summary.restored_count(), 2);
        assert_eq!(summary.failed_count(), 1);
        assert_eq!(summary.total_count(), 3);
    }

    #[test]
    fn restore_summary_format() {
        let mut layout_result = RestoreResult {
            pane_id_map: HashMap::new(),
            failed_panes: Vec::new(),
            windows_created: 1,
            tabs_created: 1,
            panes_created: 2,
        };
        layout_result.pane_id_map.insert(0, 5);
        layout_result.pane_id_map.insert(1, 6);

        let summary = RestoreSummary {
            session_id: "sess-fmt".to_string(),
            checkpoint_id: 1,
            layout_result,
            pane_states: vec![],
            scrollback_result: None,
            scrollback_error: None,
            elapsed_ms: 42,
        };

        let text = format_restore_summary(&summary);
        assert!(text.contains("sess-fmt"));
        assert!(text.contains("restored"));
        assert!(text.contains("2/2"));
        assert!(text.contains("42ms"));
    }

    #[test]
    fn restore_summary_format_partial_restore() {
        let mut layout_result = RestoreResult {
            pane_id_map: HashMap::new(),
            failed_panes: Vec::new(),
            windows_created: 1,
            tabs_created: 1,
            panes_created: 2,
        };
        layout_result.pane_id_map.insert(0, 5);
        layout_result
            .failed_panes
            .push((1, "split failed".to_string()));

        let summary = RestoreSummary {
            session_id: "sess-partial-fmt".to_string(),
            checkpoint_id: 1,
            layout_result,
            pane_states: vec![],
            scrollback_result: None,
            scrollback_error: None,
            elapsed_ms: 42,
        };

        let text = format_restore_summary(&summary);
        assert!(text.contains("sess-partial-fmt"));
        assert!(text.contains("partially restored"));
        assert!(text.contains("1/2"));
        assert!(text.contains("Failed panes:"));
    }

    #[test]
    fn pane_state_with_agent_metadata() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-agent", false);
        let cp_id = insert_checkpoint(&conn, "sess-agent", 5000, 1);

        let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
        let agent_json = r#"{"agent_type":"claude_code","session_id":"abc","state":"idle"}"#;

        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, cwd, terminal_state_json, agent_metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![cp_id, 0i64, "/home", terminal_json, agent_json],
        )
        .unwrap();

        let data = load_latest_checkpoint(&db_path, "sess-agent")
            .unwrap()
            .unwrap();
        let agent = data.pane_states[0].agent_metadata.as_ref().unwrap();
        assert_eq!(agent.agent_type, "claude_code");
        assert_eq!(agent.state.as_deref(), Some("idle"));
    }

    // =========================================================================
    // CLI query function tests
    // =========================================================================

    #[test]
    fn list_sessions_empty() {
        let (db_path, _conn, _dir) = setup_test_db();
        let sessions = list_sessions(&db_path).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_sessions_with_data() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-a", true);
        insert_session(&conn, "sess-b", false);
        let cp_id = insert_checkpoint(&conn, "sess-b", 2000, 3);
        insert_pane_state(&conn, cp_id, 0, Some("/tmp"), None);

        let sessions = list_sessions(&db_path).unwrap();
        assert_eq!(sessions.len(), 2);

        // sess-b has a checkpoint so it sorts first (higher last_checkpoint_at)
        let b = sessions.iter().find(|s| s.session_id == "sess-b").unwrap();
        assert!(!b.shutdown_clean);
        assert_eq!(b.checkpoint_count, 1);
        assert_eq!(b.pane_count, Some(3));

        let a = sessions.iter().find(|s| s.session_id == "sess-a").unwrap();
        assert!(a.shutdown_clean);
        assert_eq!(a.checkpoint_count, 1);
        assert_eq!(a.pane_count, None);
    }

    #[test]
    fn list_and_show_session_break_checkpoint_timestamp_ties_by_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-summary-tie", false);
        let older_id = insert_checkpoint(&conn, "sess-summary-tie", 2000, 1);
        let newer_id = insert_checkpoint(&conn, "sess-summary-tie", 2000, 2);

        let sessions = list_sessions(&db_path).unwrap();
        assert_eq!(sessions[0].pane_count, Some(2));
        let (_, checkpoints) = show_session(&db_path, "sess-summary-tie").unwrap();
        assert_eq!(checkpoints[0].id, newer_id);
        assert_eq!(checkpoints[1].id, older_id);
    }

    #[test]
    fn list_sessions_rejects_negative_created_at() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-neg-created", -1i64, topology, "0.1.0", 0i64],
        )
        .unwrap();

        let err = list_sessions(&db_path).expect_err("negative created_at");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.created_at",
                value: -1
            }
        ));
    }

    #[test]
    fn list_sessions_rejects_invalid_shutdown_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-bad-clean", 1000i64, topology, "0.1.0", 2i64],
        )
        .unwrap();

        let err = list_sessions(&db_path).expect_err("invalid shutdown_clean");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.shutdown_clean",
                value: 2
            }
        ));
    }

    #[test]
    fn session_doctor_rejects_invalid_shutdown_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-bad-clean", 1000i64, topology, "0.1.0", 2i64],
        )
        .unwrap();

        let err = session_doctor(&db_path).expect_err("invalid shutdown_clean");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.shutdown_clean",
                value: 2
            }
        ));
    }

    #[test]
    fn show_session_not_found() {
        let (db_path, _conn, _dir) = setup_test_db();
        let result = show_session(&db_path, "nonexistent");
        assert!(matches!(result, Err(RestoreError::NoSessions)));
    }

    #[test]
    fn show_session_with_checkpoints() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-show", false);
        insert_checkpoint(&conn, "sess-show", 1000, 2);
        insert_checkpoint(&conn, "sess-show", 2000, 3);

        let (session, checkpoints) = show_session(&db_path, "sess-show").unwrap();
        assert_eq!(session.session_id, "sess-show");
        assert_eq!(checkpoints.len(), 2);
        // Newest first
        assert_eq!(checkpoints[0].checkpoint_at, 2000);
        assert_eq!(checkpoints[0].pane_count, 3);
        assert_eq!(checkpoints[1].checkpoint_at, 1000);
    }

    #[test]
    fn session_doctor_healthy() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-d", false);
        let cp_id = insert_checkpoint(&conn, "sess-d", 1000, 1);
        insert_pane_state(&conn, cp_id, 0, None, None);
        insert_clean_receipt(&conn, "sess-d", 1001);

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.unclean_sessions, 0);
        assert_eq!(report.total_checkpoints, 2);
        assert_eq!(report.orphaned_pane_states, 0);
    }

    #[test]
    fn session_doctor_detects_unclean() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-crash1", false);
        insert_session(&conn, "sess-crash2", false);
        insert_session(&conn, "sess-clean", true);

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.total_sessions, 3);
        assert_eq!(report.unclean_sessions, 2);
    }

    #[test]
    fn session_doctor_detects_orphans() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-o", true);

        // Insert an orphaned pane state (checkpoint_id 999 doesn't exist)
        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (999, 0, '{}')",
            [],
        )
        .unwrap();

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.orphaned_pane_states, 1);
    }

    #[test]
    fn delete_session_cascades() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-del", false);
        let cp_id = insert_checkpoint(&conn, "sess-del", 1000, 2);
        insert_pane_state(&conn, cp_id, 0, None, None);
        insert_pane_state(&conn, cp_id, 1, None, None);

        let deleted = delete_session(&db_path, "sess-del").unwrap();
        assert!(deleted);

        // Verify cascade
        let sessions = list_sessions(&db_path).unwrap();
        assert!(sessions.is_empty());

        let cp_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(cp_count, 0);

        let ps_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ps_count, 0);
    }

    #[test]
    fn delete_session_nonexistent() {
        let (db_path, _conn, _dir) = setup_test_db();
        let deleted = delete_session(&db_path, "nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn delete_session_reports_cleanup_for_orphaned_checkpoint_rows() {
        let (db_path, conn, _dir) = setup_test_db();

        // SQLite foreign-key enforcement is connection-local; this helper uses
        // a fresh connection without enabling it, so corrupted older DBs can
        // still carry checkpoint rows whose parent mux_sessions row is gone.
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "sess-orphan",
                1000i64,
                "periodic",
                "0123456789abcdef",
                1i64,
                64i64
            ],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, ?3)",
            params![checkpoint_id, 7i64, "{}"],
        )
        .unwrap();

        let deleted = delete_session(&db_path, "sess-orphan").unwrap();
        assert!(
            deleted,
            "delete_session must report success when it removed orphaned checkpoint/pane rows"
        );

        let checkpoint_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints WHERE session_id = 'sess-orphan'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);

        let pane_state_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
                [checkpoint_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pane_state_count, 0);
    }

    // ---------------------------------------------------------------
    // Expanded pure unit tests (wa-1u90p.7.1)
    // ---------------------------------------------------------------

    #[test]
    fn session_restore_config_default_values() {
        let cfg = SessionRestoreConfig::default();
        assert!(!cfg.auto_restore);
        assert!(!cfg.restore_scrollback);
        assert_eq!(cfg.restore_max_lines, 5000);
        assert!(cfg.process_relaunch.launch_shells);
        assert!(!cfg.process_relaunch.launch_agents);
    }

    #[test]
    fn session_restore_config_serde_roundtrip() {
        let cfg = SessionRestoreConfig {
            auto_restore: true,
            restore_scrollback: true,
            restore_max_lines: 10_000,
            process_relaunch: LaunchConfig {
                launch_shells: false,
                launch_agents: true,
                launch_delay_ms: 250,
                agent_commands: HashMap::from([(
                    "codex".to_string(),
                    "codex --resume".to_string(),
                )]),
            },
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.auto_restore);
        assert!(parsed.restore_scrollback);
        assert_eq!(parsed.restore_max_lines, 10_000);
        assert!(!parsed.process_relaunch.launch_shells);
        assert!(parsed.process_relaunch.launch_agents);
        assert_eq!(parsed.process_relaunch.launch_delay_ms, 250);
        assert_eq!(
            parsed.process_relaunch.agent_commands.get("codex"),
            Some(&"codex --resume".to_string())
        );
    }

    #[test]
    fn session_restore_config_serde_defaults_on_missing_fields() {
        let json = "{}";
        let parsed: SessionRestoreConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.auto_restore);
        assert!(!parsed.restore_scrollback);
        assert_eq!(parsed.restore_max_lines, 5000);
        assert!(parsed.process_relaunch.launch_shells);
        assert!(!parsed.process_relaunch.launch_agents);
    }

    #[test]
    fn session_restore_config_clone() {
        let cfg = SessionRestoreConfig {
            auto_restore: true,
            restore_scrollback: false,
            restore_max_lines: 100,
            ..SessionRestoreConfig::default()
        };
        let c = cfg.clone();
        assert!(c.auto_restore);
        assert_eq!(c.restore_max_lines, 100);
        assert_eq!(
            c.process_relaunch.launch_agents,
            cfg.process_relaunch.launch_agents
        );
    }

    #[test]
    fn session_restore_config_debug() {
        let cfg = SessionRestoreConfig::default();
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("SessionRestoreConfig"));
        assert!(dbg.contains("auto_restore"));
    }

    #[test]
    fn restore_error_display_database() {
        let err = RestoreError::Database("connection refused".to_string());
        assert_eq!(err.to_string(), "database error: connection refused");
    }

    #[test]
    fn restore_error_display_no_sessions() {
        let err = RestoreError::NoSessions;
        assert_eq!(err.to_string(), "no restorable sessions found");
    }

    #[test]
    fn restore_error_display_corrupt_checkpoint() {
        let err = RestoreError::CorruptCheckpoint("invalid JSON".to_string());
        assert_eq!(err.to_string(), "checkpoint data corrupt: invalid JSON");
    }

    #[test]
    fn restore_error_display_topology_parse() {
        let err = RestoreError::TopologyParse("missing windows".to_string());
        assert_eq!(
            err.to_string(),
            "topology deserialization failed: missing windows"
        );
    }

    #[test]
    fn restore_error_display_layout_restore() {
        let err = RestoreError::LayoutRestore("split failed".to_string());
        assert_eq!(err.to_string(), "layout restoration failed: split failed");
    }

    #[test]
    fn restore_error_display_wezterm() {
        let err = RestoreError::Wezterm("not running".to_string());
        assert_eq!(err.to_string(), "wezterm command failed: not running");
    }

    #[test]
    fn restore_error_debug_format() {
        let err = RestoreError::NoSessions;
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("NoSessions"));
    }

    #[test]
    fn session_candidate_clone() {
        let c = SessionCandidate {
            session_id: "sess-1".to_string(),
            created_at: 1000,
            last_checkpoint_at: Some(2000),
            topology_json: "{}".to_string(),
            ft_version: "0.1.0".to_string(),
            host_id: Some("host-a".to_string()),
        };
        let c2 = c.clone();
        assert_eq!(c2.session_id, "sess-1");
        assert_eq!(c2.last_checkpoint_at, Some(2000));
        assert_eq!(c2.host_id, Some("host-a".to_string()));
    }

    #[test]
    fn session_candidate_debug() {
        let c = SessionCandidate {
            session_id: "s".to_string(),
            created_at: 0,
            last_checkpoint_at: None,
            topology_json: "{}".to_string(),
            ft_version: "0.1".to_string(),
            host_id: None,
        };
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("SessionCandidate"));
    }

    #[test]
    fn checkpoint_data_clone() {
        let d = CheckpointData {
            checkpoint_id: 42,
            session_id: "sess-x".to_string(),
            checkpoint_at: 5000,
            checkpoint_type: "periodic".to_string(),
            checkpoint_role: CheckpointRole::Snapshot,
            verification: CheckpointVerification::LegacyUnverified,
            topology_json: Some("{}".to_string()),
            pane_count: 3,
            total_bytes: 0,
            pane_states: vec![],
        };
        let d2 = d.clone();
        assert_eq!(d2.checkpoint_id, 42);
        assert_eq!(d2.pane_count, 3);
        assert!(d2.pane_states.is_empty());
    }

    #[test]
    fn restored_pane_state_clone() {
        let s = RestoredPaneState {
            pane_id: 7,
            cwd: Some("/home".to_string()),
            command: Some("zsh".to_string()),
            terminal_state: None,
            agent_metadata: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let s2 = s.clone();
        assert_eq!(s2.pane_id, 7);
        assert_eq!(s2.cwd.as_deref(), Some("/home"));
        assert_eq!(s2.command.as_deref(), Some("zsh"));
    }

    #[test]
    fn format_epoch_ms_midnight() {
        // 0 = epoch = 00:00:00 UTC
        assert_eq!(format_epoch_ms(0), "00:00:00 UTC");
    }

    #[test]
    fn format_epoch_ms_one_hour() {
        assert_eq!(format_epoch_ms(3_600_000), "01:00:00 UTC");
    }

    #[test]
    fn format_epoch_ms_end_of_day() {
        // 23:59:59 = 86399 seconds = 86_399_000 ms
        assert_eq!(format_epoch_ms(86_399_000), "23:59:59 UTC");
    }

    #[test]
    fn format_epoch_ms_wraps_past_24h() {
        // 25 hours = 90_000_000 ms → 01:00:00 (wraps)
        assert_eq!(format_epoch_ms(90_000_000), "01:00:00 UTC");
    }

    #[test]
    fn format_epoch_ms_subsecond_ignored() {
        // 999ms into first second → still 00:00:00
        assert_eq!(format_epoch_ms(999), "00:00:00 UTC");
    }

    #[test]
    fn restore_banner_no_pane_state() {
        let banner = restore_banner(42, "sess-abc", 3_600_000, None);
        assert!(banner.contains("01:00:00 UTC"));
        assert!(banner.contains("sess-abc"));
        assert!(banner.contains("--pane 42"));
        assert!(!banner.contains("Previously running"));
    }

    #[test]
    fn restore_banner_with_command_only() {
        let state = RestoredPaneState {
            pane_id: 1,
            cwd: None,
            command: Some("vim".to_string()),
            terminal_state: None,
            agent_metadata: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let banner = restore_banner(1, "s1", 0, Some(&state));
        assert!(banner.contains("Process: vim"));
        assert!(!banner.contains("Previously running"));
    }

    #[test]
    fn restore_banner_with_agent_and_command() {
        let state = RestoredPaneState {
            pane_id: 2,
            cwd: None,
            command: Some("python agent.py".to_string()),
            terminal_state: None,
            agent_metadata: Some(AgentMetadata {
                agent_type: "claude-code".to_string(),
                session_id: Some("agent-sess".to_string()),
                state: Some("running".to_string()),
            }),
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let banner = restore_banner(2, "s2", 0, Some(&state));
        assert!(banner.contains("Previously running: claude-code"));
        assert!(banner.contains("agent-sess"));
        assert!(banner.contains("running"));
        assert!(banner.contains("Process: python agent.py"));
    }

    #[test]
    fn session_restore_json_contract_golden() {
        #[derive(serde::Serialize)]
        struct SessionCandidateGolden<'a> {
            session_id: &'a str,
            created_at: u64,
            last_checkpoint_at: Option<u64>,
            topology_json: &'a str,
            ft_version: &'a str,
            host_id: Option<&'a str>,
        }

        #[derive(serde::Serialize)]
        struct ShowSessionGolden<'a> {
            session: SessionCandidateGolden<'a>,
            checkpoints: Vec<CheckpointInfo>,
        }

        #[derive(serde::Serialize)]
        struct SessionRestoreGolden<'a> {
            list_sessions: Vec<SessionInfo>,
            show_session: ShowSessionGolden<'a>,
            session_doctor: SessionDoctorReport,
            restore_banner: String,
        }

        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-clean", true);
        insert_session(&conn, "sess-restore", false);
        conn.execute(
            "UPDATE mux_sessions
             SET created_at = 1500, last_checkpoint_at = 3000, host_id = 'host-a'
             WHERE session_id = 'sess-restore'",
            [],
        )
        .unwrap();

        let older_checkpoint = insert_checkpoint(&conn, "sess-restore", 2000, 1);
        conn.execute(
            "UPDATE session_checkpoints
             SET total_bytes = 2048
             WHERE id = ?1",
            [older_checkpoint],
        )
        .unwrap();
        insert_pane_state(
            &conn,
            older_checkpoint,
            7,
            Some("/agents"),
            Some("codex --resume"),
        );

        let newer_checkpoint = insert_checkpoint(&conn, "sess-restore", 3000, 2);
        conn.execute(
            "UPDATE session_checkpoints
             SET checkpoint_type = 'startup',
                 total_bytes = 512,
                 state_hash = 'startup-hash',
                 metadata_json = '{\"old_to_new\":{\"7\":42}}'
             WHERE id = ?1",
            [newer_checkpoint],
        )
        .unwrap();

        let list_sessions = list_sessions(&db_path).expect("list sessions");
        let (session, checkpoints) = show_session(&db_path, "sess-restore").expect("show session");
        let session_doctor = session_doctor(&db_path).expect("session doctor");
        let restore_banner = restore_banner(
            7,
            "sess-restore",
            3_000,
            Some(&RestoredPaneState {
                pane_id: 7,
                cwd: Some("/agents".to_string()),
                command: Some("codex --resume".to_string()),
                terminal_state: None,
                agent_metadata: Some(AgentMetadata {
                    agent_type: "codex".to_string(),
                    session_id: Some("agent-sess-7".to_string()),
                    state: Some("running".to_string()),
                }),
                scrollback_checkpoint_seq: None,
                last_output_at: None,
            }),
        );

        let actual = serde_json::to_string_pretty(&SessionRestoreGolden {
            list_sessions,
            show_session: ShowSessionGolden {
                session: SessionCandidateGolden {
                    session_id: &session.session_id,
                    created_at: session.created_at,
                    last_checkpoint_at: session.last_checkpoint_at,
                    topology_json: &session.topology_json,
                    ft_version: &session.ft_version,
                    host_id: session.host_id.as_deref(),
                },
                checkpoints,
            },
            session_doctor,
            restore_banner,
        })
        .expect("serialize golden contract");

        let expected = r#"{
  "list_sessions": [
    {
      "session_id": "sess-restore",
      "created_at": 1500,
      "last_checkpoint_at": 3000,
      "shutdown_clean": false,
      "ft_version": "0.1.0",
      "host_id": "host-a",
      "checkpoint_count": 2,
      "pane_count": 2
    },
    {
      "session_id": "sess-clean",
      "created_at": 1000,
      "last_checkpoint_at": 1000,
      "shutdown_clean": true,
      "ft_version": "0.1.0",
      "host_id": null,
      "checkpoint_count": 1,
      "pane_count": null
    }
  ],
  "show_session": {
    "session": {
      "session_id": "sess-restore",
      "created_at": 1500,
      "last_checkpoint_at": 3000,
      "topology_json": "{\"schema_version\":1,\"captured_at\":1000,\"windows\":[]}",
      "ft_version": "0.1.0",
      "host_id": "host-a"
    },
    "checkpoints": [
      {
        "id": 3,
        "checkpoint_at": 3000,
        "checkpoint_type": "startup",
        "checkpoint_role": "snapshot",
        "pane_count": 2,
        "total_bytes": 512
      },
      {
        "id": 2,
        "checkpoint_at": 2000,
        "checkpoint_type": "periodic",
        "checkpoint_role": "snapshot",
        "pane_count": 1,
        "total_bytes": 2048
      }
    ]
  },
  "session_doctor": {
    "total_sessions": 2,
    "unclean_sessions": 1,
    "total_checkpoints": 3,
    "orphaned_pane_states": 0,
    "total_data_bytes": 2560
  },
  "restore_banner": "\u001b[1;36m═══ Session restored from checkpoint at 00:00:03 UTC ═══\u001b[0m\r\n\u001b[1;33m═══ Previously running: codex (session agent-sess-7, state: running) ═══\u001b[0m\r\n\u001b[90m═══ Process: codex --resume ═══\u001b[0m\r\n\u001b[90m═══ Previous output: ft session show sess-restore --pane 7 ═══\u001b[0m\r\n"
}"#;

        assert_eq!(
            actual, expected,
            "session restore JSON contract drifted; review intentional changes before updating the golden"
        );
    }

    #[test]
    fn session_info_serialize() {
        let info = SessionInfo {
            session_id: "sess-1".to_string(),
            created_at: 1000,
            last_checkpoint_at: Some(2000),
            shutdown_clean: true,
            ft_version: "0.1.0".to_string(),
            host_id: None,
            checkpoint_count: 5,
            pane_count: Some(3),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["checkpoint_count"], 5);
        assert_eq!(json["pane_count"], 3);
        assert_eq!(json["shutdown_clean"], true);
    }

    #[test]
    fn checkpoint_info_serialize() {
        let info = CheckpointInfo {
            id: 99,
            checkpoint_at: 5000,
            checkpoint_type: "periodic".to_string(),
            checkpoint_role: CheckpointRole::Snapshot,
            pane_count: 4,
            total_bytes: 8192,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], 99);
        assert_eq!(json["pane_count"], 4);
        assert_eq!(json["total_bytes"], 8192);
    }

    #[test]
    fn session_doctor_report_serialize() {
        let report = SessionDoctorReport {
            total_sessions: 3,
            unclean_sessions: 1,
            total_checkpoints: 10,
            orphaned_pane_states: 2,
            total_data_bytes: 4096,
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["total_sessions"], 3);
        assert_eq!(json["unclean_sessions"], 1);
        assert_eq!(json["orphaned_pane_states"], 2);
    }

    #[test]
    fn session_doctor_report_clone() {
        let report = SessionDoctorReport {
            total_sessions: 1,
            unclean_sessions: 0,
            total_checkpoints: 5,
            orphaned_pane_states: 0,
            total_data_bytes: 0,
        };
        let c = report.clone();
        assert_eq!(c.total_sessions, 1);
        assert_eq!(c.total_checkpoints, 5);
    }

    #[test]
    fn session_restorer_auto_restore_default() {
        let restorer = SessionRestorer::new(
            Arc::new("/tmp/test.db".to_string()),
            SessionRestoreConfig::default(),
        );
        assert!(!restorer.auto_restore());
    }

    #[test]
    fn session_restorer_auto_restore_enabled() {
        let restorer = SessionRestorer::new(
            Arc::new("/tmp/test.db".to_string()),
            SessionRestoreConfig {
                auto_restore: true,
                ..Default::default()
            },
        );
        assert!(restorer.auto_restore());
    }

    /// ft-xbnl0.2.3 Cx-first: `restore_with_cx` must produce a
    /// RestoreSummary equivalent to `restore` on a single-pane
    /// clean-restore case. Uses the same MockWezterm harness
    /// the legacy success test uses; asserts restored_count,
    /// failed_count, and pane_id_map size match expectations.
    #[test]
    fn session_restorer_restore_with_cx_matches_legacy() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-cx-success", false);
        set_single_pane_topology(&conn, "sess-cx-success", 9, "/cx-restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-cx-success", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 9, Some("/cx-restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-cx-success'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let cx = crate::cx::for_request();
        let summary =
            run_async_test(restorer.restore_with_cx(&cx, &session, &checkpoint, wezterm.clone()))
                .unwrap();

        assert_eq!(summary.restored_count(), 1);
        assert_eq!(summary.failed_count(), 0);
        assert_eq!(summary.checkpoint_id, checkpoint_id);
        assert_eq!(summary.layout_result.pane_id_map.len(), 1);
    }
}
