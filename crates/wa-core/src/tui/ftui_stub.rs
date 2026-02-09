//! FrankenTUI backend for wa TUI.
//!
//! Implements the Elm-style `Model` trait from `ftui::runtime` to drive the
//! wa interactive terminal UI.  The app shell handles:
//!
//! - View routing (Home, Panes, Events, Triage, History, Search, Help)
//! - Tab bar rendering with highlighted active view
//! - Global keybindings (Tab, 1-7, q, ?, r)
//! - Status footer with view name and refresh indicator
//! - Periodic data refresh via background tasks
//!
//! Individual view rendering functions will be migrated in FTUI-05.2 through
//! FTUI-05.7.  Until then, each view body shows a placeholder message.
//!
//! # Architecture
//!
//! ```text
//! ftui runtime event loop
//!   ↓ Event
//! WaMsg (From<Event>)
//!   ↓
//! WaModel::update()  →  Cmd (side effects)
//!   ↓
//! WaModel::view()    →  Frame (tab bar + content + footer)
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::query::QueryClient;
use super::view_adapters::{
    HealthModel, PaneRow, SearchRow, TriageRow, WorkflowRow,
    adapt_event, adapt_health, adapt_pane, adapt_search, adapt_triage, adapt_workflow,
};

// ---------------------------------------------------------------------------
// View enum — shared navigation target
// ---------------------------------------------------------------------------

/// Available views in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Home,
    Panes,
    Events,
    Triage,
    History,
    Search,
    Help,
}

