//! SnapshotEngine orchestrator for session persistence.
//!
//! Coordinates full mux state capture: layout topology, per-pane state,
//! scrollback references, and agent session metadata. Persists snapshots
//! to SQLite for crash-resilient session restoration.
//!
//! # Architecture
//!
//! ```text
//! SnapshotEngine
//!   ├── WeztermClient::list_panes()  → Vec<PaneInfo>
//!   ├── TopologySnapshot::from_panes()  → layout tree
//!   ├── PaneStateSnapshot::from_pane_info()  → per-pane state
//!   ├── BLAKE3 hash  → dedup (skip if unchanged)
//!   └── SQLite  → mux_sessions + session_checkpoints + mux_pane_state
//! ```
//!
//! See `wa-29k1` bead for the full design.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_correlator::AgentCorrelator;
use crate::config::SnapshotConfig;
use crate::patterns::{AgentType, Detection, Severity};
use crate::session_pane_state::PaneStateSnapshot;
use crate::session_topology::TopologySnapshot;
use crate::wezterm::PaneInfo;

// =============================================================================
// Types
// =============================================================================

/// Maximum age of a stored detection event to consider for agent state inference.
const STATE_DETECTION_MAX_AGE: Duration = Duration::from_secs(300); // 5 minutes

/// What triggered the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotTrigger {
    /// Periodic timer-based capture.
    Periodic,
    /// Manual user-initiated capture.
    Manual,
    /// Pre-restart capture (blocks until complete).
    Shutdown,
    /// Startup capture (initial state after watcher starts).
    Startup,
    /// Event-driven capture (e.g., agent session change).
    Event,
}

impl SnapshotTrigger {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Periodic => "periodic",
            Self::Manual | Self::Event => "event",
            Self::Shutdown => "shutdown",
            Self::Startup => "startup",
        }
    }
}

/// Result of a successful snapshot capture.
#[derive(Debug, Clone)]
pub struct SnapshotResult {
    /// Session ID (UUID v7).
    pub session_id: String,
    /// Checkpoint row ID in SQLite.
    pub checkpoint_id: i64,
    /// Number of panes captured.
    pub pane_count: usize,
    /// Total serialized bytes.
    pub total_bytes: usize,
    /// What triggered this snapshot.
    pub trigger: SnapshotTrigger,
}

