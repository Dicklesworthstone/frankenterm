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
        "SELECT session_id, created_at, last_checkpoint_at, topology_json, ft_version, host_id
         FROM mux_sessions
         WHERE shutdown_clean = 0
         ORDER BY COALESCE(last_checkpoint_at, created_at) DESC",
    )?;

    let candidates = stmt
        .query_map([], |row| {
            Ok(SessionCandidate {
                session_id: row.get(0)?,
                created_at: row.get::<_, i64>(1)? as u64,
                last_checkpoint_at: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                topology_json: row.get(3)?,
                ft_version: row.get(4)?,
                host_id: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(candidates)
}

/// Load the latest checkpoint for a session, including pane states.
pub fn load_latest_checkpoint(
    db_path: &str,
    session_id: &str,
) -> Result<Option<CheckpointData>, RestoreError> {
    let conn = open_conn(db_path)?;

    // Get latest checkpoint
    let checkpoint = conn.query_row(
        "SELECT id, checkpoint_at, checkpoint_type, pane_count
         FROM session_checkpoints
         WHERE session_id = ?1
         ORDER BY checkpoint_at DESC
         LIMIT 1",
        [session_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)? as usize,
            ))
        },
    );

    let (checkpoint_id, checkpoint_at, checkpoint_type, pane_count) = match checkpoint {
        Ok(c) => c,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(RestoreError::Database(e.to_string())),
    };

    // Load pane states
    let mut stmt = conn.prepare(
        "SELECT pane_id, cwd, command, terminal_state_json, agent_metadata_json
         FROM mux_pane_state
         WHERE checkpoint_id = ?1",
    )?;

    let pane_states = stmt
        .query_map([checkpoint_id], |row| {
            let terminal_json: Option<String> = row.get(3)?;
            let agent_json: Option<String> = row.get(4)?;

            Ok(RestoredPaneState {
                pane_id: row.get::<_, i64>(0)? as u64,
                cwd: row.get(1)?,
                command: row.get(2)?,
                terminal_state: terminal_json
                    .and_then(|j| serde_json::from_str::<TerminalState>(&j).ok()),
                agent_metadata: agent_json
                    .and_then(|j| serde_json::from_str::<AgentMetadata>(&j).ok()),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Some(CheckpointData {
        checkpoint_id,
        session_id: session_id.to_string(),
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
    let conn = open_conn(db_path)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let metadata = serde_json::json!({
        "old_to_new": pane_id_map.iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect::<HashMap<String, u64>>(),
    });

    conn.execute(
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

    Ok(conn.last_insert_rowid())
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
    /// Returns the best candidate (most recent checkpoint) or `None` if
    /// all sessions shut down cleanly.
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

        // Pick the most recent (already sorted by last_checkpoint_at DESC)
        let best = candidates.into_iter().next().unwrap();

        info!(
            session_id = %best.session_id,
            last_checkpoint = ?best.last_checkpoint_at,
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

        // 5. Mark old session as restored
        if let Err(e) = mark_session_restored(&self.db_path, &session.session_id) {
            warn!(
                session_id = %session.session_id,
                error = %e,
                "Failed to mark session as restored"
            );
        }

        let elapsed = start.elapsed().as_millis() as u64;

        info!(
            session_id = %session.session_id,
            restored = layout_result.pane_id_map.len(),
            total = total_panes,
            elapsed_ms = elapsed,
            "Session restore complete"
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
    out.push_str(&format!(
        "Session {} restored: {}/{} panes in {}ms\n",
        summary.session_id,
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
    use super::*;
    use crate::session_pane_state::AgentMetadata;
    use rusqlite::params;

    fn setup_test_db() -> (String, Connection) {
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

        // Leak the tempdir so it persists through the test
        std::mem::forget(dir);
        (db_path, conn)
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
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-abc", true);

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn detect_unclean_session() {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-crash", false);

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "sess-crash");
    }

    #[test]
    fn detect_picks_most_recent() {
        let (db_path, conn) = setup_test_db();
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
    fn load_checkpoint_returns_none_for_no_checkpoints() {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-no-cp", false);

        let result = load_latest_checkpoint(&db_path, "sess-no-cp").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_checkpoint_with_pane_states() {
        let (db_path, conn) = setup_test_db();
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
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-multi", false);
        let _old_cp = insert_checkpoint(&conn, "sess-multi", 1000, 1);
        let new_cp = insert_checkpoint(&conn, "sess-multi", 2000, 3);
        insert_pane_state(&conn, new_cp, 10, Some("/new"), None);

        let data = load_latest_checkpoint(&db_path, "sess-multi")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, new_cp);
        assert_eq!(data.checkpoint_at, 2000);
    }

    #[test]
    fn mark_session_restored_sets_clean_flag() {
        let (db_path, conn) = setup_test_db();
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
        let (db_path, conn) = setup_test_db();
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
        let (db_path, _conn) = setup_test_db();
        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_restorer_detect_clean_sessions_only() {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-clean-1", true);
        insert_session(&conn, "sess-clean-2", true);

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_restorer_detect_finds_crash() {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-clean", true);
        insert_session(&conn, "sess-crash", false);

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "sess-crash");
    }

    #[test]
    fn pane_state_parses_terminal_state_json() {
        let (db_path, conn) = setup_test_db();
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
        assert!(text.contains("2/2"));
        assert!(text.contains("42ms"));
    }

    #[test]
    fn pane_state_with_agent_metadata() {
        let (db_path, conn) = setup_test_db();
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
}
