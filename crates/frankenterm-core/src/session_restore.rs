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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::restore_layout::{LayoutRestorer, RestoreConfig, RestoreResult};
use crate::session_pane_state::{AgentMetadata, TerminalState};
use crate::session_topology::TopologySnapshot;
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
}

impl Default for SessionRestoreConfig {
    fn default() -> Self {
        Self {
            auto_restore: false,
            restore_scrollback: false,
            restore_max_lines: 5000,
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
    pub topology_json: String,
    pub ft_version: String,
    pub host_id: Option<String>,
}

/// Loaded checkpoint with per-pane state.
#[derive(Debug, Clone)]
pub struct CheckpointData {
    pub checkpoint_id: i64,
    pub session_id: String,
    pub checkpoint_at: u64,
    pub checkpoint_type: Option<String>,
    pub pane_count: usize,
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
}

// =============================================================================
// Database queries
// =============================================================================

fn open_conn(db_path: &str) -> Result<Connection, RestoreError> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    Ok(conn)
}

/// Find sessions that did not shut down cleanly.
pub fn find_unclean_sessions(db_path: &str) -> Result<Vec<SessionCandidate>, RestoreError> {
    let conn = open_conn(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT session_id, created_at, last_checkpoint_at, topology_json, ft_version, host_id,
                shutdown_clean
         FROM mux_sessions
         ORDER BY COALESCE(last_checkpoint_at, created_at) DESC",
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

    // Prefer the latest checkpoint that still has pane state rows.
    // Restore-created "startup" checkpoints only record pane ID mappings,
    // so they should not shadow the last usable capture checkpoint.
    let checkpoint_id = conn.query_row(
        "SELECT c.id
         FROM session_checkpoints c
         WHERE c.session_id = ?1
         ORDER BY EXISTS(
             SELECT 1
             FROM mux_pane_state ps
             WHERE ps.checkpoint_id = c.id
         ) DESC,
         c.checkpoint_at DESC
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
        "SELECT session_id, checkpoint_at, checkpoint_type, pane_count
         FROM session_checkpoints
         WHERE id = ?1",
        [checkpoint_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    );

    let (session_id, checkpoint_at_raw, checkpoint_type, pane_count_raw) = match checkpoint {
        Ok(c) => c,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(RestoreError::Database(e.to_string())),
    };
    let checkpoint_at = decode_u64(checkpoint_at_raw, "session_checkpoints.checkpoint_at")?;
    let pane_count = decode_usize(pane_count_raw, "session_checkpoints.pane_count")?;

    // Load pane states for this checkpoint ID.
    let mut stmt = conn.prepare(
        "SELECT pane_id, cwd, command, terminal_state_json, agent_metadata_json
         FROM mux_pane_state
         WHERE checkpoint_id = ?1",
    )?;

    let raw_pane_states = stmt
        .query_map([checkpoint_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut pane_states = Vec::with_capacity(raw_pane_states.len());
    for (pane_id_raw, cwd, command, terminal_json, agent_json) in raw_pane_states {
        let pane_id = decode_u64(pane_id_raw, "mux_pane_state.pane_id")?;
        let terminal_state =
            serde_json::from_str::<TerminalState>(&terminal_json).map_err(|error| {
                RestoreError::CorruptCheckpoint(format!(
                    "pane {pane_id} has invalid terminal_state_json: {error}"
                ))
            })?;
        let agent_metadata = match agent_json {
            Some(agent_json) => Some(serde_json::from_str::<AgentMetadata>(&agent_json).map_err(
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
            cwd,
            command,
            terminal_state: Some(terminal_state),
            agent_metadata,
        });
    }

    Ok(Some(CheckpointData {
        checkpoint_id,
        session_id,
        checkpoint_at,
        checkpoint_type,
        pane_count,
        pane_states,
    }))
}

/// Mark a session as restored (set shutdown_clean = 1).
fn mark_session_restored(db_path: &str, session_id: &str) -> Result<(), RestoreError> {
    let conn = open_conn(db_path)?;
    conn.execute(
        "UPDATE mux_sessions SET shutdown_clean = 1 WHERE session_id = ?1",
        [session_id],
    )?;
    Ok(())
}

/// Record the pane ID mapping from restore in a new startup checkpoint.
fn save_restore_checkpoint(
    db_path: &str,
    session_id: &str,
    pane_id_map: &HashMap<u64, u64>,
) -> Result<i64, RestoreError> {
    let mut conn = open_conn(db_path)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let metadata = serde_json::json!({
        "old_to_new": pane_id_map.iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect::<HashMap<String, u64>>(),
    });

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO session_checkpoints
         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes, metadata_json)
         VALUES (?1, ?2, 'startup', 'restore', ?3, 0, ?4)",
        rusqlite::params![
            session_id,
            now_ms,
            pane_id_map.len() as i64,
            metadata.to_string(),
        ],
    )?;
    let checkpoint_id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE mux_sessions SET last_checkpoint_at = ?2 WHERE session_id = ?1",
        rusqlite::params![session_id, now_ms],
    )?;
    tx.commit()?;

    Ok(checkpoint_id)
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
        "SELECT s.session_id, s.created_at, s.last_checkpoint_at, s.shutdown_clean,
                s.ft_version, s.host_id,
                (SELECT COUNT(*) FROM session_checkpoints c WHERE c.session_id = s.session_id),
                (SELECT c.pane_count FROM session_checkpoints c
                 WHERE c.session_id = s.session_id ORDER BY c.checkpoint_at DESC LIMIT 1)
         FROM mux_sessions s
         ORDER BY COALESCE(s.last_checkpoint_at, s.created_at) DESC",
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
    pub checkpoint_type: Option<String>,
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
        "SELECT id, checkpoint_at, checkpoint_type, pane_count, total_bytes
         FROM session_checkpoints
         WHERE session_id = ?1
         ORDER BY checkpoint_at DESC",
    )?;

    let raw_checkpoints = stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let checkpoints = raw_checkpoints
        .into_iter()
        .map(
            |(id, checkpoint_at, checkpoint_type, pane_count, total_bytes)| {
                Ok(CheckpointInfo {
                    id,
                    checkpoint_at: decode_u64(checkpoint_at, "session_checkpoints.checkpoint_at")?,
                    checkpoint_type,
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

    let mut shutdown_stmt = conn.prepare("SELECT shutdown_clean FROM mux_sessions")?;
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
    let conn = open_conn(db_path)?;

    // Delete pane states via checkpoint cascade
    conn.execute(
        "DELETE FROM mux_pane_state WHERE checkpoint_id IN
         (SELECT id FROM session_checkpoints WHERE session_id = ?1)",
        [session_id],
    )?;

    // Delete checkpoints
    conn.execute(
        "DELETE FROM session_checkpoints WHERE session_id = ?1",
        [session_id],
    )?;

    // Delete session
    let deleted = conn.execute(
        "DELETE FROM mux_sessions WHERE session_id = ?1",
        [session_id],
    )?;

    Ok(deleted > 0)
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

        let mut best: Option<(SessionCandidate, u64)> = None;
        let mut first_error: Option<RestoreError> = None;

        for candidate in candidates {
            match load_latest_checkpoint(&self.db_path, &candidate.session_id) {
                Ok(Some(checkpoint)) if !checkpoint.pane_states.is_empty() => {
                    let checkpoint_at = checkpoint.checkpoint_at;
                    if best
                        .as_ref()
                        .is_none_or(|(_, best_checkpoint_at)| checkpoint_at > *best_checkpoint_at)
                    {
                        best = Some((candidate, checkpoint_at));
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

        let Some((best, checkpoint_at)) = best else {
            if let Some(error) = first_error {
                return Err(error);
            }
            debug!("No restorable unclean sessions found");
            return Ok(None);
        };

        info!(
            session_id = %best.session_id,
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
        let start = std::time::Instant::now();

        // 1. Parse topology from session
        let topology = TopologySnapshot::from_json(&session.topology_json)
            .map_err(|e| RestoreError::TopologyParse(e.to_string()))?;

        let total_panes = topology.pane_count();

        info!(
            session_id = %session.session_id,
            windows = topology.windows.len(),
            panes = total_panes,
            "Restoring session topology"
        );

        // 2. Recreate layout via LayoutRestorer
        let restore_config = RestoreConfig {
            restore_working_dirs: true,
            restore_split_ratios: true,
            continue_on_error: true,
        };
        let restorer = LayoutRestorer::new(wezterm.clone(), restore_config);
        let layout_result = restorer
            .restore(&topology)
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

        // 3. Send restore banners to each restored pane
        let pane_state_map: HashMap<u64, &RestoredPaneState> = checkpoint
            .pane_states
            .iter()
            .map(|ps| (ps.pane_id, ps))
            .collect();

        for (&old_id, &new_id) in &layout_result.pane_id_map {
            let pane_state = pane_state_map.get(&old_id).copied();
            let banner = restore_banner(
                old_id,
                &session.session_id,
                checkpoint.checkpoint_at,
                pane_state,
            );

            if let Err(e) = wezterm.send_text(new_id, &banner).await {
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

        // 4. Record pane ID mapping in a new startup checkpoint
        match save_restore_checkpoint(
            &self.db_path,
            &session.session_id,
            &layout_result.pane_id_map,
        ) {
            Ok(checkpoint_id) => {
                debug!(checkpoint_id, "Saved restore checkpoint with pane mapping");
            }
            Err(e) => {
                warn!(error = %e, "Failed to save restore checkpoint");
            }
        }

        // 5. Mark the source session clean only after a full restore.
        if layout_result.failed_panes.is_empty() {
            if let Err(e) = mark_session_restored(&self.db_path, &session.session_id) {
                warn!(
                    session_id = %session.session_id,
                    error = %e,
                    "Failed to mark session as restored"
                );
            }
        } else {
            warn!(
                session_id = %session.session_id,
                restored_panes = layout_result.pane_id_map.len(),
                failed_panes = layout_result.failed_panes.len(),
                "Session restore incomplete; leaving source session unclean for retry"
            );
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
            "Session restore finished"
        );

        Ok(RestoreSummary {
            session_id: session.session_id.clone(),
            checkpoint_id: checkpoint.checkpoint_id,
            layout_result,
            pane_states: checkpoint.pane_states.clone(),
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
        // Step 1: Detect
        let session = match self.detect()? {
            Some(s) => s,
            None => return Ok(None),
        };

        // Step 2: Load checkpoint
        let checkpoint = self.load_checkpoint(&session)?;

        if checkpoint.pane_states.is_empty() {
            warn!(
                session_id = %session.session_id,
                "Checkpoint has no pane states, skipping restore"
            );
            mark_session_restored(&self.db_path, &session.session_id)?;
            return Ok(None);
        }

        // Step 3: Check WezTerm is running
        match wezterm.list_panes().await {
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

        // Step 4: Execute restore
        let summary = self.restore(&session, &checkpoint, wezterm).await?;

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

    fn run_async_test<F, T>(future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        use crate::runtime_compat::CompatRuntime;

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build session_restore test runtime");
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(future)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
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

        fn split_pane(
            &self,
            pane_id: u64,
            direction: SplitDirection,
            cwd: Option<&str>,
            percent: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            if self.split_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(async {
                    Err(crate::Error::Runtime("simulated split failure".to_string()))
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
                host_id TEXT
            );

            CREATE TABLE session_checkpoints (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                checkpoint_at INTEGER NOT NULL,
                checkpoint_type TEXT,
                state_hash TEXT NOT NULL,
                pane_count INTEGER NOT NULL,
                total_bytes INTEGER NOT NULL,
                metadata_json TEXT
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

            CREATE INDEX idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
            CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);",
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
    }

    fn insert_checkpoint(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
        pane_count: usize,
    ) -> i64 {
        conn.execute(
            "INSERT INTO session_checkpoints (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
             VALUES (?1, ?2, 'periodic', 'hash123', ?3, 1024)",
            params![session_id, checkpoint_at, pane_count as i64],
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
        let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, cwd, command, terminal_state_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![checkpoint_id, pane_id as i64, cwd, command, terminal_json],
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
    fn load_latest_checkpoint_picks_newest() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-multi", false);
        let old_cp = insert_checkpoint(&conn, "sess-multi", 1000, 1);
        let new_cp = insert_checkpoint(&conn, "sess-multi", 2000, 3);
        insert_pane_state(&conn, old_cp, 9, Some("/old"), Some("bash"));
        insert_pane_state(&conn, new_cp, 10, Some("/new"), None);

        let data = load_latest_checkpoint(&db_path, "sess-multi")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, new_cp);
        assert_eq!(data.checkpoint_at, 2000);
    }

    #[test]
    fn load_latest_checkpoint_prefers_usable_capture_over_newer_startup_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-shadowed", false);
        let capture_cp = insert_checkpoint(&conn, "sess-shadowed", 1000, 1);
        insert_pane_state(&conn, capture_cp, 42, Some("/real"), Some("bash"));

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes, metadata_json)
             VALUES (?1, ?2, 'startup', 'restore', ?3, 0, ?4)",
            params![
                "sess-shadowed",
                2000i64,
                1i64,
                r#"{"old_to_new":{"42":99}}"#,
            ],
        )
        .unwrap();

        let data = load_latest_checkpoint(&db_path, "sess-shadowed")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, capture_cp);
        assert_eq!(data.checkpoint_at, 1000);
        assert_eq!(data.pane_states.len(), 1);
        assert_eq!(data.pane_states[0].pane_id, 42);
        assert_eq!(data.pane_states[0].cwd.as_deref(), Some("/real"));
    }

    #[test]
    fn load_latest_checkpoint_falls_back_to_newest_empty_checkpoint_when_needed() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-empty-only", false);
        let old_cp = insert_checkpoint(&conn, "sess-empty-only", 1000, 1);
        let new_cp = insert_checkpoint(&conn, "sess-empty-only", 2000, 2);

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

        mark_session_restored(&db_path, "sess-restore").unwrap();

        let clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-restore'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(clean);
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

        let (cp_type, metadata_json): (String, String) = conn
            .query_row(
                "SELECT checkpoint_type, metadata_json FROM session_checkpoints WHERE id = ?1",
                [cp_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(cp_type, "startup");
        assert!(metadata_json.contains("old_to_new"));
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
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes, metadata_json)
             VALUES (?1, ?2, 'startup', 'restore', ?3, 0, ?4)",
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
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_pane_state.pane_id",
                value: -1
            }
        ));
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
        assert_eq!(a.checkpoint_count, 0);
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
        insert_session(&conn, "sess-d", true);
        let cp_id = insert_checkpoint(&conn, "sess-d", 1000, 1);
        insert_pane_state(&conn, cp_id, 0, None, None);

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.unclean_sessions, 0);
        assert_eq!(report.total_checkpoints, 1);
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

    // ---------------------------------------------------------------
    // Expanded pure unit tests (wa-1u90p.7.1)
    // ---------------------------------------------------------------

    #[test]
    fn session_restore_config_default_values() {
        let cfg = SessionRestoreConfig::default();
        assert!(!cfg.auto_restore);
        assert!(!cfg.restore_scrollback);
        assert_eq!(cfg.restore_max_lines, 5000);
    }

    #[test]
    fn session_restore_config_serde_roundtrip() {
        let cfg = SessionRestoreConfig {
            auto_restore: true,
            restore_scrollback: true,
            restore_max_lines: 10_000,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.auto_restore, true);
        assert_eq!(parsed.restore_scrollback, true);
        assert_eq!(parsed.restore_max_lines, 10_000);
    }

    #[test]
    fn session_restore_config_serde_defaults_on_missing_fields() {
        let json = "{}";
        let parsed: SessionRestoreConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.auto_restore);
        assert!(!parsed.restore_scrollback);
        assert_eq!(parsed.restore_max_lines, 5000);
    }

    #[test]
    fn session_restore_config_clone() {
        let cfg = SessionRestoreConfig {
            auto_restore: true,
            restore_scrollback: false,
            restore_max_lines: 100,
        };
        let c = cfg.clone();
        assert_eq!(c.auto_restore, true);
        assert_eq!(c.restore_max_lines, 100);
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
            checkpoint_type: Some("periodic".to_string()),
            pane_count: 3,
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
        };
        let banner = restore_banner(2, "s2", 0, Some(&state));
        assert!(banner.contains("Previously running: claude-code"));
        assert!(banner.contains("agent-sess"));
        assert!(banner.contains("running"));
        assert!(banner.contains("Process: python agent.py"));
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
            checkpoint_type: Some("periodic".to_string()),
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
}