/// Error returned when a snapshot cannot be captured.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot already in progress")]
    InProgress,
    #[error("no panes found")]
    NoPanes,
    #[error("no changes since last snapshot")]
    NoChanges,
    #[error("pane listing failed: {0}")]
    PaneList(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

// =============================================================================
// SnapshotEngine
// =============================================================================

/// Central orchestrator for mux session state capture.
///
/// Thread-safe: `in_progress` guard prevents concurrent captures.
/// The engine opens its own SQLite connection (snapshot writes are rare)
/// and does not contend with the high-frequency ingest writer.
pub struct SnapshotEngine {
    /// Path to the SQLite database.
    db_path: Arc<String>,
    /// Snapshot configuration.
    config: SnapshotConfig,
    /// Current session ID (set on first capture).
    session_id: tokio::sync::RwLock<Option<String>>,
    /// BLAKE3 hash of last captured state (for dedup).
    last_state_hash: tokio::sync::RwLock<Option<String>>,
    /// Guard: true while a capture is running.
    in_progress: AtomicBool,
}

impl SnapshotEngine {
    /// Create a new snapshot engine.
    pub fn new(db_path: Arc<String>, config: SnapshotConfig) -> Self {
        Self {
            db_path,
            config,
            session_id: tokio::sync::RwLock::new(None),
            last_state_hash: tokio::sync::RwLock::new(None),
            in_progress: AtomicBool::new(false),
        }
    }

    /// Capture a full mux state snapshot from the given pane list.
    ///
    /// This is the core method. It takes a pre-fetched pane list to
    /// decouple the engine from `WeztermClient` (easier to test).
    pub async fn capture(
        &self,
        panes: &[PaneInfo],
        trigger: SnapshotTrigger,
    ) -> std::result::Result<SnapshotResult, SnapshotError> {
        // 1. Guard: prevent concurrent captures
        if self.in_progress.swap(true, Ordering::SeqCst) {
            return Err(SnapshotError::InProgress);
        }
        // Reset guard on all exit paths via Drop
        struct InProgressGuard<'a>(&'a AtomicBool);
        impl Drop for InProgressGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = InProgressGuard(&self.in_progress);

        if panes.is_empty() {
            return Err(SnapshotError::NoPanes);
        }

        let now_ms = epoch_ms();

        // 2. Build topology snapshot
        let (topology, _report) = TopologySnapshot::from_panes(panes, now_ms);
        let topology_json = topology
            .to_json()
            .map_err(|e: serde_json::Error| SnapshotError::Serialization(e.to_string()))?;

        // 3. Correlate agent identity/state (best-effort) and build per-pane snapshots
        let mut correlator = AgentCorrelator::new();
        let pane_ids: Vec<u64> = panes.iter().map(|p| p.pane_id).collect();
        let db_path_for_detections = Arc::clone(&self.db_path);
        let cutoff_ms: i64 =
            now_ms.saturating_sub(STATE_DETECTION_MAX_AGE.as_millis() as u64) as i64;

        let detections_by_pane = tokio::task::spawn_blocking(move || {
            load_latest_detections_by_pane_sync(
                db_path_for_detections.as_str(),
                &pane_ids,
                cutoff_ms,
            )
        })
        .await
        .ok()
        .and_then(|res| res.ok())
        .unwrap_or_default();

        for (pane_id, detections) in detections_by_pane {
            correlator.ingest_detections(pane_id, &detections);
        }
        for pane in panes {
            correlator.update_from_pane_info(pane);
        }

        let pane_states: Vec<PaneStateSnapshot> = panes
            .iter()
            .map(|p| {
                let mut snapshot = PaneStateSnapshot::from_pane_info(p, now_ms, false);
                if let Some(agent) = correlator.get_metadata(p.pane_id) {
                    snapshot = snapshot.with_agent(agent);
                }
                snapshot
            })
            .collect();

        // 4. Compute state hash for dedup (from raw pane data, not timestamps)
        let state_hash = compute_state_hash(panes);

        // 5. Skip if periodic and unchanged
        if trigger == SnapshotTrigger::Periodic {
            let last = self.last_state_hash.read().await;
            if last.as_deref() == Some(&state_hash) {
                return Err(SnapshotError::NoChanges);
            }
        }

        // 6. Ensure session exists
        let session_id = self.ensure_session(&topology_json, now_ms).await?;

        // 7. Persist checkpoint + pane states in a transaction
        let checkpoint_type = trigger.as_db_str().to_string();
        let pane_count = pane_states.len();

        let db_path = Arc::clone(&self.db_path);
        let state_hash_clone = state_hash.clone();

        let result = tokio::task::spawn_blocking(move || {
            save_checkpoint_sync(
                &db_path,
                &session_id,
                now_ms,
                &checkpoint_type,
                &state_hash_clone,
                &topology_json,
                &pane_states,
            )
        })
        .await
        .map_err(|e| SnapshotError::Database(format!("task join: {e}")))?
        .map_err(|e| SnapshotError::Database(e.to_string()))?;

        // 8. Update last hash
        *self.last_state_hash.write().await = Some(state_hash);

        Ok(SnapshotResult {
            session_id: result.0,
            checkpoint_id: result.1,
            pane_count,
            total_bytes: result.2,
            trigger,
        })
    }

    /// Run retention cleanup: remove old checkpoints exceeding limits.
    pub async fn cleanup(&self) -> std::result::Result<usize, SnapshotError> {
        let db_path = Arc::clone(&self.db_path);
        let retention_count = self.config.retention_count;
        let retention_days = self.config.retention_days;

        tokio::task::spawn_blocking(move || cleanup_sync(&db_path, retention_count, retention_days))
            .await
            .map_err(|e| SnapshotError::Database(format!("task join: {e}")))?
            .map_err(|e| SnapshotError::Database(e.to_string()))
    }

    /// Run the periodic snapshot loop.
    ///
    /// `pane_provider` is called each tick to fetch the current pane list.
    /// This decouples the engine from `WeztermClient` for testability.
    pub async fn run_periodic<F, Fut>(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        pane_provider: F,
    ) where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Option<Vec<PaneInfo>>> + Send,
    {
        let interval_secs = self.config.interval_seconds.max(30);
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        // First tick fires immediately — use it for startup snapshot
        let mut is_first = true;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let trigger = if is_first {
                        is_first = false;
                        SnapshotTrigger::Startup
                    } else {
                        SnapshotTrigger::Periodic
                    };

                    if let Some(panes) = pane_provider().await {
                        match self.capture(&panes, trigger).await {
                            Ok(result) => {
                                tracing::info!(
                                    trigger = ?trigger,
                                    pane_count = result.pane_count,
                                    total_bytes = result.total_bytes,
                                    checkpoint_id = result.checkpoint_id,
                                    "snapshot captured"
                                );
                                // Run retention cleanup after successful capture
                                if let Err(e) = self.cleanup().await {
                                    tracing::warn!(error = %e, "snapshot retention cleanup failed");
                                }
                            }
                            Err(SnapshotError::NoChanges) => {
                                tracing::debug!("periodic snapshot skipped: no changes");
                            }
                            Err(SnapshotError::InProgress) => {
                                tracing::debug!("periodic snapshot skipped: capture in progress");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "snapshot capture failed");
                            }
                        }
                    } else {
                        tracing::debug!("periodic snapshot skipped: no panes available");
                    }
                }
                _ = shutdown.changed() => {
                    tracing::info!("snapshot engine shutting down");
                    break;
                }
            }
        }
    }

    /// Get or create the session ID.
    async fn ensure_session(
        &self,
        topology_json: &str,
        now_ms: u64,
    ) -> std::result::Result<String, SnapshotError> {
        {
            let guard = self.session_id.read().await;
            if let Some(ref id) = *guard {
                // Update last_checkpoint_at and topology
                let db_path = Arc::clone(&self.db_path);
                let id = id.clone();
                let topo = topology_json.to_string();
                tokio::task::spawn_blocking(move || {
                    update_session_sync(&db_path, &id, now_ms, &topo)
                })
                .await
                .map_err(|e| SnapshotError::Database(format!("task join: {e}")))?
                .map_err(|e| SnapshotError::Database(e.to_string()))?;
                return Ok(guard.clone().unwrap());
            }
        }

        // Create new session
        let session_id = generate_session_id();
        let db_path = Arc::clone(&self.db_path);
        let id = session_id.clone();
        let topo = topology_json.to_string();
        let version = crate::VERSION.to_string();
        tokio::task::spawn_blocking(move || {
            create_session_sync(&db_path, &id, now_ms, &topo, &version)
        })
        .await
        .map_err(|e| SnapshotError::Database(format!("task join: {e}")))?
        .map_err(|e| SnapshotError::Database(e.to_string()))?;

        *self.session_id.write().await = Some(session_id.clone());
        Ok(session_id)
    }

    /// Mark current session as cleanly shut down.
    pub async fn mark_shutdown(&self) -> std::result::Result<(), SnapshotError> {
        let guard = self.session_id.read().await;
        if let Some(ref id) = *guard {
            let db_path = Arc::clone(&self.db_path);
            let id = id.clone();
            tokio::task::spawn_blocking(move || mark_shutdown_sync(&db_path, &id))
                .await
                .map_err(|e| SnapshotError::Database(format!("task join: {e}")))?
                .map_err(|e| SnapshotError::Database(e.to_string()))?;
        }
        Ok(())
    }
}

