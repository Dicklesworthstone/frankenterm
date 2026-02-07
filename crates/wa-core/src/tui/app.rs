//! TUI application and event loop
//!
//! The main application struct that manages:
//! - Terminal setup/teardown
//! - Event loop (keyboard input, screen refresh)
//! - View state management
//! - Query client coordination

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
};

use super::query::{EventFilters, QueryClient, QueryError};
use super::views::{
    View, ViewState, filtered_event_indices, filtered_pane_indices, render_events_view,
    render_help_view, render_home_view, render_panes_view, render_search_view, render_tabs,
    render_triage_view,
};

/// Application configuration
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Refresh interval for data updates
    pub refresh_interval: Duration,
    /// Show debug information
    pub debug: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(5),
            debug: false,
        }
    }
}

/// Result type for TUI operations
pub type TuiResult<T> = std::result::Result<T, TuiError>;

/// Errors that can occur in the TUI
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("Terminal I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Query error: {0}")]
    Query(#[from] QueryError),

    #[error("Terminal setup failed: {0}")]
    TerminalSetup(String),
}

/// The main TUI application
pub struct App<Q: QueryClient> {
    /// Query client for data access
    query_client: Arc<Q>,
    /// Application configuration
    config: AppConfig,
    /// Current active view
    current_view: View,
    /// State for all views
    view_state: ViewState,
    /// Whether the app should exit
    should_quit: bool,
    /// Last time data was refreshed
    last_refresh: Instant,
    /// Pending command to run (triggered from UI)
    pending_command: Option<String>,
}