impl View {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Panes => "Panes",
            Self::Events => "Events",
            Self::Triage => "Triage",
            Self::History => "History",
            Self::Search => "Search",
            Self::Help => "Help",
        }
    }

    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Home,
            Self::Panes,
            Self::Events,
            Self::Triage,
            Self::History,
            Self::Search,
            Self::Help,
        ]
    }

    /// Shortcut key for direct navigation (1-7).
    #[must_use]
    pub const fn shortcut(&self) -> char {
        match self {
            Self::Home => '1',
            Self::Panes => '2',
            Self::Events => '3',
            Self::Triage => '4',
            Self::History => '5',
            Self::Search => '6',
            Self::Help => '7',
        }
    }

    /// Next view in tab order (wraps around).
    #[must_use]
    pub const fn next(&self) -> Self {
        match self {
            Self::Home => Self::Panes,
            Self::Panes => Self::Events,
            Self::Events => Self::Triage,
            Self::Triage => Self::History,
            Self::History => Self::Search,
            Self::Search => Self::Help,
            Self::Help => Self::Home,
        }
    }

    /// Previous view in tab order (wraps around).
    #[must_use]
    pub const fn prev(&self) -> Self {
        match self {
            Self::Home => Self::Help,
            Self::Panes => Self::Home,
            Self::Events => Self::Panes,
            Self::Triage => Self::Events,
            Self::History => Self::Triage,
            Self::Search => Self::History,
            Self::Help => Self::Search,
        }
    }

    /// Resolve a '1'-'7' character to a view.
    fn from_shortcut(ch: char) -> Option<Self> {
        match ch {
            '1' => Some(Self::Home),
            '2' => Some(Self::Panes),
            '3' => Some(Self::Events),
            '4' => Some(Self::Triage),
            '5' => Some(Self::History),
            '6' => Some(Self::Search),
            '7' => Some(Self::Help),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ViewState — per-view data
// ---------------------------------------------------------------------------

/// Aggregated view state.
///
/// Holds all per-view state for the TUI.  Individual view state is added as
/// views are migrated (FTUI-05.2 through FTUI-05.7).
#[derive(Debug, Default)]
pub struct ViewState {
    pub current_view: View,
    pub error_message: Option<String>,

    // -- Events view state (FTUI-05.4) --
    pub events: EventsViewState,
}

/// Events view state.
#[derive(Debug, Default)]
pub struct EventsViewState {
    /// Raw events from last data refresh.
    pub items: Vec<super::query::EventView>,
    /// Adapted render-ready rows (parallel to `items`).
    pub rows: Vec<super::view_adapters::EventRow>,
    /// Show only unhandled events.
    pub unhandled_only: bool,
    /// Pane/rule text filter (digits for pane, text for rule).
    pub pane_filter: String,
    /// Currently selected index within the filtered list.
    pub selected_index: usize,
}

impl EventsViewState {
    /// Return indices of events matching the current filters.
    pub fn filtered_indices(&self) -> Vec<usize> {
        let query = self.pane_filter.trim();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, ev)| {
                if self.unhandled_only && ev.handled {
                    return false;
                }
                if !query.is_empty() {
                    let pane_str = ev.pane_id.to_string();
                    if !pane_str.contains(query) && !ev.rule_id.contains(query) {
                        return false;
                    }
                }
                true
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Clamped selected index within filtered results.
    pub fn clamped_selection(&self) -> usize {
        let filtered = self.filtered_indices();
        self.selected_index
            .min(filtered.len().saturating_sub(1))
    }
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// TUI application configuration.
pub struct AppConfig {
    pub refresh_interval: Duration,
    pub debug: bool,
}

// ---------------------------------------------------------------------------
// WaModel — Elm-style model for ftui runtime
// ---------------------------------------------------------------------------

/// Messages that drive the wa TUI state machine.
///
/// Terminal events are converted via `From<ftui::Event>`.
#[allow(dead_code)] // Variants used as the migration progresses (FTUI-05.2+)
pub enum WaMsg {
    /// A terminal event forwarded to the active view.
    TermEvent(ftui::Event),
    /// Switch to a specific view.
    SwitchView(View),
    /// Navigate to next tab.
    NextTab,
    /// Navigate to previous tab.
    PrevTab,
    /// Periodic data refresh tick.
    Tick,
    /// Quit the application.
    Quit,
}

impl From<ftui::Event> for WaMsg {
    fn from(event: ftui::Event) -> Self {
        Self::TermEvent(event)
    }
}

/// The top-level ftui Model for wa.
///
/// Owns a `QueryClient` (behind `Arc` for `Send` + background tasks) and
/// the aggregated view state.  The runtime drives the init → update → view
/// cycle.
pub struct WaModel {
    view_state: ViewState,
    config: AppConfig,
    last_refresh: Instant,
    // QueryClient stored as trait object for type erasure (the generic Q
    // parameter is resolved at construction time in run_tui).
    query: Arc<dyn QueryClient + Send + Sync>,
    // Home dashboard state — refreshed on each Tick.
    health: Option<HealthModel>,
    unhandled_count: usize,
    triage_count: usize,
    // Panes view state.
    panes: Vec<PaneRow>,
    panes_selected: usize,
    panes_domain_filter: Option<String>,
    // Triage view state.
    triage_items: Vec<TriageRow>,
    triage_selected: usize,
    triage_expanded: Option<usize>,
    workflows: Vec<WorkflowRow>,
    // Queued action command from triage (consumed by the event loop).
    triage_queued_action: Option<String>,
    // Search view state.
    search_query: String,
    search_last_query: String,
    search_results: Vec<SearchRow>,
    search_selected: usize,
}

impl WaModel {
    fn new(query: Arc<dyn QueryClient + Send + Sync>, config: AppConfig) -> Self {
        Self {
            view_state: ViewState::default(),
            config,
            last_refresh: Instant::now(),
            query,
            health: None,
            unhandled_count: 0,
            triage_count: 0,
            panes: Vec::new(),
            panes_selected: 0,
            panes_domain_filter: None,
            triage_items: Vec::new(),
            triage_selected: 0,
            triage_expanded: None,
            workflows: Vec::new(),
            triage_queued_action: None,
            search_query: String::new(),
            search_last_query: String::new(),
            search_results: Vec::new(),
            search_selected: 0,
        }
    }

    /// Handle a key event for the active view.
    fn handle_view_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        if key.kind != ftui::KeyEventKind::Press {
            return ftui::Cmd::None;
        }

        match self.view_state.current_view {
            View::Panes => self.handle_panes_key(key),
            View::Events => self.handle_events_key(key),
            View::Triage => self.handle_triage_key(key),
            View::Search => self.handle_search_key(key),
            _ => ftui::Cmd::None,
        }
    }

    /// Handle keys specific to the Panes view.
    fn handle_panes_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let filtered = self.filtered_pane_indices();
        let count = filtered.len();

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.panes_selected = (self.panes_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if count > 0 {
                    self.panes_selected =
                        self.panes_selected.checked_sub(1).unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Char('d') => {
                // Cycle domain filter
                let domains = self.unique_domains();
                self.panes_domain_filter = match &self.panes_domain_filter {
                    None if !domains.is_empty() => Some(domains[0].clone()),
                    Some(current) => {
                        let idx = domains.iter().position(|d| d == current);
                        match idx {
                            Some(i) if i + 1 < domains.len() => {
                                Some(domains[i + 1].clone())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Escape => {
                self.panes_domain_filter = None;
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    /// Handle keys specific to the Triage view.
    ///
    /// j/k/Down/Up: navigate items.  Enter/a: run primary action.
    /// 1-9: run numbered action.  m: mute selected event.
    /// e: toggle workflow expand/collapse.
    fn handle_triage_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let count = self.triage_items.len();

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.triage_selected = (self.triage_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if count > 0 {
                    self.triage_selected =
                        self.triage_selected.checked_sub(1).unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Enter | KeyCode::Char('a') => {
                // Queue primary action (index 0) for the selected triage item.
                self.queue_triage_action(0);
                ftui::Cmd::None
            }
            KeyCode::Char('m') => {
                // Mute the selected triage item's event (if it has an event_id).
                self.mute_selected_triage_event();
                ftui::Cmd::None
            }
            KeyCode::Char('e') => {
                // Toggle workflow progress expand/collapse.
                if !self.workflows.is_empty() {
                    if self.triage_expanded.is_some() {
                        self.triage_expanded = None;
                    } else {
                        self.triage_expanded = Some(0);
                    }
                }
                ftui::Cmd::None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap_or(0);
                if idx > 0 {
                    self.queue_triage_action(idx as usize - 1);
                }
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    /// Queue a triage action command for later execution.
    fn queue_triage_action(&mut self, action_idx: usize) {
        if let Some(item) = self.triage_items.get(self.triage_selected) {
            if let Some(cmd) = item.action_commands.get(action_idx) {
                self.triage_queued_action = Some(cmd.clone());
            }
        }
    }

    /// Mute the event associated with the selected triage item.
    fn mute_selected_triage_event(&mut self) {
        let event_id_str = self
            .triage_items
            .get(self.triage_selected)
            .map(|item| item.event_id.clone())
            .unwrap_or_default();
        if let Ok(event_id) = event_id_str.parse::<i64>() {
            if let Err(e) = self.query.mark_event_muted(event_id) {
                self.view_state.error_message =
                    Some(format!("Mute failed: {e}"));
            }
        }
    }

    /// Handle keys specific to the Search view.
    ///
    /// Text input: chars append to query, Backspace removes, Enter executes,
    /// Escape clears.  j/k/Down/Up navigate results.
    fn handle_search_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        match key.code {
            KeyCode::Char(c) => {
                self.search_query.push(c);
                ftui::Cmd::None
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                ftui::Cmd::None
            }
            KeyCode::Enter => {
                let query = self.search_query.trim().to_string();
                if query.is_empty() {
                    return ftui::Cmd::None;
                }
                self.search_last_query.clone_from(&query);
                match self.query.search(&query, 50) {
                    Ok(results) => {
                        self.search_results =
                            results.iter().map(adapt_search).collect();
                        self.search_selected = 0;
                    }
                    Err(e) => {
                        self.view_state.error_message =
                            Some(format!("Search failed: {e}"));
                        self.search_results.clear();
                    }
                }
                ftui::Cmd::None
            }
            KeyCode::Escape => {
                self.search_query.clear();
                self.search_last_query.clear();
                self.search_results.clear();
                self.search_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Down => {
                let count = self.search_results.len();
                if count > 0 {
                    self.search_selected = (self.search_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up => {
                let count = self.search_results.len();
                if count > 0 {
                    self.search_selected =
                        self.search_selected.checked_sub(1).unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    /// Handle keys specific to the Events view.
    ///
    /// j/k/Down/Up navigate, u toggles unhandled filter, Backspace removes
    /// last filter char, Esc clears filter, digits append to pane filter.
    fn handle_events_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let filtered = self.view_state.events.filtered_indices();
        let count = filtered.len();

        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.view_state.events.selected_index =
                        (self.view_state.events.selected_index + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if count > 0 {
                    self.view_state.events.selected_index = self
                        .view_state
                        .events
                        .selected_index
                        .checked_sub(1)
                        .unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Char('u') => {
                self.view_state.events.unhandled_only =
                    !self.view_state.events.unhandled_only;
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Backspace => {
                self.view_state.events.pane_filter.pop();
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Escape => {
                self.view_state.events.pane_filter.clear();
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                self.view_state.events.pane_filter.push(c);
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    /// Return indices of panes matching the current domain filter.
    fn filtered_pane_indices(&self) -> Vec<usize> {
        self.panes
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                self.panes_domain_filter
                    .as_ref()
                    .is_none_or(|f| p.domain == *f)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Collect unique domain names from pane data.
    fn unique_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self
            .panes
            .iter()
            .map(|p| p.domain.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        domains.sort();
        domains
    }

    /// Refresh dashboard data from the QueryClient.
    fn refresh_data(&mut self) {
        // Health status
        match self.query.health() {
            Ok(health) => {
                self.health = Some(adapt_health(&health));
            }
            Err(e) => {
                self.view_state.error_message =
                    Some(format!("Health query failed: {e}"));
            }
        }

        // Pane data (also used for unhandled count)
        match self.query.list_panes() {
            Ok(panes) => {
                self.unhandled_count = panes
                    .iter()
                    .map(|p| p.unhandled_event_count as usize)
                    .sum();
                self.panes = panes.iter().map(adapt_pane).collect();
                // Clamp selection
                if !self.panes.is_empty() {
                    self.panes_selected = self.panes_selected.min(self.panes.len() - 1);
                } else {
                    self.panes_selected = 0;
                }
            }
            Err(_) => { /* health query already reports errors */ }
        }

        // Triage items (used for both count on Home and Triage view)
        match self.query.list_triage_items() {
            Ok(items) => {
                self.triage_count = items.len();
                self.triage_items = items.iter().map(adapt_triage).collect();
                if self.triage_items.is_empty() {
                    self.triage_selected = 0;
                } else {
                    self.triage_selected =
                        self.triage_selected.min(self.triage_items.len() - 1);
                }
            }
            Err(_) => { /* non-fatal */ }
        }

        // Active workflows (for Triage view progress panel)
        match self.query.list_active_workflows() {
            Ok(wfs) => {
                self.workflows = wfs.iter().map(adapt_workflow).collect();
            }
            Err(_) => { /* non-fatal */ }
        }

        // Events data
        match self.query.list_events(&super::query::EventFilters {
            pane_id: None,
            rule_id: None,
            event_type: None,
            unhandled_only: false,
            limit: 500,
        }) {
            Ok(events) => {
                self.view_state.events.rows =
                    events.iter().map(adapt_event).collect();
                self.view_state.events.items = events;
                // Clamp selection within filtered results
                let filtered_len =
                    self.view_state.events.filtered_indices().len();
                if filtered_len > 0 {
                    self.view_state.events.selected_index = self
                        .view_state
                        .events
                        .selected_index
                        .min(filtered_len - 1);
                } else {
                    self.view_state.events.selected_index = 0;
                }
            }
            Err(_) => { /* non-fatal */ }
        }
    }

    /// Handle a key event at the global level.  Returns `Some(Cmd)` if the
    /// key was consumed, `None` if it should be forwarded to the active view.
    fn handle_global_key(&mut self, key: &ftui::KeyEvent) -> Option<ftui::Cmd<WaMsg>> {
        use ftui::KeyCode;

        // Only handle key-down events.
        if key.kind != ftui::KeyEventKind::Press {
            return Some(ftui::Cmd::None);
        }

        let in_search = self.view_state.current_view == View::Search;
        let in_events = self.view_state.current_view == View::Events;
        let in_triage = self.view_state.current_view == View::Triage;

        match key.code {
            // Tab/BackTab navigation is always global (even in Search).
            KeyCode::Tab => {
                self.view_state.current_view = self.view_state.current_view.next();
                Some(ftui::Cmd::None)
            }
            KeyCode::BackTab => {
                self.view_state.current_view = self.view_state.current_view.prev();
                Some(ftui::Cmd::None)
            }
            // Character-based shortcuts are suppressed in Search view so that
            // keystrokes flow to the search query input instead.
            KeyCode::Char('q') if !in_search => Some(ftui::Cmd::Quit),
            KeyCode::Char('?') if !in_search => {
                self.view_state.current_view = View::Help;
                Some(ftui::Cmd::None)
            }
            KeyCode::Char('r') if !in_search => {
                self.view_state.error_message = None;
                self.refresh_data();
                Some(ftui::Cmd::None)
            }
            // In Events/Triage views, digits go to view-specific handlers.
            KeyCode::Char(ch @ '1'..='7') if !in_search && !in_events && !in_triage => {
                if let Some(view) = View::from_shortcut(ch) {
                    self.view_state.current_view = view;
                }
                Some(ftui::Cmd::None)
            }
            _ => None, // Not consumed — forward to view
        }
    }
}

impl ftui::Model for WaModel {
    type Message = WaMsg;

    fn init(&mut self) -> ftui::Cmd<WaMsg> {
        // Load initial data before first render.
        self.refresh_data();
        // Schedule periodic data refresh.
        ftui::Cmd::Tick(self.config.refresh_interval)
    }

    fn update(&mut self, msg: WaMsg) -> ftui::Cmd<WaMsg> {
        match msg {
            WaMsg::TermEvent(ftui::Event::Key(ref key)) => {
                if let Some(cmd) = self.handle_global_key(key) {
                    return cmd;
                }
                // Forward to active view handler
                self.handle_view_key(key)
            }
            WaMsg::TermEvent(_) => {
                // Resize, mouse, paste — forward to view when implemented
                ftui::Cmd::None
            }
            WaMsg::SwitchView(view) => {
                self.view_state.current_view = view;
                ftui::Cmd::None
            }
            WaMsg::NextTab => {
                self.view_state.current_view = self.view_state.current_view.next();
                ftui::Cmd::None
            }
            WaMsg::PrevTab => {
                self.view_state.current_view = self.view_state.current_view.prev();
                ftui::Cmd::None
            }
            WaMsg::Tick => {
                self.last_refresh = Instant::now();
                self.view_state.error_message = None;
                self.refresh_data();
                // Re-schedule next tick
                ftui::Cmd::Tick(self.config.refresh_interval)
            }
            WaMsg::Quit => ftui::Cmd::Quit,
        }
    }

    fn view(&self, frame: &mut ftui::Frame) {
        let width = frame.width();
        let height = frame.height();

        if height < 3 {
            // Terminal too small — render nothing meaningful
            return;
        }

        // Layout: [tab bar: 1 row] [content: remaining] [footer: 1 row]
        let tab_row = 0u16;
        let content_y = 1u16;
        let content_h = height.saturating_sub(2);
        let footer_row = height.saturating_sub(1);

        // -- Tab bar --
        render_tab_bar(frame, tab_row, width, self.view_state.current_view);

        // -- Content area --
        match self.view_state.current_view {
            View::Home => render_home_view(
                frame,
                content_y,
                width,
                content_h,
                self.health.as_ref(),
                self.unhandled_count,
                self.triage_count,
            ),
            View::Panes => {
                let filtered = self.filtered_pane_indices();
                render_panes_view(
                    frame,
                    content_y,
                    width,
                    content_h,
                    &self.panes,
                    &filtered,
                    self.panes_selected,
                    self.panes_domain_filter.as_deref(),
                );
            }
            View::Search => render_search_view(
                frame,
                content_y,
                width,
                content_h,
                &self.search_query,
                &self.search_last_query,
                &self.search_results,
                self.search_selected,
            ),
            View::Help => render_help_view(frame, content_y, width, content_h),
            View::Events => {
                let filtered = self.view_state.events.filtered_indices();
                let clamped_sel = self.view_state.events.clamped_selection();
                render_events_view(
                    frame,
                    content_y,
                    width,
                    content_h,
                    &self.view_state.events,
                    &filtered,
                    clamped_sel,
                );
            }
            View::Triage => render_triage_view(
                frame,
                content_y,
                width,
                content_h,
                &self.triage_items,
                self.triage_selected,
                &self.workflows,
                self.triage_expanded,
            ),
            View::History => render_view_placeholder(
                frame,
                content_y,
                width,
                content_h,
                self.view_state.current_view,
            ),
        }

        // -- Footer / status bar --
        render_footer(
            frame,
            footer_row,
            width,
            self.view_state.current_view,
            self.view_state.error_message.as_deref(),
        );
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Render the tab bar at the given row.
fn render_tab_bar(frame: &mut ftui::Frame, row: u16, width: u16, active: View) {
    let mut col = 0u16;
    for &view in View::all() {
        let label = format!(" {} {} ", view.shortcut(), view.name());
        let label_width = label.len() as u16;

        if col + label_width > width {
            break;
        }

        let style = if view == active {
            CellStyle::new().bold().reverse()
        } else {
            CellStyle::new()
        };

        write_styled(frame, col, row, &label, style);
        col += label_width;

        // Separator
        if col < width {
            write_styled(frame, col, row, "|", CellStyle::new().dim());
            col += 1;
        }
    }

    // Fill rest of tab bar row
    let remaining = width.saturating_sub(col);
    if remaining > 0 {
        let fill = " ".repeat(remaining as usize);
        write_styled(frame, col, row, &fill, CellStyle::new());
    }
}

/// Render a placeholder for the view content area.
///
/// Individual view rendering will be migrated in FTUI-05.2 through FTUI-05.7.
fn render_view_placeholder(frame: &mut ftui::Frame, y: u16, width: u16, height: u16, view: View) {
    if height == 0 {
        return;
    }

    // Title line
    let title = format!("  {} view", view.name());
    let title_style = CellStyle::new().bold();
    write_styled(frame, 0, y, &title, title_style);
    // Fill rest of title
    let title_len = title.len() as u16;
    if title_len < width {
        let fill = " ".repeat((width - title_len) as usize);
        write_styled(frame, title_len, y, &fill, CellStyle::new());
    }

    // Placeholder body
    if height > 1 {
        let msg = format!(
            "  [FTUI migration in progress — {view} view not yet ported]",
            view = view.name(),
        );
        write_styled(frame, 0, y + 1, &msg, CellStyle::new().dim());
    }

    // Blank remaining rows
    for row in (y + 2)..y.saturating_add(height) {
        let blank = " ".repeat(width as usize);
        write_styled(frame, 0, row, &blank, CellStyle::new());
    }
}

/// Render the Home dashboard view.
///
/// Layout (rows from content_y):
///   Row 0:      Title — "WezTerm Automata" + aggregate health badge
///   Rows 1-2:   blank separator
///   Rows 3-8:   System status detail (watcher, db, wezterm, circuit)
///   Rows 9-10:  blank separator
///   Rows 11-14: Metrics snapshot (panes, events, unhandled, triage)
///   Remaining:  Quick help
fn render_home_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    health: Option<&HealthModel>,
    unhandled_count: usize,
    triage_count: usize,
) {
    if height == 0 {
        return;
    }

    let mut row = y;
    let max_row = y.saturating_add(height);

    // -- Title + aggregate health badge --
    let title = "  WezTerm Automata";
    write_styled(frame, 0, row, title, CellStyle::new().bold());

    let (badge, badge_style) = match health {
        None => ("  LOADING", CellStyle::new().dim()),
        Some(h) if h.watcher_label == "stopped" || h.db_label == "unavailable" => {
            ("  ERROR", CellStyle::new().bold())
        }
        Some(h) if h.circuit_label == "OPEN" => ("  WARNING", CellStyle::new().bold()),
        Some(_) => ("  OK", CellStyle::new().bold()),
    };
    let badge_col = title.len() as u16;
    write_styled(frame, badge_col, row, badge, badge_style);
    // Fill rest of title row
    let used = badge_col + badge.len() as u16;
    if used < width {
        let fill = " ".repeat((width - used) as usize);
        write_styled(frame, used, row, &fill, CellStyle::new());
    }

    row += 1;

    // Blank separator
    if row < max_row {
        let blank = " ".repeat(width as usize);
        write_styled(frame, 0, row, &blank, CellStyle::new());
        row += 1;
    }

    // -- System status section --
    if let Some(h) = health {
        let status_lines: &[(&str, &str, bool)] = &[
            ("  Watcher:        ", &h.watcher_label, h.watcher_label == "running"),
            ("  Database:       ", &h.db_label, h.db_label == "ok"),
            ("  WezTerm CLI:    ", &h.wezterm_label, h.wezterm_label == "ok"),
            ("  Circuit Breaker:", &h.circuit_label, h.circuit_label == "closed"),
        ];

        for &(label, value, ok) in status_lines {
            if row >= max_row {
                break;
            }
            write_styled(frame, 0, row, label, CellStyle::new());
            let val_col = label.len() as u16;
            let val_style = if ok {
                CellStyle::new()
            } else {
                CellStyle::new().bold()
            };
            write_styled(frame, val_col, row, &format!(" {value}"), val_style);
            // Fill rest
            let end = val_col + 1 + value.len() as u16;
            if end < width {
                let fill = " ".repeat((width - end) as usize);
                write_styled(frame, end, row, &fill, CellStyle::new());
            }
            row += 1;
        }
    } else if row < max_row {
        write_styled(frame, 0, row, "  Loading health data...", CellStyle::new().dim());
        let used = 24u16;
        if used < width {
            let fill = " ".repeat((width - used) as usize);
            write_styled(frame, used, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // Blank separator
    if row < max_row {
        let blank = " ".repeat(width as usize);
        write_styled(frame, 0, row, &blank, CellStyle::new());
        row += 1;
    }

    // -- Metrics section --
    if let Some(h) = health {
        let metrics: &[(&str, &str, bool)] = &[
            ("  Panes:          ", &h.pane_count, h.pane_count != "0"),
            ("  Events:         ", &h.event_count, true),
        ];
        for &(label, value, _ok) in metrics {
            if row >= max_row {
                break;
            }
            write_styled(frame, 0, row, label, CellStyle::new());
            let val_col = label.len() as u16;
            write_styled(frame, val_col, row, &format!(" {value}"), CellStyle::new());
            let end = val_col + 1 + value.len() as u16;
            if end < width {
                let fill = " ".repeat((width - end) as usize);
                write_styled(frame, end, row, &fill, CellStyle::new());
            }
            row += 1;
        }
    }

    // Unhandled events
    if row < max_row {
        let label = "  Unhandled:      ";
        let value = unhandled_count.to_string();
        write_styled(frame, 0, row, label, CellStyle::new());
        let val_col = label.len() as u16;
        let val_style = if unhandled_count > 0 {
            CellStyle::new().bold()
        } else {
            CellStyle::new()
        };
        write_styled(frame, val_col, row, &format!(" {value}"), val_style);
        let end = val_col + 1 + value.len() as u16;
        if end < width {
            let fill = " ".repeat((width - end) as usize);
            write_styled(frame, end, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // Triage items
    if row < max_row {
        let label = "  Triage Items:   ";
        let value = triage_count.to_string();
        write_styled(frame, 0, row, label, CellStyle::new());
        let val_col = label.len() as u16;
        let val_style = if triage_count > 0 {
            CellStyle::new().bold()
        } else {
            CellStyle::new()
        };
        write_styled(frame, val_col, row, &format!(" {value}"), val_style);
        let end = val_col + 1 + value.len() as u16;
        if end < width {
            let fill = " ".repeat((width - end) as usize);
            write_styled(frame, end, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // Blank separator
    if row < max_row {
        let blank = " ".repeat(width as usize);
        write_styled(frame, 0, row, &blank, CellStyle::new());
        row += 1;
    }

    // -- Quick help --
    if row < max_row {
        write_styled(frame, 0, row, "  Navigation:", CellStyle::new().bold());
        let rest = width.saturating_sub(14);
        if rest > 0 {
            let fill = " ".repeat(rest as usize);
            write_styled(frame, 14, row, &fill, CellStyle::new());
        }
        row += 1;
    }
    if row < max_row {
        let help = "    Tab/Shift+Tab: Switch views   q: Quit   r: Refresh   ?: Help";
        write_styled(frame, 0, row, help, CellStyle::new().dim());
        let help_len = help.len() as u16;
        if help_len < width {
            let fill = " ".repeat((width - help_len) as usize);
            write_styled(frame, help_len, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // Fill remaining rows
    let blank = " ".repeat(width as usize);
    while row < max_row {
        write_styled(frame, 0, row, &blank, CellStyle::new());
        row += 1;
    }
}

/// Render the Panes view.
///
/// Two-panel layout:
///   Left 2/3: Pane list with column headers, selection, and filter indicator.
///   Right 1/3: Detail panel for the selected pane.
fn render_panes_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    panes: &[PaneRow],
    filtered_indices: &[usize],
    selected: usize,
    domain_filter: Option<&str>,
) {
    if height == 0 {
        return;
    }

    let max_row = y.saturating_add(height);
    let list_width = (width * 2 / 3).max(20);
    let detail_x = list_width;
    let detail_width = width.saturating_sub(list_width);

    let mut row = y;

    // -- Header: count and filter status --
    let header = format!(
        "  Panes ({}/{})  domain={}",
        filtered_indices.len(),
        panes.len(),
        domain_filter.unwrap_or("all"),
    );
    write_styled(frame, 0, row, &header, CellStyle::new().bold());
    let hlen = header.len() as u16;
    if hlen < list_width {
        let fill = " ".repeat((list_width - hlen) as usize);
        write_styled(frame, hlen, row, &fill, CellStyle::new());
    }
    row += 1;

    // -- Column headers --
    if row < max_row {
        let col_header = format!(
            "  {:>3} {:8} {:12} {:>9}  {}",
            "ID", "Agent", "State", "Unhandled", "Title"
        );
        write_styled(frame, 0, row, &col_header, CellStyle::new().dim());
        let clen = col_header.len() as u16;
        if clen < list_width {
            let fill = " ".repeat((list_width - clen) as usize);
            write_styled(frame, clen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // -- Pane rows --
    if filtered_indices.is_empty() && row < max_row {
        write_styled(
            frame,
            0,
            row,
            "  No panes match current filters.",
            CellStyle::new().dim(),
        );
        let msg_len = 34u16;
        if msg_len < list_width {
            let fill = " ".repeat((list_width - msg_len) as usize);
            write_styled(frame, msg_len, row, &fill, CellStyle::new());
        }
        row += 1;
    } else {
        for (pos, &pane_idx) in filtered_indices.iter().enumerate() {
            if row >= max_row {
                break;
            }
            let pane = &panes[pane_idx];
            let line = format!(
                "  {:>3} {:8} {:12} {:>9}  {}",
                pane.pane_id,
                truncate_str(&pane.agent_label, 8),
                truncate_str(&pane.state_label, 12),
                pane.unhandled_badge,
                truncate_str(&pane.title, 24),
            );
            let style = if pos == selected {
                CellStyle::new().bold().reverse()
            } else if !pane.unhandled_badge.is_empty() {
                CellStyle::new().bold()
            } else {
                CellStyle::new()
            };
            write_styled(frame, 0, row, &line, style);
            let llen = line.len() as u16;
            if llen < list_width {
                let fill = " ".repeat((list_width - llen) as usize);
                write_styled(frame, llen, row, &fill, style);
            }
            row += 1;
        }
    }

    // Fill remaining list area
    let blank_list = " ".repeat(list_width as usize);
    while row < max_row {
        write_styled(frame, 0, row, &blank_list, CellStyle::new());
        row += 1;
    }

    // -- Detail panel (right side) --
    let selected_pane = filtered_indices
        .get(selected)
        .and_then(|&idx| panes.get(idx));

    let mut drow = y;

    // Detail header
    write_styled(
        frame,
        detail_x,
        drow,
        " Pane Details",
        CellStyle::new().bold(),
    );
    let dhlen = 13u16;
    if dhlen < detail_width {
        let fill = " ".repeat((detail_width - dhlen) as usize);
        write_styled(frame, detail_x + dhlen, drow, &fill, CellStyle::new());
    }
    drow += 1;

    if let Some(pane) = selected_pane {
        let detail_lines: Vec<String> = vec![
            format!(" ID:       {}", pane.pane_id),
            format!(" Title:    {}", pane.title),
            format!(" Domain:   {}", pane.domain),
            format!(" Agent:    {}", pane.agent_label),
            format!(" State:    {}", pane.state_label),
            format!(" CWD:      {}", pane.cwd),
            format!(
                " Unhandled:{}",
                if pane.unhandled_badge.is_empty() {
                    " 0".to_string()
                } else {
                    format!(" {}", pane.unhandled_badge)
                }
            ),
            String::new(),
            " Keys: j/k=nav d=domain Esc=clear".to_string(),
        ];

        for line in &detail_lines {
            if drow >= max_row {
                break;
            }
            write_styled(frame, detail_x, drow, line, CellStyle::new());
            let llen = line.len() as u16;
            if llen < detail_width {
                let fill = " ".repeat((detail_width - llen) as usize);
                write_styled(frame, detail_x + llen, drow, &fill, CellStyle::new());
            }
            drow += 1;
        }
    } else if drow < max_row {
        write_styled(
            frame,
            detail_x,
            drow,
            " No pane selected.",
            CellStyle::new().dim(),
        );
        let msg_len = 19u16;
        if msg_len < detail_width {
            let fill = " ".repeat((detail_width - msg_len) as usize);
            write_styled(frame, detail_x + msg_len, drow, &fill, CellStyle::new());
        }
        drow += 1;
    }

    // Fill remaining detail area
    let blank_detail = " ".repeat(detail_width as usize);
    while drow < max_row {
        write_styled(frame, detail_x, drow, &blank_detail, CellStyle::new());
        drow += 1;
    }
}

/// Render the Search view.
///
/// Layout:
///   Row 0:    Search input bar with cursor/prompt
///   Row 1:    Separator / status
///   Rows 2+:  Two-panel (results list left 55%, detail right 45%) or empty message
#[allow(clippy::too_many_arguments)]
fn render_search_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    query: &str,
    last_query: &str,
    results: &[SearchRow],
    selected: usize,
) {
    if height == 0 {
        return;
    }

    let max_row = y.saturating_add(height);
    let mut row = y;
    let blank_line = " ".repeat(width as usize);

    // -- Search input bar --
    let prompt = if query.is_empty() {
        "Search (FTS5) — type query, Enter to search"
    } else {
        "Search (FTS5) — Enter to search, Esc to clear"
    };
    let input_line = format!("  {prompt}: {query}_");
    write_styled(frame, 0, row, &input_line, CellStyle::new().bold());
    let ilen = input_line.len() as u16;
    if ilen < width {
        let fill = " ".repeat((width - ilen) as usize);
        write_styled(frame, ilen, row, &fill, CellStyle::new());
    }
    row += 1;

    // -- Status / separator --
    if row < max_row {
        let status = if results.is_empty() {
            if last_query.is_empty() {
                "  Type a query + Enter to search.".to_string()
            } else {
                format!("  No results for '{}'.", truncate_str(last_query, 30))
            }
        } else {
            format!(
                "  {} matches for '{}'",
                results.len(),
                truncate_str(last_query, 30),
            )
        };
        write_styled(frame, 0, row, &status, CellStyle::new().dim());
        let slen = status.len() as u16;
        if slen < width {
            let fill = " ".repeat((width - slen) as usize);
            write_styled(frame, slen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // -- Empty state --
    if results.is_empty() {
        while row < max_row {
            write_styled(frame, 0, row, &blank_line, CellStyle::new());
            row += 1;
        }
        return;
    }

    // -- Two-panel: results list (left 55%) + detail (right 45%) --
    let list_width = (width * 55 / 100).max(20);
    let detail_x = list_width;
    let detail_width = width.saturating_sub(list_width);

    let clamped_sel = selected.min(results.len().saturating_sub(1));
    let results_start_row = row;

    // Column header
    if row < max_row {
        let header = format!("  {:>4} {:>6}  {}", "Pane", "Rank", "Snippet");
        write_styled(frame, 0, row, &header, CellStyle::new().dim());
        let hlen = header.len() as u16;
        if hlen < list_width {
            let fill = " ".repeat((list_width - hlen) as usize);
            write_styled(frame, hlen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // Result rows
    let snippet_max = list_width.saturating_sub(16).max(5) as usize;
    for (pos, result) in results.iter().enumerate() {
        if row >= max_row {
            break;
        }
        let line = format!(
            "  P{:>3} {:>6}  {}",
            result.pane_id,
            result.rank_label,
            truncate_str(&result.snippet, snippet_max),
        );
        let style = if pos == clamped_sel {
            CellStyle::new().bold().reverse()
        } else {
            CellStyle::new()
        };
        write_styled(frame, 0, row, &line, style);
        let llen = line.len() as u16;
        if llen < list_width {
            let fill = " ".repeat((list_width - llen) as usize);
            write_styled(frame, llen, row, &fill, style);
        }
        row += 1;
    }

    // Fill remaining list area
    let blank_list = " ".repeat(list_width as usize);
    while row < max_row {
        write_styled(frame, 0, row, &blank_list, CellStyle::new());
        row += 1;
    }

    // -- Detail panel (right side) --
    let mut drow = results_start_row;

    // Detail header
    write_styled(frame, detail_x, drow, " Match Context", CellStyle::new().bold());
    let dhlen = 14u16;
    if dhlen < detail_width {
        let fill = " ".repeat((detail_width - dhlen) as usize);
        write_styled(frame, detail_x + dhlen, drow, &fill, CellStyle::new());
    }
    drow += 1;

    if let Some(result) = results.get(clamped_sel) {
        let detail_lines: Vec<String> = vec![
            format!(" Pane:     P{}", result.pane_id),
            format!(" Rank:     {}", result.rank_label),
            format!(" Captured: {}", result.timestamp),
            String::new(),
            " Snippet:".to_string(),
            format!(
                " {}",
                truncate_str(&result.snippet, detail_width.saturating_sub(2) as usize)
            ),
            String::new(),
            " Keys: Down/Up=nav Enter=search Esc=clear".to_string(),
        ];

        for line in &detail_lines {
            if drow >= max_row {
                break;
            }
            write_styled(frame, detail_x, drow, line, CellStyle::new());
            let llen = line.len() as u16;
            if llen < detail_width {
                let fill = " ".repeat((detail_width - llen) as usize);
                write_styled(frame, detail_x + llen, drow, &fill, CellStyle::new());
            }
            drow += 1;
        }
    }

    // Fill remaining detail area
    let blank_detail = " ".repeat(detail_width as usize);
    while drow < max_row {
        write_styled(frame, detail_x, drow, &blank_detail, CellStyle::new());
        drow += 1;
    }
}

/// Render the Help view — static keybinding reference.
fn render_help_view(frame: &mut ftui::Frame, y: u16, width: u16, height: u16) {
    if height == 0 {
        return;
    }

    let max_row = y.saturating_add(height);
    let mut row = y;
    let blank_line = " ".repeat(width as usize);

    let help_lines: &[(&str, bool)] = &[
        ("  WezTerm Automata TUI", true), // bold
        ("", false),
        ("  Global Keybindings:", true),
        ("    q          Quit", false),
        ("    ?          Show this help", false),
        ("    r          Refresh current view", false),
        ("    Tab        Next view", false),
        ("    Shift+Tab  Previous view", false),
        ("    1-7        Jump to view by number", false),
        ("", false),
        ("  List Navigation:", true),
        ("    j / Down   Move selection down", false),
        ("    k / Up     Move selection up", false),
        ("    Enter      Run primary action (triage)", false),
        ("    1-9        Run action by number (triage)", false),
        ("    m          Mute selected event (triage)", false),
        ("    d          Cycle domain filter (panes)", false),
        ("    Esc        Clear filter / reset", false),
        ("", false),
        ("  Search:", true),
        ("    Type text  Build query", false),
        ("    Enter      Execute search", false),
        ("    Down/Up    Navigate results", false),
        ("    Esc        Clear query and results", false),
        ("", false),
        ("  Views:", true),
        ("    1. Home    System overview and health", false),
        ("    2. Panes   List all WezTerm panes", false),
        ("    3. Events  Recent detection events", false),
        ("    4. Triage  Prioritized issues + actions", false),
        ("    5. History Audit action timeline", false),
        ("    6. Search  Full-text search", false),
        ("    7. Help    This screen", false),
    ];

    for &(line, bold) in help_lines {
        if row >= max_row {
            break;
        }
        let style = if bold {
            CellStyle::new().bold()
        } else {
            CellStyle::new()
        };
        write_styled(frame, 0, row, line, style);
        let llen = line.len() as u16;
        if llen < width {
            let fill = " ".repeat((width - llen) as usize);
            write_styled(frame, llen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // Fill remaining rows
    while row < max_row {
        write_styled(frame, 0, row, &blank_line, CellStyle::new());
        row += 1;
    }
}

/// Render the Events view.
///
/// Two-panel layout:
///   Left 60%: Event list with filter header, selection, and severity indicators.
///   Right 40%: Detail panel for the selected event.
fn render_events_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    events_state: &EventsViewState,
    filtered_indices: &[usize],
    selected: usize,
) {
    if height == 0 {
        return;
    }

    let max_row = y.saturating_add(height);
    let list_width = (width * 3 / 5).max(20); // 60%
    let detail_x = list_width;
    let detail_width = width.saturating_sub(list_width);

    let mut row = y;

    // -- Header: count and filter status --
    let header = format!(
        "  Events ({}/{})  unhandled_only={}  pane/rule='{}'",
        filtered_indices.len(),
        events_state.items.len(),
        events_state.unhandled_only,
        events_state.pane_filter,
    );
    write_styled(frame, 0, row, &header, CellStyle::new().bold());
    let hlen = header.len() as u16;
    if hlen < list_width {
        let fill = " ".repeat((list_width - hlen) as usize);
        write_styled(frame, hlen, row, &fill, CellStyle::new());
    }
    row += 1;

    // -- Column headers --
    if row < max_row {
        let col_header = format!(
            "  {:8}  {:>4}  {:28}  {}",
            "sev", "pane", "rule", "status"
        );
        write_styled(frame, 0, row, &col_header, CellStyle::new().dim());
        let clen = col_header.len() as u16;
        if clen < list_width {
            let fill = " ".repeat((list_width - clen) as usize);
            write_styled(frame, clen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // -- Event rows --
    if filtered_indices.is_empty() && row < max_row {
        let msg = if events_state.items.is_empty() {
            "  No events yet. Watcher will capture pattern matches here."
        } else {
            "  No events match the current filters."
        };
        write_styled(frame, 0, row, msg, CellStyle::new().dim());
        let msg_len = msg.len() as u16;
        if msg_len < list_width {
            let fill = " ".repeat((list_width - msg_len) as usize);
            write_styled(frame, msg_len, row, &fill, CellStyle::new());
        }
        row += 1;
    } else {
        for (pos, &event_idx) in filtered_indices.iter().enumerate() {
            if row >= max_row {
                break;
            }
            let event = &events_state.items[event_idx];
            let handled_marker = if event.handled { " " } else { "*" };
            let line = format!(
                "  [{:8}] {:>4}  {:28} {}",
                truncate_str(&event.severity, 8),
                event.pane_id,
                truncate_str(&event.rule_id, 28),
                handled_marker,
            );
            let style = if pos == selected {
                CellStyle::new().bold().reverse()
            } else if !event.handled {
                CellStyle::new().bold()
            } else {
                CellStyle::new()
            };
            write_styled(frame, 0, row, &line, style);
            let llen = line.len() as u16;
            if llen < list_width {
                let fill = " ".repeat((list_width - llen) as usize);
                write_styled(frame, llen, row, &fill, style);
            }
            row += 1;
        }
    }

    // Fill remaining list area
    let blank_list = " ".repeat(list_width as usize);
    while row < max_row {
        write_styled(frame, 0, row, &blank_list, CellStyle::new());
        row += 1;
    }

    // -- Detail panel (right side) --
    let selected_event = filtered_indices
        .get(selected)
        .and_then(|&idx| events_state.items.get(idx));
    let selected_row = filtered_indices
        .get(selected)
        .and_then(|&idx| events_state.rows.get(idx));

    let mut drow = y;

    // Detail header
    write_styled(
        frame,
        detail_x,
        drow,
        " Event Details",
        CellStyle::new().bold(),
    );
    let dhlen = 14u16;
    if dhlen < detail_width {
        let fill = " ".repeat((detail_width - dhlen) as usize);
        write_styled(frame, detail_x + dhlen, drow, &fill, CellStyle::new());
    }
    drow += 1;

    if let (Some(event), Some(erow)) = (selected_event, selected_row) {
        let triage_display = if erow.triage_label.is_empty() {
            "unset"
        } else {
            &erow.triage_label
        };
        let labels_display = if erow.labels_label.is_empty() {
            "none".to_string()
        } else {
            erow.labels_label.clone()
        };
        let note_display = if erow.note_preview.is_empty() {
            "none".to_string()
        } else {
            erow.note_preview.clone()
        };
        let detail_lines: Vec<String> = vec![
            format!(" ID:       {}", event.id),
            format!(" Pane:     {}", event.pane_id),
            format!(" Severity: {}", erow.severity_label),
            format!(" Status:   {}", erow.handled_label),
            format!(" Triage:   {triage_display}"),
            format!(" Labels:   {labels_display}"),
            format!(" Note:     {note_display}"),
            String::new(),
            " Rule:".to_string(),
            format!("   {}", event.rule_id),
            String::new(),
            " Match:".to_string(),
            format!("   {}", truncate_str(&erow.message, 40)),
            String::new(),
            format!(" Captured: {}", erow.timestamp),
            String::new(),
            " Keys: j/k=nav u=unhandled 0-9=pane Esc=clear".to_string(),
        ];

        for line in &detail_lines {
            if drow >= max_row {
                break;
            }
            write_styled(frame, detail_x, drow, line, CellStyle::new());
            let llen = line.len() as u16;
            if llen < detail_width {
                let fill = " ".repeat((detail_width - llen) as usize);
                write_styled(frame, detail_x + llen, drow, &fill, CellStyle::new());
            }
            drow += 1;
        }
    } else if drow < max_row {
        write_styled(
            frame,
            detail_x,
            drow,
            " No event selected.",
            CellStyle::new().dim(),
        );
        let msg_len = 20u16;
        if msg_len < detail_width {
            let fill = " ".repeat((detail_width - msg_len) as usize);
            write_styled(frame, detail_x + msg_len, drow, &fill, CellStyle::new());
        }
        drow += 1;
    }

    // Fill remaining detail area
    let blank_detail = " ".repeat(detail_width as usize);
    while drow < max_row {
        write_styled(frame, detail_x, drow, &blank_detail, CellStyle::new());
        drow += 1;
    }
}

/// Render the Triage view.
///
/// Vertical layout:
///   Block 1 (50% or fill): Triage item list with severity indicators and selection.
///   Block 2 (25%, optional): Active workflow progress panel (when workflows exist).
///   Block 3 (6 rows fixed): Details + action affordances for the selected item.
#[allow(clippy::too_many_arguments)]
fn render_triage_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    triage_items: &[TriageRow],
    selected: usize,
    workflows: &[WorkflowRow],
    expanded: Option<usize>,
) {
    if height == 0 {
        return;
    }

    let max_row = y.saturating_add(height);
    let blank_line = " ".repeat(width as usize);

    // Calculate layout: triage list, optional workflow panel, detail panel (6 rows).
    let has_workflows = !workflows.is_empty();
    let detail_height: u16 = 6;
    let workflow_height: u16 = if has_workflows {
        (height / 4).max(4)
    } else {
        0
    };
    let list_height = height
        .saturating_sub(detail_height)
        .saturating_sub(workflow_height);

    // -- Triage list section --
    let mut row = y;
    let list_end = y.saturating_add(list_height);

    // Header
    let header = if triage_items.is_empty() && !has_workflows {
        "  Triage (prioritized) — all clear".to_string()
    } else {
        format!("  Triage (prioritized) — {} items", triage_items.len())
    };
    write_styled(frame, 0, row, &header, CellStyle::new().bold());
    let hlen = header.len() as u16;
    if hlen < width {
        let fill = " ".repeat((width - hlen) as usize);
        write_styled(frame, hlen, row, &fill, CellStyle::new());
    }
    row += 1;

    // Empty state
    if triage_items.is_empty() && !has_workflows {
        if row < list_end {
            let msg = "  All clear. No items need attention.";
            write_styled(frame, 0, row, msg, CellStyle::new().dim());
            let mlen = msg.len() as u16;
            if mlen < width {
                let fill = " ".repeat((width - mlen) as usize);
                write_styled(frame, mlen, row, &fill, CellStyle::new());
            }
            row += 1;
        }
    } else {
        // Column header
        if row < list_end {
            let col_header = format!(
                "  {:8}  {:8}  {}",
                "severity", "section", "title"
            );
            write_styled(frame, 0, row, &col_header, CellStyle::new().dim());
            let clen = col_header.len() as u16;
            if clen < width {
                let fill = " ".repeat((width - clen) as usize);
                write_styled(frame, clen, row, &fill, CellStyle::new());
            }
            row += 1;
        }

        // Triage item rows
        let clamped_sel = selected.min(triage_items.len().saturating_sub(1));
        for (pos, item) in triage_items.iter().enumerate() {
            if row >= list_end {
                break;
            }
            let line = format!(
                "  [{:7}] {:8} | {}",
                truncate_str(&item.severity_label, 7),
                truncate_str(&item.section, 8),
                truncate_str(&item.title, 80),
            );
            let style = if pos == clamped_sel {
                CellStyle::new().bold().reverse()
            } else {
                CellStyle::new()
            };
            write_styled(frame, 0, row, &line, style);
            let llen = line.len() as u16;
            if llen < width {
                let fill = " ".repeat((width - llen) as usize);
                write_styled(frame, llen, row, &fill, style);
            }
            row += 1;
        }
    }

    // Fill remaining list area
    while row < list_end {
        write_styled(frame, 0, row, &blank_line, CellStyle::new());
        row += 1;
    }

    // -- Workflow progress panel (optional) --
    if has_workflows {
        let wf_end = row.saturating_add(workflow_height);

        // Workflow header
        let wf_header = format!("  Active Workflows ({})", workflows.len());
        write_styled(frame, 0, row, &wf_header, CellStyle::new().bold());
        let whlen = wf_header.len() as u16;
        if whlen < width {
            let fill = " ".repeat((width - whlen) as usize);
            write_styled(frame, whlen, row, &fill, CellStyle::new());
        }
        row += 1;

        for (i, wf) in workflows.iter().enumerate() {
            if row >= wf_end {
                break;
            }
            let is_expanded = expanded == Some(i);
            let marker = if is_expanded { "v" } else { ">" };
            let line = format!(
                "  {} {:20} P{:>3} {:8} {}",
                marker,
                truncate_str(&wf.name, 20),
                wf.pane_id,
                truncate_str(&wf.status_label, 8),
                wf.progress_label,
            );
            write_styled(frame, 0, row, &line, CellStyle::new());
            let llen = line.len() as u16;
            if llen < width {
                let fill = " ".repeat((width - llen) as usize);
                write_styled(frame, llen, row, &fill, CellStyle::new());
            }
            row += 1;

            // Expanded detail
            if is_expanded {
                if row < wf_end {
                    let id_line = format!("    ID: {}", wf.id);
                    write_styled(frame, 0, row, &id_line, CellStyle::new().dim());
                    let ilen = id_line.len() as u16;
                    if ilen < width {
                        let fill = " ".repeat((width - ilen) as usize);
                        write_styled(frame, ilen, row, &fill, CellStyle::new());
                    }
                    row += 1;
                }
                if row < wf_end {
                    let step_line = format!(
                        "    Step {} | started {}",
                        wf.progress_label, wf.started_at,
                    );
                    write_styled(frame, 0, row, &step_line, CellStyle::new().dim());
                    let slen = step_line.len() as u16;
                    if slen < width {
                        let fill = " ".repeat((width - slen) as usize);
                        write_styled(frame, slen, row, &fill, CellStyle::new());
                    }
                    row += 1;
                }
                if let Some(ref error) = wf.error {
                    if row < wf_end {
                        let err_line = format!(
                            "    ERROR: {}",
                            truncate_str(error, 60),
                        );
                        write_styled(frame, 0, row, &err_line, CellStyle::new().bold());
                        let elen = err_line.len() as u16;
                        if elen < width {
                            let fill = " ".repeat((width - elen) as usize);
                            write_styled(frame, elen, row, &fill, CellStyle::new());
                        }
                        row += 1;
                    }
                }
            }
        }

        // Fill remaining workflow area
        while row < wf_end {
            write_styled(frame, 0, row, &blank_line, CellStyle::new());
            row += 1;
        }
    }

    // -- Details + Actions panel --
    let detail_header = "  Details / Actions (Enter or 1-9 to run, m to mute, e to expand)";
    if row < max_row {
        write_styled(frame, 0, row, detail_header, CellStyle::new().bold());
        let dhlen = detail_header.len() as u16;
        if dhlen < width {
            let fill = " ".repeat((width - dhlen) as usize);
            write_styled(frame, dhlen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    let clamped_sel = selected.min(triage_items.len().saturating_sub(1));
    if let Some(item) = triage_items.get(clamped_sel) {
        // Detail text
        if !item.detail.is_empty() && row < max_row {
            let detail_line = format!("  {}", truncate_str(&item.detail, width.saturating_sub(4) as usize));
            write_styled(frame, 0, row, &detail_line, CellStyle::new());
            let dlen = detail_line.len() as u16;
            if dlen < width {
                let fill = " ".repeat((width - dlen) as usize);
                write_styled(frame, dlen, row, &fill, CellStyle::new());
            }
            row += 1;
        }

        // Actions
        if !item.action_labels.is_empty() && row < max_row {
            let actions_header = "  Actions:";
            write_styled(frame, 0, row, actions_header, CellStyle::new().bold());
            let ahlen = actions_header.len() as u16;
            if ahlen < width {
                let fill = " ".repeat((width - ahlen) as usize);
                write_styled(frame, ahlen, row, &fill, CellStyle::new());
            }
            row += 1;

            for (idx, label) in item.action_labels.iter().enumerate() {
                if row >= max_row {
                    break;
                }
                let cmd_display = item
                    .action_commands
                    .get(idx)
                    .map(|c| truncate_str(c, 40))
                    .unwrap_or_default();
                let action_line = format!("    {}. {} ({})", idx + 1, label, cmd_display);
                write_styled(frame, 0, row, &action_line, CellStyle::new());
                let alen = action_line.len() as u16;
                if alen < width {
                    let fill = " ".repeat((width - alen) as usize);
                    write_styled(frame, alen, row, &fill, CellStyle::new());
                }
                row += 1;
            }
        }

        // Cross-reference IDs
        if row < max_row && (!item.event_id.is_empty() || !item.pane_id.is_empty()) {
            let ref_line = format!("  event={} pane={} wf={}", item.event_id, item.pane_id, item.workflow_id);
            write_styled(frame, 0, row, &ref_line, CellStyle::new().dim());
            let rlen = ref_line.len() as u16;
            if rlen < width {
                let fill = " ".repeat((width - rlen) as usize);
                write_styled(frame, rlen, row, &fill, CellStyle::new());
            }
            row += 1;
        }
    }

    // Fill remaining rows
    while row < max_row {
        write_styled(frame, 0, row, &blank_line, CellStyle::new());
        row += 1;
    }
}

/// Truncate a string for display.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 2 {
        format!("{}..", &s[..max - 2])
    } else {
        s[..max].to_string()
    }
}

/// Render the status footer.
fn render_footer(frame: &mut ftui::Frame, row: u16, width: u16, view: View, error: Option<&str>) {
    let left = if let Some(err) = error {
        format!(" ERR: {err}")
    } else {
        format!(" {}", view.name())
    };

    let right = " q:quit  Tab:nav  ?:help  r:refresh ";
    let left_len = left.len() as u16;
    let right_len = right.len() as u16;

    let style = if error.is_some() {
        CellStyle::new().bold()
    } else {
        CellStyle::new().reverse()
    };

    write_styled(frame, 0, row, &left, style);

    // Fill middle
    let mid = width.saturating_sub(left_len + right_len);
    if mid > 0 {
        let fill = " ".repeat(mid as usize);
        write_styled(frame, left_len, row, &fill, style);
    }

    if left_len + mid + right_len <= width {
        write_styled(frame, left_len + mid, row, right, style);
    }
}

/// Compact style hint for the low-level `write_styled` helper.
///
/// We avoid using `ftui::Style` (high-level, designed for stylesheet-driven
/// rendering) in the cell-level writer because the facade's `StyleFlags`
/// (u16, from ftui-style) differs from the render cell's internal `StyleFlags`
/// (u8, bitflags in ftui-render).  Instead we track a small bitmask directly.
#[derive(Clone, Copy, Default)]
struct CellStyle {
    bold: bool,
    dim: bool,
    reverse: bool,
}

impl CellStyle {
    const fn new() -> Self {
        Self {
            bold: false,
            dim: false,
            reverse: false,
        }
    }

    const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    /// Convert to the render-cell `StyleFlags`.
    fn to_cell_flags(self) -> ftui::render::cell::StyleFlags {
        let mut flags = ftui::render::cell::StyleFlags::empty();
        if self.bold {
            flags |= ftui::render::cell::StyleFlags::BOLD;
        }
        if self.dim {
            flags |= ftui::render::cell::StyleFlags::DIM;
        }
        if self.reverse {
            flags |= ftui::render::cell::StyleFlags::REVERSE;
        }
        flags
    }
}

/// Write a styled string into the frame buffer at (col, row).
///
/// Characters that would overflow the frame width are silently clipped.
fn write_styled(frame: &mut ftui::Frame, col: u16, row: u16, text: &str, style: CellStyle) {
    let buf = &mut frame.buffer;
    let w = buf.width();
    let h = buf.height();

    if row >= h {
        return;
    }

    let flags = style.to_cell_flags();

    let mut x = col;
    for ch in text.chars() {
        if x >= w {
            break;
        }
        if let Some(cell) = buf.get_mut(x, row) {
            cell.content = ftui::render::cell::CellContent::from_char(ch);
            cell.attrs = ftui::CellAttrs::new(flags, 0);
        }
        x += 1;
    }
}

// ---------------------------------------------------------------------------
// Public API — matches the ratatui backend's exports
// ---------------------------------------------------------------------------

/// FrankenTUI application shell.
pub struct App<Q: QueryClient> {
    _query: Q,
    _config: AppConfig,
}

/// Run the TUI using the FrankenTUI backend.
///
/// Constructs a `WaModel` and hands it to the ftui runtime via
/// `App::fullscreen(model).run()`.
pub fn run_tui<Q: QueryClient + Send + Sync + 'static>(
    query: Q,
    config: AppConfig,
) -> Result<(), crate::Error> {
    let query: Arc<dyn QueryClient + Send + Sync> = Arc::new(query);
    let model = WaModel::new(query, config);

    ftui::App::fullscreen(model)
        .run()
        .map_err(|e| crate::Error::Runtime(format!("ftui runtime error: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::CircuitBreakerStatus;
    use crate::tui::query::{
        EventFilters, EventView, HealthStatus, PaneView, QueryError,
        SearchResultView, TriageItemView, WorkflowProgressView,
    };

    // -- Mock QueryClient --

    struct MockQuery {
        healthy: bool,
        pane_count: usize,
        unhandled_per_pane: u32,
        triage_count: usize,
        triage_items_detailed: Vec<TriageItemView>,
        workflows_data: Vec<WorkflowProgressView>,
        search_results: Vec<SearchResultView>,
        events: Vec<EventView>,
    }

    impl MockQuery {
        fn healthy() -> Self {
            Self {
                healthy: true,
                pane_count: 3,
                unhandled_per_pane: 2,
                triage_count: 1,
                triage_items_detailed: Vec::new(),
                workflows_data: Vec::new(),
                search_results: Vec::new(),
                events: vec![],
            }
        }

        fn degraded() -> Self {
            Self {
                healthy: false,
                pane_count: 0,
                unhandled_per_pane: 0,
                triage_count: 0,
                triage_items_detailed: Vec::new(),
                workflows_data: Vec::new(),
                search_results: Vec::new(),
                events: vec![],
            }
        }

        fn with_events() -> Self {
            Self {
                healthy: true,
                pane_count: 3,
                unhandled_per_pane: 2,
                triage_count: 1,
                triage_items_detailed: Vec::new(),
                workflows_data: Vec::new(),
                search_results: Vec::new(),
                events: vec![
                    EventView {
                        id: 1,
                        rule_id: "rate_limit_detected".to_string(),
                        pane_id: 42,
                        severity: "warning".to_string(),
                        message: "Rate limit exceeded".to_string(),
                        timestamp: 1_700_000_000_000,
                        handled: false,
                        triage_state: Some("escalated".to_string()),
                        labels: vec!["api".to_string()],
                        note: Some("Check throttle config".to_string()),
                    },
                    EventView {
                        id: 2,
                        rule_id: "error_detected".to_string(),
                        pane_id: 7,
                        severity: "error".to_string(),
                        message: "Fatal error in module".to_string(),
                        timestamp: 1_700_000_060_000,
                        handled: true,
                        triage_state: None,
                        labels: vec![],
                        note: None,
                    },
                    EventView {
                        id: 3,
                        rule_id: "pattern_match".to_string(),
                        pane_id: 42,
                        severity: "info".to_string(),
                        message: "Pattern matched".to_string(),
                        timestamp: 1_700_000_120_000,
                        handled: false,
                        triage_state: None,
                        labels: vec![],
                        note: None,
                    },
                ],
            }
        }

        fn with_search_results(mut self, results: Vec<SearchResultView>) -> Self {
            self.search_results = results;
            self
        }

        fn with_triage() -> Self {
            use crate::tui::query::TriageAction;
            Self {
                healthy: true,
                pane_count: 3,
                unhandled_per_pane: 2,
                triage_count: 0, // overridden by triage_items_detailed
                triage_items_detailed: vec![
                    TriageItemView {
                        section: "events".to_string(),
                        severity: "error".to_string(),
                        title: "Fatal crash in pane 7".to_string(),
                        detail: "Process exited with signal 11 (SIGSEGV)".to_string(),
                        actions: vec![
                            TriageAction {
                                label: "Restart".to_string(),
                                command: "wa pane restart 7".to_string(),
                            },
                            TriageAction {
                                label: "Investigate".to_string(),
                                command: "wa events show --pane 7".to_string(),
                            },
                        ],
                        event_id: Some(101),
                        pane_id: Some(7),
                        workflow_id: None,
                    },
                    TriageItemView {
                        section: "health".to_string(),
                        severity: "warning".to_string(),
                        title: "Rate limit approaching on pane 42".to_string(),
                        detail: "80% of rate limit consumed".to_string(),
                        actions: vec![TriageAction {
                            label: "Throttle".to_string(),
                            command: "wa rules throttle 42".to_string(),
                        }],
                        event_id: Some(102),
                        pane_id: Some(42),
                        workflow_id: Some("wf-abc".to_string()),
                    },
                    TriageItemView {
                        section: "workflow".to_string(),
                        severity: "info".to_string(),
                        title: "Workflow deploy-prod completed".to_string(),
                        detail: "All 5 steps finished successfully".to_string(),
                        actions: vec![],
                        event_id: None,
                        pane_id: None,
                        workflow_id: Some("wf-xyz".to_string()),
                    },
                ],
                workflows_data: vec![WorkflowProgressView {
                    id: "wf-abc".to_string(),
                    workflow_name: "rate-limit-handler".to_string(),
                    pane_id: 42,
                    current_step: 2,
                    total_steps: 4,
                    status: "running".to_string(),
                    error: None,
                    started_at: 1_700_000_000_000,
                    updated_at: 1_700_000_060_000,
                }],
                search_results: Vec::new(),
                events: vec![],
            }
        }
    }

    impl QueryClient for MockQuery {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok((0..self.pane_count)
                .map(|i| PaneView {
                    pane_id: i as u64,
                    title: format!("pane-{i}"),
                    domain: "local".to_string(),
                    cwd: None,
                    is_excluded: false,
                    agent_type: None,
                    pane_state: "PromptActive".to_string(),
                    last_activity_ts: None,
                    unhandled_event_count: self.unhandled_per_pane,
                })
                .collect())
        }

        fn list_events(&self, _: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            Ok(self.events.clone())
        }

        fn list_triage_items(&self) -> Result<Vec<TriageItemView>, QueryError> {
            if !self.triage_items_detailed.is_empty() {
                return Ok(self.triage_items_detailed.clone());
            }
            Ok((0..self.triage_count)
                .map(|_| TriageItemView {
                    section: "test".to_string(),
                    severity: "warning".to_string(),
                    title: "test".to_string(),
                    detail: "test".to_string(),
                    actions: vec![],
                    event_id: None,
                    pane_id: None,
                    workflow_id: None,
                })
                .collect())
        }

        fn search(&self, _: &str, _: usize) -> Result<Vec<SearchResultView>, QueryError> {
            Ok(self.search_results.clone())
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: self.healthy,
                db_accessible: self.healthy,
                wezterm_accessible: self.healthy,
                wezterm_circuit: CircuitBreakerStatus::default(),
                pane_count: self.pane_count,
                event_count: 42,
                last_capture_ts: Some(1_700_000_000_000),
            })
        }

        fn is_watcher_running(&self) -> bool {
            self.healthy
        }

        fn mark_event_muted(&self, _: i64) -> Result<(), QueryError> {
            Ok(())
        }

        fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
            Ok(self.workflows_data.clone())
        }
    }

    // -- Helpers --

    fn make_model(query: impl QueryClient + Send + Sync + 'static) -> WaModel {
        let query: Arc<dyn QueryClient + Send + Sync> = Arc::new(query);
        WaModel::new(
            query,
            AppConfig {
                refresh_interval: Duration::from_secs(5),
                debug: false,
            },
        )
    }

    /// Extract text content from a frame row as a string.
    fn read_row(frame: &ftui::Frame, row: u16) -> String {
        let w = frame.buffer.width();
        let mut s = String::with_capacity(w as usize);
        for x in 0..w {
            if let Some(cell) = frame.buffer.get(x, row) {
                if cell.content.is_empty() || cell.content.is_continuation() {
                    s.push(' ');
                } else if let Some(ch) = cell.content.as_char() {
                    s.push(ch);
                } else {
                    s.push('?');
                }
            }
        }
        s
    }

    // -- View navigation tests --

    #[test]
    fn view_all_returns_seven_views() {
        assert_eq!(View::all().len(), 7);
    }

    #[test]
    fn view_next_wraps() {
        assert_eq!(View::Help.next(), View::Home);
        assert_eq!(View::Home.next(), View::Panes);
    }

    #[test]
    fn view_prev_wraps() {
        assert_eq!(View::Home.prev(), View::Help);
        assert_eq!(View::Panes.prev(), View::Home);
    }

    #[test]
    fn view_shortcut_roundtrip() {
        for &view in View::all() {
            let ch = view.shortcut();
            let resolved = View::from_shortcut(ch);
            assert_eq!(resolved, Some(view));
        }
    }

    #[test]
    fn view_from_shortcut_invalid() {
        assert_eq!(View::from_shortcut('0'), None);
        assert_eq!(View::from_shortcut('8'), None);
        assert_eq!(View::from_shortcut('a'), None);
    }

    #[test]
    fn view_names_are_non_empty() {
        for &view in View::all() {
            assert!(!view.name().is_empty());
        }
    }

    #[test]
    fn view_state_default_is_home() {
        let state = ViewState::default();
        assert_eq!(state.current_view, View::Home);
        assert!(state.error_message.is_none());
    }

    // -- Data refresh tests --

    #[test]
    fn refresh_data_populates_health() {
        let mut model = make_model(MockQuery::healthy());
        assert!(model.health.is_none());

        model.refresh_data();

        assert!(model.health.is_some());
        let h = model.health.as_ref().unwrap();
        assert_eq!(h.watcher_label, "running");
        assert_eq!(h.db_label, "ok");
        assert_eq!(h.pane_count, "3");
    }

    #[test]
    fn refresh_data_populates_counts() {
        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        // 3 panes × 2 unhandled each = 6
        assert_eq!(model.unhandled_count, 6);
        assert_eq!(model.triage_count, 1);
    }

    #[test]
    fn refresh_data_degraded_system() {
        let mut model = make_model(MockQuery::degraded());
        model.refresh_data();

        let h = model.health.as_ref().unwrap();
        assert_eq!(h.watcher_label, "stopped");
        assert_eq!(h.db_label, "unavailable");
        assert_eq!(model.unhandled_count, 0);
        assert_eq!(model.triage_count, 0);
    }

    // -- Home view rendering tests --

    #[test]
    fn render_home_shows_title() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        render_home_view(
            &mut frame,
            0,
            80,
            22,
            model.health.as_ref(),
            model.unhandled_count,
            model.triage_count,
        );

        let row0 = read_row(&frame, 0);
        assert!(row0.contains("WezTerm Automata"));
        assert!(row0.contains("OK"));
    }

    #[test]
    fn render_home_degraded_shows_error_badge() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        let mut model = make_model(MockQuery::degraded());
        model.refresh_data();

        render_home_view(
            &mut frame,
            0,
            80,
            22,
            model.health.as_ref(),
            model.unhandled_count,
            model.triage_count,
        );

        let row0 = read_row(&frame, 0);
        assert!(row0.contains("ERROR"));
    }

    #[test]
    fn render_home_no_health_shows_loading() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        render_home_view(&mut frame, 0, 80, 22, None, 0, 0);

        let row0 = read_row(&frame, 0);
        assert!(row0.contains("LOADING"));
    }

    #[test]
    fn render_home_shows_system_status() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        render_home_view(
            &mut frame,
            0,
            80,
            22,
            model.health.as_ref(),
            model.unhandled_count,
            model.triage_count,
        );

        // Check system status rows (starting at row 2 after title+separator)
        let mut found_watcher = false;
        let mut found_db = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Watcher") && text.contains("running") {
                found_watcher = true;
            }
            if text.contains("Database") && text.contains("ok") {
                found_db = true;
            }
        }
        assert!(found_watcher, "Watcher status not found");
        assert!(found_db, "Database status not found");
    }

    #[test]
    fn render_home_shows_metrics() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        render_home_view(
            &mut frame,
            0,
            80,
            22,
            model.health.as_ref(),
            model.unhandled_count,
            model.triage_count,
        );

        let mut found_panes = false;
        let mut found_unhandled = false;
        let mut found_triage = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Panes") && text.contains("3") {
                found_panes = true;
            }
            if text.contains("Unhandled") && text.contains("6") {
                found_unhandled = true;
            }
            if text.contains("Triage") && text.contains("1") {
                found_triage = true;
            }
        }
        assert!(found_panes, "Pane count not found");
        assert!(found_unhandled, "Unhandled count not found");
        assert!(found_triage, "Triage count not found");
    }

    #[test]
    fn render_home_shows_quick_help() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        render_home_view(
            &mut frame,
            0,
            80,
            22,
            model.health.as_ref(),
            model.unhandled_count,
            model.triage_count,
        );

        let mut found_help = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Tab") && text.contains("Quit") {
                found_help = true;
                break;
            }
        }
        assert!(found_help, "Quick help not found");
    }

    #[test]
    fn render_home_minimum_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 3, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        // Should not panic with minimal height
        render_home_view(
            &mut frame,
            0,
            40,
            1,
            model.health.as_ref(),
            model.unhandled_count,
            model.triage_count,
        );
    }

    #[test]
    fn render_home_zero_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        // Zero height should be a no-op
        render_home_view(&mut frame, 0, 80, 0, None, 0, 0);
    }

    #[test]
    fn model_r_key_triggers_refresh() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.error_message = Some("old error".to_string());

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('r'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };

        let result = model.handle_global_key(&key);
        assert!(result.is_some());
        // Error should be cleared
        assert!(model.view_state.error_message.is_none());
        // Health should be populated
        assert!(model.health.is_some());
    }

    // -- Panes view tests --

    fn press_key(model: &mut WaModel, code: ftui::KeyCode) {
        let key = ftui::KeyEvent {
            code,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&key);
    }

    #[test]
    fn refresh_data_populates_panes() {
        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();
        assert_eq!(model.panes.len(), 3);
        assert_eq!(model.panes[0].pane_id, "0");
    }

    #[test]
    fn panes_navigation_down_wraps() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Panes;
        model.refresh_data();

        assert_eq!(model.panes_selected, 0);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.panes_selected, 1);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.panes_selected, 2);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.panes_selected, 0); // Wraps
    }

    #[test]
    fn panes_navigation_up_wraps() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Panes;
        model.refresh_data();

        assert_eq!(model.panes_selected, 0);
        press_key(&mut model, ftui::KeyCode::Up);
        assert_eq!(model.panes_selected, 2); // Wraps to end
    }

    #[test]
    fn panes_j_k_navigation() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Panes;
        model.refresh_data();

        press_key(&mut model, ftui::KeyCode::Char('j'));
        assert_eq!(model.panes_selected, 1);
        press_key(&mut model, ftui::KeyCode::Char('k'));
        assert_eq!(model.panes_selected, 0);
    }

    #[test]
    fn panes_domain_filter_cycles() {
        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();
        model.view_state.current_view = View::Panes;

        assert!(model.panes_domain_filter.is_none());
        press_key(&mut model, ftui::KeyCode::Char('d'));
        assert!(model.panes_domain_filter.is_some());
        assert_eq!(model.panes_domain_filter.as_deref(), Some("local"));
    }

    #[test]
    fn panes_esc_clears_filter() {
        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();
        model.view_state.current_view = View::Panes;

        model.panes_domain_filter = Some("local".to_string());
        model.panes_selected = 2;
        press_key(&mut model, ftui::KeyCode::Escape);
        assert!(model.panes_domain_filter.is_none());
        assert_eq!(model.panes_selected, 0);
    }

    #[test]
    fn render_panes_shows_header_and_columns() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        let filtered = model.filtered_pane_indices();
        render_panes_view(
            &mut frame,
            0,
            100,
            22,
            &model.panes,
            &filtered,
            0,
            None,
        );

        let row0 = read_row(&frame, 0);
        assert!(row0.contains("Panes (3/3)"));
        assert!(row0.contains("domain=all"));

        let row1 = read_row(&frame, 1);
        assert!(row1.contains("ID"));
        assert!(row1.contains("Agent"));
        assert!(row1.contains("State"));
    }

    #[test]
    fn render_panes_shows_pane_rows() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        let filtered = model.filtered_pane_indices();
        render_panes_view(
            &mut frame,
            0,
            100,
            22,
            &model.panes,
            &filtered,
            0,
            None,
        );

        // Pane rows start at row 2
        let row2 = read_row(&frame, 2);
        assert!(row2.contains("0")); // pane_id
        assert!(row2.contains("PromptAc")); // state (truncated)
    }

    #[test]
    fn render_panes_shows_detail_panel() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        let filtered = model.filtered_pane_indices();
        render_panes_view(
            &mut frame,
            0,
            100,
            22,
            &model.panes,
            &filtered,
            0,
            None,
        );

        // Detail panel is in the right 1/3 — check rows for "Pane Details"
        let mut found_detail = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Pane Details") {
                found_detail = true;
                break;
            }
        }
        assert!(found_detail, "Detail panel header not found");
    }

    #[test]
    fn render_panes_empty_shows_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        render_panes_view(&mut frame, 0, 100, 22, &[], &[], 0, None);

        let mut found_msg = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("No panes") {
                found_msg = true;
                break;
            }
        }
        assert!(found_msg, "Empty panes message not found");
    }

    #[test]
    fn render_panes_minimum_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 3, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        let filtered = model.filtered_pane_indices();
        render_panes_view(
            &mut frame,
            0,
            40,
            1,
            &model.panes,
            &filtered,
            0,
            None,
        );
    }

    // -- Search view tests --

    fn sample_search_results() -> Vec<SearchResultView> {
        vec![
            SearchResultView { pane_id: 10, timestamp: 1_700_000_000_000, snippet: "error: connection refused".into(), rank: 0.95 },
            SearchResultView { pane_id: 20, timestamp: 1_700_000_001_000, snippet: "error: timeout exceeded".into(), rank: 0.88 },
        ]
    }

    #[test]
    fn search_char_input_appends_to_query() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('h'));
        press_key(&mut model, ftui::KeyCode::Char('i'));
        assert_eq!(model.search_query, "hi");
    }

    #[test]
    fn search_backspace_removes_char() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('a'));
        press_key(&mut model, ftui::KeyCode::Char('b'));
        press_key(&mut model, ftui::KeyCode::Backspace);
        assert_eq!(model.search_query, "a");
    }

    #[test]
    fn search_enter_executes_query() {
        let mock = MockQuery::healthy().with_search_results(sample_search_results());
        let mut model = make_model(mock);
        model.view_state.current_view = View::Search;
        model.search_query = "error".into();
        press_key(&mut model, ftui::KeyCode::Enter);
        assert_eq!(model.search_last_query, "error");
        assert_eq!(model.search_results.len(), 2);
        assert_eq!(model.search_selected, 0);
    }

    #[test]
    fn search_enter_empty_query_noop() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        model.search_query = "  ".into();
        press_key(&mut model, ftui::KeyCode::Enter);
        assert!(model.search_results.is_empty());
        assert!(model.search_last_query.is_empty());
    }

    #[test]
    fn search_esc_clears_all() {
        let mock = MockQuery::healthy().with_search_results(sample_search_results());
        let mut model = make_model(mock);
        model.view_state.current_view = View::Search;
        model.search_query = "error".into();
        press_key(&mut model, ftui::KeyCode::Enter);
        assert!(!model.search_results.is_empty());
        press_key(&mut model, ftui::KeyCode::Escape);
        assert!(model.search_query.is_empty());
        assert!(model.search_last_query.is_empty());
        assert!(model.search_results.is_empty());
        assert_eq!(model.search_selected, 0);
    }

    #[test]
    fn search_arrow_navigation_wraps() {
        let mock = MockQuery::healthy().with_search_results(sample_search_results());
        let mut model = make_model(mock);
        model.view_state.current_view = View::Search;
        model.search_query = "error".into();
        press_key(&mut model, ftui::KeyCode::Enter);
        assert_eq!(model.search_selected, 0);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.search_selected, 1);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.search_selected, 0);
        press_key(&mut model, ftui::KeyCode::Up);
        assert_eq!(model.search_selected, 1);
    }

    #[test]
    fn search_global_q_does_not_quit() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        let key = ftui::KeyEvent { code: ftui::KeyCode::Char('q'), kind: ftui::KeyEventKind::Press, modifiers: ftui::Modifiers::empty() };
        let result = model.handle_global_key(&key);
        assert!(result.is_none());
        model.handle_view_key(&key);
        assert_eq!(model.search_query, "q");
    }

    #[test]
    fn search_tab_still_navigates_views() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        let key = ftui::KeyEvent { code: ftui::KeyCode::Tab, kind: ftui::KeyEventKind::Press, modifiers: ftui::Modifiers::empty() };
        let result = model.handle_global_key(&key);
        assert!(result.is_some());
        assert_eq!(model.view_state.current_view, View::Help);
    }

    #[test]
    fn render_search_empty_shows_prompt() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(&mut frame, 0, 80, 22, "", "", &[], 0);
        let row0 = read_row(&frame, 0);
        assert!(row0.contains("Search (FTS5)"));
        let row1 = read_row(&frame, 1);
        assert!(row1.contains("Type a query"));
    }

    #[test]
    fn render_search_no_results_shows_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(&mut frame, 0, 80, 22, "test", "test", &[], 0);
        let row1 = read_row(&frame, 1);
        assert!(row1.contains("No results"));
    }

    #[test]
    fn render_search_with_results_shows_list_and_detail() {
        let rows: Vec<super::SearchRow> = sample_search_results().iter().map(super::adapt_search).collect();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);
        render_search_view(&mut frame, 0, 100, 22, "error", "error", &rows, 0);
        let row1 = read_row(&frame, 1);
        assert!(row1.contains("2 matches"));
        let row2 = read_row(&frame, 2);
        assert!(row2.contains("Pane"));
        assert!(row2.contains("Rank"));
        let row3 = read_row(&frame, 3);
        assert!(row3.contains("P 10"));
        let mut found = false;
        for r in 0..22 { if read_row(&frame, r).contains("Match Context") { found = true; break; } }
        assert!(found, "Detail panel header not found");
    }

    #[test]
    fn render_search_zero_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(&mut frame, 0, 80, 0, "q", "q", &[], 0);
    }

    // -- Help view tests --

    #[test]
    fn render_help_shows_title_and_sections() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 40, &mut pool);
        render_help_view(&mut frame, 0, 80, 38);
        let row0 = read_row(&frame, 0);
        assert!(row0.contains("WezTerm Automata TUI"));
        let mut g = false; let mut v = false; let mut s = false;
        for r in 0..38 {
            let t = read_row(&frame, r);
            if t.contains("Global Keybindings") { g = true; }
            if t.contains("Views:") { v = true; }
            if t.contains("Search:") { s = true; }
        }
        assert!(g, "Global keybindings section not found");
        assert!(v, "Views section not found");
        assert!(s, "Search section not found");
    }

    #[test]
    fn render_help_zero_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_help_view(&mut frame, 0, 80, 0);
    }

    #[test]
    fn render_help_small_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(40, 5, &mut pool);
        render_help_view(&mut frame, 0, 40, 3);
        let row0 = read_row(&frame, 0);
        assert!(row0.contains("WezTerm Automata"));
    }

    // -- Events view tests --

    #[test]
    fn refresh_data_populates_events() {
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();
        assert_eq!(model.view_state.events.items.len(), 3);
        assert_eq!(model.view_state.events.rows.len(), 3);
    }

    #[test]
    fn events_filtering_all() {
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();
        let indices = model.view_state.events.filtered_indices();
        assert_eq!(indices.len(), 3);
    }

    #[test]
    fn events_filtering_unhandled_only() {
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();
        model.view_state.events.unhandled_only = true;
        let indices = model.view_state.events.filtered_indices();
        assert_eq!(indices.len(), 2); // events 0 and 2 are unhandled
    }

    #[test]
    fn events_filtering_pane_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();
        model.view_state.events.pane_filter = "42".to_string();
        let indices = model.view_state.events.filtered_indices();
        assert_eq!(indices.len(), 2); // events 0 and 2 are pane 42
    }

    #[test]
    fn events_filtering_combined() {
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();
        model.view_state.events.unhandled_only = true;
        model.view_state.events.pane_filter = "7".to_string();
        let indices = model.view_state.events.filtered_indices();
        assert_eq!(indices.len(), 0); // pane 7 event is handled
    }

    #[test]
    fn events_navigation_down_wraps() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();

        assert_eq!(model.view_state.events.selected_index, 0);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.view_state.events.selected_index, 1);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.view_state.events.selected_index, 2);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.view_state.events.selected_index, 0); // Wraps
    }

    #[test]
    fn events_navigation_up_wraps() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();

        assert_eq!(model.view_state.events.selected_index, 0);
        press_key(&mut model, ftui::KeyCode::Up);
        assert_eq!(model.view_state.events.selected_index, 2); // Wraps to end
    }

    #[test]
    fn events_j_k_navigation() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();

        press_key(&mut model, ftui::KeyCode::Char('j'));
        assert_eq!(model.view_state.events.selected_index, 1);
        press_key(&mut model, ftui::KeyCode::Char('k'));
        assert_eq!(model.view_state.events.selected_index, 0);
    }

    #[test]
    fn events_u_toggles_unhandled_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();

        assert!(!model.view_state.events.unhandled_only);
        press_key(&mut model, ftui::KeyCode::Char('u'));
        assert!(model.view_state.events.unhandled_only);
        press_key(&mut model, ftui::KeyCode::Char('u'));
        assert!(!model.view_state.events.unhandled_only);
    }

    #[test]
    fn events_digit_appends_pane_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();

        press_key(&mut model, ftui::KeyCode::Char('4'));
        assert_eq!(model.view_state.events.pane_filter, "4");
        press_key(&mut model, ftui::KeyCode::Char('2'));
        assert_eq!(model.view_state.events.pane_filter, "42");
    }

    #[test]
    fn events_backspace_removes_filter_char() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        model.view_state.events.pane_filter = "42".to_string();

        press_key(&mut model, ftui::KeyCode::Backspace);
        assert_eq!(model.view_state.events.pane_filter, "4");
    }

    #[test]
    fn events_esc_clears_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        model.view_state.events.pane_filter = "42".to_string();
        model.view_state.events.selected_index = 1;

        press_key(&mut model, ftui::KeyCode::Escape);
        assert!(model.view_state.events.pane_filter.is_empty());
        assert_eq!(model.view_state.events.selected_index, 0);
    }

    #[test]
    fn events_digits_not_consumed_globally() {
        // In Events view, digit keys should go to pane filter, not view switching.
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('4'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let result = model.handle_global_key(&key);
        assert!(result.is_none(), "digit should not be consumed globally in Events view");
    }

    #[test]
    fn render_events_shows_header_and_columns() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();

        let filtered = model.view_state.events.filtered_indices();
        let clamped = model.view_state.events.clamped_selection();
        render_events_view(
            &mut frame, 0, 100, 22,
            &model.view_state.events, &filtered, clamped,
        );

        let row0 = read_row(&frame, 0);
        assert!(row0.contains("Events (3/3)"));

        let row1 = read_row(&frame, 1);
        assert!(row1.contains("sev"));
        assert!(row1.contains("pane"));
        assert!(row1.contains("rule"));
    }

    #[test]
    fn render_events_shows_event_rows() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();

        let filtered = model.view_state.events.filtered_indices();
        let clamped = model.view_state.events.clamped_selection();
        render_events_view(
            &mut frame, 0, 100, 22,
            &model.view_state.events, &filtered, clamped,
        );

        // Event rows start at row 2
        let row2 = read_row(&frame, 2);
        assert!(row2.contains("warning"));
        assert!(row2.contains("42"));
        assert!(row2.contains("rate_limit"));
    }

    #[test]
    fn render_events_shows_detail_panel() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();

        let filtered = model.view_state.events.filtered_indices();
        let clamped = model.view_state.events.clamped_selection();
        render_events_view(
            &mut frame, 0, 100, 22,
            &model.view_state.events, &filtered, clamped,
        );

        let mut found_detail = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Event Details") {
                found_detail = true;
                break;
            }
        }
        assert!(found_detail, "Detail panel header not found");
    }

    #[test]
    fn render_events_empty_shows_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let events_state = EventsViewState::default();
        render_events_view(&mut frame, 0, 100, 22, &events_state, &[], 0);

        let mut found_msg = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("No events") {
                found_msg = true;
                break;
            }
        }
        assert!(found_msg, "Empty events message not found");
    }

    #[test]
    fn render_events_zero_height_no_panic() {
        let events_state = EventsViewState::default();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_events_view(&mut frame, 0, 80, 0, &events_state, &[], 0);
    }

    // -- Triage view tests --

    #[test]
    fn refresh_data_populates_triage_items() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        assert_eq!(model.triage_items.len(), 3);
        assert_eq!(model.triage_items[0].severity_label, "error");
        assert_eq!(model.triage_items[1].severity_label, "warning");
        assert_eq!(model.triage_items[2].severity_label, "info");
    }

    #[test]
    fn refresh_data_populates_workflows() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        assert_eq!(model.workflows.len(), 1);
        assert_eq!(model.workflows[0].name, "rate-limit-handler");
        assert_eq!(model.workflows[0].status_label, "running");
    }

    #[test]
    fn triage_navigation_down_wraps() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        // Navigate past last item should wrap to 0
        model.triage_selected = 2; // last item (index 2)
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Down,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert_eq!(model.triage_selected, 0);
    }

    #[test]
    fn triage_navigation_up_wraps() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        model.triage_selected = 0;
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Up,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert_eq!(model.triage_selected, 2);
    }

    #[test]
    fn triage_j_k_navigation() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        let key_j = ftui::KeyEvent {
            code: ftui::KeyCode::Char('j'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let key_k = ftui::KeyEvent {
            code: ftui::KeyCode::Char('k'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };

        assert_eq!(model.triage_selected, 0);
        model.handle_triage_key(&key_j);
        assert_eq!(model.triage_selected, 1);
        model.handle_triage_key(&key_k);
        assert_eq!(model.triage_selected, 0);
    }

    #[test]
    fn triage_enter_queues_primary_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("wa pane restart 7"),
        );
    }

    #[test]
    fn triage_digit_queues_numbered_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        // Digit '2' should queue action at index 1 ("Investigate")
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('2'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("wa events show --pane 7"),
        );
    }

    #[test]
    fn triage_digit_out_of_range_no_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        // Digit '9' — no action at index 8
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('9'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert!(model.triage_queued_action.is_none());
    }

    #[test]
    fn triage_mute_calls_mark_event_muted() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('m'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        // Should not error (MockQuery.mark_event_muted returns Ok)
        model.handle_triage_key(&key);
        assert!(model.view_state.error_message.is_none());
    }

    #[test]
    fn triage_e_toggles_workflow_expand() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        assert!(model.triage_expanded.is_none());

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('e'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert_eq!(model.triage_expanded, Some(0));

        model.handle_triage_key(&key);
        assert!(model.triage_expanded.is_none());
    }

    #[test]
    fn triage_e_no_op_without_workflows() {
        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('e'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert!(model.triage_expanded.is_none());
    }

    #[test]
    fn triage_digits_not_consumed_globally() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        // Digit '2' in Triage should NOT switch views
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('2'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let result = model.handle_global_key(&key);
        assert!(result.is_none(), "Digit should not be consumed globally in Triage view");
        assert_eq!(model.view_state.current_view, View::Triage);
    }

    #[test]
    fn render_triage_shows_header_and_items() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        render_triage_view(
            &mut frame, 0, 100, 22,
            &model.triage_items, model.triage_selected,
            &model.workflows, model.triage_expanded,
        );

        let row0 = read_row(&frame, 0);
        assert!(row0.contains("Triage"), "Header should contain 'Triage'");
        assert!(row0.contains("3 items"), "Header should show item count");
    }

    #[test]
    fn render_triage_shows_severity_and_title() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        render_triage_view(
            &mut frame, 0, 100, 22,
            &model.triage_items, model.triage_selected,
            &model.workflows, model.triage_expanded,
        );

        // Item rows start after header + column header
        let mut found_error = false;
        let mut found_warning = false;
        for r in 2..12 {
            let text = read_row(&frame, r);
            if text.contains("error") && text.contains("Fatal crash") {
                found_error = true;
            }
            if text.contains("warning") && text.contains("Rate limit") {
                found_warning = true;
            }
        }
        assert!(found_error, "Error severity item not found");
        assert!(found_warning, "Warning severity item not found");
    }

    #[test]
    fn render_triage_shows_workflow_panel() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        render_triage_view(
            &mut frame, 0, 100, 22,
            &model.triage_items, model.triage_selected,
            &model.workflows, model.triage_expanded,
        );

        let mut found_wf = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Active Workflows") {
                found_wf = true;
                break;
            }
        }
        assert!(found_wf, "Workflow panel header not found");
    }

    #[test]
    fn render_triage_shows_detail_actions() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        render_triage_view(
            &mut frame, 0, 100, 22,
            &model.triage_items, model.triage_selected,
            &model.workflows, model.triage_expanded,
        );

        let mut found_actions = false;
        let mut found_restart = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Actions") {
                found_actions = true;
            }
            if text.contains("Restart") && text.contains("wa pane restart") {
                found_restart = true;
            }
        }
        assert!(found_actions, "Actions header not found");
        assert!(found_restart, "Restart action not found");
    }

    #[test]
    fn render_triage_empty_shows_all_clear() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        render_triage_view(&mut frame, 0, 100, 22, &[], 0, &[], None);

        let mut found_clear = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("All clear") {
                found_clear = true;
                break;
            }
        }
        assert!(found_clear, "All clear message not found");
    }

    #[test]
    fn render_triage_zero_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_triage_view(&mut frame, 0, 80, 0, &[], 0, &[], None);
    }

    #[test]
    fn render_triage_no_workflows_hides_panel() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        // Remove workflows to test without them
        let empty_wf: Vec<WorkflowRow> = vec![];
        render_triage_view(
            &mut frame, 0, 100, 22,
            &model.triage_items, model.triage_selected,
            &empty_wf, None,
        );

        let mut found_wf = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Active Workflows") {
                found_wf = true;
                break;
            }
        }
        assert!(!found_wf, "Workflow panel should not appear without workflows");
    }

    #[test]
    fn triage_selection_clamps_after_refresh() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.triage_selected = 10; // Past end
        model.refresh_data();
        assert_eq!(model.triage_selected, 2); // Clamped to last item
    }
}