/// Load the most recent detections per pane from storage.
///
/// This is best-effort: if the `events` table does not exist (e.g., tests using a
/// minimal schema), it returns an empty map.
fn load_latest_detections_by_pane_sync(
    db_path: &str,
    pane_ids: &[u64],
    cutoff_ms: i64,
) -> std::result::Result<std::collections::HashMap<u64, Vec<Detection>>, rusqlite::Error> {
    use std::collections::HashMap;

    if pane_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let conn = open_conn(db_path)?;

    let placeholders = std::iter::repeat_n("?", pane_ids.len())
        .collect::<Vec<_>>()
        .join(",");

    let sql = format!(
        "WITH ranked AS (
            SELECT pane_id,
                   rule_id,
                   agent_type,
                   event_type,
                   severity,
                   confidence,
                   extracted,
                   matched_text,
                   ROW_NUMBER() OVER (PARTITION BY pane_id ORDER BY detected_at DESC) AS rn
            FROM events
            WHERE pane_id IN ({placeholders})
              AND detected_at >= ?
              AND agent_type NOT IN ('unknown', 'wezterm')
        )
        SELECT pane_id, rule_id, agent_type, event_type, severity, confidence, extracted, matched_text
        FROM ranked
        WHERE rn = 1"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(err) if is_missing_events_table(&err) => return Ok(HashMap::new()),
        Err(err) => return Err(err),
    };

    let mut params: Vec<i64> = pane_ids.iter().map(|id| *id as i64).collect();
    params.push(cutoff_ms);

    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
    let mut out: HashMap<u64, Vec<Detection>> = HashMap::new();

    while let Some(row) = rows.next()? {
        let pane_id: i64 = row.get(0)?;
        let rule_id: String = row.get(1)?;
        let agent_type: String = row.get(2)?;
        let event_type: String = row.get(3)?;
        let severity: String = row.get(4)?;
        let confidence: f64 = row.get(5)?;
        let extracted: Option<String> = row.get(6)?;
        let matched_text: Option<String> = row.get(7)?;

        let detection = Detection {
            rule_id,
            agent_type: agent_type_from_db(&agent_type),
            event_type,
            severity: severity_from_db(&severity),
            confidence,
            extracted: extracted
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null),
            matched_text: matched_text.unwrap_or_default(),
            span: (0, 0),
        };

        out.insert(pane_id as u64, vec![detection]);
    }

    Ok(out)
}