impl<Q: QueryClient> App<Q> {
    /// Create a new TUI application
    pub fn new(query_client: Q, config: AppConfig) -> Self {
        Self {
            query_client: Arc::new(query_client),
            config,
            current_view: View::default(),
            view_state: ViewState::default(),
            should_quit: false,
            last_refresh: Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now), // Force initial refresh
            pending_command: None,
        }
    }

    /// Run the event loop
    pub fn run(&mut self) -> TuiResult<()> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err.into());
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                return Err(err.into());
            }
        };

        // Initial data load
        self.refresh_data();

        // Main event loop
        let result = self.event_loop(&mut terminal);

        // Cleanup terminal
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
        let _ = terminal.show_cursor();

        result
    }

    /// Main event loop
    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> TuiResult<()> {
        let tick_rate = Duration::from_millis(100);

        while !self.should_quit {
            // Draw UI
            terminal.draw(|frame| {
                self.render(frame.area(), frame.buffer_mut());
            })?;

            // Execute any pending command outside the draw phase
            if let Some(command) = self.pending_command.take() {
                if let Err(err) = self.run_command(terminal, &command) {
                    self.view_state.set_error(format!("Action failed: {err}"));
                }
            }

            // Handle events with timeout
            if event::poll(tick_rate)? {
                if let Event::Key(key) = event::read()? {
                    self.handle_key_event(key);
                }
            }

            // Auto-refresh data periodically
            if self.last_refresh.elapsed() >= self.config.refresh_interval {
                self.refresh_data();
            }
        }

        Ok(())
    }

    /// Handle keyboard input
    fn handle_key_event(&mut self, key: KeyEvent) {
        // Global keybindings (work in any view)
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.current_view = View::Help;
                return;
            }
            KeyCode::Char('r') => {
                self.refresh_data();
                return;
            }
            KeyCode::Tab => {
                self.current_view = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.current_view.prev()
                } else {
                    self.current_view.next()
                };
                return;
            }
            KeyCode::BackTab => {
                self.current_view = self.current_view.prev();
                return;
            }
            // Number keys for direct view access
            KeyCode::Char('1') => {
                self.current_view = View::Home;
                return;
            }
            KeyCode::Char('2') => {
                self.current_view = View::Panes;
                return;
            }
            KeyCode::Char('3') => {
                self.current_view = View::Events;
                return;
            }
            KeyCode::Char('4') => {
                self.current_view = View::Triage;
                return;
            }
            KeyCode::Char('5') => {
                self.current_view = View::Search;
                return;
            }
            KeyCode::Char('6') => {
                self.current_view = View::Help;
                return;
            }
            _ => {}
        }

        // View-specific keybindings
        match self.current_view {
            View::Panes => self.handle_panes_key(key),
            View::Events => self.handle_events_key(key),
            View::Triage => self.handle_triage_key(key),
            View::Search => self.handle_search_key(key),
            View::Home | View::Help => {}
        }
    }

    /// Handle key events in the panes view
    fn handle_panes_key(&mut self, key: KeyEvent) {
        let filtered_len = filtered_pane_indices(&self.view_state).len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if filtered_len > 0 {
                    self.view_state.selected_index =
                        (self.view_state.selected_index + 1) % filtered_len;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if filtered_len > 0 {
                    self.view_state.selected_index = self
                        .view_state
                        .selected_index
                        .checked_sub(1)
                        .unwrap_or(filtered_len - 1);
                }
            }
            KeyCode::Char('u') => {
                self.view_state.panes_unhandled_only = !self.view_state.panes_unhandled_only;
                self.view_state.selected_index = 0;
            }
            KeyCode::Char('a') => {
                self.view_state.panes_agent_filter =
                    Self::next_agent_filter(self.view_state.panes_agent_filter.as_deref());
                self.view_state.selected_index = 0;
            }
            KeyCode::Char('d') => {
                self.view_state.panes_domain_filter =
                    Self::next_domain_filter(self.view_state.panes_domain_filter.as_deref());
                self.view_state.selected_index = 0;
            }
            KeyCode::Backspace => {
                self.view_state.panes_filter_query.pop();
                self.view_state.selected_index = 0;
            }
            KeyCode::Esc => {
                self.view_state.panes_filter_query.clear();
                self.view_state.selected_index = 0;
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.view_state.panes_filter_query.push(c);
                self.view_state.selected_index = 0;
            }
            _ => {}
        }
    }

    fn next_agent_filter(current: Option<&str>) -> Option<String> {
        match current {
            None => Some("codex".to_string()),
            Some("codex") => Some("claude".to_string()),
            Some("claude") => Some("gemini".to_string()),
            Some("gemini") => Some("unknown".to_string()),
            _ => None,
        }
    }

    fn next_domain_filter(current: Option<&str>) -> Option<String> {
        match current {
            None => Some("local".to_string()),
            Some("local") => Some("ssh".to_string()),
            _ => None,
        }
    }

    /// Handle key events in the events view
    fn handle_events_key(&mut self, key: KeyEvent) {
        let filtered_len = filtered_event_indices(&self.view_state).len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if filtered_len > 0 {
                    self.view_state.events_selected_index =
                        (self.view_state.events_selected_index + 1) % filtered_len;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if filtered_len > 0 {
                    self.view_state.events_selected_index = self
                        .view_state
                        .events_selected_index
                        .checked_sub(1)
                        .unwrap_or(filtered_len - 1);
                }
            }
            KeyCode::Char('u') => {
                self.view_state.events_unhandled_only = !self.view_state.events_unhandled_only;
                self.view_state.events_selected_index = 0;
            }
            KeyCode::Backspace => {
                self.view_state.events_pane_filter.pop();
                self.view_state.events_selected_index = 0;
            }
            KeyCode::Esc => {
                self.view_state.events_pane_filter.clear();
                self.view_state.events_selected_index = 0;
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                self.view_state.events_pane_filter.push(c);
                self.view_state.events_selected_index = 0;
            }
            _ => {}
        }
    }

    /// Handle key events in the triage view
    fn handle_triage_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.view_state.triage_items.is_empty() {
                    self.view_state.triage_selected_index = (self.view_state.triage_selected_index
                        + 1)
                        % self.view_state.triage_items.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.view_state.triage_items.is_empty() {
                    self.view_state.triage_selected_index = self
                        .view_state
                        .triage_selected_index
                        .checked_sub(1)
                        .unwrap_or(self.view_state.triage_items.len() - 1);
                }
            }
            KeyCode::Enter | KeyCode::Char('a') => {
                self.queue_triage_action(0);
            }
            KeyCode::Char('m') => {
                self.mute_selected_event();
            }
            KeyCode::Char('e') => {
                // Toggle expand/collapse for workflow progress
                if !self.view_state.workflows.is_empty() {
                    if self.view_state.triage_expanded.is_some() {
                        self.view_state.triage_expanded = None;
                    } else {
                        self.view_state.triage_expanded = Some(0);
                    }
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap_or(0);
                if idx > 0 {
                    self.queue_triage_action(idx as usize - 1);
                }
            }
            _ => {}
        }
    }

    /// Handle key events in the search view
    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j')
                if !self.view_state.search_results.is_empty() =>
            {
                self.view_state.search_selected_index =
                    (self.view_state.search_selected_index + 1)
                        % self.view_state.search_results.len();
            }
            KeyCode::Up | KeyCode::Char('k') if !self.view_state.search_results.is_empty() => {
                self.view_state.search_selected_index = self
                    .view_state
                    .search_selected_index
                    .checked_sub(1)
                    .unwrap_or(self.view_state.search_results.len() - 1);
            }
            KeyCode::Char(c) => {
                self.view_state.search_query.push(c);
            }
            KeyCode::Backspace => {
                self.view_state.search_query.pop();
            }
            KeyCode::Enter => {
                self.execute_search();
            }
            KeyCode::Esc => {
                self.view_state.search_query.clear();
                self.view_state.search_results.clear();
                self.view_state.search_last_query.clear();
                self.view_state.search_selected_index = 0;
            }
            _ => {}
        }
    }

    /// Execute FTS search using query client
    fn execute_search(&mut self) {
        let query = self.view_state.search_query.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.view_state.search_last_query = query.clone();
        self.view_state.search_selected_index = 0;
        match self.query_client.search(&query, 50) {
            Ok(results) => {
                self.view_state.search_results = results;
                self.view_state.clear_error();
            }
            Err(e) => {
                self.view_state.search_results.clear();
                self.view_state.set_error(format!("Search failed: {e}"));
            }
        }
    }

    /// Refresh data from the query client
    fn refresh_data(&mut self) {
        self.view_state.clear_error();

        // Refresh health status
        match self.query_client.health() {
            Ok(health) => {
                self.view_state.health = Some(health);
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Health check failed: {e}"));
            }
        }

        // Refresh panes
        match self.query_client.list_panes() {
            Ok(panes) => {
                self.view_state.panes = panes;
                // Reset selection if out of bounds
                let filtered_count = filtered_pane_indices(&self.view_state).len();
                if self.view_state.selected_index >= filtered_count {
                    self.view_state.selected_index = 0;
                }
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to list panes: {e}"));
            }
        }

        // Refresh events
        let filters = EventFilters {
            limit: 50,
            ..Default::default()
        };
        match self.query_client.list_events(&filters) {
            Ok(events) => {
                self.view_state.events = events;
            }
            Err(QueryError::DatabaseNotInitialized(_)) => {
                // This is expected if watcher hasn't run yet
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to list events: {e}"));
            }
        }

        // Refresh active workflows
        match self.query_client.list_active_workflows() {
            Ok(workflows) => {
                self.view_state.workflows = workflows;
                // Reset expanded if workflow list changed
                if let Some(idx) = self.view_state.triage_expanded {
                    if idx >= self.view_state.workflows.len() {
                        self.view_state.triage_expanded = None;
                    }
                }
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to list workflows: {e}"));
            }
        }

        // Refresh triage items
        match self.query_client.list_triage_items() {
            Ok(items) => {
                self.view_state.triage_items = items;
                if self.view_state.triage_selected_index >= self.view_state.triage_items.len() {
                    self.view_state.triage_selected_index = 0;
                }
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to build triage: {e}"));
            }
        }

        self.last_refresh = Instant::now();
    }

    /// Render the current UI state
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Tab bar
                Constraint::Min(10),   // Main content
            ])
            .split(area);

        // Render tab navigation
        render_tabs(self.current_view, chunks[0], buf);

        // Render current view
        match self.current_view {
            View::Home => render_home_view(&self.view_state, chunks[1], buf),
            View::Panes => render_panes_view(&self.view_state, chunks[1], buf),
            View::Events => render_events_view(&self.view_state, chunks[1], buf),
            View::Triage => render_triage_view(&self.view_state, chunks[1], buf),
            View::Search => render_search_view(&self.view_state, chunks[1], buf),
            View::Help => render_help_view(chunks[1], buf),
        }
    }

    fn queue_triage_action(&mut self, index: usize) {
        let Some(item) = self
            .view_state
            .triage_items
            .get(self.view_state.triage_selected_index)
        else {
            self.view_state.set_error("No triage items available");
            return;
        };

        let Some(action) = item.actions.get(index) else {
            self.view_state
                .set_error(format!("No action #{} for this item", index + 1));
            return;
        };

        self.pending_command = Some(action.command.clone());
    }

    fn mute_selected_event(&mut self) {
        let Some(item) = self
            .view_state
            .triage_items
            .get(self.view_state.triage_selected_index)
        else {
            self.view_state.set_error("No triage items available");
            return;
        };

        let Some(event_id) = item.event_id else {
            self.view_state
                .set_error("Selected triage item is not an event");
            return;
        };

        if let Err(e) = self.query_client.mark_event_muted(event_id) {
            self.view_state
                .set_error(format!("Failed to mute event: {e}"));
        } else {
            self.refresh_data();
        }
    }

    fn run_command(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        command: &str,
    ) -> TuiResult<()> {
        let mut parts = command.split_whitespace();
        let Some(program) = parts.next() else {
            return Ok(());
        };

        // Leave alternate screen to show command output
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        println!("Running: {command}\n");

        let status = std::process::Command::new(program).args(parts).status();
        match status {
            Ok(status) => println!("Exit status: {status}"),
            Err(err) => println!("Command failed: {err}"),
        }
        println!("\nPress Enter to return to the TUI...");
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);

        // Restore TUI
        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
        enable_raw_mode()?;
        self.refresh_data();
        Ok(())
    }
}

