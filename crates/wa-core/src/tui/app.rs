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
    View, ViewState, filtered_event_indices, filtered_history_indices, filtered_pane_indices,
    render_events_view, render_help_view, render_history_view, render_home_view, render_panes_view,
    render_search_view, render_tabs, render_timeline_placeholder, render_triage_view,
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
    #[allow(dead_code)] // Reserved for terminal setup error paths
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
            KeyCode::Char('r') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
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
                self.current_view = View::History;
                return;
            }
            KeyCode::Char('6') => {
                self.current_view = View::Search;
                return;
            }
            KeyCode::Char('7') => {
                self.current_view = View::Help;
                return;
            }
            KeyCode::Char('8') => {
                self.current_view = View::Timeline;
                return;
            }
            _ => {}
        }

        // View-specific keybindings
        match self.current_view {
            View::Panes => self.handle_panes_key(key),
            View::Events => self.handle_events_key(key),
            View::History => self.handle_history_key(key),
            View::Triage => self.handle_triage_key(key),
            View::Search => self.handle_search_key(key),
            View::Home | View::Help | View::Timeline => {}
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
            KeyCode::Char('b') => {
                self.view_state.panes_bookmarked_only = !self.view_state.panes_bookmarked_only;
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
            KeyCode::Char('p') => {
                if let Some(profile_state) = &self.view_state.ruleset_profile_state
                    && !profile_state.profiles.is_empty()
                {
                    self.view_state.selected_ruleset_profile_index =
                        (self.view_state.selected_ruleset_profile_index + 1)
                            % profile_state.profiles.len();
                }
            }
            KeyCode::Enter => {
                if let Some(profile_state) = &self.view_state.ruleset_profile_state
                    && let Some(selected) = profile_state.profiles.get(
                        self.view_state
                            .selected_ruleset_profile_index
                            .min(profile_state.profiles.len().saturating_sub(1)),
                    )
                    && selected.name != profile_state.active_profile
                {
                    self.pending_command =
                        Some(format!("wa rules profile apply {}", selected.name));
                }
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
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.view_state.saved_searches.is_empty() {
                    self.view_state.saved_search_selected_index =
                        (self.view_state.saved_search_selected_index + 1)
                            % self.view_state.saved_searches.len();
                }
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.view_state.saved_searches.is_empty() {
                    self.view_state.saved_search_selected_index = self
                        .view_state
                        .saved_search_selected_index
                        .checked_sub(1)
                        .unwrap_or(self.view_state.saved_searches.len() - 1);
                }
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(saved) = self
                    .view_state
                    .saved_searches
                    .get(self.view_state.saved_search_selected_index)
                {
                    self.pending_command = Some(format!("wa search saved run {}", saved.name));
                } else {
                    self.view_state.set_error("No saved search selected");
                }
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(saved) = self
                    .view_state
                    .saved_searches
                    .get(self.view_state.saved_search_selected_index)
                {
                    if saved.enabled {
                        self.pending_command =
                            Some(format!("wa search saved disable {}", saved.name));
                    } else if saved.schedule_interval_ms.is_some() {
                        self.pending_command =
                            Some(format!("wa search saved enable {}", saved.name));
                    } else {
                        self.view_state.set_error(
                            "Saved search has no schedule; set one via `wa search saved schedule`",
                        );
                    }
                } else {
                    self.view_state.set_error("No saved search selected");
                }
            }
            KeyCode::Down | KeyCode::Char('j') if !self.view_state.search_results.is_empty() => {
                self.view_state.search_selected_index = (self.view_state.search_selected_index + 1)
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
                self.refresh_search_suggestions();
            }
            KeyCode::Backspace => {
                self.view_state.search_query.pop();
                self.refresh_search_suggestions();
            }
            KeyCode::Enter => {
                self.view_state.search_suggestions.clear();
                self.execute_search();
            }
            KeyCode::Esc => {
                self.view_state.search_query.clear();
                self.view_state.search_results.clear();
                self.view_state.search_last_query.clear();
                self.view_state.search_selected_index = 0;
                self.view_state.search_suggestions.clear();
            }
            _ => {}
        }
    }

    /// Handle key events in the history view
    fn handle_history_key(&mut self, key: KeyEvent) {
        let filtered_len = filtered_history_indices(&self.view_state).len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if filtered_len > 0 {
                    self.view_state.history_selected_index =
                        (self.view_state.history_selected_index + 1) % filtered_len;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if filtered_len > 0 {
                    self.view_state.history_selected_index = self
                        .view_state
                        .history_selected_index
                        .checked_sub(1)
                        .unwrap_or(filtered_len - 1);
                }
            }
            KeyCode::Char('u') => {
                self.view_state.history_undoable_only = !self.view_state.history_undoable_only;
                self.view_state.history_selected_index = 0;
            }
            KeyCode::Backspace => {
                self.view_state.history_filter_query.pop();
                self.view_state.history_selected_index = 0;
            }
            KeyCode::Esc => {
                self.view_state.history_filter_query.clear();
                self.view_state.history_undoable_only = false;
                self.view_state.history_selected_index = 0;
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.view_state.history_filter_query.push(c);
                self.view_state.history_selected_index = 0;
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
        self.view_state.search_last_query.clone_from(&query);
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

    fn refresh_search_suggestions(&mut self) {
        self.view_state.search_suggestions =
            crate::storage::search_query_suggestions(&self.view_state.search_query, 5);
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

        // Refresh action history
        match self.query_client.list_action_history(200) {
            Ok(entries) => {
                self.view_state.history_entries = entries;
                let filtered_count = filtered_history_indices(&self.view_state).len();
                if self.view_state.history_selected_index >= filtered_count {
                    self.view_state.history_selected_index = 0;
                }
            }
            Err(QueryError::DatabaseNotInitialized(_)) => {
                // Expected when watcher/storage has not been initialized yet
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to list action history: {e}"));
            }
        }

        // Refresh saved searches
        match self.query_client.list_saved_searches() {
            Ok(saved_searches) => {
                self.view_state.saved_searches = saved_searches;
                if self.view_state.saved_search_selected_index
                    >= self.view_state.saved_searches.len()
                {
                    self.view_state.saved_search_selected_index = 0;
                }
            }
            Err(QueryError::DatabaseNotInitialized(_)) => {
                self.view_state.saved_searches.clear();
                self.view_state.saved_search_selected_index = 0;
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to list saved searches: {e}"));
            }
        }

        // Refresh pane bookmarks
        match self.query_client.list_pane_bookmarks() {
            Ok(bookmarks) => {
                self.view_state.pane_bookmarks = bookmarks;
            }
            Err(QueryError::DatabaseNotInitialized(_)) => {
                self.view_state.pane_bookmarks.clear();
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to list pane bookmarks: {e}"));
            }
        }

        // Refresh ruleset profile state
        match self.query_client.ruleset_profile_state() {
            Ok(profile_state) => {
                let active_index = profile_state
                    .profiles
                    .iter()
                    .position(|p| p.name == profile_state.active_profile)
                    .unwrap_or(0);
                if self.view_state.selected_ruleset_profile_index >= profile_state.profiles.len() {
                    self.view_state.selected_ruleset_profile_index = active_index;
                }
                if self.view_state.selected_ruleset_profile_index == 0
                    && !profile_state.profiles.is_empty()
                    && self.view_state.ruleset_profile_state.is_none()
                {
                    self.view_state.selected_ruleset_profile_index = active_index;
                }
                self.view_state.ruleset_profile_state = Some(profile_state);
            }
            Err(e) => {
                self.view_state
                    .set_error(format!("Failed to resolve ruleset profiles: {e}"));
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
            View::History => render_history_view(&self.view_state, chunks[1], buf),
            View::Triage => render_triage_view(&self.view_state, chunks[1], buf),
            View::Search => render_search_view(&self.view_state, chunks[1], buf),
            View::Help => render_help_view(chunks[1], buf),
            View::Timeline => render_timeline_placeholder(chunks[1], buf),
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
        // Gate-aware writes: asserts not Active in debug builds (FTUI-03.2.a).
        crate::gated_println!("Running: {command}\n");

        let status = std::process::Command::new(program).args(parts).status();
        match status {
            Ok(status) => crate::gated_println!("Exit status: {status}"),
            Err(err) => crate::gated_println!("Command failed: {err}"),
        }
        crate::gated_println!("\nPress Enter to return to the TUI...");
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
    use crate::tui::query::{
        EventView, HealthStatus, HistoryEntryView, PaneView, SearchResultView, WorkflowProgressView,
    };
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

        fn list_saved_searches(
            &self,
        ) -> Result<Vec<crate::tui::query::SavedSearchView>, QueryError> {
            Ok(vec![
                crate::tui::query::SavedSearchView {
                    id: "ss-1".to_string(),
                    name: "errors".to_string(),
                    query: "error".to_string(),
                    pane_id: None,
                    limit: 50,
                    since_mode: "last_run".to_string(),
                    since_ms: None,
                    schedule_interval_ms: Some(60_000),
                    enabled: false,
                    last_run_at: None,
                    last_result_count: None,
                    last_error: None,
                    created_at: 1_700_000_000_000,
                    updated_at: 1_700_000_000_000,
                },
                crate::tui::query::SavedSearchView {
                    id: "ss-2".to_string(),
                    name: "warnings".to_string(),
                    query: "warning".to_string(),
                    pane_id: Some(2),
                    limit: 25,
                    since_mode: "last_run".to_string(),
                    since_ms: None,
                    schedule_interval_ms: Some(120_000),
                    enabled: true,
                    last_run_at: Some(1_700_000_001_000),
                    last_result_count: Some(7),
                    last_error: None,
                    created_at: 1_700_000_000_000,
                    updated_at: 1_700_000_001_000,
                },
                crate::tui::query::SavedSearchView {
                    id: "ss-3".to_string(),
                    name: "manual".to_string(),
                    query: "panic".to_string(),
                    pane_id: None,
                    limit: 10,
                    since_mode: "fixed".to_string(),
                    since_ms: Some(1_700_000_000_000),
                    schedule_interval_ms: None,
                    enabled: false,
                    last_run_at: None,
                    last_result_count: None,
                    last_error: Some("invalid query".to_string()),
                    created_at: 1_700_000_000_000,
                    updated_at: 1_700_000_002_000,
                },
            ])
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
                    triage_state: None,
                    labels: Vec::new(),
                    note: None,
                },
                EventView {
                    id: 2,
                    rule_id: "claude.error".to_string(),
                    pane_id: 20,
                    severity: "critical".to_string(),
                    message: "agent error".to_string(),
                    timestamp: 1_700_000_001_000,
                    handled: true,
                    triage_state: None,
                    labels: Vec::new(),
                    note: None,
                },
                EventView {
                    id: 3,
                    rule_id: "core.idle".to_string(),
                    pane_id: 10,
                    severity: "info".to_string(),
                    message: "pane idle".to_string(),
                    timestamp: 1_700_000_002_000,
                    handled: false,
                    triage_state: None,
                    labels: Vec::new(),
                    note: None,
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

    #[test]
    fn search_saved_shortcuts_cycle_and_queue_actions() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.refresh_data();
        assert_eq!(app.view_state.saved_searches.len(), 3);
        assert_eq!(app.view_state.saved_search_selected_index, 0);

        app.handle_search_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.view_state.saved_search_selected_index, 1);

        app.handle_search_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.view_state.saved_search_selected_index, 0);

        app.handle_search_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(
            app.pending_command.as_deref(),
            Some("wa search saved run errors")
        );

        app.handle_search_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(
            app.pending_command.as_deref(),
            Some("wa search saved enable errors")
        );

        app.view_state.saved_search_selected_index = 1;
        app.handle_search_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(
            app.pending_command.as_deref(),
            Some("wa search saved disable warnings")
        );

        app.view_state.saved_search_selected_index = 2;
        app.handle_search_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(
            app.view_state
                .error_message
                .as_deref()
                .is_some_and(|msg| msg.contains("no schedule")),
            "manual-only saved search should surface a schedule guidance error"
        );
    }

    #[test]
    fn history_navigation_filter_and_toggle() {
        let mut app = App::new(FixtureQueryClient, AppConfig::default());
        app.refresh_data();
        app.current_view = View::History;
        assert!(app.view_state.history_entries.len() >= 2);

        app.handle_history_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.history_selected_index, 1);

        app.handle_history_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        assert!(app.view_state.history_undoable_only);
        assert_eq!(app.view_state.history_selected_index, 0);

        app.handle_history_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        app.handle_history_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(app.view_state.history_filter_query, "wf");

        app.handle_history_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.view_state.history_filter_query, "w");

        app.handle_history_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.view_state.history_filter_query.is_empty());
        assert!(!app.view_state.history_undoable_only);
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

    // -----------------------------------------------------------------------
    // Comprehensive keybinding & state transition tests (wa-nu4.3.7.7)
    // -----------------------------------------------------------------------

    #[test]
    fn global_q_quits() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        assert!(!app.should_quit);
        app.handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(app.should_quit);
    }

    #[test]
    fn global_question_mark_goes_to_help() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        assert_eq!(app.current_view, View::Home);
        app.handle_key_event(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Help);
    }

    #[test]
    fn global_tab_cycles_forward() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        assert_eq!(app.current_view, View::Home);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Panes);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Events);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Triage);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::History);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Search);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Help);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Timeline);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Home); // Wraps
    }

    #[test]
    fn global_shift_tab_cycles_backward() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        assert_eq!(app.current_view, View::Home);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.current_view, View::Timeline);
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.current_view, View::Help);
    }

    #[test]
    fn global_backtab_cycles_backward() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.handle_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Timeline);
    }

    #[test]
    fn global_number_keys_switch_views() {
        let mut app = App::new(TestQueryClient, AppConfig::default());

        app.handle_key_event(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Panes);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Events);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Triage);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::History);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Search);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Help);

        app.handle_key_event(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(app.current_view, View::Home);
    }

    #[test]
    fn global_r_refreshes() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        // Clear health to prove refresh restores it
        app.view_state.health = None;
        app.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(app.view_state.health.is_some());
    }

    #[test]
    fn panes_navigation_down_up_wraps() {
        let mut app = App::new(MultiPaneQueryClient, AppConfig::default());
        app.refresh_data();
        assert_eq!(app.view_state.panes.len(), 3);

        // Navigate down through all panes
        app.handle_panes_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.selected_index, 1);
        app.handle_panes_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.selected_index, 2);
        // Wrap
        app.handle_panes_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.view_state.selected_index, 0);

        // Up wraps
        app.handle_panes_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.view_state.selected_index, 2);
    }

    #[test]
    fn panes_domain_filter_cycles() {
        let mut app = App::new(MultiPaneQueryClient, AppConfig::default());
        app.refresh_data();

        app.handle_panes_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.view_state.panes_domain_filter.as_deref(), Some("local"));

        app.handle_panes_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.view_state.panes_domain_filter.as_deref(), Some("ssh"));

        app.handle_panes_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(app.view_state.panes_domain_filter.is_none());
    }

    #[test]
    fn panes_esc_clears_filter() {
        let mut app = App::new(MultiPaneQueryClient, AppConfig::default());
        app.refresh_data();
        app.view_state.panes_filter_query = "test".to_string();
        app.handle_panes_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.view_state.panes_filter_query.is_empty());
    }

    #[test]
    fn triage_navigation_down_up_wraps() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.refresh_data();
        // TestQueryClient doesn't provide triage items; add manually
        app.view_state.triage_items = vec![
            crate::tui::query::TriageItemView {
                section: "events".to_string(),
                severity: "warning".to_string(),
                title: "item 1".to_string(),
                detail: "d1".to_string(),
                actions: vec![],
                event_id: Some(1),
                pane_id: Some(0),
                workflow_id: None,
            },
            crate::tui::query::TriageItemView {
                section: "events".to_string(),
                severity: "error".to_string(),
                title: "item 2".to_string(),
                detail: "d2".to_string(),
                actions: vec![],
                event_id: Some(2),
                pane_id: Some(0),
                workflow_id: None,
            },
        ];

        app.handle_triage_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.view_state.triage_selected_index, 1);

        // Wrap
        app.handle_triage_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.view_state.triage_selected_index, 0);

        // Up wraps
        app.handle_triage_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.view_state.triage_selected_index, 1);
    }

    #[test]
    fn triage_action_queues_pending_command() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.view_state.triage_items = vec![crate::tui::query::TriageItemView {
            section: "events".to_string(),
            severity: "warning".to_string(),
            title: "test".to_string(),
            detail: "".to_string(),
            actions: vec![crate::tui::query::TriageAction {
                label: "Explain".to_string(),
                command: "wa why --recent".to_string(),
            }],
            event_id: Some(1),
            pane_id: Some(0),
            workflow_id: None,
        }];

        app.handle_triage_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.pending_command.as_deref(), Some("wa why --recent"));
    }

    #[test]
    fn triage_action_by_number() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.view_state.triage_items = vec![crate::tui::query::TriageItemView {
            section: "events".to_string(),
            severity: "warning".to_string(),
            title: "test".to_string(),
            detail: "".to_string(),
            actions: vec![
                crate::tui::query::TriageAction {
                    label: "First".to_string(),
                    command: "wa first".to_string(),
                },
                crate::tui::query::TriageAction {
                    label: "Second".to_string(),
                    command: "wa second".to_string(),
                },
            ],
            event_id: Some(1),
            pane_id: Some(0),
            workflow_id: None,
        }];

        app.handle_triage_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.pending_command.as_deref(), Some("wa second"));
    }

    #[test]
    fn triage_invalid_action_sets_error() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.view_state.triage_items = vec![crate::tui::query::TriageItemView {
            section: "events".to_string(),
            severity: "warning".to_string(),
            title: "test".to_string(),
            detail: "".to_string(),
            actions: vec![],
            event_id: Some(1),
            pane_id: Some(0),
            workflow_id: None,
        }];

        app.handle_triage_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.view_state.error_message.is_some());
    }

    #[test]
    fn search_j_k_noop_without_results() {
        let mut app = App::new(SearchQueryClient, AppConfig::default());
        app.refresh_data();
        assert!(app.view_state.search_results.is_empty());

        // j/k should not panic with empty results
        app.handle_search_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.view_state.search_selected_index, 0);
        app.handle_search_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.view_state.search_selected_index, 0);
    }

    #[test]
    fn refresh_data_clears_previous_error() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.view_state.set_error("old error");
        app.refresh_data();
        // Should clear error on successful refresh
        assert!(app.view_state.error_message.is_none());
    }

    #[test]
    fn refresh_data_resets_selected_index_when_out_of_bounds() {
        let mut app = App::new(TestQueryClient, AppConfig::default());
        app.view_state.selected_index = 99;
        app.refresh_data();
        // TestQueryClient returns 1 pane, so selected_index should reset
        assert_eq!(app.view_state.selected_index, 0);
    }

    #[test]
    fn app_config_default_values() {
        let config = AppConfig::default();
        assert_eq!(config.refresh_interval, std::time::Duration::from_secs(5));
        assert!(!config.debug);
    }

    // =========================================================================
    // E2E TUI smoke test (bd-12f4)
    // =========================================================================
    //
    // Drives a scripted TUI session through all views using a rich mock
    // query client with deterministic fixture data. Renders to a Buffer
    // after each interaction to verify no panics and stable output.

    /// Rich fixture query client with panes, events, triage items, workflows
    struct FixtureQueryClient;

    fn fixture_panes() -> Vec<PaneView> {
        vec![
            pane(1, "claude-code session", Some("claude_code"), 3),
            pane(2, "codex build", Some("codex"), 0),
            pane(3, "manual shell", None, 1),
        ]
    }

    fn fixture_events() -> Vec<EventView> {
        vec![
            EventView {
                id: 1,
                rule_id: "auth_prompt".to_string(),
                pane_id: 1,
                severity: "warning".to_string(),
                message: "auth.prompt: Please authenticate".to_string(),
                timestamp: 1_700_000_000_000,
                handled: false,
                triage_state: Some("new".to_string()),
                labels: vec!["auth".to_string()],
                note: Some("Waiting for operator follow-up".to_string()),
            },
            EventView {
                id: 2,
                rule_id: "secret_leak".to_string(),
                pane_id: 1,
                severity: "critical".to_string(),
                message: "secret_detected: API key found in output".to_string(),
                timestamp: 1_700_000_001_000,
                handled: true,
                triage_state: Some("mitigated".to_string()),
                labels: vec!["security".to_string(), "urgent".to_string()],
                note: Some("Credential rotated".to_string()),
            },
            EventView {
                id: 3,
                rule_id: "build_error".to_string(),
                pane_id: 3,
                severity: "error".to_string(),
                message: "error.compilation: cargo build failed".to_string(),
                timestamp: 1_700_000_002_000,
                handled: false,
                triage_state: Some("investigating".to_string()),
                labels: vec!["build".to_string()],
                note: None,
            },
        ]
    }

    fn fixture_triage_items() -> Vec<crate::tui::query::TriageItemView> {
        use crate::tui::query::TriageAction;
        vec![
            crate::tui::query::TriageItemView {
                section: "Events".to_string(),
                severity: "warning".to_string(),
                title: "auth.prompt on pane 1".to_string(),
                detail: "Please authenticate".to_string(),
                actions: vec![TriageAction {
                    label: "Run auth workflow".to_string(),
                    command: "wa workflow run auth_recovery --pane 1".to_string(),
                }],
                event_id: Some(1),
                pane_id: Some(1),
                workflow_id: None,
            },
            crate::tui::query::TriageItemView {
                section: "Events".to_string(),
                severity: "error".to_string(),
                title: "error.compilation on pane 3".to_string(),
                detail: "cargo build failed".to_string(),
                actions: vec![
                    TriageAction {
                        label: "View build log".to_string(),
                        command: "wa export segments --pane-id 3".to_string(),
                    },
                    TriageAction {
                        label: "Retry build".to_string(),
                        command: "wa robot send 3 'cargo build'".to_string(),
                    },
                ],
                event_id: Some(3),
                pane_id: Some(3),
                workflow_id: None,
            },
        ]
    }

    fn fixture_workflows() -> Vec<WorkflowProgressView> {
        vec![WorkflowProgressView {
            id: "wf-auth-1".to_string(),
            workflow_name: "auth_recovery".to_string(),
            pane_id: 1,
            current_step: 2,
            total_steps: 4,
            status: "running".to_string(),
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_001_000,
        }]
    }

    fn fixture_history() -> Vec<HistoryEntryView> {
        vec![
            HistoryEntryView {
                audit_id: 101,
                timestamp: 1_700_000_001_500,
                pane_id: Some(1),
                workflow_id: Some("wf-auth-1".to_string()),
                action_kind: "workflow_step".to_string(),
                result: "success".to_string(),
                actor_kind: "workflow".to_string(),
                step_name: Some("request_auth".to_string()),
                undoable: true,
                undone: false,
                undo_strategy: Some("workflow_abort".to_string()),
                undo_hint: Some("Abort wf-auth-1 to rollback".to_string()),
                rule_id: Some("auth_prompt".to_string()),
                summary: "Prompted user to authenticate".to_string(),
            },
            HistoryEntryView {
                audit_id: 102,
                timestamp: 1_700_000_001_800,
                pane_id: Some(3),
                workflow_id: None,
                action_kind: "send_text".to_string(),
                result: "denied".to_string(),
                actor_kind: "robot".to_string(),
                step_name: None,
                undoable: false,
                undone: false,
                undo_strategy: None,
                undo_hint: None,
                rule_id: Some("policy.command_gate".to_string()),
                summary: "Blocked unsafe command".to_string(),
            },
        ]
    }

    impl QueryClient for FixtureQueryClient {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok(fixture_panes())
        }

        fn list_events(&self, filters: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            let events = fixture_events();
            let filtered: Vec<_> = events
                .into_iter()
                .filter(|e| {
                    if filters.unhandled_only && e.handled {
                        return false;
                    }
                    true
                })
                .collect();
            Ok(filtered)
        }

        fn list_triage_items(&self) -> Result<Vec<crate::tui::query::TriageItemView>, QueryError> {
            Ok(fixture_triage_items())
        }

        fn search(&self, query: &str, _limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
            if query.is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![SearchResultView {
                pane_id: 1,
                timestamp: 1_700_000_000_000,
                snippet: format!("…match for '{query}'…"),
                rank: 0.95,
            }])
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: true,
                db_accessible: true,
                wezterm_accessible: true,
                wezterm_circuit: crate::circuit_breaker::CircuitBreakerStatus::default(),
                pane_count: 3,
                event_count: 3,
                last_capture_ts: Some(1_700_000_002_000),
            })
        }

        fn is_watcher_running(&self) -> bool {
            true
        }

        fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
            Ok(())
        }

        fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
            Ok(fixture_workflows())
        }

        fn list_action_history(&self, _limit: usize) -> Result<Vec<HistoryEntryView>, QueryError> {
            Ok(fixture_history())
        }
    }

    /// Extract text content from a Buffer as a vector of line strings.
    fn buffer_to_lines(buf: &Buffer, area: Rect) -> Vec<String> {
        let mut lines = Vec::new();
        for y in area.y..area.y + area.height {
            let mut line = String::new();
            for x in area.x..area.x + area.width {
                let cell = buf.cell((x, y)).unwrap();
                line.push_str(cell.symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines
    }

    /// Emit an E2E artifact for CI debugging.
    fn emit_artifact(label: &str, content: &str) {
        eprintln!("[ARTIFACT][tui-e2e] {label}:\n{content}\n");
    }

    /// Render the app into a fresh buffer and return the text lines.
    fn render_snapshot(app: &App<FixtureQueryClient>, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        app.render(area, &mut buf);
        buffer_to_lines(&buf, area)
    }

    #[test]
    fn e2e_smoke_full_interaction_flow() {
        let mut app = App::new(FixtureQueryClient, AppConfig::default());
        app.refresh_data();

        let mut transcript = String::new();
        let mut step = 0u32;

        let record =
            |transcript: &mut String, step: &mut u32, desc: &str, app: &App<FixtureQueryClient>| {
                *step += 1;
                let lines = render_snapshot(app, 100, 30);
                let snapshot = lines.join("\n");
                transcript.push_str(&format!("--- Step {step}: {desc} ---\n{snapshot}\n\n"));
            };

        // Step 1: Home view (default)
        record(&mut transcript, &mut step, "Home view (initial)", &app);
        let lines = render_snapshot(&app, 100, 30);
        assert!(
            lines.iter().any(|l| l.contains("Home")),
            "Home view should show Home tab"
        );

        // Step 2: Navigate to Panes view
        app.handle_key_event(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Panes view", &app);
        let lines = render_snapshot(&app, 100, 30);
        assert!(
            lines.iter().any(|l| l.contains("claude-code")),
            "Panes view should show fixture pane"
        );

        // Step 3: Navigate down in panes
        app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Panes: down", &app);

        // Step 4: Navigate to Events view
        app.handle_key_event(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Events view", &app);
        let lines = render_snapshot(&app, 100, 30);
        assert!(
            lines.iter().any(|l| l.contains("auth.prompt")
                || l.contains("secret_detected")
                || l.contains("auth_prompt")),
            "Events view should show fixture events"
        );

        // Step 5: Toggle unhandled filter
        app.handle_key_event(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Events: toggle unhandled", &app);

        // Step 6: Navigate down in events
        app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Events: down", &app);

        // Step 7: Navigate to Triage view
        app.handle_key_event(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Triage view", &app);
        let lines = render_snapshot(&app, 100, 30);
        assert!(
            lines.iter().any(|l| l.contains("auth.prompt")
                || l.contains("compilation")
                || l.contains("Events")),
            "Triage view should show triage items"
        );

        // Step 8: Navigate down in triage
        app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Triage: down", &app);

        // Step 9: Expand workflow panel
        app.handle_key_event(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Triage: expand workflows", &app);

        // Step 10: Collapse workflow panel
        app.handle_key_event(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        record(
            &mut transcript,
            &mut step,
            "Triage: collapse workflows",
            &app,
        );

        // Step 11: Navigate to History view
        app.handle_key_event(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "History view", &app);
        let lines = render_snapshot(&app, 100, 30);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("workflow_step") || l.contains("#   101")),
            "History view should show fixture history entries"
        );

        // Step 12: Navigate to Search view
        app.handle_key_event(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Search view", &app);

        // Step 13-16: Type search query
        for ch in "test".chars() {
            app.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        record(&mut transcript, &mut step, "Search: typed 'test'", &app);

        // Step 17: Execute search
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Search: execute", &app);
        let lines = render_snapshot(&app, 100, 30);
        assert!(
            lines.iter().any(|l| l.contains("test")),
            "Search results should contain query match"
        );

        // Step 18: Clear search
        app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Search: clear", &app);

        // Step 19: Navigate to Help view
        app.handle_key_event(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Help view", &app);
        let lines = render_snapshot(&app, 100, 30);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Keybindings") || l.contains("Help")),
            "Help view should show help content"
        );

        // Step 20: Go back to Home
        app.handle_key_event(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Back to Home", &app);

        // Step 21: Tab through views
        for i in 0..7 {
            app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            record(
                &mut transcript,
                &mut step,
                &format!("Tab cycle {}", i + 1),
                &app,
            );
        }

        // Step 22: Refresh
        app.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        record(&mut transcript, &mut step, "Refresh", &app);

        // Final: verify app is still functional
        assert!(
            !app.should_quit,
            "App should not have quit during smoke test"
        );

        // Emit full transcript as artifact
        emit_artifact("session_transcript", &transcript);
        emit_artifact("total_steps", &format!("{step}"));
    }

    #[test]
    fn e2e_smoke_resize_stability() {
        let mut app = App::new(FixtureQueryClient, AppConfig::default());
        app.refresh_data();

        // Test rendering at various terminal sizes without panicking
        let sizes: Vec<(u16, u16)> = vec![
            (80, 24),  // Standard
            (120, 40), // Large
            (40, 10),  // Minimum viable
            (200, 60), // Extra wide
            (60, 15),  // Narrow
            (80, 12),  // Short
        ];

        let views = [
            View::Home,
            View::Panes,
            View::Events,
            View::Triage,
            View::History,
            View::Search,
            View::Help,
        ];

        for &(width, height) in &sizes {
            for &view in &views {
                app.current_view = view;
                let lines = render_snapshot(&app, width, height);
                // Should have rendered something (not all empty)
                assert!(
                    lines.iter().any(|l| !l.is_empty()),
                    "Render at {width}x{height} in {:?} should produce output",
                    view
                );
            }
        }
    }

    #[test]
    fn e2e_smoke_rapid_key_sequence() {
        let mut app = App::new(FixtureQueryClient, AppConfig::default());
        app.refresh_data();

        // Rapid-fire key sequence that exercises many code paths
        let keys = vec![
            KeyCode::Char('2'), // Panes
            KeyCode::Down,      // Nav down
            KeyCode::Down,      // Nav down
            KeyCode::Up,        // Nav up
            KeyCode::Char('u'), // Toggle unhandled
            KeyCode::Char('a'), // Cycle agent filter
            KeyCode::Char('a'), // Cycle again
            KeyCode::Esc,       // Clear filter
            KeyCode::Char('3'), // Events
            KeyCode::Char('j'), // Nav down
            KeyCode::Char('k'), // Nav up
            KeyCode::Char('u'), // Toggle unhandled
            KeyCode::Char('4'), // Triage
            KeyCode::Char('j'), // Nav down
            KeyCode::Char('e'), // Expand workflows
            KeyCode::Char('e'), // Collapse
            KeyCode::Char('a'), // Queue action
            KeyCode::Char('5'), // History
            KeyCode::Char('w'), // History filter
            KeyCode::Esc,       // Clear history filter
            KeyCode::Char('6'), // Search
            KeyCode::Char('h'), // Type search
            KeyCode::Char('i'),
            KeyCode::Enter,     // Execute
            KeyCode::Char('j'), // Nav down in results
            KeyCode::Esc,       // Clear
            KeyCode::Char('7'), // Help
            KeyCode::Char('1'), // Home
            KeyCode::Tab,       // Tab through
            KeyCode::Tab,
            KeyCode::BackTab,   // Back-tab
            KeyCode::Char('r'), // Refresh
        ];

        for key in &keys {
            app.handle_key_event(KeyEvent::new(*key, KeyModifiers::NONE));
            // Render after every key to catch any panic
            let _ = render_snapshot(&app, 80, 24);
        }

        assert!(!app.should_quit, "App should survive rapid key sequence");
    }

    #[test]
    fn e2e_smoke_pane_view_renders_all_fixture_data() {
        let mut app = App::new(FixtureQueryClient, AppConfig::default());
        app.refresh_data();
        app.handle_key_event(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));

        let lines = render_snapshot(&app, 120, 30);
        let text = lines.join("\n");

        // All fixture panes should be visible
        assert!(text.contains("claude-code"), "Should show claude-code pane");
        assert!(text.contains("codex"), "Should show codex pane");
        assert!(
            text.contains("manual shell") || text.contains("shell"),
            "Should show manual pane"
        );
    }

    #[test]
    fn e2e_smoke_workflow_panel_in_triage() {
        let mut app = App::new(FixtureQueryClient, AppConfig::default());
        app.refresh_data();
        app.handle_key_event(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));

        let lines = render_snapshot(&app, 120, 30);
        let text = lines.join("\n");

        // Workflow panel should show the running workflow
        assert!(
            text.contains("auth_recovery") || text.contains("running"),
            "Triage view should show workflow progress"
        );
    }

    #[test]
    fn e2e_smoke_home_view_shows_health() {
        let mut app = App::new(FixtureQueryClient, AppConfig::default());
        app.refresh_data();

        let lines = render_snapshot(&app, 100, 30);
        let text = lines.join("\n");

        // Health info should appear
        assert!(
            text.contains("Watcher") || text.contains("watcher") || text.contains("Panes"),
            "Home view should show health/status info"
        );
    }
}