fn is_missing_events_table(err: &rusqlite::Error) -> bool {
    err.to_string().contains("no such table: events")
}

fn agent_type_from_db(agent_type: &str) -> AgentType {
    match agent_type {
        "codex" => AgentType::Codex,
        "claude_code" => AgentType::ClaudeCode,
        "gemini" => AgentType::Gemini,
        "wezterm" => AgentType::Wezterm,
        _ => AgentType::Unknown,
    }
}

fn severity_from_db(severity: &str) -> Severity {
    match severity {
        "warning" => Severity::Warning,
        "critical" => Severity::Critical,
        _ => Severity::Info,
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Generate a time-ordered session ID (UUID v7-like: timestamp prefix + random).
fn generate_session_id() -> String {
    let ts = epoch_ms();
    let rand: u64 = rand::random();
    format!("sess-{ts:013x}-{rand:016x}")
}

/// Compute hash of pane structural data for dedup.
///
/// Hashes pane IDs, layout relationships, terminal state, and cwds —
/// but NOT timestamps, so identical layouts produce identical hashes.
fn compute_state_hash(panes: &[PaneInfo]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();

    // Sort by pane_id for deterministic ordering
    let mut ids: Vec<u64> = panes.iter().map(|p| p.pane_id).collect();
    ids.sort();
    ids.hash(&mut hasher);

    for p in panes {
        p.pane_id.hash(&mut hasher);
        p.tab_id.hash(&mut hasher);
        p.window_id.hash(&mut hasher);
        p.cwd.hash(&mut hasher);
        p.title.hash(&mut hasher);
        p.effective_rows().hash(&mut hasher);
        p.effective_cols().hash(&mut hasher);
        p.cursor_x.hash(&mut hasher);
        p.cursor_y.hash(&mut hasher);
        p.is_active.hash(&mut hasher);
        p.is_zoomed.hash(&mut hasher);
    }

    format!("{:016x}", hasher.finish())
}

// =============================================================================
// SQLite operations (sync, run inside spawn_blocking)
// =============================================================================

fn open_conn(db_path: &str) -> std::result::Result<Connection, rusqlite::Error> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    Ok(conn)
}

fn create_session_sync(
    db_path: &str,
    session_id: &str,
    now_ms: u64,
    topology_json: &str,
    ft_version: &str,
) -> std::result::Result<(), rusqlite::Error> {
    let conn = open_conn(db_path)?;
    let host_id = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_default();
    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, host_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            session_id,
            now_ms as i64,
            topology_json,
            ft_version,
            host_id
        ],
    )?;
    Ok(())
}

