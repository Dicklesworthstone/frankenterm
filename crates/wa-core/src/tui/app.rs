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
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    Terminal,
};

use super::query::{QueryClient, QueryError, EventFilters};
use super::views::{View, ViewState, render_tabs, render_home_view, render_panes_view, render_events_view, render_search_view, render_help_view};

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
            last_refresh: Instant::now() - Duration::from_secs(60), // Force initial refresh
        }
    }

    /// Run the event loop
    pub fn run(&mut self) -> TuiResult<()> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Initial data load
        self.refresh_data();

        // Main event loop
        let result = self.event_loop(&mut terminal);

        // Cleanup terminal
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }

    /// Main event loop
    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> TuiResult<()> {
        let tick_rate = Duration::from_millis(100);

        while !self.should_quit {
            // Draw UI
            terminal.draw(|frame| {
                self.render(frame.area(), frame.buffer_mut());
            })?;

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
                self.current_view = View::Search;
                return;
            }
            KeyCode::Char('5') => {
                self.current_view = View::Help;
                return;
            }
            _ => {}
        }

        // View-specific keybindings
        match self.current_view {
            View::Panes => self.handle_panes_key(key),
            View::Events => self.handle_events_key(key),
            View::Search => self.handle_search_key(key),
            View::Home | View::Help => {}
        }
    }

    /// Handle key events in the panes view
    fn handle_panes_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.view_state.panes.is_empty() {
                    self.view_state.selected_index =
                        (self.view_state.selected_index + 1) % self.view_state.panes.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.view_state.panes.is_empty() {
                    self.view_state.selected_index = self
                        .view_state
                        .selected_index
                        .checked_sub(1)
                        .unwrap_or(self.view_state.panes.len() - 1);
                }
            }
            _ => {}
        }
    }

    /// Handle key events in the events view
    fn handle_events_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.view_state.events.is_empty() {
                    self.view_state.selected_index =
                        (self.view_state.selected_index + 1) % self.view_state.events.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.view_state.events.is_empty() {
                    self.view_state.selected_index = self
                        .view_state
                        .selected_index
                        .checked_sub(1)
                        .unwrap_or(self.view_state.events.len() - 1);
                }
            }
            _ => {}
        }
    }

    /// Handle key events in the search view
    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) => {
                self.view_state.search_query.push(c);
            }
            KeyCode::Backspace => {
                self.view_state.search_query.pop();
            }
            KeyCode::Enter => {
                // TODO: Execute search
                self.view_state.clear_error();
            }
            KeyCode::Esc => {
                self.view_state.search_query.clear();
            }
            _ => {}
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
                self.view_state.set_error(format!("Health check failed: {e}"));
            }
        }

        // Refresh panes
        match self.query_client.list_panes() {
            Ok(panes) => {
                self.view_state.panes = panes;
                // Reset selection if out of bounds
                if self.view_state.selected_index >= self.view_state.panes.len() {
                    self.view_state.selected_index = 0;
                }
            }
            Err(e) => {
                self.view_state.set_error(format!("Failed to list panes: {e}"));
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
                self.view_state.set_error(format!("Failed to list events: {e}"));
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
            View::Search => render_search_view(&self.view_state, chunks[1], buf),
            View::Help => render_help_view(chunks[1], buf),
        }
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
    use crate::tui::query::{PaneView, EventView, HealthStatus, SearchResultView};

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
            }])
        }

        fn list_events(&self, _: &EventFilters) -> Result<Vec<EventView>, QueryError> {
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
                pane_count: 1,
                event_count: 0,
                last_capture_ts: None,
            })
        }

        fn is_watcher_running(&self) -> bool {
            true
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
}
