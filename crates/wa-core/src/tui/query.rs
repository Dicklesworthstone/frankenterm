//! Query client abstraction for TUI data access
//!
//! The `QueryClient` trait provides a clean abstraction over the wa-core
//! query layer, enabling:
//!
//! - Testability: Mock implementations for unit tests
//! - Consistency: Same data access patterns as robot mode
//! - Decoupling: UI doesn't know about SQLite or storage internals

use std::path::PathBuf;

use crate::config::WorkspaceLayout;
use crate::storage::StorageHandle;
use crate::wezterm::{PaneInfo, WeztermClient};

/// Errors that can occur during query operations
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("Watcher is not running")]
    WatcherNotRunning,

    #[error("Database not initialized: {0}")]
    DatabaseNotInitialized(String),

    #[error("WezTerm error: {0}")]
    WeztermError(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Query failed: {0}")]
    QueryFailed(String),
}

/// Pane information for TUI display
#[derive(Debug, Clone)]
pub struct PaneView {
    pub pane_id: u64,
    pub title: String,
    pub domain: String,
    pub cwd: Option<String>,
    pub is_excluded: bool,
    pub agent_type: Option<String>,
}

impl From<&PaneInfo> for PaneView {
    fn from(info: &PaneInfo) -> Self {
        Self {
            pane_id: info.pane_id,
            title: info.title.clone(),
            domain: info.domain.clone(),
            cwd: info.cwd.clone(),
            is_excluded: false,
            agent_type: None,
        }
    }
}

/// Event information for TUI display
#[derive(Debug, Clone)]
pub struct EventView {
    pub id: i64,
    pub rule_id: String,
    pub pane_id: u64,
    pub severity: String,
    pub message: String,
    pub timestamp: i64,
    pub handled: bool,
}

/// Search result for TUI display
#[derive(Debug, Clone)]
pub struct SearchResultView {
    pub pane_id: u64,
    pub timestamp: i64,
    pub snippet: String,
    pub rank: f64,
}

/// Event filters for querying
#[derive(Debug, Default, Clone)]
pub struct EventFilters {
    pub pane_id: Option<u64>,
    pub rule_id: Option<String>,
    pub event_type: Option<String>,
    pub unhandled_only: bool,
    pub limit: usize,
}

/// Health status information
#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub watcher_running: bool,
    pub db_accessible: bool,
    pub wezterm_accessible: bool,
    pub pane_count: usize,
    pub event_count: usize,
    pub last_capture_ts: Option<i64>,
}

/// Abstraction over wa-core query layer for TUI data access
///
/// This trait allows the TUI to be tested with mock implementations
/// while using the same query patterns as robot mode in production.
pub trait QueryClient: Send + Sync {
    /// List all panes from WezTerm
    fn list_panes(&self) -> Result<Vec<PaneView>, QueryError>;

    /// List recent events with optional filters
    fn list_events(&self, filters: &EventFilters) -> Result<Vec<EventView>, QueryError>;

    /// Full-text search across captured output
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResultView>, QueryError>;

    /// Check system health status
    fn health(&self) -> Result<HealthStatus, QueryError>;

    /// Check if the watcher is running
    fn is_watcher_running(&self) -> bool;
}

/// Production implementation of QueryClient
///
/// Uses the actual wa-core storage and wezterm client to query data.
pub struct ProductionQueryClient {
    workspace_layout: WorkspaceLayout,
    wezterm: WeztermClient,
    storage: Option<StorageHandle>,
}

impl ProductionQueryClient {
    /// Create a new production query client
    pub fn new(workspace_layout: WorkspaceLayout) -> Self {
        Self {
            workspace_layout,
            wezterm: WeztermClient::new(),
            storage: None,
        }
    }

    /// Create with an existing storage handle
    pub fn with_storage(workspace_layout: WorkspaceLayout, storage: StorageHandle) -> Self {
        Self {
            workspace_layout,
            wezterm: WeztermClient::new(),
            storage: Some(storage),
        }
    }

    /// Get the database path
    fn db_path(&self) -> PathBuf {
        self.workspace_layout.database_file()
    }

    /// Check if the database exists
    fn db_exists(&self) -> bool {
        self.db_path().exists()
    }
}

impl QueryClient for ProductionQueryClient {
    fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
        // Use tokio runtime to run async code
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|e| QueryError::QueryFailed(format!("No tokio runtime: {e}")))?;

        let wezterm = &self.wezterm;
        rt.block_on(async {
            wezterm
                .list_panes()
                .await
                .map(|panes| panes.iter().map(PaneView::from).collect())
                .map_err(|e| QueryError::WeztermError(e.to_string()))
        })
    }

    fn list_events(&self, filters: &EventFilters) -> Result<Vec<EventView>, QueryError> {
        if !self.db_exists() {
            return Err(QueryError::DatabaseNotInitialized(
                "Database file not found. Run `wa watch` first.".to_string(),
            ));
        }

        // For now, return empty list - full implementation requires storage access
        // This is scaffolding; the actual query will be wired up later
        let _ = filters;
        Ok(Vec::new())
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
        if !self.db_exists() {
            return Err(QueryError::DatabaseNotInitialized(
                "Database file not found. Run `wa watch` first.".to_string(),
            ));
        }

        // For now, return empty list - full implementation requires storage access
        let _ = (query, limit);
        Ok(Vec::new())
    }

    fn health(&self) -> Result<HealthStatus, QueryError> {
        let wezterm_accessible = self
            .list_panes()
            .map(|p| !p.is_empty())
            .unwrap_or(false);

        let db_accessible = self.db_exists();
        let watcher_running = self.is_watcher_running();
        let pane_count = self.list_panes().map(|p| p.len()).unwrap_or(0);

        Ok(HealthStatus {
            watcher_running,
            db_accessible,
            wezterm_accessible,
            pane_count,
            event_count: 0,
            last_capture_ts: None,
        })
    }

    fn is_watcher_running(&self) -> bool {
        let lock_path = self.workspace_layout.watcher_lock();
        lock_path.exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock query client for testing
    struct MockQueryClient {
        panes: Vec<PaneView>,
        events: Vec<EventView>,
        watcher_running: bool,
    }

    impl MockQueryClient {
        fn new() -> Self {
            Self {
                panes: vec![
                    PaneView {
                        pane_id: 0,
                        title: "test-pane".to_string(),
                        domain: "local".to_string(),
                        cwd: Some("/home/test".to_string()),
                        is_excluded: false,
                        agent_type: Some("claude-code".to_string()),
                    },
                ],
                events: Vec::new(),
                watcher_running: true,
            }
        }
    }

    impl QueryClient for MockQueryClient {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok(self.panes.clone())
        }

        fn list_events(&self, _filters: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            Ok(self.events.clone())
        }

        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
            Ok(Vec::new())
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: self.watcher_running,
                db_accessible: true,
                wezterm_accessible: true,
                pane_count: self.panes.len(),
                event_count: self.events.len(),
                last_capture_ts: None,
            })
        }

        fn is_watcher_running(&self) -> bool {
            self.watcher_running
        }
    }

    #[test]
    fn mock_client_lists_panes() {
        let client = MockQueryClient::new();
        let panes = client.list_panes().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, 0);
        assert_eq!(panes[0].title, "test-pane");
    }

    #[test]
    fn mock_client_health_status() {
        let client = MockQueryClient::new();
        let health = client.health().unwrap();
        assert!(health.watcher_running);
        assert!(health.db_accessible);
        assert_eq!(health.pane_count, 1);
    }
}