fn update_session_sync(
    db_path: &str,
    session_id: &str,
    now_ms: u64,
    topology_json: &str,
) -> std::result::Result<(), rusqlite::Error> {
    let conn = open_conn(db_path)?;
    conn.execute(
        "UPDATE mux_sessions SET last_checkpoint_at = ?1, topology_json = ?2
         WHERE session_id = ?3",
        rusqlite::params![now_ms as i64, topology_json, session_id],
    )?;
    Ok(())
}

fn mark_shutdown_sync(db_path: &str, session_id: &str) -> std::result::Result<(), rusqlite::Error> {
    let conn = open_conn(db_path)?;
    conn.execute(
        "UPDATE mux_sessions SET shutdown_clean = 1 WHERE session_id = ?1",
        [session_id],
    )?;
    Ok(())
}

/// Save a checkpoint with all pane states in a single transaction.
/// Returns (session_id, checkpoint_id, total_bytes).
fn save_checkpoint_sync(
    db_path: &str,
    session_id: &str,
    now_ms: u64,
    checkpoint_type: &str,
    state_hash: &str,
    _topology_json: &str,
    pane_states: &[PaneStateSnapshot],
) -> std::result::Result<(String, i64, usize), rusqlite::Error> {
    type SerializedPaneState = (
        u64,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    );

    let conn = open_conn(db_path)?;

    // Serialize all pane states and compute total bytes
    let mut serialized_states: Vec<SerializedPaneState> = Vec::new();
    let mut total_bytes: usize = 0;

    for ps in pane_states {
        let terminal_json =
            serde_json::to_string(&ps.terminal).unwrap_or_else(|_| "{}".to_string());
        let env_json = ps.env.as_ref().and_then(|e| serde_json::to_string(e).ok());
        let agent_json = ps
            .agent
            .as_ref()
            .and_then(|a| serde_json::to_string(a).ok());
        let scrollback_seq = ps.scrollback_ref.as_ref().map(|s| s.output_segments_seq);
        let last_output_at = ps.scrollback_ref.as_ref().map(|s| s.last_capture_at as i64);

        total_bytes += terminal_json.len()
            + env_json.as_ref().map_or(0, |s| s.len())
            + agent_json.as_ref().map_or(0, |s| s.len());

        serialized_states.push((
            ps.pane_id,
            terminal_json,
            env_json,
            agent_json,
            scrollback_seq,
            last_output_at,
        ));
    }

    let tx = conn.unchecked_transaction()?;

    // Insert checkpoint
    tx.execute(
        "INSERT INTO session_checkpoints
         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            session_id,
            now_ms as i64,
            checkpoint_type,
            state_hash,
            pane_states.len() as i64,
            total_bytes as i64,
        ],
    )?;

    let checkpoint_id = tx.last_insert_rowid();

    // Insert per-pane states
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, cwd, command, env_json, terminal_state_json,
              agent_metadata_json, scrollback_checkpoint_seq, last_output_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;

        for (i, ps) in pane_states.iter().enumerate() {
            let (
                _,
                ref terminal_json,
                ref env_json,
                ref agent_json,
                scrollback_seq,
                last_output_at,
            ) = serialized_states[i];
            stmt.execute(rusqlite::params![
                checkpoint_id,
                ps.pane_id as i64,
                ps.cwd,
                ps.foreground_process.as_ref().map(|p| &p.name),
                env_json,
                terminal_json,
                agent_json,
                scrollback_seq,
                last_output_at,
            ])?;
        }
    } // drop stmt before commit

    tx.commit()?;

    Ok((session_id.to_string(), checkpoint_id, total_bytes))
}