/// Run the TUI application
///
/// This is the main entry point for starting the TUI.
///
/// # Example
///
/// ```ignore
/// use wa_core::tui::{run_tui, ProductionQueryClient, AppConfig};
///
/// let client = ProductionQueryClient::new(layout);
/// run_tui(client, AppConfig::default())?;
/// ```
pub fn run_tui<Q: QueryClient>(query_client: Q, config: AppConfig) -> TuiResult<()> {
    let mut app = App::new(query_client, config);
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::query::{EventView, HealthStatus, PaneView, SearchResultView, WorkflowProgressView};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    struct TestQueryClient;

    impl QueryClient for TestQueryClient {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok(vec![PaneView {
                pane_id: 0,
                title: "test".to_string(),
                domain: "local".to_string(),
                cwd: None,
                is_excluded: false,
                agent_type: None,
                pane_state: "PromptActive".to_string(),
                last_activity_ts: Some(1_700_000_000_000),
                unhandled_event_count: 0,
            }])
        }

        fn list_events(&self, _: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            Ok(Vec::new())
        }

        fn list_triage_items(&self) -> Result<Vec<crate::tui::query::TriageItemView>, QueryError> {
            Ok(Vec::new())
        }

        fn search(&self, _: &str, _: usize) -> Result<Vec<SearchResultView>, QueryError> {
            Ok(Vec::new())
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: true,
                db_accessible: true,
                wezterm_accessible: true,
                wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
                pane_count: 1,
                event_count: 0,
                last_capture_ts: None,
            })
        }

        fn is_watcher_running(&self) -> bool {
            true
        }

        fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
            Ok(())
        }

        fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn app_initializes_with_default_view() {
        let app = App::new(TestQueryClient, AppConfig::default());
        assert_eq!(app.current_view, View::Home);
        assert!(!app.should_quit);
    }

    #[test]
    fn app_refreshes_data_on_creation() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.refresh_data();
        assert!(app.view_state.health.is_some());
        assert_eq!(app.view_state.panes.len(), 1);
    }

    struct MultiPaneQueryClient;

    fn pane(id: u64, title: &str, agent: Option<&str>, unhandled: u32) -> PaneView {
        PaneView {
            pane_id: id,
            title: title.to_string(),
            domain: "local".to_string(),
            cwd: Some(format!("/tmp/{title}")),
            is_excluded: false,
            agent_type: agent.map(str::to_string),
            pane_state: "PromptActive".to_string(),
            last_activity_ts: Some(1_700_000_000_000),
            unhandled_event_count: unhandled,
        }
    }

    impl QueryClient for MultiPaneQueryClient {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok(vec![
                pane(1, "codex-main", Some("codex"), 1),
                pane(2, "claude-docs", Some("claude"), 0),
                pane(3, "shell", None, 0),
            ])
        }

        fn list_events(&self, _: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            Ok(Vec::new())
        }

        fn list_triage_items(&self) -> Result<Vec<crate::tui::query::TriageItemView>, QueryError> {
            Ok(Vec::new())
        }

        fn search(&self, _: &str, _: usize) -> Result<Vec<SearchResultView>, QueryError> {
            Ok(Vec::new())
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: true,
                db_accessible: true,
                wezterm_accessible: true,
                wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
                pane_count: 3,
                event_count: 0,
                last_capture_ts: None,
            })
        }

        fn is_watcher_running(&self) -> bool {
            true
        }

        fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
            Ok(())
        }

        fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn panes_filters_and_navigation_update_state() {
        let mut app = App::new(MultiPaneQueryClient, AppConfig::default());
        app.refresh_data();

        app.handle_panes_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        assert!(app.view_state.panes_unhandled_only);

        app.handle_panes_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.view_state.panes_filter_query, "c");

        app.handle_panes_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(app.view_state.panes_filter_query.is_empty());

        app.handle_panes_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.view_state.panes_agent_filter.as_deref(), Some("codex"));
        app.handle_panes_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(app.view_state.panes_agent_filter.as_deref(), Some("claude"));

        app.handle_panes_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.selected_index, 0);
    }

    // -----------------------------------------------------------------------
    // Events view keybinding tests (wa-nu4.3.7.3)
    // -----------------------------------------------------------------------

    struct EventQueryClient;

    impl QueryClient for EventQueryClient {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok(vec![pane(1, "test", None, 0)])
        }

        fn list_events(&self, _: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            Ok(vec![
                EventView {
                    id: 1,
                    rule_id: "codex.usage_reached".to_string(),
                    pane_id: 10,
                    severity: "warning".to_string(),
                    message: "usage limit hit".to_string(),
                    timestamp: 1_700_000_000_000,
                    handled: false,
                },
                EventView {
                    id: 2,
                    rule_id: "claude.error".to_string(),
                    pane_id: 20,
                    severity: "critical".to_string(),
                    message: "agent error".to_string(),
                    timestamp: 1_700_000_001_000,
                    handled: true,
                },
                EventView {
                    id: 3,
                    rule_id: "core.idle".to_string(),
                    pane_id: 10,
                    severity: "info".to_string(),
                    message: "pane idle".to_string(),
                    timestamp: 1_700_000_002_000,
                    handled: false,
                },
            ])
        }

        fn list_triage_items(&self) -> Result<Vec<crate::tui::query::TriageItemView>, QueryError> {
            Ok(Vec::new())
        }

        fn search(&self, _: &str, _: usize) -> Result<Vec<SearchResultView>, QueryError> {
            Ok(Vec::new())
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: true,
                db_accessible: true,
                wezterm_accessible: true,
                wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
                pane_count: 1,
                event_count: 3,
                last_capture_ts: None,
            })
        }

        fn is_watcher_running(&self) -> bool {
            true
        }

        fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
            Ok(())
        }

        fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn events_navigation_wraps() {
        let mut app = App::new(EventQueryClient, AppConfig::default());
        app.refresh_data();
        assert_eq!(app.view_state.events.len(), 3);
        assert_eq!(app.view_state.events_selected_index, 0);

        // Navigate down
        app.handle_events_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.view_state.events_selected_index, 1);

        app.handle_events_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.view_state.events_selected_index, 2);

        // Wrap around
        app.handle_events_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.events_selected_index, 0);

        // Navigate up wraps
        app.handle_events_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.view_state.events_selected_index, 2);
    }

    #[test]
    fn events_unhandled_toggle_resets_selection() {
        let mut app = App::new(EventQueryClient, AppConfig::default());
        app.refresh_data();
        app.view_state.events_selected_index = 2;

        app.handle_events_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        assert!(app.view_state.events_unhandled_only);
        assert_eq!(app.view_state.events_selected_index, 0);

        // Toggle off
        app.handle_events_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        assert!(!app.view_state.events_unhandled_only);
    }

    #[test]
    fn events_pane_filter_accepts_digits() {
        let mut app = App::new(EventQueryClient, AppConfig::default());
        app.refresh_data();

        app.handle_events_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.view_state.events_pane_filter, "2");
        app.handle_events_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert_eq!(app.view_state.events_pane_filter, "20");

        // Backspace
        app.handle_events_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.view_state.events_pane_filter, "2");

        // Esc clears
        app.handle_events_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.view_state.events_pane_filter.is_empty());
    }

    // -----------------------------------------------------------------------
    // Search view tests (wa-nu4.3.7.4)
    // -----------------------------------------------------------------------

    struct SearchQueryClient;

    impl QueryClient for SearchQueryClient {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok(Vec::new())
        }

        fn list_events(&self, _: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            Ok(Vec::new())
        }

        fn list_triage_items(&self) -> Result<Vec<crate::tui::query::TriageItemView>, QueryError> {
            Ok(Vec::new())
        }

        fn search(&self, query: &str, _limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
            if query == "error" {
                return Err(QueryError::DatabaseNotInitialized("test".to_string()));
            }
            if query.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![
                SearchResultView {
                    pane_id: 10,
                    timestamp: 1_700_000_000_000,
                    snippet: format!(">>matched<< text for {query}"),
                    rank: 0.95,
                },
                SearchResultView {
                    pane_id: 20,
                    timestamp: 1_700_000_001_000,
                    snippet: format!("another >>result<< with {query}"),
                    rank: 0.75,
                },
            ])
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: true,
                db_accessible: true,
                wezterm_accessible: true,
                wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
                pane_count: 0,
                event_count: 0,
                last_capture_ts: None,
            })
        }

        fn is_watcher_running(&self) -> bool {
            true
        }

        fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
            Ok(())
        }

        fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn search_executes_on_enter() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.refresh_data();

        // Type a query
        app.handle_search_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        app.handle_search_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        app.handle_search_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        app.handle_search_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.view_state.search_query, "test");

        // Execute search
        app.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.view_state.search_last_query, "test");
        assert_eq!(app.view_state.search_results.len(), 2);
        assert_eq!(app.view_state.search_selected_index, 0);
    }

    #[test]
    fn search_navigation_wraps() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.view_state.search_query = "test".to_string();
        app.execute_search();
        assert_eq!(app.view_state.search_results.len(), 2);

        // Navigate down
        app.handle_search_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.search_selected_index, 1);

        // Wrap around
        app.handle_search_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.search_selected_index, 0);

        // Navigate up wraps
        app.handle_search_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.view_state.search_selected_index, 1);
    }

    #[test]
    fn search_esc_clears_all() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.view_state.search_query = "test".to_string();
        app.execute_search();
        assert!(!app.view_state.search_results.is_empty());

        app.handle_search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.view_state.search_query.is_empty());
        assert!(app.view_state.search_results.is_empty());
        assert!(app.view_state.search_last_query.is_empty());
    }

    #[test]
    fn search_error_sets_error_message() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.view_state.search_query = "error".to_string();
        app.execute_search();
        assert!(app.view_state.search_results.is_empty());
        assert!(app.view_state.error_message.is_some());
    }

    #[test]
    fn search_empty_query_does_nothing() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.view_state.search_query = "  ".to_string();
        app.execute_search();
        assert!(app.view_state.search_results.is_empty());
        assert!(app.view_state.search_last_query.is_empty());
    }

    #[test]
    fn search_backspace_removes_char() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.view_state.search_query = "test".to_string();
        app.handle_search_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.view_state.search_query, "tes");
    }

    // -----------------------------------------------------------------------
    // Triage expand/collapse tests (wa-nu4.3.7.5)
    // -----------------------------------------------------------------------

    #[test]
    fn triage_expand_toggles_with_workflows() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.refresh_data();
        // Add workflows to state
        app.view_state.workflows = vec![WorkflowProgressView {
            id: "wf-1".to_string(),
            workflow_name: "notify_user".to_string(),
            pane_id: 10,
            current_step: 1,
            total_steps: 3,
            status: "running".to_string(),
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_001_000,
        }];

        assert!(app.view_state.triage_expanded.is_none());

        // Press 'e' to expand
        app.handle_triage_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(app.view_state.triage_expanded, Some(0));

        // Press 'e' again to collapse
        app.handle_triage_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.view_state.triage_expanded.is_none());
    }

    #[test]
    fn triage_expand_noop_without_workflows() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.refresh_data();
        assert!(app.view_state.workflows.is_empty());

        app.handle_triage_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert!(app.view_state.triage_expanded.is_none());
    }
}