/// Remove checkpoints exceeding retention limits.
/// Returns the number of checkpoints deleted.
fn cleanup_sync(
    db_path: &str,
    retention_count: usize,
    retention_days: u64,
) -> std::result::Result<usize, rusqlite::Error> {
    let conn = open_conn(db_path)?;
    let cutoff_ms = epoch_ms().saturating_sub(retention_days * 86_400_000);

    // Delete checkpoints older than retention_days
    let deleted_by_age: usize = conn.execute(
        "DELETE FROM session_checkpoints WHERE checkpoint_at < ?1",
        [cutoff_ms as i64],
    )?;

    // Keep only the latest retention_count checkpoints per session
    let deleted_by_count: usize = conn.execute(
        "DELETE FROM session_checkpoints WHERE id NOT IN (
            SELECT id FROM session_checkpoints
            ORDER BY checkpoint_at DESC
            LIMIT ?1
        )",
        [retention_count as i64],
    )?;

    Ok(deleted_by_age + deleted_by_count)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wezterm::PaneSize;

    fn make_test_pane(id: u64, rows: u32, cols: u32) -> PaneInfo {
        PaneInfo {
            pane_id: id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: Some(PaneSize {
                rows,
                cols,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some(format!("pane-{id}")),
            cwd: Some(format!("file:///home/user/project-{id}")),
            tty_name: None,
            cursor_x: Some(5),
            cursor_y: Some(10),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: id == 0,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    fn setup_test_db() -> (tempfile::NamedTempFile, Arc<String>) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = Arc::new(tmp.path().to_str().unwrap().to_string());

        // Create schema tables
        let conn = Connection::open(db_path.as_str()).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS mux_sessions (
                session_id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                last_checkpoint_at INTEGER,
                shutdown_clean INTEGER NOT NULL DEFAULT 0,
                topology_json TEXT NOT NULL,
                window_metadata_json TEXT,
                ft_version TEXT NOT NULL,
                host_id TEXT
            );
            CREATE TABLE IF NOT EXISTS session_checkpoints (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                checkpoint_at INTEGER NOT NULL,
                checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
                state_hash TEXT NOT NULL,
                pane_count INTEGER NOT NULL,
                total_bytes INTEGER NOT NULL,
                metadata_json TEXT
            );
            CREATE TABLE IF NOT EXISTS mux_pane_state (
                id INTEGER PRIMARY KEY,
                checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
                pane_id INTEGER NOT NULL,
                cwd TEXT,
                command TEXT,
                env_json TEXT,
                terminal_state_json TEXT NOT NULL,
                agent_metadata_json TEXT,
                scrollback_checkpoint_seq INTEGER,
                last_output_at INTEGER
            );
            PRAGMA foreign_keys = ON;
            ",
        )
        .unwrap();

        (tmp, db_path)
    }

    #[tokio::test]
    async fn capture_single_pane() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        let result = engine.capture(&panes, SnapshotTrigger::Manual).await;
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.pane_count, 1);
        assert!(result.checkpoint_id > 0);
        assert!(result.session_id.starts_with("sess-"));
    }

    #[tokio::test]
    async fn capture_multiple_panes() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![
            make_test_pane(1, 24, 80),
            make_test_pane(2, 24, 80),
            make_test_pane(3, 30, 120),
        ];

        let result = engine
            .capture(&panes, SnapshotTrigger::Startup)
            .await
            .unwrap();
        assert_eq!(result.pane_count, 3);

        // Verify pane states were written
        let conn = Connection::open(db_path.as_str()).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
                [result.checkpoint_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn agent_metadata_persisted_when_detected_from_title() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let mut pane = make_test_pane(1, 24, 80);
        pane.title = Some("claude-code".to_string());

        let result = engine
            .capture(&[pane], SnapshotTrigger::Manual)
            .await
            .unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        let meta_json: Option<String> = conn
            .query_row(
                "SELECT agent_metadata_json FROM mux_pane_state WHERE checkpoint_id = ?1 AND pane_id = ?2",
                rusqlite::params![result.checkpoint_id, 1i64],
                |row| row.get(0),
            )
            .unwrap();

        let meta_json = meta_json.expect("agent_metadata_json should be present");
        let meta: crate::session_pane_state::AgentMetadata =
            serde_json::from_str(&meta_json).unwrap();
        assert_eq!(meta.agent_type, "claude_code");
        assert_eq!(meta.state.as_deref(), Some("active"));
    }

    #[tokio::test]
    async fn dedup_skips_unchanged_periodic() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        // First capture succeeds
        let r1 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
        assert!(r1.is_ok());

        // Second periodic capture with same data should be skipped
        let r2 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
        assert!(matches!(r2, Err(SnapshotError::NoChanges)));
    }

    #[tokio::test]
    async fn dedup_does_not_skip_manual() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        let r1 = engine.capture(&panes, SnapshotTrigger::Manual).await;
        assert!(r1.is_ok());

        // Manual capture should NOT be skipped even if unchanged
        let r2 = engine.capture(&panes, SnapshotTrigger::Manual).await;
        assert!(r2.is_ok());
    }

    #[tokio::test]
    async fn empty_panes_returns_error() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        let result = engine.capture(&[], SnapshotTrigger::Manual).await;
        assert!(matches!(result, Err(SnapshotError::NoPanes)));
    }

    #[tokio::test]
    async fn session_reused_across_captures() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        let panes1 = vec![make_test_pane(1, 24, 80)];
        let panes2 = vec![make_test_pane(1, 30, 120)]; // changed size

        let r1 = engine
            .capture(&panes1, SnapshotTrigger::Startup)
            .await
            .unwrap();
        let r2 = engine
            .capture(&panes2, SnapshotTrigger::Periodic)
            .await
            .unwrap();

        // Same session, different checkpoints
        assert_eq!(r1.session_id, r2.session_id);
        assert_ne!(r1.checkpoint_id, r2.checkpoint_id);
    }

    #[tokio::test]
    async fn cleanup_removes_old_checkpoints() {
        let (_tmp, db_path) = setup_test_db();
        let config = SnapshotConfig {
            retention_count: 2,
            retention_days: 365, // don't prune by age in this test
            ..SnapshotConfig::default()
        };
        let engine = SnapshotEngine::new(db_path.clone(), config);

        // Create 4 snapshots with different pane data
        for i in 0..4u64 {
            let panes = vec![make_test_pane(i, 24 + i as u32, 80)];
            engine
                .capture(&panes, SnapshotTrigger::Manual)
                .await
                .unwrap();
        }

        // Should have 4 checkpoints
        let conn = Connection::open(db_path.as_str()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 4);

        // Cleanup should remove 2 (keep latest 2)
        let deleted = engine.cleanup().await.unwrap();
        assert_eq!(deleted, 2);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn mark_shutdown_sets_flag() {
        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let panes = vec![make_test_pane(1, 24, 80)];

        let r = engine
            .capture(&panes, SnapshotTrigger::Startup)
            .await
            .unwrap();
        engine.mark_shutdown().await.unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        let clean: i64 = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                [&r.session_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(clean, 1);
    }

    #[test]
    fn snapshot_trigger_db_str() {
        assert_eq!(SnapshotTrigger::Periodic.as_db_str(), "periodic");
        assert_eq!(SnapshotTrigger::Manual.as_db_str(), "event");
        assert_eq!(SnapshotTrigger::Shutdown.as_db_str(), "shutdown");
        assert_eq!(SnapshotTrigger::Startup.as_db_str(), "startup");
        assert_eq!(SnapshotTrigger::Event.as_db_str(), "event");
    }

    #[test]
    fn state_hash_deterministic() {
        let panes = vec![make_test_pane(1, 24, 80)];
        let h1 = compute_state_hash(&panes);
        let h2 = compute_state_hash(&panes);
        assert_eq!(h1, h2);
    }

    #[test]
    fn state_hash_changes_on_different_input() {
        let panes1 = vec![make_test_pane(1, 24, 80)];
        let panes2 = vec![make_test_pane(1, 30, 120)];
        let h1 = compute_state_hash(&panes1);
        let h2 = compute_state_hash(&panes2);
        assert_ne!(h1, h2);
    }

    #[test]
    fn generate_session_id_format() {
        let id = generate_session_id();
        assert!(id.starts_with("sess-"));
        assert!(id.len() > 20);
    }
}
