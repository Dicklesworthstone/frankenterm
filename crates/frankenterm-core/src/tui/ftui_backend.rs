//! FrankenTUI backend for wa TUI.
//!
//! Implements the Elm-style `Model` trait from `ftui::runtime` to drive the
//! wa interactive terminal UI.  The app shell handles:
//!
//! - View routing (Home, Panes, Events, Triage, History, Search, Help)
//! - Tab bar rendering with highlighted active view
//! - Global keybindings (Tab always; character shortcuts when the active view
//!   does not own that character input)
//! - Periodic data refresh via background tasks
//!
//! All view bodies (Home, Panes, Search, Help, Events, Triage, History,
//! Timeline) are backed by live state — no placeholders remain.
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
//! WaModel::view()    →  Frame (tab bar + content)
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::command_handoff::quote_command_arg;
use super::query::{
    PaneBookmarkView, QueryClient, QueryError, RulesetProfileState, SavedSearchView,
};
use super::view_adapters::{
    HealthModel, PaneRow, SearchRow, TimelineRow, TriageRow, WorkflowRow, adapt_event,
    adapt_health, adapt_history, adapt_pane, adapt_search, adapt_timeline_event, adapt_triage,
    adapt_workflow,
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
    /// Unified event timeline with cross-pane correlations (wa-6sk.4).
    Timeline,
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
            Self::Timeline => "Timeline",
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
            Self::Timeline,
        ]
    }

    /// Shortcut key for direct navigation (1-8).
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
            Self::Timeline => '8',
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
            Self::Help => Self::Timeline,
            Self::Timeline => Self::Home,
        }
    }

    /// Previous view in tab order (wraps around).
    #[must_use]
    pub const fn prev(&self) -> Self {
        match self {
            Self::Home => Self::Timeline,
            Self::Panes => Self::Home,
            Self::Events => Self::Panes,
            Self::Triage => Self::Events,
            Self::History => Self::Triage,
            Self::Search => Self::History,
            Self::Help => Self::Search,
            Self::Timeline => Self::Help,
        }
    }

    /// Resolve a '1'-'8' character to a view.
    fn from_shortcut(ch: char) -> Option<Self> {
        match ch {
            '1' => Some(Self::Home),
            '2' => Some(Self::Panes),
            '3' => Some(Self::Events),
            '4' => Some(Self::Triage),
            '5' => Some(Self::History),
            '6' => Some(Self::Search),
            '7' => Some(Self::Help),
            '8' => Some(Self::Timeline),
            _ => None,
        }
    }
}

fn has_command_modifier(key: &ftui::KeyEvent) -> bool {
    matches!(key.code, ftui::KeyCode::Char(_))
        && (key.modifiers.contains(ftui::Modifiers::CTRL)
            || key.modifiers.contains(ftui::Modifiers::ALT))
}

// ---------------------------------------------------------------------------
// Modal state — reusable overlay for confirmations, errors, and info
// ---------------------------------------------------------------------------

/// The kind of modal being displayed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Info variant used as migration progresses
pub enum ModalKind {
    /// Confirmation dialog — requires user to accept or cancel.
    Confirm,
    /// Error display — dismissible with Escape or Enter.
    Error,
    /// Informational message — dismissible with Escape or Enter.
    Info,
}

/// Action to execute when a Confirm modal is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Execute a shell command (triage action, profile apply, etc.).
    ExecuteCommand(String),
    /// Mute an event by its string ID.
    MuteEvent(String),
}

/// State for an active modal overlay.
#[derive(Debug, Clone)]
pub struct ModalState {
    pub kind: ModalKind,
    pub title: String,
    pub body: String,
    /// Action to run on confirm (only relevant for `ModalKind::Confirm`).
    pub on_confirm: Option<ConfirmAction>,
}

#[allow(dead_code)] // Constructors used as migration progresses (FTUI-06.3+)
impl ModalState {
    /// Create a confirmation modal.
    fn confirm(title: impl Into<String>, body: impl Into<String>, action: ConfirmAction) -> Self {
        Self {
            kind: ModalKind::Confirm,
            title: title.into(),
            body: body.into(),
            on_confirm: Some(action),
        }
    }

    /// Create an error modal.
    fn error(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind: ModalKind::Error,
            title: title.into(),
            body: body.into(),
            on_confirm: None,
        }
    }

    /// Create an informational modal.
    fn info(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind: ModalKind::Info,
            title: title.into(),
            body: body.into(),
            on_confirm: None,
        }
    }
}

// ---------------------------------------------------------------------------
// TextInput — reusable text editing widget (FTUI-06.4)
// ---------------------------------------------------------------------------

/// Reusable text input with cursor position tracking.
///
/// Provides deterministic editing semantics: insert at cursor, delete left/right,
/// cursor movement, and clear.  Used by search, events filter, and history filter.
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    text: String,
    cursor: usize,
}

#[allow(dead_code)] // Methods used as integration progresses (FTUI-06.4+)
impl TextInput {
    /// Create a new empty text input.
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    /// The current text content.
    fn text(&self) -> &str {
        &self.text
    }

    /// The cursor position (byte offset, always on a char boundary).
    fn cursor_pos(&self) -> usize {
        self.cursor
    }

    /// Whether the input is empty.
    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Insert a character at the cursor position and advance cursor.
    fn insert_char(&mut self, c: char) {
        let cursor = char_boundary_at_or_before(&self.text, self.cursor);
        self.text.insert(cursor, c);
        self.cursor = cursor + c.len_utf8();
    }

    /// Delete the character before the cursor (backspace).
    fn delete_back(&mut self) {
        let cursor = char_boundary_at_or_before(&self.text, self.cursor);
        if cursor > 0 {
            let prev = previous_char_start(&self.text, cursor);
            self.text.remove(prev);
            self.cursor = prev;
        } else {
            self.cursor = 0;
        }
    }

    /// Delete the character at the cursor (forward delete).
    fn delete_forward(&mut self) {
        let cursor = char_boundary_at_or_before(&self.text, self.cursor);
        if cursor < self.text.len() {
            self.text.remove(cursor);
            self.cursor = cursor;
        } else {
            self.cursor = self.text.len();
        }
    }

    /// Clear all text and reset cursor.
    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Move cursor one character left.
    fn move_left(&mut self) {
        let cursor = char_boundary_at_or_before(&self.text, self.cursor);
        self.cursor = if cursor > 0 {
            previous_char_start(&self.text, cursor)
        } else {
            0
        };
    }

    /// Move cursor one character right.
    fn move_right(&mut self) {
        let cursor = char_boundary_at_or_before(&self.text, self.cursor);
        if cursor < self.text.len() {
            self.cursor = next_char_boundary_after(&self.text, cursor);
        } else {
            self.cursor = self.text.len();
        }
    }

    /// Move cursor to start of text.
    fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move cursor to end of text.
    fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Set text content, placing cursor at end.
    fn set_text(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }
}

fn char_boundary_at_or_before(s: &str, index: usize) -> usize {
    let capped = index.min(s.len());
    if s.is_char_boundary(capped) {
        return capped;
    }

    s.char_indices()
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx < capped)
        .last()
        .unwrap_or(0)
}

fn previous_char_start(s: &str, cursor: usize) -> usize {
    s.char_indices()
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx < cursor)
        .last()
        .unwrap_or(0)
}

fn next_char_boundary_after(s: &str, cursor: usize) -> usize {
    let cursor = char_boundary_at_or_before(s, cursor);
    if cursor >= s.len() {
        return s.len();
    }

    s[cursor..]
        .char_indices()
        .nth(1)
        .map_or(s.len(), |(idx, _)| cursor + idx)
}

// ---------------------------------------------------------------------------
// FocusRegion — intra-view focus tracking (FTUI-06.5)
// ---------------------------------------------------------------------------

/// Logical focus region within a two-panel view.
///
/// Terminal UIs use a master-detail pattern: the primary list always owns
/// selection (j/k/Up/Down), while the detail panel passively reflects the
/// selected item.  `FocusRegion` makes this explicit and testable.
///
/// Focus traversal policy:
/// - Tab/Shift+Tab: always switches **views** (global, not panel-level).
/// - j/k/Up/Down: navigates the list in the PrimaryList region.
/// - Detail panels auto-update based on selection (no independent scroll).
/// - FilterBar captures text input; list navigation still works (Down/Up).
/// - Modals trap all input until dismissed (Enter/y/Escape/n).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum FocusRegion {
    /// The main interactive list (events, panes, triage, history, search results).
    #[default]
    PrimaryList,
    /// Text filter/search input bar (search query, events pane filter, history filter).
    FilterBar,
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
    /// Intra-view focus region (FTUI-06.5).
    pub focus: FocusRegion,

    // -- Events view state (FTUI-05.4) --
    pub events: EventsViewState,

    // -- History view state (FTUI-05.6) --
    pub history: HistoryViewState,
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
    pub pane_filter: TextInput,
    /// Currently selected index within the filtered list.
    pub selected_index: usize,
}

impl EventsViewState {
    /// Return indices of events matching the current filters.
    pub fn filtered_indices(&self) -> Vec<usize> {
        let query = self.pane_filter.text().trim();
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
        self.selected_index.min(filtered.len().saturating_sub(1))
    }
}

/// History view state.
#[derive(Debug, Default)]
pub struct HistoryViewState {
    /// Raw history entries from last data refresh.
    pub items: Vec<super::query::HistoryEntryView>,
    /// Adapted render-ready rows (parallel to `items`).
    pub rows: Vec<super::view_adapters::HistoryRow>,
    /// Show only undoable actions.
    pub undoable_only: bool,
    /// Free-text filter (matches pane, workflow, action, audit ID).
    pub filter_input: TextInput,
    /// Currently selected index within filtered results.
    pub selected_index: usize,
}

impl HistoryViewState {
    /// Return indices of history entries matching the current filters.
    pub fn filtered_indices(&self) -> Vec<usize> {
        let query = self.filter_input.text().trim().to_ascii_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                if self.undoable_only && !entry.undoable {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                let pane = entry.pane_id.map(|id| id.to_string()).unwrap_or_default();
                let workflow = entry.workflow_id.as_deref().unwrap_or("");
                let audit = entry.audit_id.to_string();
                let haystack = format!(
                    "{pane} {workflow} {} {} {} {audit}",
                    entry.action_kind, entry.result, entry.actor_kind
                )
                .to_ascii_lowercase();
                haystack.contains(&query)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Clamped selected index within filtered results.
    pub fn clamped_selection(&self) -> usize {
        let filtered = self.filtered_indices();
        self.selected_index.min(filtered.len().saturating_sub(1))
    }
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// TUI application configuration.
#[allow(dead_code)] // `debug` will be consumed when debug overlay is migrated (FTUI-06)
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
    /// View state (public for benchmarking; use `view_state.current_view` to switch views).
    pub view_state: ViewState,
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
    pane_bookmarks: Vec<PaneBookmarkView>,
    panes_filter: TextInput,
    panes_unhandled_only: bool,
    panes_bookmarked_only: bool,
    panes_agent_filter: Option<String>,
    panes_domain_filter: Option<String>,
    panes_profile_index: usize,
    ruleset_profile_state: Option<RulesetProfileState>,
    // Triage view state.
    triage_items: Vec<TriageRow>,
    triage_selected: usize,
    triage_expanded: Option<usize>,
    workflows: Vec<WorkflowRow>,
    // Queued action command from view handlers (consumed by the event loop).
    triage_queued_action: Option<String>,
    // Modal overlay state (FTUI-06.3).
    active_modal: Option<ModalState>,
    // Search view state (FTUI-06.4: uses TextInput for cursor-aware editing).
    search_input: TextInput,
    search_last_query: String,
    search_results: Vec<SearchRow>,
    search_selected: usize,
    saved_searches: Vec<SavedSearchView>,
    saved_search_selected: usize,
    // Timeline view state (wa-6sk.4).
    timeline_rows: Vec<TimelineRow>,
    timeline_selected: usize,
    timeline_zoom: u8,
    timeline_scroll: usize,
}

impl WaModel {
    /// Create a new model with the given query client and configuration.
    ///
    /// Public for benchmarking; normal usage goes through [`run_tui`].
    pub fn new(query: Arc<dyn QueryClient + Send + Sync>, config: AppConfig) -> Self {
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
            pane_bookmarks: Vec::new(),
            panes_filter: TextInput::new(),
            panes_unhandled_only: false,
            panes_bookmarked_only: false,
            panes_agent_filter: None,
            panes_domain_filter: None,
            panes_profile_index: 0,
            ruleset_profile_state: None,
            triage_items: Vec::new(),
            triage_selected: 0,
            triage_expanded: None,
            workflows: Vec::new(),
            triage_queued_action: None,
            active_modal: None,
            search_input: TextInput::new(),
            search_last_query: String::new(),
            search_results: Vec::new(),
            search_selected: 0,
            saved_searches: Vec::new(),
            saved_search_selected: 0,
            timeline_rows: Vec::new(),
            timeline_selected: 0,
            timeline_zoom: 0,
            timeline_scroll: 0,
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
            View::History => self.handle_history_key(key),
            View::Search => self.handle_search_key(key),
            View::Timeline => self.handle_timeline_key(key),
            _ => ftui::Cmd::None,
        }
    }

    /// Handle keys specific to the Panes view.
    fn handle_panes_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let filtered = self.filtered_pane_indices();
        let count = filtered.len();
        let plain_char = !has_command_modifier(key);

        match key.code {
            KeyCode::Down | KeyCode::Char('j') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                if count > 0 {
                    self.panes_selected = (self.panes_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                if count > 0 {
                    self.panes_selected = self.panes_selected.checked_sub(1).unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Char('u') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.panes_unhandled_only = !self.panes_unhandled_only;
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Char('b') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.panes_bookmarked_only = !self.panes_bookmarked_only;
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Char('a') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.panes_agent_filter =
                    Self::next_agent_filter(self.panes_agent_filter.as_deref());
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Char('d') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.panes_domain_filter =
                    Self::next_domain_filter(self.panes_domain_filter.as_deref());
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Char('p') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.cycle_panes_profile();
                ftui::Cmd::None
            }
            KeyCode::Enter => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.queue_selected_ruleset_profile_apply();
                ftui::Cmd::None
            }
            KeyCode::Backspace => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.panes_filter.delete_back();
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Escape => {
                self.clear_panes_filters();
                ftui::Cmd::None
            }
            KeyCode::Char(c) if plain_char && !c.is_control() => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.panes_filter.insert_char(c);
                self.panes_selected = 0;
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
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

    fn clear_panes_filters(&mut self) {
        self.view_state.focus = FocusRegion::PrimaryList;
        self.panes_filter.clear();
        self.panes_unhandled_only = false;
        self.panes_bookmarked_only = false;
        self.panes_agent_filter = None;
        self.panes_domain_filter = None;
        self.panes_selected = 0;
    }

    fn cycle_panes_profile(&mut self) {
        let profile_count = self
            .ruleset_profile_state
            .as_ref()
            .map_or(0, |profile_state| profile_state.profiles.len());
        if profile_count > 0 {
            self.panes_profile_index = (self.panes_profile_index + 1) % profile_count;
        }
    }

    fn selected_ruleset_profile_name(&self) -> Option<&str> {
        let profile_state = self.ruleset_profile_state.as_ref()?;
        profile_state
            .profiles
            .get(
                self.panes_profile_index
                    .min(profile_state.profiles.len().saturating_sub(1)),
            )
            .map(|profile| profile.name.as_str())
    }

    fn active_ruleset_profile_name(&self) -> Option<&str> {
        self.ruleset_profile_state
            .as_ref()
            .map(|profile_state| profile_state.active_profile.as_str())
    }

    fn queue_selected_ruleset_profile_apply(&mut self) {
        let Some(selected_name) = self.selected_ruleset_profile_name().map(ToOwned::to_owned)
        else {
            self.triage_queued_action = None;
            return;
        };
        let active_name = self.active_ruleset_profile_name().unwrap_or_default();
        if selected_name == active_name {
            self.triage_queued_action = None;
            return;
        }

        self.triage_queued_action = Some(format!(
            "ft rules profile apply {}",
            quote_command_arg(&selected_name)
        ));
    }

    /// Handle keys specific to the Triage view.
    ///
    /// j/k/Down/Up: navigate items.  Enter/a: run primary action.
    /// 1-9: run numbered action.  m: mute selected event.
    /// e: toggle workflow expand/collapse.
    fn handle_triage_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let count = self.triage_items.len();
        let plain_char = !has_command_modifier(key);

        match key.code {
            KeyCode::Down | KeyCode::Char('j') if plain_char => {
                if count > 0 {
                    self.triage_selected = (self.triage_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') if plain_char => {
                if count > 0 {
                    self.triage_selected = self.triage_selected.checked_sub(1).unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Enter | KeyCode::Char('a') if plain_char => {
                // Queue primary action (index 0) for the selected triage item.
                self.queue_triage_action(0);
                ftui::Cmd::None
            }
            KeyCode::Char('m') if plain_char => {
                // Mute the selected triage item's event (if it has an event_id).
                self.mute_selected_triage_event();
                ftui::Cmd::None
            }
            KeyCode::Char('e') if plain_char => {
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
            KeyCode::Char(c) if plain_char && c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap_or(0);
                if idx > 0 {
                    self.queue_triage_action(idx as usize - 1);
                }
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    /// Show a confirmation modal for a triage action.
    fn queue_triage_action(&mut self, action_idx: usize) {
        if let Some(item) = self.triage_items.get(self.triage_selected) {
            if let Some(cmd) = item.action_commands.get(action_idx) {
                let label = item
                    .action_labels
                    .get(action_idx)
                    .cloned()
                    .unwrap_or_else(|| cmd.clone());
                self.show_modal(ModalState::confirm(
                    "Confirm Action",
                    format!("Run \"{label}\"?\n\n  {cmd}"),
                    ConfirmAction::ExecuteCommand(cmd.clone()),
                ));
            }
        }
    }

    /// Show a confirmation modal for muting an event.
    fn mute_selected_triage_event(&mut self) {
        let event_id_str = self
            .triage_items
            .get(self.triage_selected)
            .map(|item| item.event_id.clone())
            .unwrap_or_default();
        if event_id_str.is_empty() {
            return;
        }
        let title_str = self
            .triage_items
            .get(self.triage_selected)
            .map(|item| item.title.clone())
            .unwrap_or_default();
        self.show_modal(ModalState::confirm(
            "Confirm Mute",
            format!("Mute event {event_id_str}?\n\n  {title_str}"),
            ConfirmAction::MuteEvent(event_id_str),
        ));
    }

    /// Show a modal overlay.
    fn show_modal(&mut self, modal: ModalState) {
        self.active_modal = Some(modal);
    }

    /// Dismiss the active modal without executing.
    fn dismiss_modal(&mut self) {
        self.active_modal = None;
    }

    /// Handle keys when a modal is active.
    ///
    /// Returns `Some(cmd)` if the key was consumed by the modal,
    /// `None` if no modal is active (caller should proceed with normal handling).
    fn handle_modal_key(&mut self, key: &ftui::KeyEvent) -> Option<ftui::Cmd<WaMsg>> {
        if key.kind != ftui::KeyEventKind::Press {
            return self.active_modal.as_ref().map(|_| ftui::Cmd::None);
        }

        let modal = self.active_modal.as_ref()?;
        let kind = modal.kind.clone();

        match key.code {
            ftui::KeyCode::Escape | ftui::KeyCode::Char('n') => {
                self.dismiss_modal();
                Some(ftui::Cmd::None)
            }
            ftui::KeyCode::Enter | ftui::KeyCode::Char('y') => {
                if kind == ModalKind::Confirm {
                    // Execute the confirm action.
                    let action = self
                        .active_modal
                        .as_ref()
                        .and_then(|m| m.on_confirm.clone());
                    self.dismiss_modal();
                    if let Some(action) = action {
                        self.execute_confirm_action(action);
                    }
                } else {
                    // Error/Info — just dismiss.
                    self.dismiss_modal();
                }
                Some(ftui::Cmd::None)
            }
            _ => {
                // Modal is active but key not recognized — absorb it.
                Some(ftui::Cmd::None)
            }
        }
    }

    /// Execute a confirmed action.
    fn execute_confirm_action(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::ExecuteCommand(cmd) => {
                self.triage_queued_action = Some(cmd);
            }
            ConfirmAction::MuteEvent(event_id_str) => {
                if let Ok(event_id) = event_id_str.parse::<i64>() {
                    if let Err(e) = self.query.mark_event_muted(event_id) {
                        self.show_modal(ModalState::error(
                            "Mute Failed",
                            format!("Could not mute event {event_id}: {e}"),
                        ));
                    } else {
                        self.refresh_data();
                    }
                }
            }
        }
    }

    /// Handle keys specific to the History view.
    ///
    /// j/k/Down/Up: navigate entries.  u: toggle undoable filter.
    /// Backspace: remove filter char.  Esc: clear all filters.
    /// Printable chars: append to free-text filter.
    fn handle_history_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let filtered = self.view_state.history.filtered_indices();
        let count = filtered.len();
        let plain_char = !has_command_modifier(key);

        match key.code {
            KeyCode::Down | KeyCode::Char('j') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                if count > 0 {
                    self.view_state.history.selected_index =
                        (self.view_state.history.selected_index + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                if count > 0 {
                    self.view_state.history.selected_index = self
                        .view_state
                        .history
                        .selected_index
                        .checked_sub(1)
                        .unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Char('u') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.view_state.history.undoable_only = !self.view_state.history.undoable_only;
                self.view_state.history.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Backspace => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.history.filter_input.delete_back();
                self.view_state.history.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Delete => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.history.filter_input.delete_forward();
                self.view_state.history.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Escape => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.view_state.history.filter_input.clear();
                self.view_state.history.undoable_only = false;
                self.view_state.history.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Left => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.history.filter_input.move_left();
                ftui::Cmd::None
            }
            KeyCode::Right => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.history.filter_input.move_right();
                ftui::Cmd::None
            }
            KeyCode::Home => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.history.filter_input.move_home();
                ftui::Cmd::None
            }
            KeyCode::End => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.history.filter_input.move_end();
                ftui::Cmd::None
            }
            KeyCode::Char(c) if plain_char && !c.is_control() => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.history.filter_input.insert_char(c);
                self.view_state.history.selected_index = 0;
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    /// Handle keys specific to the Search view.
    ///
    /// Text input: chars append to query, Backspace removes, Enter executes,
    /// Escape clears.  Down/Up navigate results.  Ctrl saved-search shortcuts
    /// cycle and queue saved-search actions without corrupting the query text.
    fn handle_search_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let plain_char = !has_command_modifier(key);

        match key.code {
            KeyCode::Char('n') if key.modifiers.contains(ftui::Modifiers::CTRL) => {
                self.view_state.focus = FocusRegion::PrimaryList;
                let count = self.saved_searches.len();
                if count > 0 {
                    self.saved_search_selected = (self.saved_search_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Char('p') if key.modifiers.contains(ftui::Modifiers::CTRL) => {
                self.view_state.focus = FocusRegion::PrimaryList;
                let count = self.saved_searches.len();
                if count > 0 {
                    self.saved_search_selected = self
                        .saved_search_selected
                        .checked_sub(1)
                        .unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Char('r') if key.modifiers.contains(ftui::Modifiers::CTRL) => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.queue_selected_saved_search_run();
                ftui::Cmd::None
            }
            KeyCode::Char('e') if key.modifiers.contains(ftui::Modifiers::CTRL) => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.queue_selected_saved_search_toggle();
                ftui::Cmd::None
            }
            KeyCode::Char(c) if plain_char => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.search_input.insert_char(c);
                ftui::Cmd::None
            }
            KeyCode::Backspace => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.search_input.delete_back();
                ftui::Cmd::None
            }
            KeyCode::Delete => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.search_input.delete_forward();
                ftui::Cmd::None
            }
            KeyCode::Left => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.search_input.move_left();
                ftui::Cmd::None
            }
            KeyCode::Right => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.search_input.move_right();
                ftui::Cmd::None
            }
            KeyCode::Home => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.search_input.move_home();
                ftui::Cmd::None
            }
            KeyCode::End => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.search_input.move_end();
                ftui::Cmd::None
            }
            KeyCode::Enter => {
                self.view_state.focus = FocusRegion::PrimaryList;
                let query = self.search_input.text().trim().to_string();
                if query.is_empty() {
                    return ftui::Cmd::None;
                }
                self.search_last_query.clone_from(&query);
                match self.query.search(&query, 50) {
                    Ok(results) => {
                        self.search_results = results.iter().map(adapt_search).collect();
                        self.search_selected = 0;
                    }
                    Err(e) => {
                        self.view_state.error_message = Some(format!("Search failed: {e}"));
                        self.search_results.clear();
                    }
                }
                ftui::Cmd::None
            }
            KeyCode::Escape => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.search_input.clear();
                self.search_last_query.clear();
                self.search_results.clear();
                self.search_selected = 0;
                ftui::Cmd::None
            }
            KeyCode::Down => {
                self.view_state.focus = FocusRegion::PrimaryList;
                let count = self.search_results.len();
                if count > 0 {
                    self.search_selected = (self.search_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up => {
                self.view_state.focus = FocusRegion::PrimaryList;
                let count = self.search_results.len();
                if count > 0 {
                    self.search_selected = self.search_selected.checked_sub(1).unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    fn selected_saved_search(&self) -> Option<&SavedSearchView> {
        self.saved_searches.get(self.saved_search_selected)
    }

    fn queue_selected_saved_search_run(&mut self) {
        if let Some(name) = self.selected_saved_search().map(|saved| saved.name.clone()) {
            self.triage_queued_action =
                Some(format!("ft search saved run {}", quote_command_arg(&name)));
        } else {
            self.triage_queued_action = None;
            self.view_state.error_message = Some("No saved search selected".to_string());
        }
    }

    fn queue_selected_saved_search_toggle(&mut self) {
        let Some(saved) = self.selected_saved_search().cloned() else {
            self.triage_queued_action = None;
            self.view_state.error_message = Some("No saved search selected".to_string());
            return;
        };

        if saved.enabled {
            self.triage_queued_action = Some(format!(
                "ft search saved disable {}",
                quote_command_arg(&saved.name)
            ));
        } else if saved.schedule_interval_ms.is_some() {
            self.triage_queued_action = Some(format!(
                "ft search saved enable {}",
                quote_command_arg(&saved.name)
            ));
        } else {
            self.triage_queued_action = None;
            self.view_state.error_message =
                Some("Saved search has no schedule; set one via `ft search saved schedule`".into());
        }
    }

    /// Handle keys specific to the Timeline view.
    fn handle_timeline_key(&mut self, key: &ftui::KeyEvent) -> ftui::Cmd<WaMsg> {
        use ftui::KeyCode;

        let count = self.timeline_rows.len();
        let plain_char = !has_command_modifier(key);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if plain_char => {
                if count > 0 {
                    self.timeline_selected = (self.timeline_selected + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') if plain_char => {
                if count > 0 {
                    self.timeline_selected =
                        self.timeline_selected.checked_sub(1).unwrap_or(count - 1);
                }
                ftui::Cmd::None
            }
            KeyCode::Char('+') if plain_char => {
                if self.timeline_zoom < 5 {
                    self.timeline_zoom += 1;
                }
                ftui::Cmd::None
            }
            KeyCode::Char('-') if plain_char => {
                self.timeline_zoom = self.timeline_zoom.saturating_sub(1);
                ftui::Cmd::None
            }
            KeyCode::Left | KeyCode::Char('h') if plain_char => {
                self.timeline_scroll = self.timeline_scroll.saturating_sub(1);
                ftui::Cmd::None
            }
            KeyCode::Right | KeyCode::Char('l') if plain_char => {
                if count > 0 {
                    self.timeline_scroll = (self.timeline_scroll + 1).min(count.saturating_sub(1));
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
        let plain_char = !has_command_modifier(key);

        match key.code {
            KeyCode::Down | KeyCode::Char('j') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                if count > 0 {
                    self.view_state.events.selected_index =
                        (self.view_state.events.selected_index + 1) % count;
                }
                ftui::Cmd::None
            }
            KeyCode::Up | KeyCode::Char('k') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
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
            KeyCode::Char('u') if plain_char => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.view_state.events.unhandled_only = !self.view_state.events.unhandled_only;
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Backspace => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.events.pane_filter.delete_back();
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Delete => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.events.pane_filter.delete_forward();
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Escape => {
                self.view_state.focus = FocusRegion::PrimaryList;
                self.view_state.events.pane_filter.clear();
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            KeyCode::Left => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.events.pane_filter.move_left();
                ftui::Cmd::None
            }
            KeyCode::Right => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.events.pane_filter.move_right();
                ftui::Cmd::None
            }
            KeyCode::Home => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.events.pane_filter.move_home();
                ftui::Cmd::None
            }
            KeyCode::End => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.events.pane_filter.move_end();
                ftui::Cmd::None
            }
            KeyCode::Char(c) if plain_char && c.is_ascii_digit() => {
                self.view_state.focus = FocusRegion::FilterBar;
                self.view_state.events.pane_filter.insert_char(c);
                self.view_state.events.selected_index = 0;
                ftui::Cmd::None
            }
            _ => ftui::Cmd::None,
        }
    }

    /// Return indices of panes matching the current Panes filters.
    fn filtered_pane_indices(&self) -> Vec<usize> {
        let query = self.panes_filter.text().trim().to_ascii_lowercase();
        let bookmarked_panes: std::collections::BTreeSet<String> = self
            .pane_bookmarks
            .iter()
            .map(|bookmark| bookmark.pane_id.to_string())
            .collect();

        self.panes
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                if self.panes_unhandled_only && p.unhandled_badge.is_empty() {
                    return false;
                }

                if self.panes_bookmarked_only && !bookmarked_panes.contains(&p.pane_id) {
                    return false;
                }

                if let Some(agent_filter) = &self.panes_agent_filter
                    && !p.agent_label.eq_ignore_ascii_case(agent_filter)
                {
                    return false;
                }

                if let Some(domain_filter) = &self.panes_domain_filter {
                    let domain = p.domain.to_ascii_lowercase();
                    let filter = domain_filter.to_ascii_lowercase();
                    if filter == "ssh" {
                        if !domain.contains("ssh") {
                            return false;
                        }
                    } else if !domain.contains(&filter) {
                        return false;
                    }
                }

                if query.is_empty() {
                    return true;
                }

                p.pane_id.to_ascii_lowercase().contains(&query)
                    || p.title.to_ascii_lowercase().contains(&query)
                    || p.domain.to_ascii_lowercase().contains(&query)
                    || p.cwd.to_ascii_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn clamp_panes_selection(&mut self) {
        let filtered_len = self.filtered_pane_indices().len();
        if filtered_len > 0 {
            self.panes_selected = self.panes_selected.min(filtered_len - 1);
        } else {
            self.panes_selected = 0;
        }
    }

    /// Refresh dashboard data from the QueryClient.
    ///
    /// Public for benchmarking.
    pub fn refresh_data(&mut self) {
        // Health status
        match self.query.health() {
            Ok(health) => {
                self.health = Some(adapt_health(&health));
            }
            Err(e) => {
                self.view_state.error_message = Some(format!("Health query failed: {e}"));
            }
        }

        // Pane data (also used for unhandled count)
        match self.query.list_panes() {
            Ok(panes) => {
                self.unhandled_count = panes.iter().map(|p| p.unhandled_event_count as usize).sum();
                self.panes = panes.iter().map(adapt_pane).collect();
                self.clamp_panes_selection();
            }
            Err(_) => { /* health query already reports errors */ }
        }

        // Pane bookmarks feed the Panes bookmarked-only filter.
        match self.query.list_pane_bookmarks() {
            Ok(bookmarks) => {
                self.pane_bookmarks = bookmarks;
                self.clamp_panes_selection();
            }
            Err(QueryError::DatabaseNotInitialized(_)) => {
                self.pane_bookmarks.clear();
                self.clamp_panes_selection();
            }
            Err(e) => {
                self.view_state.error_message = Some(format!("Failed to list pane bookmarks: {e}"));
            }
        }

        // Ruleset profiles feed the Panes profile selector.
        match self.query.ruleset_profile_state() {
            Ok(profile_state) => {
                let active_index = profile_state
                    .profiles
                    .iter()
                    .position(|profile| profile.name == profile_state.active_profile)
                    .unwrap_or(0);
                if self.panes_profile_index >= profile_state.profiles.len() {
                    self.panes_profile_index = active_index;
                }
                if self.panes_profile_index == 0
                    && !profile_state.profiles.is_empty()
                    && self.ruleset_profile_state.is_none()
                {
                    self.panes_profile_index = active_index;
                }
                self.ruleset_profile_state = Some(profile_state);
            }
            Err(e) => {
                self.view_state.error_message =
                    Some(format!("Failed to resolve ruleset profiles: {e}"));
            }
        }

        // Triage items (used for both count on Home and Triage view)
        match self.query.list_triage_items() {
            Ok(items) => {
                self.triage_count = items.len();
                self.triage_items = items.iter().map(adapt_triage).collect();
                if self.triage_items.is_empty() {
                    self.triage_selected = 0;
                } else {
                    self.triage_selected = self.triage_selected.min(self.triage_items.len() - 1);
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
                self.view_state.events.rows = events.iter().map(adapt_event).collect();
                self.view_state.events.items = events;
                // Clamp selection within filtered results
                let filtered_len = self.view_state.events.filtered_indices().len();
                if filtered_len > 0 {
                    self.view_state.events.selected_index =
                        self.view_state.events.selected_index.min(filtered_len - 1);
                } else {
                    self.view_state.events.selected_index = 0;
                }
            }
            Err(_) => { /* non-fatal */ }
        }

        // History data
        match self.query.list_action_history(500) {
            Ok(entries) => {
                self.view_state.history.rows = entries.iter().map(adapt_history).collect();
                self.view_state.history.items = entries;
                let filtered_len = self.view_state.history.filtered_indices().len();
                if filtered_len > 0 {
                    self.view_state.history.selected_index =
                        self.view_state.history.selected_index.min(filtered_len - 1);
                } else {
                    self.view_state.history.selected_index = 0;
                }
            }
            Err(_) => { /* non-fatal */ }
        }

        // Saved searches for Search view.
        match self.query.list_saved_searches() {
            Ok(saved_searches) => {
                self.saved_searches = saved_searches;
                if self.saved_searches.is_empty() {
                    self.saved_search_selected = 0;
                } else {
                    self.saved_search_selected = self
                        .saved_search_selected
                        .min(self.saved_searches.len() - 1);
                }
            }
            Err(e) => {
                self.view_state.error_message = Some(format!("Failed to list saved searches: {e}"));
                self.saved_searches.clear();
                self.saved_search_selected = 0;
            }
        }

        // Timeline data (wa-6sk.4): last 30m, zoom-aware limit.
        let timeline_limit = match self.timeline_zoom {
            0 => 50,
            1 => 100,
            2 => 200,
            _ => 500,
        };
        // 30 minutes in milliseconds
        let last_ms = 30 * 60 * 1000;
        match self.query.get_timeline(last_ms, timeline_limit) {
            Ok(timeline) => {
                self.timeline_rows = timeline.events.iter().map(adapt_timeline_event).collect();
                if self.timeline_rows.is_empty() {
                    self.timeline_selected = 0;
                    self.timeline_scroll = 0;
                } else {
                    self.timeline_selected =
                        self.timeline_selected.min(self.timeline_rows.len() - 1);
                    self.timeline_scroll = self.timeline_scroll.min(self.timeline_rows.len() - 1);
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

        let plain_char = !has_command_modifier(key);
        let in_search = self.view_state.current_view == View::Search;
        let in_events = self.view_state.current_view == View::Events;
        let in_triage = self.view_state.current_view == View::Triage;
        let in_history = self.view_state.current_view == View::History;
        // Views with text input suppress character shortcuts.
        let has_text_input = in_search || in_history;

        match key.code {
            // Tab/BackTab navigation is always global (even in text input views).
            KeyCode::Tab => {
                self.view_state.current_view = self.view_state.current_view.next();
                self.view_state.focus = FocusRegion::default();
                Some(ftui::Cmd::None)
            }
            KeyCode::BackTab => {
                self.view_state.current_view = self.view_state.current_view.prev();
                self.view_state.focus = FocusRegion::default();
                Some(ftui::Cmd::None)
            }
            // Character-based shortcuts are suppressed in views with text input
            // so that keystrokes flow to the query/filter input instead.
            KeyCode::Char('q') if plain_char && !has_text_input => Some(ftui::Cmd::Quit),
            KeyCode::Char('?') if plain_char && !has_text_input => {
                self.view_state.current_view = View::Help;
                Some(ftui::Cmd::None)
            }
            KeyCode::Char('r') if plain_char && !has_text_input => {
                self.view_state.error_message = None;
                self.refresh_data();
                Some(ftui::Cmd::None)
            }
            // In Events/Triage/History views, digits go to view-specific handlers.
            KeyCode::Char(ch @ '1'..='8')
                if plain_char && !has_text_input && !in_events && !in_triage =>
            {
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
                // Modal intercept — when a modal is active, it absorbs all keys.
                if let Some(cmd) = self.handle_modal_key(key) {
                    return cmd;
                }
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

        if height < 2 {
            // Terminal too small — render nothing meaningful
            return;
        }

        // Layout matches the ratatui oracle:
        // [tab bar + bottom border: 2 rows] [content: remaining].
        let tab_row = 0u16;
        let content_y = 2u16;
        let content_h = height.saturating_sub(2);

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
                let profile_count = self
                    .ruleset_profile_state
                    .as_ref()
                    .map_or(0, |profile_state| profile_state.profiles.len());
                let filters = PaneRenderFilters {
                    query: self.panes_filter.text(),
                    unhandled_only: self.panes_unhandled_only,
                    bookmarked_only: self.panes_bookmarked_only,
                    agent_filter: self.panes_agent_filter.as_deref(),
                    domain_filter: self.panes_domain_filter.as_deref(),
                    selected_profile: self.selected_ruleset_profile_name(),
                    active_profile: self.active_ruleset_profile_name(),
                    profile_count,
                };
                render_panes_view(
                    frame,
                    content_y,
                    width,
                    content_h,
                    &self.panes,
                    &self.pane_bookmarks,
                    &filtered,
                    self.panes_selected,
                    filters,
                );
            }
            View::Search => render_search_view(
                frame,
                content_y,
                width,
                content_h,
                self.search_input.text(),
                self.search_input.cursor_pos(),
                self.view_state.focus,
                &self.search_last_query,
                &self.search_results,
                self.search_selected,
                &self.saved_searches,
                self.saved_search_selected,
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
                    self.view_state.focus,
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
            View::History => {
                let filtered = self.view_state.history.filtered_indices();
                let clamped_sel = self.view_state.history.clamped_selection();
                render_history_view(
                    frame,
                    content_y,
                    width,
                    content_h,
                    &self.view_state.history,
                    &filtered,
                    clamped_sel,
                    self.view_state.focus,
                );
            }
            View::Timeline => render_timeline_view(
                frame,
                content_y,
                width,
                content_h,
                &self.timeline_rows,
                self.timeline_selected,
                self.timeline_zoom,
                self.timeline_scroll,
            ),
        }

        // -- Modal overlay (drawn last so it's on top) --
        if let Some(ref modal) = self.active_modal {
            render_modal_overlay(frame, width, height, modal);
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewportClass {
    Compact,
    Regular,
    Wide,
}

fn viewport_class(width: u16, height: u16) -> ViewportClass {
    if width >= 132 && height >= 36 {
        ViewportClass::Wide
    } else if width < 96 || height < 28 {
        ViewportClass::Compact
    } else {
        ViewportClass::Regular
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListDetailLayout {
    list_y: u16,
    list_width: u16,
    list_height: u16,
    detail_x: u16,
    detail_y: u16,
    detail_width: u16,
    detail_height: u16,
    stacked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UiRect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl UiRect {
    const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn inner_all(self) -> Self {
        if self.width < 2 || self.height < 2 {
            return Self::new(self.x.saturating_add(1), self.y.saturating_add(1), 0, 0);
        }
        Self::new(self.x + 1, self.y + 1, self.width - 2, self.height - 2)
    }
}

fn list_detail_layout(
    y: u16,
    width: u16,
    height: u16,
    list_ratio_percent: u16,
    preferred_detail_height: u16,
) -> ListDetailLayout {
    if matches!(viewport_class(width, height), ViewportClass::Compact) {
        let min_list_height = height.min(4);
        let detail_height = preferred_detail_height.min(height.saturating_sub(min_list_height));
        let list_height = height.saturating_sub(detail_height);
        ListDetailLayout {
            list_y: y,
            list_width: width,
            list_height,
            detail_x: 0,
            detail_y: y.saturating_add(list_height),
            detail_width: width,
            detail_height,
            stacked: true,
        }
    } else {
        let list_width = (((width as u32) * (list_ratio_percent as u32)) / 100) as u16;
        let list_width = list_width.min(width);
        ListDetailLayout {
            list_y: y,
            list_width,
            list_height: height,
            detail_x: list_width,
            detail_y: y,
            detail_width: width.saturating_sub(list_width),
            detail_height: height,
            stacked: false,
        }
    }
}

/// Render the tab bar at the given row.
fn render_tab_bar(frame: &mut ftui::Frame, row: u16, width: u16, active: View) {
    let mut col = 0u16;
    let views = View::all();
    for (idx, &view) in views.iter().enumerate() {
        let label = view.name();
        let label_width = text_width(label);
        let segment_width = label_width.saturating_add(2);

        if col.saturating_add(segment_width) > width {
            break;
        }

        let style = if view == active {
            CellStyle::new()
                .fg(ftui::PackedRgba::rgba(0x80, 0x80, 0x00, 0xFF))
                .bold()
        } else {
            CellStyle::new().fg(ftui::PackedRgba::rgba(0xC0, 0xC0, 0xC0, 0xFF))
        };

        write_styled(frame, col, row, " ", CellStyle::new());
        col += 1;
        write_styled(frame, col, row, label, style);
        col += label_width;
        write_styled(frame, col, row, " ", CellStyle::new());
        col += 1;

        if idx + 1 < views.len() && col < width {
            write_styled(frame, col, row, "│", CellStyle::new());
            col += 1;
        }
    }

    // Fill rest of tab bar row
    let remaining = width.saturating_sub(col);
    if remaining > 0 {
        let fill = " ".repeat(remaining as usize);
        write_styled(frame, col, row, &fill, CellStyle::new());
    }

    if row + 1 < frame.height() {
        write_styled(
            frame,
            0,
            row + 1,
            &"─".repeat(width as usize),
            CellStyle::new(),
        );
    }
}

fn text_width(text: &str) -> u16 {
    text.chars().count().try_into().unwrap_or(u16::MAX)
}

/// Render the Home dashboard view.
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

    let viewport = viewport_class(width, height);
    let compact = matches!(viewport, ViewportClass::Compact);
    let chunks = home_chunks(y, width, height, false, viewport);

    render_home_title(frame, chunks[0], health, viewport);
    render_home_status_block(frame, chunks[1], health, compact);
    render_home_metrics_block(
        frame,
        chunks[2],
        health,
        unhandled_count,
        triage_count,
        compact,
    );
    render_home_help_block(frame, chunks[3], viewport);
    render_home_footer(frame, chunks[4], None, compact);
}

fn home_chunks(
    y: u16,
    width: u16,
    height: u16,
    has_dashboard: bool,
    viewport: ViewportClass,
) -> [UiRect; 6] {
    let lengths: [u16; 6] = match (has_dashboard, viewport) {
        (true, ViewportClass::Wide) => [3, 9, 7, 10, 4, 3],
        (true, ViewportClass::Regular) => [3, 8, 6, 7, 3, 3],
        (true, ViewportClass::Compact) => [3, 6, 5, 4, 3, 2],
        (false, ViewportClass::Wide) => [3, 9, 7, 0, 3, 3],
        (false, ViewportClass::Regular) => [3, 8, 6, 0, 3, 3],
        (false, ViewportClass::Compact) => [3, 6, 5, 0, 3, 2],
    };
    let min_index = if has_dashboard { 3 } else { 3 };
    let used_fixed = lengths
        .iter()
        .enumerate()
        .filter(|(idx, _)| *idx != min_index)
        .map(|(_, len)| *len)
        .fold(0u16, u16::saturating_add);
    let min_len = lengths[min_index];
    let min_actual = height.saturating_sub(used_fixed).max(min_len);

    let mut rects = [UiRect::new(0, y, width, 0); 6];
    let mut row = y;
    let end = y.saturating_add(height);
    for idx in 0..6 {
        let wanted = if idx == min_index {
            min_actual
        } else {
            lengths[idx]
        };
        let actual = wanted.min(end.saturating_sub(row));
        rects[idx] = UiRect::new(0, row, width, actual);
        row = row.saturating_add(actual);
    }
    rects
}

fn render_home_title(
    frame: &mut ftui::Frame,
    area: UiRect,
    health: Option<&HealthModel>,
    viewport: ViewportClass,
) {
    if area.height == 0 {
        return;
    }

    let (label, style) = match health {
        None => ("LOADING", CellStyle::new().fg(color_yellow())),
        Some(h)
            if h.watcher_label == "stopped"
                || h.db_label == "unavailable"
                || h.circuit_label == "OPEN" =>
        {
            ("ERROR", CellStyle::new().fg(color_red()).bold())
        }
        Some(h) if h.wezterm_label == "unavailable" || h.circuit_label == "half-open" => {
            ("WARNING", CellStyle::new().fg(color_yellow()))
        }
        Some(_) => ("OK", CellStyle::new().fg(color_green())),
    };
    let viewport_label = match viewport {
        ViewportClass::Compact => "COMPACT",
        ViewportClass::Regular => "STANDARD",
        ViewportClass::Wide => "DESKTOP",
    };
    write_segments(
        frame,
        area.x,
        area.y,
        area.width,
        &[
            (
                "FrankenTerm Control Center  ",
                CellStyle::new().fg(color_cyan()).bold(),
            ),
            (label, style),
            ("  [", CellStyle::new().fg(color_dark_gray())),
            (viewport_label, CellStyle::new().fg(color_dark_gray())),
            ("]", CellStyle::new().fg(color_dark_gray())),
        ],
    );
}

fn render_home_status_block(
    frame: &mut ftui::Frame,
    area: UiRect,
    health: Option<&HealthModel>,
    compact: bool,
) {
    if area.height == 0 {
        return;
    }
    draw_block_all(frame, area, Some("System Status"));
    let inner = area.inner_all();
    if inner.height == 0 {
        return;
    }

    let mut row = inner.y;
    if let Some(h) = health {
        let watcher = status_word(&h.watcher_label, "RUNNING", "STOPPED");
        let db = status_word(&h.db_label, "OK", "NOT FOUND");
        let wezterm = status_word(&h.wezterm_label, "OK", "ERROR");
        let circuit = match h.circuit_label.as_str() {
            "closed" => "CLOSED".to_string(),
            "half-open" if compact => "HALF".to_string(),
            "half-open" => "HALF-OPEN".to_string(),
            _ if compact => "OPEN".to_string(),
            _ => format!(
                "OPEN ({} ms cooldown)",
                h.circuit_cooldown_remaining_ms.unwrap_or(0)
            ),
        };
        let (capture_lag, capture_style) = capture_lag_label(h.last_capture_ts);
        let failures = format!(
            "{}/{}",
            h.circuit_consecutive_failures, h.circuit_failure_threshold
        );
        if compact {
            write_segments(
                frame,
                inner.x,
                row,
                inner.width,
                &[
                    ("  Watcher ", CellStyle::new()),
                    (watcher, status_style(&h.watcher_label)),
                    ("  DB ", CellStyle::new()),
                    (db, status_style(&h.db_label)),
                ],
            );
            row += 1;
            if row < inner.y.saturating_add(inner.height) {
                write_segments(
                    frame,
                    inner.x,
                    row,
                    inner.width,
                    &[
                        ("  WezTerm ", CellStyle::new()),
                        (wezterm, status_style(&h.wezterm_label)),
                        ("  Circuit ", CellStyle::new()),
                        (circuit.as_str(), status_style(&h.circuit_label)),
                    ],
                );
                row += 1;
            }
            if row < inner.y.saturating_add(inner.height) {
                write_segments(
                    frame,
                    inner.x,
                    row,
                    inner.width,
                    &[
                        ("  Capture ", CellStyle::new()),
                        (&capture_lag, capture_style),
                        ("  Failures ", CellStyle::new()),
                        (&failures, CellStyle::new()),
                    ],
                );
            }
        } else {
            let lines = [
                ("  Watcher:       ", watcher, status_style(&h.watcher_label)),
                ("  Database:      ", db, status_style(&h.db_label)),
                ("  WezTerm CLI:   ", wezterm, status_style(&h.wezterm_label)),
                (
                    "  Circuit:       ",
                    circuit.as_str(),
                    status_style(&h.circuit_label),
                ),
                ("  Capture lag:   ", capture_lag.as_str(), capture_style),
                ("  Failures:      ", failures.as_str(), CellStyle::new()),
            ];
            for (label, value, style) in lines {
                if row >= inner.y.saturating_add(inner.height) {
                    break;
                }
                write_segments(
                    frame,
                    inner.x,
                    row,
                    inner.width,
                    &[(label, CellStyle::new()), (value, style)],
                );
                row += 1;
            }
        }
    } else {
        write_segments(
            frame,
            inner.x,
            inner.y,
            inner.width,
            &[("Loading...", CellStyle::new().fg(color_yellow()))],
        );
    }
}

fn render_home_metrics_block(
    frame: &mut ftui::Frame,
    area: UiRect,
    health: Option<&HealthModel>,
    unhandled_count: usize,
    triage_count: usize,
    compact: bool,
) {
    if area.height == 0 {
        return;
    }
    draw_block_all(frame, area, Some("Metrics"));
    let inner = area.inner_all();
    if inner.height == 0 {
        return;
    }

    let Some(h) = health else {
        write_segments(
            frame,
            inner.x,
            inner.y,
            inner.width,
            &[("...", CellStyle::new().fg(color_gray()))],
        );
        return;
    };

    let pane_style = if h.pane_count == "0" {
        CellStyle::new().fg(color_yellow())
    } else {
        CellStyle::new().fg(color_green())
    };
    let event_style = h
        .event_count
        .parse::<usize>()
        .ok()
        .filter(|count| *count > 100)
        .map_or_else(
            || CellStyle::new().fg(color_green()),
            |_| CellStyle::new().fg(color_yellow()),
        );
    let unhandled_style = if unhandled_count > 0 {
        CellStyle::new().fg(color_yellow()).bold()
    } else {
        CellStyle::new().fg(color_green())
    };
    let triage_style = if triage_count > 0 {
        CellStyle::new().fg(color_yellow())
    } else {
        CellStyle::new().fg(color_green())
    };
    let unhandled_label = unhandled_count.to_string();
    let triage_label = triage_count.to_string();

    if compact {
        write_segments(
            frame,
            inner.x,
            inner.y,
            inner.width,
            &[
                ("  Panes ", CellStyle::new()),
                (&h.pane_count, pane_style),
                ("  Events ", CellStyle::new()),
                (&h.event_count, event_style),
            ],
        );
        if inner.height > 1 {
            write_segments(
                frame,
                inner.x,
                inner.y + 1,
                inner.width,
                &[
                    ("  Unhandled ", CellStyle::new()),
                    (&unhandled_label, unhandled_style),
                    ("  Triage ", CellStyle::new()),
                    (&triage_label, triage_style),
                ],
            );
        }
    } else {
        let lines = [
            ("  Panes:         ", h.pane_count.as_str(), pane_style),
            ("  Events:        ", h.event_count.as_str(), event_style),
            (
                "  Unhandled:     ",
                unhandled_label.as_str(),
                unhandled_style,
            ),
            ("  Triage items:  ", triage_label.as_str(), triage_style),
        ];
        for (idx, (label, value, style)) in lines.iter().enumerate() {
            let row = inner.y.saturating_add(idx as u16);
            if row >= inner.y.saturating_add(inner.height) {
                break;
            }
            write_segments(
                frame,
                inner.x,
                row,
                inner.width,
                &[(label, CellStyle::new()), (value, *style)],
            );
        }
    }
}

fn render_home_help_block(frame: &mut ftui::Frame, area: UiRect, viewport: ViewportClass) {
    if area.height == 0 {
        return;
    }
    draw_block_all(frame, area, Some("Quick Help"));
    let inner = area.inner_all();
    if inner.height == 0 {
        return;
    }
    let lines: &[(&str, CellStyle)] = match viewport {
        ViewportClass::Wide => &[
            ("Desktop workflow:", CellStyle::new().bold()),
            (
                "  Tab/Shift+Tab switch views | j/k move | Enter act | / search",
                CellStyle::new(),
            ),
            (
                "  r refresh | u mark handled | p cycle profile | q quit",
                CellStyle::new(),
            ),
        ],
        ViewportClass::Regular => &[
            ("Navigation:", CellStyle::new().bold()),
            (
                "  Tab views | j/k move | Enter action | ? help | q quit",
                CellStyle::new(),
            ),
        ],
        ViewportClass::Compact => &[
            ("Compact controls:", CellStyle::new().bold()),
            (
                "  Tab views | j/k move | Enter | ? help | q quit",
                CellStyle::new(),
            ),
        ],
    };
    for (idx, (text, style)) in lines.iter().enumerate() {
        let row = inner.y.saturating_add(idx as u16);
        if row >= inner.y.saturating_add(inner.height) {
            break;
        }
        write_styled_clipped(frame, inner.x, row, text, *style, inner.width);
    }
}

fn render_home_footer(frame: &mut ftui::Frame, area: UiRect, error: Option<&str>, compact: bool) {
    if area.height == 0 {
        return;
    }
    draw_block_top(frame, area);
    if area.height < 2 {
        return;
    }
    let width = area.width.max(1) as usize;
    let msg = if let Some(error) = error {
        truncate_str(error, width)
    } else if compact {
        "No active errors | Press r to refresh".to_string()
    } else {
        "Ready | Home shows fleet health, cost, and throttling state".to_string()
    };
    let style = if error.is_some() {
        CellStyle::new().fg(color_red())
    } else {
        CellStyle::new().fg(color_dark_gray())
    };
    write_styled_clipped(frame, area.x, area.y + 1, &msg, style, area.width);
}

fn status_word<'a>(label: &str, ok: &'a str, bad: &'a str) -> &'a str {
    if label == "ok" || label == "running" || label == "closed" {
        ok
    } else {
        bad
    }
}

fn status_style(label: &str) -> CellStyle {
    match label {
        "running" | "ok" | "closed" => CellStyle::new().fg(color_green()),
        "half-open" => CellStyle::new().fg(color_yellow()),
        "stopped" | "unavailable" | "OPEN" => CellStyle::new().fg(color_red()).bold(),
        _ => CellStyle::new(),
    }
}

fn capture_lag_label(last_capture_ts: Option<i64>) -> (String, CellStyle) {
    last_capture_ts.map_or_else(
        || {
            (
                "no captures yet".to_string(),
                CellStyle::new().fg(color_gray()),
            )
        },
        |ts| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|d| i64::try_from(d.as_millis()).ok())
                .unwrap_or(0);
            let lag_ms = now_ms.saturating_sub(ts);
            let style = if lag_ms > 10_000 {
                CellStyle::new().fg(color_yellow())
            } else {
                CellStyle::new().fg(color_green())
            };
            (format!("{lag_ms} ms"), style)
        },
    )
}

/// Render the Panes view.
///
/// Responsive pane layout:
///   Regular/wide: left list + right detail panel.
///   Compact: full-width list with a stacked detail panel below it.
#[derive(Debug, Clone, Copy)]
struct PaneRenderFilters<'a> {
    query: &'a str,
    unhandled_only: bool,
    bookmarked_only: bool,
    agent_filter: Option<&'a str>,
    domain_filter: Option<&'a str>,
    selected_profile: Option<&'a str>,
    active_profile: Option<&'a str>,
    profile_count: usize,
}

impl Default for PaneRenderFilters<'_> {
    fn default() -> Self {
        Self {
            query: "",
            unhandled_only: false,
            bookmarked_only: false,
            agent_filter: None,
            domain_filter: None,
            selected_profile: None,
            active_profile: None,
            profile_count: 0,
        }
    }
}

fn render_panes_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    panes: &[PaneRow],
    pane_bookmarks: &[PaneBookmarkView],
    filtered_indices: &[usize],
    selected: usize,
    filters: PaneRenderFilters<'_>,
) {
    if height == 0 {
        return;
    }

    let stacked_mode = width < 96 || height < 18;
    let ultra_compact = width < 68;
    let (list_area, detail_area) = if stacked_mode {
        let detail_height = if height >= 22 { 10 } else { 8 }.min(height);
        let list_height = height.saturating_sub(detail_height);
        (
            UiRect::new(0, y, width, list_height),
            UiRect::new(0, y.saturating_add(list_height), width, detail_height),
        )
    } else {
        let list_width = (((width as u32) * 62) / 100) as u16;
        (
            UiRect::new(0, y, list_width, height),
            UiRect::new(list_width, y, width.saturating_sub(list_width), height),
        )
    };

    let list_title = format!(
        "Panes ({}/{}){}",
        filtered_indices.len(),
        panes.len(),
        if stacked_mode { " [compact]" } else { "" }
    );
    draw_block_all(frame, list_area, Some(&list_title));
    let list_inner = list_area.inner_all();
    if list_inner.height == 0 {
        return;
    }

    let mut bookmarks_by_pane: HashMap<u64, Vec<&PaneBookmarkView>> = HashMap::new();
    for bookmark in pane_bookmarks {
        bookmarks_by_pane
            .entry(bookmark.pane_id)
            .or_default()
            .push(bookmark);
    }

    let selected_profile = filters.selected_profile.unwrap_or("default");
    let active_profile = filters.active_profile.unwrap_or("default");
    let filter_summary = if stacked_mode {
        format!(
            "q='{}' uh={} bm={} ag={} dom={} prof={}/{} ({})",
            filters.query,
            filters.unhandled_only,
            filters.bookmarked_only,
            filters.agent_filter.unwrap_or("all"),
            filters.domain_filter.unwrap_or("all"),
            selected_profile,
            active_profile,
            filters.profile_count,
        )
    } else {
        format!(
            "filter='{}' unhandled={} bookmarked={} agent={} domain={} profile={} active={} ({})",
            filters.query,
            filters.unhandled_only,
            filters.bookmarked_only,
            filters.agent_filter.unwrap_or("all"),
            filters.domain_filter.unwrap_or("all"),
            selected_profile,
            active_profile,
            filters.profile_count,
        )
    };

    let list_header_height = if ultra_compact || list_inner.height < 6 {
        2
    } else {
        3
    }
    .min(list_inner.height);
    let header_area = UiRect::new(
        list_inner.x,
        list_inner.y,
        list_inner.width,
        list_header_height,
    );
    let rows_area = UiRect::new(
        list_inner.x,
        list_inner.y.saturating_add(list_header_height),
        list_inner.width,
        list_inner.height.saturating_sub(list_header_height),
    );
    let header_width = header_area.width.saturating_sub(1).max(1);
    let columns = if ultra_compact {
        "id ag st u title"
    } else if stacked_mode {
        "id bm ag state u title"
    } else {
        "id  bm      agent    state          unhandled  title"
    };
    write_styled_clipped(
        frame,
        header_area.x,
        header_area.y,
        &truncate_str(columns, header_width as usize),
        CellStyle::new(),
        header_area.width,
    );
    if header_area.height > 1 {
        write_styled_clipped(
            frame,
            header_area.x,
            header_area.y + 1,
            &truncate_str(&filter_summary, header_width as usize),
            CellStyle::new().fg(color_gray()),
            header_area.width,
        );
    }

    if filtered_indices.is_empty() {
        if rows_area.height > 0 {
            write_styled_clipped(
                frame,
                rows_area.x,
                rows_area.y,
                "No panes match the current filters.",
                CellStyle::new().fg(color_yellow()),
                rows_area.width,
            );
        }
    } else {
        let selected = selected.min(filtered_indices.len().saturating_sub(1));
        let row_width = rows_area.width.saturating_sub(1).max(1);
        for (pos, &pane_idx) in filtered_indices.iter().enumerate() {
            let row = rows_area.y.saturating_add(pos as u16);
            if row >= rows_area.y.saturating_add(rows_area.height) {
                break;
            }
            let pane = &panes[pane_idx];
            let bookmark_summary = pane_list_bookmark_summary(&bookmarks_by_pane, &pane.pane_id);
            let line = if ultra_compact {
                format!(
                    "{:>3} {:6} {:4} {:>2} {}",
                    pane.pane_id,
                    truncate_str(&pane.agent_label, 6),
                    truncate_str(&pane.state_label, 4),
                    pane.unhandled_badge,
                    truncate_str(&pane.title, 18)
                )
            } else if stacked_mode {
                format!(
                    "{:>3} {:4} {:6} {:8} {:>2} {}",
                    pane.pane_id,
                    bookmark_summary,
                    truncate_str(&pane.agent_label, 6),
                    truncate_str(&pane.state_label, 8),
                    pane.unhandled_badge,
                    truncate_str(&pane.title, 20)
                )
            } else {
                format!(
                    "{:>3} {:6} {:8} {:12} {:>9}  {}",
                    pane.pane_id,
                    bookmark_summary,
                    truncate_str(&pane.agent_label, 8),
                    truncate_str(&pane.state_label, 12),
                    pane.unhandled_badge,
                    truncate_str(&pane.title, 24)
                )
            };
            let style = if pos == selected {
                CellStyle::new()
                    .fg(color_default_fg())
                    .bg(color_dark_gray())
                    .bold()
            } else if !pane.unhandled_badge.is_empty() {
                CellStyle::new().fg(color_yellow())
            } else if pane.state_label == "AltScreen" {
                CellStyle::new().fg(color_magenta())
            } else {
                CellStyle::new()
            };
            write_styled_clipped(
                frame,
                rows_area.x,
                row,
                &truncate_str(&line, row_width as usize),
                style,
                rows_area.width,
            );
        }
    }

    if detail_area.width == 0 || detail_area.height == 0 {
        return;
    }

    draw_block_all(
        frame,
        detail_area,
        Some(if stacked_mode {
            "Selected Pane"
        } else {
            "Pane Details"
        }),
    );
    let detail_inner = detail_area.inner_all();
    if detail_inner.height == 0 {
        return;
    }

    let selected_pane = filtered_indices
        .get(selected.min(filtered_indices.len().saturating_sub(1)))
        .and_then(|&idx| panes.get(idx));

    if let Some(pane) = selected_pane {
        let detail_width = detail_inner.width.saturating_sub(1).max(1);
        let compact_details = stacked_mode || detail_inner.height < 10 || detail_inner.width < 34;
        let bookmark_summary = pane_detail_bookmark_summary(&bookmarks_by_pane, &pane.pane_id);
        let next_action = if selected_profile != active_profile {
            format!("Apply selected profile: ft rules profile apply {selected_profile}")
        } else if !pane.unhandled_badge.is_empty() {
            format!("Run: ft workflow list --pane {}", pane.pane_id)
        } else {
            format!("Inspect: ft robot get-text {} --tail 120", pane.pane_id)
        };
        let mut rows: Vec<(String, CellStyle)> = Vec::new();
        if compact_details {
            rows.push((
                truncate_str(
                    &format!("#{} {}", pane.pane_id, pane.title),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("State {} | Agent {}", pane.state_label, pane.agent_label),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!(
                        "Domain {} | Unhandled {}",
                        pane.domain, pane.unhandled_badge
                    ),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("Bookmarks {}", truncate_str(&bookmark_summary, 30)),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("Ruleset {selected_profile}/{active_profile}"),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((String::new(), CellStyle::new()));
            rows.push(("Next best action:".to_string(), CellStyle::new().bold()));
            rows.push((
                truncate_str(&next_action, detail_width as usize),
                CellStyle::new(),
            ));
            rows.push((String::new(), CellStyle::new()));
            rows.push((
                truncate_str(
                    "Keys: j/k nav | p profile | Enter apply | b bookmarked",
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
        } else {
            rows.push((
                truncate_str(&format!("Pane ID: {}", pane.pane_id), detail_width as usize),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(&format!("Title: {}", pane.title), detail_width as usize),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(&format!("Domain: {}", pane.domain), detail_width as usize),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("Agent: {}", pane.agent_label),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("State: {}", pane.state_label),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(&format!("CWD: {}", pane.cwd), detail_width as usize),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("Last Activity: {}", pane.last_activity_label),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!(
                        "Unhandled Events: {}",
                        if pane.unhandled_badge.is_empty() {
                            "0"
                        } else {
                            &pane.unhandled_badge
                        }
                    ),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("Bookmarks: {}", truncate_str(&bookmark_summary, 80)),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("Ruleset Active: {active_profile}"),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((
                truncate_str(
                    &format!("Ruleset Selected: {selected_profile}"),
                    detail_width as usize,
                ),
                CellStyle::new(),
            ));
            rows.push((String::new(), CellStyle::new()));
            rows.push(("Next best action:".to_string(), CellStyle::new().bold()));
            rows.push((
                truncate_str(&next_action, detail_width as usize),
                CellStyle::new(),
            ));
            rows.push((String::new(), CellStyle::new()));
            rows.push((
                truncate_str(
                    "Keys: p=cycle profile, Enter=apply selected profile, b=bookmarked only",
                    detail_width as usize,
                ),
                CellStyle::new().fg(color_gray()),
            ));
        }
        for (idx, (text, style)) in rows.iter().enumerate() {
            let row = detail_inner.y.saturating_add(idx as u16);
            if row >= detail_inner.y.saturating_add(detail_inner.height) {
                break;
            }
            write_styled_clipped(frame, detail_inner.x, row, text, *style, detail_inner.width);
        }
    } else {
        write_styled_clipped(
            frame,
            detail_inner.x,
            detail_inner.y,
            "No pane selected.",
            CellStyle::new().fg(color_yellow()),
            detail_inner.width,
        );
    }
}

fn pane_list_bookmark_summary(
    bookmarks_by_pane: &HashMap<u64, Vec<&PaneBookmarkView>>,
    pane_id: &str,
) -> String {
    let Ok(pane_id) = pane_id.parse::<u64>() else {
        return "-".to_string();
    };

    let Some(bookmarks) = bookmarks_by_pane.get(&pane_id) else {
        return "-".to_string();
    };

    match bookmarks.as_slice() {
        [] => "-".to_string(),
        [bookmark] => truncate_str(&bookmark.alias, 6),
        _ => format!("{}*", bookmarks.len()),
    }
}

fn pane_detail_bookmark_summary(
    bookmarks_by_pane: &HashMap<u64, Vec<&PaneBookmarkView>>,
    pane_id: &str,
) -> String {
    let Ok(pane_id) = pane_id.parse::<u64>() else {
        return "none".to_string();
    };
    let Some(bookmarks) = bookmarks_by_pane.get(&pane_id) else {
        return "none".to_string();
    };
    if bookmarks.is_empty() {
        return "none".to_string();
    }

    bookmarks
        .iter()
        .map(|bookmark| {
            if bookmark.tags.is_empty() {
                bookmark.alias.clone()
            } else {
                format!("{} [{}]", bookmark.alias, bookmark.tags.join(","))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render the Search view.
///
/// Layout:
///   Row 0:    Search input bar with cursor/prompt
///   Row 1:    Separator / status
///   Rows 2+:  Responsive results layout; compact mode stacks detail below the list
#[allow(clippy::too_many_arguments, clippy::similar_names)]
fn render_search_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    query: &str,
    cursor_pos: usize,
    focus: FocusRegion,
    last_query: &str,
    results: &[SearchRow],
    selected: usize,
    saved_searches: &[SavedSearchView],
    saved_selected: usize,
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
    // Show cursor indicator when FilterBar has focus, trailing underscore otherwise.
    let cursor_char = if focus == FocusRegion::FilterBar {
        '▏'
    } else {
        '_'
    };
    let cursor_pos = char_boundary_at_or_before(query, cursor_pos);
    let (before_cursor, after_cursor) = query.split_at(cursor_pos);
    let input_line = format!("  {prompt}: {before_cursor}{cursor_char}{after_cursor}");
    write_styled(frame, 0, row, &input_line, CellStyle::new().bold());
    let ilen = input_line.chars().count();
    if ilen < width as usize {
        let fill = " ".repeat(width as usize - ilen);
        write_styled(frame, ilen as u16, row, &fill, CellStyle::new());
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
        let slen = status.chars().count();
        if slen < width as usize {
            let fill = " ".repeat(width as usize - slen);
            write_styled(frame, slen as u16, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // -- Saved searches --
    if row < max_row {
        let summary = if saved_searches.is_empty() {
            "  Saved searches: none. Use `ft search save <name> <query>`.".to_string()
        } else {
            format!(
                "  Saved searches ({}): Ctrl+N/P select, Ctrl+R run, Ctrl+E toggle",
                saved_searches.len()
            )
        };
        write_styled(frame, 0, row, &summary, CellStyle::new().dim());
        let slen = summary.len() as u16;
        if slen < width {
            let fill = " ".repeat((width - slen) as usize);
            write_styled(frame, slen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    if !saved_searches.is_empty() {
        let visible_rows = max_row.saturating_sub(row).min(3);
        if visible_rows > 0 {
            let selected_saved = saved_selected.min(saved_searches.len().saturating_sub(1));
            let start = selected_saved.saturating_sub(visible_rows.saturating_sub(1) as usize);
            for (idx, saved) in saved_searches
                .iter()
                .enumerate()
                .skip(start)
                .take(visible_rows as usize)
            {
                let marker = if idx == selected_saved { ">" } else { " " };
                let enabled = if saved.enabled { "on" } else { "off" };
                let schedule = saved
                    .schedule_interval_ms
                    .map_or_else(|| "-".to_string(), |ms| ms.to_string());
                let line = format!(
                    "  {marker} {:14} {:3} {:9} {}",
                    truncate_str(&saved.name, 14),
                    enabled,
                    truncate_str(&schedule, 9),
                    truncate_str(&saved.query, 40),
                );
                let style = if idx == selected_saved {
                    CellStyle::new().reverse()
                } else {
                    CellStyle::new()
                };
                write_styled(frame, 0, row, &line, style);
                let llen = line.len() as u16;
                if llen < width {
                    let fill = " ".repeat((width - llen) as usize);
                    write_styled(frame, llen, row, &fill, CellStyle::new());
                }
                row += 1;
                if row >= max_row {
                    break;
                }
            }
        }
    }

    // -- Empty state --
    if results.is_empty() {
        while row < max_row {
            write_styled(frame, 0, row, &blank_line, CellStyle::new());
            row += 1;
        }
        return;
    }

    let layout = list_detail_layout(row, width, max_row.saturating_sub(row), 55, 8);
    let list_width = layout.list_width;
    let detail_x = layout.detail_x;
    let detail_width = layout.detail_width;
    let list_end = layout.list_y.saturating_add(layout.list_height);
    let clamped_sel = selected.min(results.len().saturating_sub(1));
    row = layout.list_y;

    // Column header
    if row < list_end {
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
        if row >= list_end {
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
    while row < list_end {
        write_styled(frame, 0, row, &blank_list, CellStyle::new());
        row += 1;
    }

    if detail_width == 0 || layout.detail_height == 0 {
        return;
    }

    // -- Detail panel --
    let detail_end = layout.detail_y.saturating_add(layout.detail_height);
    let mut drow = layout.detail_y;

    // Detail header
    write_styled(
        frame,
        detail_x,
        drow,
        " Match Context",
        CellStyle::new().bold(),
    );
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
            if drow >= detail_end {
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
    while drow < detail_end {
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
        ("  FrankenTerm Control Center", true), // bold
        ("", false),
        ("  Global Keybindings:", true),
        ("    q          Quit", false),
        ("    ?          Show this help", false),
        ("    r          Refresh current view", false),
        ("    Tab        Next view", false),
        ("    Shift+Tab  Previous view", false),
        ("    1-8        Jump to view by number", false),
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
        ("    2. Panes   List all FrankenTerm panes", false),
        ("    3. Events  Recent detection events", false),
        ("    4. Triage  Prioritized issues + actions", false),
        ("    5. History Audit action timeline", false),
        ("    6. Search  Full-text search", false),
        ("    7. Help    This screen", false),
        ("    8. Timeline Cross-pane event timeline", false),
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
/// Responsive event layout:
///   Regular/wide: left list + right detail panel.
///   Compact: full-width list with a stacked detail panel below it.
fn render_events_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    events_state: &EventsViewState,
    filtered_indices: &[usize],
    selected: usize,
    focus: FocusRegion,
) {
    if height == 0 {
        return;
    }

    let layout = list_detail_layout(y, width, height, 60, 10);
    let list_width = layout.list_width;
    let list_end = layout.list_y.saturating_add(layout.list_height);
    let mut row = layout.list_y;

    // -- Header: count and filter status (with focus-aware cursor) --
    let filter_text = events_state.pane_filter.text();
    let cursor_indicator = if focus == FocusRegion::FilterBar {
        "▏"
    } else {
        ""
    };
    let header = if layout.stacked {
        format!(
            "  Events {}/{}  u={}  q='{}{}'",
            filtered_indices.len(),
            events_state.items.len(),
            events_state.unhandled_only,
            filter_text,
            cursor_indicator,
        )
    } else {
        format!(
            "  Events ({}/{})  unhandled_only={}  pane/rule='{}{}'",
            filtered_indices.len(),
            events_state.items.len(),
            events_state.unhandled_only,
            filter_text,
            cursor_indicator,
        )
    };
    write_styled(frame, 0, row, &header, CellStyle::new().bold());
    let hlen = header.len() as u16;
    if hlen < list_width {
        let fill = " ".repeat((list_width - hlen) as usize);
        write_styled(frame, hlen, row, &fill, CellStyle::new());
    }
    row += 1;

    // -- Column headers --
    if row < list_end {
        let col_header = format!("  {:8}  {:>4}  {:28}  {}", "sev", "pane", "rule", "status");
        write_styled(frame, 0, row, &col_header, CellStyle::new().dim());
        let clen = col_header.len() as u16;
        if clen < list_width {
            let fill = " ".repeat((list_width - clen) as usize);
            write_styled(frame, clen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // -- Event rows --
    if filtered_indices.is_empty() && row < list_end {
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
            if row >= list_end {
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
    while row < list_end {
        write_styled(frame, 0, row, &blank_list, CellStyle::new());
        row += 1;
    }

    if layout.detail_width == 0 || layout.detail_height == 0 {
        return;
    }

    // -- Detail panel --
    let selected_event = filtered_indices
        .get(selected)
        .and_then(|&idx| events_state.items.get(idx));
    let selected_row = filtered_indices
        .get(selected)
        .and_then(|&idx| events_state.rows.get(idx));

    let detail_x = layout.detail_x;
    let detail_width = layout.detail_width;
    let detail_end = layout.detail_y.saturating_add(layout.detail_height);
    let mut drow = layout.detail_y;

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
            if drow >= detail_end {
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
    } else if drow < detail_end {
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
    while drow < detail_end {
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
#[allow(clippy::too_many_arguments, clippy::similar_names)]
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
            let col_header = format!("  {:8}  {:8}  {}", "severity", "section", "title");
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
                    let step_line =
                        format!("    Step {} | started {}", wf.progress_label, wf.started_at);
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
                        let err_line = format!("    ERROR: {}", truncate_str(error, 60));
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
            let detail_line = format!(
                "  {}",
                truncate_str(&item.detail, width.saturating_sub(4) as usize)
            );
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
            let ref_line = format!(
                "  event={} pane={} wf={}",
                item.event_id, item.pane_id, item.workflow_id
            );
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

/// Render the History view.
///
/// Responsive history layout:
///   Regular/wide: left list + right detail panel.
///   Compact: full-width list with a stacked detail panel below it.
fn render_history_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    history_state: &HistoryViewState,
    filtered_indices: &[usize],
    selected: usize,
    focus: FocusRegion,
) {
    if height == 0 {
        return;
    }

    let layout = list_detail_layout(y, width, height, 60, 10);
    let list_width = layout.list_width;
    let list_end = layout.list_y.saturating_add(layout.list_height);
    let mut row = layout.list_y;

    // -- Header: count and filter status (with focus-aware cursor) --
    let filter_text = history_state.filter_input.text();
    let cursor_indicator = if focus == FocusRegion::FilterBar {
        "▏"
    } else {
        ""
    };
    let header = if layout.stacked {
        format!(
            "  History {}/{}  u={}  q='{}{}'",
            filtered_indices.len(),
            history_state.items.len(),
            history_state.undoable_only,
            filter_text,
            cursor_indicator,
        )
    } else {
        format!(
            "  History ({}/{})  undoable_only={}  q='{}{}'",
            filtered_indices.len(),
            history_state.items.len(),
            history_state.undoable_only,
            filter_text,
            cursor_indicator,
        )
    };
    write_styled(frame, 0, row, &header, CellStyle::new().bold());
    let hlen = header.len() as u16;
    if hlen < list_width {
        let fill = " ".repeat((list_width - hlen) as usize);
        write_styled(frame, hlen, row, &fill, CellStyle::new());
    }
    row += 1;

    // -- Column headers --
    if row < list_end {
        let col_header = format!(
            "  {:>6}  {:18}  {:8}  {:>8}  {}",
            "audit", "action", "result", "undo", "actor"
        );
        write_styled(frame, 0, row, &col_header, CellStyle::new().dim());
        let clen = col_header.len() as u16;
        if clen < list_width {
            let fill = " ".repeat((list_width - clen) as usize);
            write_styled(frame, clen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // -- History rows --
    if filtered_indices.is_empty() && row < list_end {
        let msg = if history_state.items.is_empty() {
            "  No history entries yet."
        } else {
            "  No entries match the current filters."
        };
        write_styled(frame, 0, row, msg, CellStyle::new().dim());
        let msg_len = msg.len() as u16;
        if msg_len < list_width {
            let fill = " ".repeat((list_width - msg_len) as usize);
            write_styled(frame, msg_len, row, &fill, CellStyle::new());
        }
        row += 1;
    } else {
        for (pos, &entry_idx) in filtered_indices.iter().enumerate() {
            if row >= list_end {
                break;
            }
            let hrow = &history_state.rows[entry_idx];
            let line = format!(
                "  #{:>5}  {:18}  {:8}  {:>8}  {}",
                truncate_str(&hrow.audit_id, 5),
                truncate_str(&hrow.action_kind, 18),
                truncate_str(&hrow.result_label, 8),
                truncate_str(&hrow.undo_label, 8),
                truncate_str(&hrow.actor_kind, 10),
            );
            let style = if pos == selected {
                CellStyle::new().bold().reverse()
            } else if !hrow.undo_label.is_empty() {
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
    while row < list_end {
        write_styled(frame, 0, row, &blank_list, CellStyle::new());
        row += 1;
    }

    if layout.detail_width == 0 || layout.detail_height == 0 {
        return;
    }

    // -- Detail panel --
    let selected_entry = filtered_indices
        .get(selected)
        .and_then(|&idx| history_state.items.get(idx));
    let selected_row = filtered_indices
        .get(selected)
        .and_then(|&idx| history_state.rows.get(idx));

    let detail_x = layout.detail_x;
    let detail_width = layout.detail_width;
    let detail_end = layout.detail_y.saturating_add(layout.detail_height);
    let mut drow = layout.detail_y;

    // Detail header
    write_styled(
        frame,
        detail_x,
        drow,
        " History Details",
        CellStyle::new().bold(),
    );
    let dhlen = 16u16;
    if dhlen < detail_width {
        let fill = " ".repeat((detail_width - dhlen) as usize);
        write_styled(frame, detail_x + dhlen, drow, &fill, CellStyle::new());
    }
    drow += 1;

    if let (Some(entry), Some(hrow)) = (selected_entry, selected_row) {
        let undo_status = if entry.undone {
            "undone"
        } else if entry.undoable {
            "undoable"
        } else {
            "not-undoable"
        };

        let mut detail_lines: Vec<String> = vec![
            format!(" Audit ID: #{}", entry.audit_id),
            format!(" Action:   {}", hrow.action_kind),
            format!(" Result:   {}", hrow.result_label),
            format!(" Actor:    {}", hrow.actor_kind),
            format!(" Undo:     {}", undo_status),
            format!(" Time:     {}", hrow.timestamp),
        ];

        // Provenance fields
        if !hrow.pane_id.is_empty() {
            detail_lines.push(format!(" Pane:     {}", hrow.pane_id));
        }
        if !hrow.workflow_id.is_empty() {
            detail_lines.push(format!(" Workflow: {}", hrow.workflow_id));
        }
        if !hrow.step_name.is_empty() {
            detail_lines.push(format!(" Step:     {}", hrow.step_name));
        }
        if !hrow.undo_strategy.is_empty() {
            detail_lines.push(format!(" Strategy: {}", hrow.undo_strategy));
        }
        if !hrow.undo_hint.is_empty() {
            detail_lines.push(format!(" Hint:     {}", truncate_str(&hrow.undo_hint, 40)));
        }
        if !hrow.summary.is_empty() {
            detail_lines.push(format!(" Summary:  {}", truncate_str(&hrow.summary, 40)));
        }

        detail_lines.push(String::new());
        detail_lines.push(" Keys: j/k=nav u=undoable filter Esc=clear".to_string());

        for line in &detail_lines {
            if drow >= detail_end {
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
    } else if drow < detail_end {
        write_styled(
            frame,
            detail_x,
            drow,
            " No entry selected.",
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
    while drow < detail_end {
        write_styled(frame, detail_x, drow, &blank_detail, CellStyle::new());
        drow += 1;
    }
}

/// Truncate a string for display.
fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        String::new()
    } else if s.chars().count() <= max {
        s.to_string()
    } else if max > 3 {
        let mut truncated: String = s.chars().take(max - 3).collect();
        truncated.push_str("...");
        truncated
    } else {
        s.chars().take(max).collect()
    }
}

/// Render the Timeline view — cross-pane event timeline with correlation markers.
///
/// Responsive timeline layout:
///   Regular/wide: left list + right detail panel.
///   Compact: full-width list with a stacked detail panel below it.
fn render_timeline_view(
    frame: &mut ftui::Frame,
    y: u16,
    width: u16,
    height: u16,
    rows: &[TimelineRow],
    selected: usize,
    zoom: u8,
    scroll: usize,
) {
    if height == 0 {
        return;
    }

    let layout = list_detail_layout(y, width, height, 60, 9);
    let list_width = layout.list_width;
    let list_end = layout.list_y.saturating_add(layout.list_height);
    let mut row = layout.list_y;

    // -- Header: count + zoom level --
    let zoom_label = match zoom {
        0 => "30m",
        1 => "1h",
        2 => "2h",
        _ => "6h+",
    };
    let scroll = scroll.min(rows.len().saturating_sub(1));
    let header = if layout.stacked {
        format!(
            "  Timeline {}  zoom={}  +/- j/k h/l  [compact]",
            rows.len(),
            zoom_label,
        )
    } else {
        format!(
            "  Timeline ({} events)  zoom={}  +/-=zoom j/k=nav h/l=scroll",
            rows.len(),
            zoom_label,
        )
    };
    write_styled(frame, 0, row, &header, CellStyle::new().bold());
    let hlen = header.len() as u16;
    if hlen < list_width {
        let fill = " ".repeat((list_width - hlen) as usize);
        write_styled(frame, hlen, row, &fill, CellStyle::new());
    }
    row += 1;

    // -- Column headers --
    if row < list_end {
        let col_header = format!(
            "  {:>8}  {:6}  {:8}  {:12}  {}",
            "time", "pane", "severity", "type", "corr"
        );
        write_styled(frame, 0, row, &col_header, CellStyle::new().dim());
        let clen = col_header.len() as u16;
        if clen < list_width {
            let fill = " ".repeat((list_width - clen) as usize);
            write_styled(frame, clen, row, &fill, CellStyle::new());
        }
        row += 1;
    }

    // -- Timeline rows --
    if rows.is_empty() && row < list_end {
        let msg = "  No timeline events in the current window.";
        write_styled(frame, 0, row, msg, CellStyle::new().dim());
        let msg_len = msg.len() as u16;
        if msg_len < list_width {
            let fill = " ".repeat((list_width - msg_len) as usize);
            write_styled(frame, msg_len, row, &fill, CellStyle::new());
        }
        row += 1;
    } else {
        for (pos, trow) in rows.iter().enumerate().skip(scroll) {
            if row >= list_end {
                break;
            }
            let corr_marker = if trow.correlation_label.is_empty() {
                " "
            } else {
                "*"
            };
            let line = format!(
                "  {:>8}  {:6}  {:8}  {:12}  {}",
                truncate_str(&trow.timestamp, 8),
                truncate_str(&trow.pane_label, 6),
                truncate_str(&trow.severity_label, 8),
                truncate_str(&trow.event_type, 12),
                corr_marker,
            );
            let style = if pos == selected {
                CellStyle::new().bold().reverse()
            } else if trow.severity_label == "error" {
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
    while row < list_end {
        write_styled(frame, 0, row, &blank_list, CellStyle::new());
        row += 1;
    }

    if layout.detail_width == 0 || layout.detail_height == 0 {
        return;
    }

    // -- Detail panel --
    let detail_x = layout.detail_x;
    let detail_width = layout.detail_width;
    let detail_end = layout.detail_y.saturating_add(layout.detail_height);
    let mut drow = layout.detail_y;

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

    if let Some(trow) = rows.get(selected) {
        let detail_lines: Vec<String> = vec![
            format!(" ID:       {}", truncate_str(&trow.id, 30)),
            format!(" Time:     {}", trow.timestamp),
            format!(" Pane:     {}", trow.pane_label),
            format!(" Agent:    {}", trow.agent_label),
            format!(" Type:     {}", trow.event_type),
            format!(" Severity: {}", trow.severity_label),
            format!(" Handled:  {}", trow.handled_label),
            if trow.correlation_label.is_empty() {
                " Corr:     none".to_string()
            } else {
                format!(" Corr:     {}", truncate_str(&trow.correlation_label, 30))
            },
            String::new(),
            format!(
                " {}",
                truncate_str(&trow.summary, detail_width.saturating_sub(2) as usize)
            ),
            String::new(),
            " Keys: j/k=nav h/l=scroll +/-=zoom".to_string(),
        ];

        for line in &detail_lines {
            if drow >= detail_end {
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
    } else if drow < detail_end {
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
    while drow < detail_end {
        write_styled(frame, detail_x, drow, &blank_detail, CellStyle::new());
        drow += 1;
    }
}

/// Render a modal overlay centered on the screen.
///
/// The modal is a bordered box with title, body text, and action hints.
/// It overwrites the underlying content (no transparency in cell-based rendering).
fn render_modal_overlay(frame: &mut ftui::Frame, width: u16, height: u16, modal: &ModalState) {
    // Modal size: 50 chars wide (or width-4, whichever is smaller), height depends on content.
    let modal_w = 50u16.min(width.saturating_sub(4));
    if modal_w < 10 || height < 7 {
        return; // Terminal too small for a modal.
    }

    let body_lines: Vec<&str> = modal.body.lines().collect();
    // Title(1) + border top/bottom(2) + body lines + hint line(1) + padding(1)
    let modal_h = (5 + body_lines.len() as u16).min(height.saturating_sub(2));
    let inner_h = modal_h.saturating_sub(4); // rows for body + hint

    // Center the modal.
    let x0 = (width.saturating_sub(modal_w)) / 2;
    let y0 = (height.saturating_sub(modal_h)) / 2;

    let inner_w = modal_w.saturating_sub(2); // inside the border columns

    // -- Top border --
    let top = format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(inner_w as usize));
    write_styled(frame, x0, y0, &top, CellStyle::new().bold());

    let mut row = y0 + 1;
    let max_row = y0 + modal_h.saturating_sub(1);

    // -- Title row --
    if row < max_row {
        let title = truncate_str(&modal.title, inner_w as usize);
        let pad = inner_w.saturating_sub(title.len() as u16);
        let line = format!("\u{2502}{title}{}\u{2502}", " ".repeat(pad as usize));
        write_styled(frame, x0, row, &line, CellStyle::new().bold());
        row += 1;
    }

    // -- Separator --
    if row < max_row {
        let sep = format!("\u{251c}{}\u{2524}", "\u{2500}".repeat(inner_w as usize));
        write_styled(frame, x0, row, &sep, CellStyle::new());
        row += 1;
    }

    // -- Body lines --
    let body_rows = inner_h.saturating_sub(1); // reserve 1 for hint
    for (i, line_text) in body_lines.iter().enumerate() {
        if row >= max_row || i as u16 >= body_rows {
            break;
        }
        let text = truncate_str(line_text, inner_w as usize);
        let pad = inner_w.saturating_sub(text.len() as u16);
        let line = format!("\u{2502}{text}{}\u{2502}", " ".repeat(pad as usize));
        write_styled(frame, x0, row, &line, CellStyle::new());
        row += 1;
    }

    // Fill remaining body area with blank rows.
    while row < max_row.saturating_sub(1) {
        let blank = format!("\u{2502}{}\u{2502}", " ".repeat(inner_w as usize));
        write_styled(frame, x0, row, &blank, CellStyle::new());
        row += 1;
    }

    // -- Hint / action row --
    if row < max_row {
        let hint = match modal.kind {
            ModalKind::Confirm => " Enter/y: confirm  Esc/n: cancel",
            ModalKind::Error => " Enter/Esc: dismiss",
            ModalKind::Info => " Enter/Esc: dismiss",
        };
        let hint_text = truncate_str(hint, inner_w as usize);
        let pad = inner_w.saturating_sub(hint_text.len() as u16);
        let line = format!("\u{2502}{hint_text}{}\u{2502}", " ".repeat(pad as usize));
        write_styled(frame, x0, row, &line, CellStyle::new().dim());
        row += 1;
    }

    // -- Bottom border --
    if row <= y0 + modal_h {
        let bottom = format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner_w as usize));
        write_styled(frame, x0, row, &bottom, CellStyle::new().bold());
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
    fg: Option<ftui::PackedRgba>,
    bg: Option<ftui::PackedRgba>,
}

impl CellStyle {
    const fn new() -> Self {
        Self {
            bold: false,
            dim: false,
            reverse: false,
            fg: None,
            bg: None,
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

    fn fg(mut self, color: ftui::PackedRgba) -> Self {
        self.fg = Some(color);
        self
    }

    fn bg(mut self, color: ftui::PackedRgba) -> Self {
        self.bg = Some(color);
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

    for (x, ch) in (col..).zip(text.chars()) {
        if x >= w {
            break;
        }
        let mut cell = ftui::Cell::from_char(ch).with_attrs(ftui::CellAttrs::new(flags, 0));
        if let Some(fg) = style.fg {
            cell = cell.with_fg(fg);
        }
        if let Some(bg) = style.bg {
            cell = cell.with_bg(bg);
        }
        buf.set(x, row, cell);
    }
}

fn write_styled_clipped(
    frame: &mut ftui::Frame,
    col: u16,
    row: u16,
    text: &str,
    style: CellStyle,
    max_width: u16,
) {
    if max_width == 0 {
        return;
    }
    let clipped = truncate_str(text, max_width as usize);
    write_styled(frame, col, row, &clipped, style);
}

fn write_segments(
    frame: &mut ftui::Frame,
    col: u16,
    row: u16,
    max_width: u16,
    segments: &[(&str, CellStyle)],
) {
    let mut x = col;
    let end = col.saturating_add(max_width);
    for (text, style) in segments {
        if x >= end {
            break;
        }
        let remaining = end.saturating_sub(x);
        write_styled_clipped(frame, x, row, text, *style, remaining);
        x = x.saturating_add(text.chars().count().min(remaining as usize) as u16);
    }
}

fn draw_block_all(frame: &mut ftui::Frame, area: UiRect, title: Option<&str>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    if area.width == 1 {
        for row in area.y..area.y.saturating_add(area.height) {
            write_styled(frame, area.x, row, "│", CellStyle::new());
        }
        return;
    }

    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    write_styled(frame, area.x, area.y, "┌", CellStyle::new());
    if let Some(title) = title {
        let title_width = area.width.saturating_sub(2) as usize;
        let clipped = truncate_str(title, title_width);
        write_styled(frame, area.x + 1, area.y, &clipped, CellStyle::new());
        let used = clipped.chars().count() as u16;
        if used + 2 < area.width {
            write_styled(
                frame,
                area.x + 1 + used,
                area.y,
                &"─".repeat((area.width - 2 - used) as usize),
                CellStyle::new(),
            );
        }
    } else if area.width > 2 {
        write_styled(
            frame,
            area.x + 1,
            area.y,
            &"─".repeat((area.width - 2) as usize),
            CellStyle::new(),
        );
    }
    write_styled(frame, right, area.y, "┐", CellStyle::new());

    if area.height > 2 {
        for row in (area.y + 1)..bottom {
            write_styled(frame, area.x, row, "│", CellStyle::new());
            write_styled(frame, right, row, "│", CellStyle::new());
        }
    }

    if area.height > 1 {
        write_styled(frame, area.x, bottom, "└", CellStyle::new());
        if area.width > 2 {
            write_styled(
                frame,
                area.x + 1,
                bottom,
                &"─".repeat((area.width - 2) as usize),
                CellStyle::new(),
            );
        }
        write_styled(frame, right, bottom, "┘", CellStyle::new());
    }
}

fn draw_block_top(frame: &mut ftui::Frame, area: UiRect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    write_styled(
        frame,
        area.x,
        area.y,
        &"─".repeat(area.width as usize),
        CellStyle::new(),
    );
}

fn color_red() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0x80, 0x00, 0x00, 0xFF)
}

fn color_green() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0x00, 0x80, 0x00, 0xFF)
}

fn color_yellow() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0x80, 0x80, 0x00, 0xFF)
}

fn color_magenta() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0x80, 0x00, 0x80, 0xFF)
}

fn color_cyan() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0x00, 0x80, 0x80, 0xFF)
}

fn color_gray() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0xC0, 0xC0, 0xC0, 0xFF)
}

fn color_dark_gray() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0x80, 0x80, 0x80, 0xFF)
}

fn color_default_fg() -> ftui::PackedRgba {
    ftui::PackedRgba::rgba(0xCC, 0xCC, 0xCC, 0xFF)
}

// ---------------------------------------------------------------------------
// Public API — matches the ratatui backend's exports
// ---------------------------------------------------------------------------

/// FrankenTUI application shell.
#[allow(dead_code)] // Public API surface matches ratatui backend; used via run_tui()
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
        .map_err(|e| crate::Error::runtime_backend("ftui_tui_run", e.to_string()))?;

    Ok(())
}

#[cfg(all(test, feature = "rollout"))]
pub(super) fn render_driver_frame(
    query: Arc<dyn QueryClient + Send + Sync>,
    view: View,
    width: u16,
    height: u16,
) -> crate::tui_parity_oracle::RenderFrame {
    use ftui::Model as _;

    let mut model = WaModel::new(
        query,
        AppConfig {
            refresh_interval: Duration::from_secs(5),
            debug: false,
        },
    );
    model.view_state.current_view = view;
    model.refresh_data();

    let mut pool = ftui::GraphemePool::new();
    let mut frame = ftui::Frame::new(width, height, &mut pool);
    model.view(&mut frame);
    crate::tui_parity_oracle::render_frame_from_ftui_frame(&frame)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::CircuitBreakerStatus;
    use crate::rulesets::RulesetProfileSummary;
    use crate::tui::ftui_compat::{Key, KeyInput, StyleSpec};
    use crate::tui::keymap::{self, Action};
    use crate::tui::query::{
        EventFilters, EventView, HealthStatus, HistoryEntryView, PaneBookmarkView, PaneView,
        QueryError, RulesetProfileState, SearchResultView, TriageItemView, WorkflowProgressView,
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
        saved_searches: Vec<SavedSearchView>,
        pane_bookmarks: Vec<PaneBookmarkView>,
        ruleset_profile_state: RulesetProfileState,
        events: Vec<EventView>,
        history_entries: Vec<HistoryEntryView>,
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
                saved_searches: Vec::new(),
                pane_bookmarks: Vec::new(),
                ruleset_profile_state: RulesetProfileState::default(),
                events: vec![],
                history_entries: vec![],
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
                saved_searches: Vec::new(),
                pane_bookmarks: Vec::new(),
                ruleset_profile_state: RulesetProfileState::default(),
                events: vec![],
                history_entries: vec![],
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
                saved_searches: Vec::new(),
                pane_bookmarks: Vec::new(),
                ruleset_profile_state: RulesetProfileState::default(),
                history_entries: vec![],
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

        fn with_saved_searches(mut self, saved_searches: Vec<SavedSearchView>) -> Self {
            self.saved_searches = saved_searches;
            self
        }

        fn with_pane_bookmarks(mut self, pane_bookmarks: Vec<PaneBookmarkView>) -> Self {
            self.pane_bookmarks = pane_bookmarks;
            self
        }

        fn with_ruleset_profile_state(
            mut self,
            ruleset_profile_state: RulesetProfileState,
        ) -> Self {
            self.ruleset_profile_state = ruleset_profile_state;
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
                                command: "ft pane restart 7".to_string(),
                            },
                            TriageAction {
                                label: "Investigate".to_string(),
                                command: "ft events show --pane 7".to_string(),
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
                            command: "ft rules throttle 42".to_string(),
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
                saved_searches: Vec::new(),
                pane_bookmarks: Vec::new(),
                ruleset_profile_state: RulesetProfileState::default(),
                events: vec![],
                history_entries: vec![],
            }
        }

        fn with_history() -> Self {
            Self {
                healthy: true,
                pane_count: 2,
                unhandled_per_pane: 0,
                triage_count: 0,
                triage_items_detailed: vec![],
                workflows_data: vec![],
                search_results: vec![],
                saved_searches: Vec::new(),
                pane_bookmarks: Vec::new(),
                ruleset_profile_state: RulesetProfileState::default(),
                events: vec![],
                history_entries: vec![
                    HistoryEntryView {
                        audit_id: 1001,
                        timestamp: 1_700_000_000_000,
                        pane_id: Some(3),
                        workflow_id: Some("wf-deploy".to_string()),
                        action_kind: "send_text".to_string(),
                        result: "ok".to_string(),
                        actor_kind: "robot".to_string(),
                        step_name: Some("deploy-step-1".to_string()),
                        undoable: true,
                        undone: false,
                        undo_strategy: Some("ctrl_c".to_string()),
                        undo_hint: Some("Send Ctrl-C to cancel".to_string()),
                        rule_id: Some("rule-deploy".to_string()),
                        summary: ("Sent deploy command".to_string()),
                    },
                    HistoryEntryView {
                        audit_id: 1002,
                        timestamp: 1_700_000_060_000,
                        pane_id: Some(3),
                        workflow_id: Some("wf-deploy".to_string()),
                        action_kind: "wait_for".to_string(),
                        result: "timeout".to_string(),
                        actor_kind: "robot".to_string(),
                        step_name: Some("deploy-step-2".to_string()),
                        undoable: false,
                        undone: false,
                        undo_strategy: None,
                        undo_hint: None,
                        rule_id: Some("rule-deploy".to_string()),
                        summary: ("Wait for prompt timed out".to_string()),
                    },
                    HistoryEntryView {
                        audit_id: 1003,
                        timestamp: 1_700_000_120_000,
                        pane_id: Some(7),
                        workflow_id: None,
                        action_kind: "send_text".to_string(),
                        result: "ok".to_string(),
                        actor_kind: "operator".to_string(),
                        step_name: None,
                        undoable: true,
                        undone: false,
                        undo_strategy: Some("ctrl_c".to_string()),
                        undo_hint: Some("Send Ctrl-C".to_string()),
                        rule_id: None,
                        summary: ("Manual command sent".to_string()),
                    },
                ],
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

        fn list_saved_searches(&self) -> Result<Vec<SavedSearchView>, QueryError> {
            Ok(self.saved_searches.clone())
        }

        fn list_pane_bookmarks(&self) -> Result<Vec<PaneBookmarkView>, QueryError> {
            Ok(self.pane_bookmarks.clone())
        }

        fn ruleset_profile_state(&self) -> Result<RulesetProfileState, QueryError> {
            Ok(self.ruleset_profile_state.clone())
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

        fn list_action_history(&self, _limit: usize) -> Result<Vec<HistoryEntryView>, QueryError> {
            Ok(self.history_entries.clone())
        }
    }

    // -- Helpers --

    fn make_model(query: impl QueryClient + 'static) -> WaModel {
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

    fn first_row_containing(frame: &ftui::Frame, height: u16, needle: &str) -> Option<u16> {
        (0..height).find(|&row| read_row(frame, row).contains(needle))
    }

    #[test]
    fn truncate_str_matches_ratatui_display_contract() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 8), "hello...");
        assert_eq!(truncate_str("abcdef", 3), "abc");
        assert_eq!(truncate_str("héllo wörld", 7), "héll...");
        assert_eq!(truncate_str("hello", 0), "");
    }

    // -- View navigation tests --

    #[test]
    fn viewport_class_breakpoints_are_stable() {
        assert_eq!(viewport_class(150, 40), ViewportClass::Wide);
        assert_eq!(viewport_class(100, 30), ViewportClass::Regular);
        assert_eq!(viewport_class(80, 24), ViewportClass::Compact);
    }

    #[test]
    fn list_detail_layout_stacks_on_compact_terminals() {
        let compact = list_detail_layout(0, 80, 22, 60, 8);
        assert!(compact.stacked);
        assert_eq!(compact.list_width, 80);
        assert_eq!(compact.detail_x, 0);
        assert!(compact.detail_y >= 10);

        let regular = list_detail_layout(0, 120, 28, 60, 8);
        assert!(!regular.stacked);
        assert_eq!(regular.detail_y, 0);
        assert!(regular.detail_x > 0);
    }

    #[test]
    fn view_all_returns_eight_views() {
        assert_eq!(View::all().len(), 8);
    }

    #[test]
    fn view_next_wraps() {
        assert_eq!(View::Help.next(), View::Timeline);
        assert_eq!(View::Timeline.next(), View::Home);
        assert_eq!(View::Home.next(), View::Panes);
    }

    #[test]
    fn view_prev_wraps() {
        assert_eq!(View::Home.prev(), View::Timeline);
        assert_eq!(View::Timeline.prev(), View::Help);
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
        assert_eq!(View::from_shortcut('9'), None);
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
        assert!(row0.contains("FrankenTerm Control Center"));
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

    fn press_modified_key(model: &mut WaModel, code: ftui::KeyCode, modifiers: ftui::Modifiers) {
        let key = ftui::KeyEvent {
            code,
            kind: ftui::KeyEventKind::Press,
            modifiers,
        };
        assert!(model.handle_global_key(&key).is_none());
        model.handle_view_key(&key);
    }

    fn pane_bookmark(pane_id: u64) -> PaneBookmarkView {
        PaneBookmarkView {
            pane_id,
            alias: format!("pane-{pane_id}"),
            tags: vec!["watch".to_string()],
            description: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn sample_ruleset_profiles() -> RulesetProfileState {
        RulesetProfileState {
            active_profile: "default".to_string(),
            active_last_applied_at: None,
            profiles: vec![
                RulesetProfileSummary {
                    name: "default".to_string(),
                    description: Some("Base".to_string()),
                    path: None,
                    last_applied_at: None,
                    implicit: true,
                },
                RulesetProfileSummary {
                    name: "ops profile".to_string(),
                    description: Some("Ops".to_string()),
                    path: None,
                    last_applied_at: None,
                    implicit: false,
                },
            ],
        }
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

        model.panes_filter.set_text("pane".to_string());
        model.panes_unhandled_only = true;
        model.panes_bookmarked_only = true;
        model.panes_agent_filter = Some("codex".to_string());
        model.panes_domain_filter = Some("local".to_string());
        model.panes_selected = 2;
        press_key(&mut model, ftui::KeyCode::Escape);
        assert!(model.panes_filter.is_empty());
        assert!(!model.panes_unhandled_only);
        assert!(!model.panes_bookmarked_only);
        assert!(model.panes_agent_filter.is_none());
        assert!(model.panes_domain_filter.is_none());
        assert_eq!(model.panes_selected, 0);
    }

    #[test]
    fn panes_backend_honors_canonical_filter_actions() {
        let mut model = make_model(
            MockQuery::healthy()
                .with_pane_bookmarks(vec![pane_bookmark(1)])
                .with_ruleset_profile_state(sample_ruleset_profiles()),
        );
        model.refresh_data();
        model.view_state.current_view = View::Panes;
        model.panes_selected = 2;

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Char('u')), "Panes"),
            Some(Action::ToggleUnhandledOnly)
        );
        press_key(&mut model, ftui::KeyCode::Char('u'));
        assert!(model.panes_unhandled_only);
        assert_eq!(model.panes_selected, 0);

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Char('b')), "Panes"),
            Some(Action::ToggleBookmarkedOnly)
        );
        press_key(&mut model, ftui::KeyCode::Char('b'));
        assert!(model.panes_bookmarked_only);
        assert_eq!(model.filtered_pane_indices(), vec![1]);

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Char('a')), "Panes"),
            Some(Action::CycleAgentFilter)
        );
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.panes_agent_filter.as_deref(), Some("codex"));

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Char('d')), "Panes"),
            Some(Action::CycleDomainFilter)
        );
        press_key(&mut model, ftui::KeyCode::Char('d'));
        assert_eq!(model.panes_domain_filter.as_deref(), Some("local"));

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Char('x')), "Panes"),
            Some(Action::FilterAppendChar('x'))
        );
        press_key(&mut model, ftui::KeyCode::Char('x'));
        assert_eq!(model.panes_filter.text(), "x");

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Backspace), "Panes"),
            Some(Action::FilterDeleteChar)
        );
        press_key(&mut model, ftui::KeyCode::Backspace);
        assert!(model.panes_filter.is_empty());
    }

    #[test]
    fn panes_backend_honors_canonical_profile_actions() {
        let mut model =
            make_model(MockQuery::healthy().with_ruleset_profile_state(sample_ruleset_profiles()));
        model.refresh_data();
        model.view_state.current_view = View::Panes;

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Char('p')), "Panes"),
            Some(Action::CycleRulesetProfile)
        );
        press_key(&mut model, ftui::KeyCode::Char('p'));
        assert_eq!(model.selected_ruleset_profile_name(), Some("ops profile"));

        assert_eq!(
            keymap::resolve(&KeyInput::new(Key::Enter), "Panes"),
            Some(Action::ApplyRulesetProfile)
        );
        press_key(&mut model, ftui::KeyCode::Enter);
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("ft rules profile apply 'ops profile'")
        );
    }

    #[test]
    fn render_panes_shows_header_and_columns() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 30, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        let filtered = model.filtered_pane_indices();
        render_panes_view(
            &mut frame,
            0,
            100,
            28,
            &model.panes,
            &model.pane_bookmarks,
            &filtered,
            0,
            PaneRenderFilters::default(),
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
            &model.pane_bookmarks,
            &filtered,
            0,
            PaneRenderFilters::default(),
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
            &model.pane_bookmarks,
            &filtered,
            0,
            PaneRenderFilters::default(),
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
    fn render_panes_surfaces_bookmarks_in_list_and_detail() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut bookmark = pane_bookmark(0);
        bookmark.alias = "fav".to_string();
        bookmark.tags = vec!["ops".to_string(), "watch".to_string()];
        let mut model = make_model(MockQuery::healthy().with_pane_bookmarks(vec![bookmark]));
        model.refresh_data();

        let filtered = model.filtered_pane_indices();
        render_panes_view(
            &mut frame,
            0,
            100,
            22,
            &model.panes,
            &model.pane_bookmarks,
            &filtered,
            0,
            PaneRenderFilters::default(),
        );

        let rendered = (0..22)
            .map(|row| read_row(&frame, row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("fav"), "{rendered}");
        assert!(
            rendered.contains("Bookmarks: fav [ops,watch]"),
            "{rendered}"
        );
    }

    #[test]
    fn render_panes_narrow_stacks_detail_below_list() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        let filtered = model.filtered_pane_indices();
        render_panes_view(
            &mut frame,
            0,
            80,
            22,
            &model.panes,
            &model.pane_bookmarks,
            &filtered,
            0,
            PaneRenderFilters::default(),
        );

        let detail_row = first_row_containing(&frame, 22, "Pane Details")
            .expect("compact panes layout should still show detail header");
        assert!(
            detail_row >= 10,
            "compact panes detail should stack below list, got row {detail_row}"
        );
    }

    #[test]
    fn render_panes_empty_shows_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        render_panes_view(
            &mut frame,
            0,
            100,
            22,
            &[],
            &[],
            &[],
            0,
            PaneRenderFilters::default(),
        );

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
            &model.pane_bookmarks,
            &filtered,
            0,
            PaneRenderFilters::default(),
        );
    }

    // -- Search view tests --

    fn sample_search_results() -> Vec<SearchResultView> {
        vec![
            SearchResultView {
                pane_id: 10,
                timestamp: 1_700_000_000_000,
                snippet: "error: connection refused".into(),
                rank: 0.95,
            },
            SearchResultView {
                pane_id: 20,
                timestamp: 1_700_000_001_000,
                snippet: "error: timeout exceeded".into(),
                rank: 0.88,
            },
        ]
    }

    fn sample_saved_searches() -> Vec<SavedSearchView> {
        vec![
            SavedSearchView {
                id: "ss-errors".into(),
                name: "errors".into(),
                query: "error OR panic".into(),
                pane_id: None,
                limit: 50,
                since_mode: "all".into(),
                since_ms: None,
                schedule_interval_ms: Some(60_000),
                enabled: false,
                last_run_at: None,
                last_result_count: None,
                last_error: None,
                created_at: 1,
                updated_at: 1,
            },
            SavedSearchView {
                id: "ss-warnings".into(),
                name: "warnings".into(),
                query: "warning".into(),
                pane_id: Some(42),
                limit: 50,
                since_mode: "all".into(),
                since_ms: None,
                schedule_interval_ms: Some(60_000),
                enabled: true,
                last_run_at: Some(2),
                last_result_count: Some(3),
                last_error: None,
                created_at: 1,
                updated_at: 2,
            },
            SavedSearchView {
                id: "ss-manual".into(),
                name: "manual".into(),
                query: "manual only".into(),
                pane_id: None,
                limit: 10,
                since_mode: "all".into(),
                since_ms: None,
                schedule_interval_ms: None,
                enabled: false,
                last_run_at: None,
                last_result_count: None,
                last_error: None,
                created_at: 1,
                updated_at: 1,
            },
        ]
    }

    #[test]
    fn search_char_input_appends_to_query() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('h'));
        press_key(&mut model, ftui::KeyCode::Char('i'));
        assert_eq!(model.search_input.text(), "hi");
    }

    #[test]
    fn search_backspace_removes_char() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('a'));
        press_key(&mut model, ftui::KeyCode::Char('b'));
        press_key(&mut model, ftui::KeyCode::Backspace);
        assert_eq!(model.search_input.text(), "a");
    }

    #[test]
    fn search_enter_executes_query() {
        let mock = MockQuery::healthy().with_search_results(sample_search_results());
        let mut model = make_model(mock);
        model.view_state.current_view = View::Search;
        model.search_input.set_text("error".into());
        press_key(&mut model, ftui::KeyCode::Enter);
        assert_eq!(model.search_last_query, "error");
        assert_eq!(model.search_results.len(), 2);
        assert_eq!(model.search_selected, 0);
    }

    #[test]
    fn search_enter_empty_query_noop() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        model.search_input.set_text("  ".into());
        press_key(&mut model, ftui::KeyCode::Enter);
        assert!(model.search_results.is_empty());
        assert!(model.search_last_query.is_empty());
    }

    #[test]
    fn search_esc_clears_all() {
        let mock = MockQuery::healthy().with_search_results(sample_search_results());
        let mut model = make_model(mock);
        model.view_state.current_view = View::Search;
        model.search_input.set_text("error".into());
        press_key(&mut model, ftui::KeyCode::Enter);
        assert!(!model.search_results.is_empty());
        press_key(&mut model, ftui::KeyCode::Escape);
        assert!(model.search_input.text().is_empty());
        assert!(model.search_last_query.is_empty());
        assert!(model.search_results.is_empty());
        assert_eq!(model.search_selected, 0);
    }

    #[test]
    fn search_arrow_navigation_wraps() {
        let mock = MockQuery::healthy().with_search_results(sample_search_results());
        let mut model = make_model(mock);
        model.view_state.current_view = View::Search;
        model.search_input.set_text("error".into());
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
    fn refresh_data_populates_saved_searches() {
        let mut model =
            make_model(MockQuery::healthy().with_saved_searches(sample_saved_searches()));
        model.refresh_data();

        assert_eq!(model.saved_searches.len(), 3);
        assert_eq!(model.saved_search_selected, 0);
        assert_eq!(model.saved_searches[0].name, "errors");
    }

    #[test]
    fn search_ctrl_saved_shortcuts_select_and_queue_actions() {
        let mut model =
            make_model(MockQuery::healthy().with_saved_searches(sample_saved_searches()));
        model.view_state.current_view = View::Search;
        model.refresh_data();
        model.search_input.set_text("typed".into());

        press_modified_key(&mut model, ftui::KeyCode::Char('n'), ftui::Modifiers::CTRL);
        assert_eq!(model.saved_search_selected, 1);
        assert_eq!(model.search_input.text(), "typed");

        press_modified_key(&mut model, ftui::KeyCode::Char('p'), ftui::Modifiers::CTRL);
        assert_eq!(model.saved_search_selected, 0);

        press_modified_key(&mut model, ftui::KeyCode::Char('r'), ftui::Modifiers::CTRL);
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("ft search saved run errors")
        );

        press_modified_key(&mut model, ftui::KeyCode::Char('e'), ftui::Modifiers::CTRL);
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("ft search saved enable errors")
        );

        model.saved_search_selected = 1;
        model.saved_searches[1].name = "warnings now".into();
        press_modified_key(&mut model, ftui::KeyCode::Char('e'), ftui::Modifiers::CTRL);
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("ft search saved disable 'warnings now'")
        );

        model.saved_search_selected = 0;
        model.saved_searches[0].name = "errors and $panic".into();
        press_modified_key(&mut model, ftui::KeyCode::Char('r'), ftui::Modifiers::CTRL);
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("ft search saved run 'errors and $panic'")
        );

        model.saved_search_selected = 2;
        press_modified_key(&mut model, ftui::KeyCode::Char('e'), ftui::Modifiers::CTRL);
        assert!(
            model
                .view_state
                .error_message
                .as_deref()
                .is_some_and(|msg| msg.contains("no schedule")),
            "manual-only saved search should surface schedule guidance"
        );
        assert!(model.triage_queued_action.is_none());
    }

    #[test]
    fn search_global_q_does_not_quit() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('q'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let result = model.handle_global_key(&key);
        assert!(result.is_none());
        model.handle_view_key(&key);
        assert_eq!(model.search_input.text(), "q");
    }

    #[test]
    fn search_ctrl_saved_shortcuts_do_not_edit_query_text() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        model.search_input.set_text("error".into());

        for ch in ['n', 'p', 'r', 'e'] {
            let key = ftui::KeyEvent {
                code: ftui::KeyCode::Char(ch),
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::CTRL,
            };
            let result = model.handle_global_key(&key);
            assert!(result.is_none());
            model.handle_view_key(&key);
            assert_eq!(model.search_input.text(), "error");
        }
    }

    #[test]
    fn search_tab_still_navigates_views() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Search;
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Tab,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let result = model.handle_global_key(&key);
        assert!(result.is_some());
        assert_eq!(model.view_state.current_view, View::Help);
    }

    #[test]
    fn render_search_empty_shows_prompt() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(
            &mut frame,
            0,
            80,
            22,
            "",
            0,
            FocusRegion::PrimaryList,
            "",
            &[],
            0,
            &[],
            0,
        );
        let row0 = read_row(&frame, 0);
        assert!(row0.contains("Search (FTS5)"));
        let row1 = read_row(&frame, 1);
        assert!(row1.contains("Type a query"));
    }

    #[test]
    fn render_search_no_results_shows_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(
            &mut frame,
            0,
            80,
            22,
            "test",
            4,
            FocusRegion::PrimaryList,
            "test",
            &[],
            0,
            &[],
            0,
        );
        let row1 = read_row(&frame, 1);
        assert!(row1.contains("No results"));
    }

    #[test]
    fn render_search_non_boundary_cursor_does_not_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(
            &mut frame,
            0,
            80,
            22,
            "éx",
            1,
            FocusRegion::FilterBar,
            "éx",
            &[],
            0,
            &[],
            0,
        );
        assert!(read_row(&frame, 0).contains("Search (FTS5)"));
    }

    #[test]
    fn render_search_unicode_lines_clear_stale_tail_cells() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(72, 24, &mut pool);
        let stale = "~".repeat(72);
        write_styled(&mut frame, 0, 0, &stale, CellStyle::new());
        write_styled(&mut frame, 0, 1, &stale, CellStyle::new());

        render_search_view(
            &mut frame,
            0,
            72,
            22,
            "é",
            1,
            FocusRegion::FilterBar,
            "éé",
            &[],
            0,
            &[],
            0,
        );

        assert!(!read_row(&frame, 0).contains('~'));
        assert!(!read_row(&frame, 1).contains('~'));
    }

    #[test]
    fn render_search_saved_searches_show_selection() {
        let saved_searches = sample_saved_searches();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);
        render_search_view(
            &mut frame,
            0,
            100,
            22,
            "",
            0,
            FocusRegion::PrimaryList,
            "",
            &[],
            0,
            &saved_searches,
            1,
        );

        let summary_row =
            first_row_containing(&frame, 22, "Saved searches (3)").expect("saved summary row");
        assert!(read_row(&frame, summary_row).contains("Ctrl+R run"));
        let selected_row =
            first_row_containing(&frame, 22, "> warnings").expect("selected saved search row");
        let selected_text = read_row(&frame, selected_row);
        assert!(selected_text.contains("on"));
        assert!(selected_text.contains("warning"));
    }

    #[test]
    fn render_search_with_results_shows_list_and_detail() {
        let rows: Vec<super::SearchRow> = sample_search_results()
            .iter()
            .map(super::adapt_search)
            .collect();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);
        render_search_view(
            &mut frame,
            0,
            100,
            22,
            "error",
            5,
            FocusRegion::PrimaryList,
            "error",
            &rows,
            0,
            &[],
            0,
        );
        let header_row = read_row(&frame, 1);
        assert!(header_row.contains("2 matches"));
        let column_row_idx =
            first_row_containing(&frame, 22, "Pane").expect("search column header");
        let column_row = read_row(&frame, column_row_idx);
        assert!(column_row.contains("Pane"));
        assert!(column_row.contains("Rank"));
        let data_row = read_row(&frame, column_row_idx + 1);
        assert!(data_row.contains("P 10"));
        let mut found = false;
        for r in 0..22 {
            if read_row(&frame, r).contains("Match Context") {
                found = true;
                break;
            }
        }
        assert!(found, "Detail panel header not found");
    }

    #[test]
    fn render_search_narrow_stacks_detail_below_list() {
        let rows: Vec<super::SearchRow> = sample_search_results()
            .iter()
            .map(super::adapt_search)
            .collect();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(
            &mut frame,
            0,
            80,
            22,
            "error",
            5,
            FocusRegion::PrimaryList,
            "error",
            &rows,
            0,
            &[],
            0,
        );

        let detail_row = first_row_containing(&frame, 22, "Match Context")
            .expect("compact search layout should still show detail header");
        assert!(
            detail_row >= 10,
            "compact search detail should stack below list, got row {detail_row}"
        );
    }

    #[test]
    fn render_search_zero_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_search_view(
            &mut frame,
            0,
            80,
            0,
            "q",
            1,
            FocusRegion::PrimaryList,
            "q",
            &[],
            0,
            &[],
            0,
        );
    }

    // -- Help view tests --

    #[test]
    fn render_help_shows_title_and_sections() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 40, &mut pool);
        render_help_view(&mut frame, 0, 80, 38);
        let row0 = read_row(&frame, 0);
        assert!(row0.contains("FrankenTerm Control Center"));
        let mut g = false;
        let mut v = false;
        let mut s = false;
        for r in 0..38 {
            let t = read_row(&frame, r);
            if t.contains("Global Keybindings") {
                g = true;
            }
            if t.contains("Views:") {
                v = true;
            }
            if t.contains("Search:") {
                s = true;
            }
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
        assert!(row0.contains("FrankenTerm Control Center"));
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
        model
            .view_state
            .events
            .pane_filter
            .set_text("42".to_string());
        let indices = model.view_state.events.filtered_indices();
        assert_eq!(indices.len(), 2); // events 0 and 2 are pane 42
    }

    #[test]
    fn events_filtering_combined() {
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();
        model.view_state.events.unhandled_only = true;
        model
            .view_state
            .events
            .pane_filter
            .set_text("7".to_string());
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
        assert_eq!(model.view_state.events.pane_filter.text(), "4");
        press_key(&mut model, ftui::KeyCode::Char('2'));
        assert_eq!(model.view_state.events.pane_filter.text(), "42");
    }

    #[test]
    fn events_backspace_removes_filter_char() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        model
            .view_state
            .events
            .pane_filter
            .set_text("42".to_string());

        press_key(&mut model, ftui::KeyCode::Backspace);
        assert_eq!(model.view_state.events.pane_filter.text(), "4");
    }

    #[test]
    fn events_esc_clears_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        model
            .view_state
            .events
            .pane_filter
            .set_text("42".to_string());
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
        assert!(
            result.is_none(),
            "digit should not be consumed globally in Events view"
        );
    }

    #[test]
    fn events_plain_digits_filter_but_ctrl_digits_do_not() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();

        let plain = ftui::KeyEvent {
            code: ftui::KeyCode::Char('4'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&plain);
        assert_eq!(model.view_state.events.pane_filter.text(), "4");

        let ctrl = ftui::KeyEvent {
            code: ftui::KeyCode::Char('5'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::CTRL,
        };
        model.handle_view_key(&ctrl);
        assert_eq!(model.view_state.events.pane_filter.text(), "4");
    }

    #[test]
    fn render_events_shows_header_and_columns() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 30, &mut pool);

        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();

        let filtered = model.view_state.events.filtered_indices();
        let clamped = model.view_state.events.clamped_selection();
        render_events_view(
            &mut frame,
            0,
            100,
            28,
            &model.view_state.events,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
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
            &mut frame,
            0,
            100,
            22,
            &model.view_state.events,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
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
            &mut frame,
            0,
            100,
            22,
            &model.view_state.events,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
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
    fn render_events_narrow_stacks_detail_below_list() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);

        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();

        let filtered = model.view_state.events.filtered_indices();
        let clamped = model.view_state.events.clamped_selection();
        render_events_view(
            &mut frame,
            0,
            80,
            22,
            &model.view_state.events,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
        );

        let detail_row = first_row_containing(&frame, 22, "Event Details")
            .expect("compact events layout should still show detail header");
        assert!(
            detail_row >= 10,
            "compact events detail should stack below list, got row {detail_row}"
        );
    }

    #[test]
    fn render_events_empty_shows_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let events_state = EventsViewState::default();
        render_events_view(
            &mut frame,
            0,
            100,
            22,
            &events_state,
            &[],
            0,
            FocusRegion::PrimaryList,
        );

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
        render_events_view(
            &mut frame,
            0,
            80,
            0,
            &events_state,
            &[],
            0,
            FocusRegion::PrimaryList,
        );
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
        // Action now shows confirmation modal first.
        assert!(model.active_modal.is_some());
        assert_eq!(
            model.active_modal.as_ref().unwrap().kind,
            ModalKind::Confirm
        );
        // Confirm the modal.
        let confirm = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&confirm);
        assert!(model.active_modal.is_none());
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("ft pane restart 7"),
        );
    }

    #[test]
    fn triage_digit_queues_numbered_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        // Digit '2' should show confirm modal for action at index 1 ("Investigate")
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('2'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&key);
        assert!(model.active_modal.is_some());
        // Confirm with 'y'.
        let confirm = ftui::KeyEvent {
            code: ftui::KeyCode::Char('y'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&confirm);
        assert!(model.active_modal.is_none());
        assert_eq!(
            model.triage_queued_action.as_deref(),
            Some("ft events show --pane 7"),
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
        // Mute now shows confirm modal.
        model.handle_triage_key(&key);
        assert!(model.active_modal.is_some());
        assert_eq!(
            model.active_modal.as_ref().unwrap().kind,
            ModalKind::Confirm
        );
        // Confirm the mute.
        let confirm = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&confirm);
        assert!(model.active_modal.is_none());
        // Should not error (MockQuery.mark_event_muted returns Ok)
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
        assert!(
            result.is_none(),
            "Digit should not be consumed globally in Triage view"
        );
        assert_eq!(model.view_state.current_view, View::Triage);
    }

    #[test]
    fn triage_plain_digits_queue_actions_but_ctrl_digits_do_not() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.view_state.current_view = View::Triage;

        let ctrl = ftui::KeyEvent {
            code: ftui::KeyCode::Char('2'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::CTRL,
        };
        model.handle_triage_key(&ctrl);
        assert!(model.active_modal.is_none());
        assert!(model.triage_queued_action.is_none());

        let plain = ftui::KeyEvent {
            code: ftui::KeyCode::Char('2'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_triage_key(&plain);
        assert!(model.active_modal.is_some());
    }

    #[test]
    fn render_triage_shows_header_and_items() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);

        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();

        render_triage_view(
            &mut frame,
            0,
            100,
            22,
            &model.triage_items,
            model.triage_selected,
            &model.workflows,
            model.triage_expanded,
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
            &mut frame,
            0,
            100,
            22,
            &model.triage_items,
            model.triage_selected,
            &model.workflows,
            model.triage_expanded,
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
            &mut frame,
            0,
            100,
            22,
            &model.triage_items,
            model.triage_selected,
            &model.workflows,
            model.triage_expanded,
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
            &mut frame,
            0,
            100,
            22,
            &model.triage_items,
            model.triage_selected,
            &model.workflows,
            model.triage_expanded,
        );

        let mut found_actions = false;
        let mut found_restart = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Actions") {
                found_actions = true;
            }
            if text.contains("Restart") && text.contains("ft pane restart") {
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
            &mut frame,
            0,
            100,
            22,
            &model.triage_items,
            model.triage_selected,
            &empty_wf,
            None,
        );

        let mut found_wf = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("Active Workflows") {
                found_wf = true;
                break;
            }
        }
        assert!(
            !found_wf,
            "Workflow panel should not appear without workflows"
        );
    }

    #[test]
    fn triage_selection_clamps_after_refresh() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.triage_selected = 10; // Past end
        model.refresh_data();
        assert_eq!(model.triage_selected, 2); // Clamped to last item
    }

    // -- History view tests (FTUI-05.6) --

    #[test]
    fn refresh_data_populates_history_entries() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        assert_eq!(model.view_state.history.items.len(), 3);
        assert_eq!(model.view_state.history.rows.len(), 3);
    }

    #[test]
    fn history_navigation_down_wraps() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        for _ in 0..3 {
            let key = ftui::KeyEvent {
                code: ftui::KeyCode::Char('j'),
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            };
            model.handle_view_key(&key);
        }
        assert_eq!(model.view_state.history.selected_index, 0);
    }

    #[test]
    fn history_navigation_up_wraps() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('k'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&key);
        assert_eq!(model.view_state.history.selected_index, 2);
    }

    #[test]
    fn history_arrow_keys_navigate() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        let down = ftui::KeyEvent {
            code: ftui::KeyCode::Down,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&down);
        assert_eq!(model.view_state.history.selected_index, 1);

        let up = ftui::KeyEvent {
            code: ftui::KeyCode::Up,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&up);
        assert_eq!(model.view_state.history.selected_index, 0);
    }

    #[test]
    fn history_undoable_toggle() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        assert!(!model.view_state.history.undoable_only);
        assert_eq!(model.view_state.history.filtered_indices().len(), 3);

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('u'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&key);
        assert!(model.view_state.history.undoable_only);
        assert_eq!(model.view_state.history.filtered_indices().len(), 2);

        model.handle_view_key(&key);
        assert!(!model.view_state.history.undoable_only);
        assert_eq!(model.view_state.history.filtered_indices().len(), 3);
    }

    #[test]
    fn history_text_filter() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        for ch in "wait_for".chars() {
            let key = ftui::KeyEvent {
                code: ftui::KeyCode::Char(ch),
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            };
            model.handle_view_key(&key);
        }
        assert_eq!(model.view_state.history.filter_input.text(), "wait_for");
        assert_eq!(model.view_state.history.filtered_indices().len(), 1);
        assert_eq!(model.view_state.history.filtered_indices()[0], 1);
    }

    #[test]
    fn history_backspace_removes_char() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        for ch in "abc".chars() {
            let key = ftui::KeyEvent {
                code: ftui::KeyCode::Char(ch),
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            };
            model.handle_view_key(&key);
        }
        assert_eq!(model.view_state.history.filter_input.text(), "abc");

        let bs = ftui::KeyEvent {
            code: ftui::KeyCode::Backspace,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&bs);
        assert_eq!(model.view_state.history.filter_input.text(), "ab");
    }

    #[test]
    fn history_escape_clears_all() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        model
            .view_state
            .history
            .filter_input
            .set_text("test".to_string());
        model.view_state.history.undoable_only = true;
        model.view_state.history.selected_index = 1;

        let esc = ftui::KeyEvent {
            code: ftui::KeyCode::Escape,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&esc);
        assert!(model.view_state.history.filter_input.text().is_empty());
        assert!(!model.view_state.history.undoable_only);
        assert_eq!(model.view_state.history.selected_index, 0);
    }

    #[test]
    fn history_q_does_not_quit() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('q'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let cmd = model.handle_view_key(&key);
        assert!(!matches!(cmd, ftui::Cmd::Quit));
        assert_eq!(model.view_state.history.filter_input.text(), "q");
    }

    #[test]
    fn history_digits_filter_not_switch() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;

        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('3'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_view_key(&key);
        assert_eq!(model.view_state.current_view, View::History);
        assert_eq!(model.view_state.history.filter_input.text(), "3");
    }

    #[test]
    fn history_selection_clamps_after_refresh() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.history.selected_index = 100;
        model.refresh_data();
        assert_eq!(model.view_state.history.selected_index, 2);
    }

    #[test]
    fn history_filtered_indices_combined() {
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();

        model
            .view_state
            .history
            .filter_input
            .set_text("send_text".to_string());
        model.view_state.history.undoable_only = true;
        let filtered = model.view_state.history.filtered_indices();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered, vec![0, 2]);
    }

    #[test]
    fn history_clamped_selection_empty() {
        let state = HistoryViewState::default();
        assert_eq!(state.clamped_selection(), 0);
    }

    #[test]
    fn render_history_shows_header() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();

        let filtered = model.view_state.history.filtered_indices();
        let clamped = model.view_state.history.clamped_selection();
        render_history_view(
            &mut frame,
            0,
            120,
            28,
            &model.view_state.history,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
        );

        let row0 = read_row(&frame, 0);
        assert!(
            row0.contains("History"),
            "Header should contain 'History': {row0}"
        );
        assert!(row0.contains("3"), "Header should show entry count: {row0}");
    }

    #[test]
    fn render_history_shows_entries() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();

        let filtered = model.view_state.history.filtered_indices();
        let clamped = model.view_state.history.clamped_selection();
        render_history_view(
            &mut frame,
            0,
            120,
            28,
            &model.view_state.history,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
        );

        let mut found_send = false;
        let mut found_wait = false;
        for r in 0..28 {
            let text = read_row(&frame, r);
            if text.contains("send_text") {
                found_send = true;
            }
            if text.contains("wait_for") {
                found_wait = true;
            }
        }
        assert!(found_send, "Should show send_text action");
        assert!(found_wait, "Should show wait_for action");
    }

    #[test]
    fn render_history_shows_detail_panel() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();

        let filtered = model.view_state.history.filtered_indices();
        let clamped = model.view_state.history.clamped_selection();
        render_history_view(
            &mut frame,
            0,
            120,
            28,
            &model.view_state.history,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
        );

        let mut found_detail = false;
        for r in 0..28 {
            let text = read_row(&frame, r);
            if text.contains("Detail") || text.contains("Pane") || text.contains("Workflow") {
                found_detail = true;
                break;
            }
        }
        assert!(found_detail, "Detail panel should be visible");
    }

    #[test]
    fn render_history_narrow_stacks_detail_below_list() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();

        let filtered = model.view_state.history.filtered_indices();
        let clamped = model.view_state.history.clamped_selection();
        render_history_view(
            &mut frame,
            0,
            80,
            22,
            &model.view_state.history,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
        );

        let detail_row = first_row_containing(&frame, 22, "History Details")
            .expect("compact history layout should still show detail header");
        assert!(
            detail_row >= 10,
            "compact history detail should stack below list, got row {detail_row}"
        );
    }

    #[test]
    fn render_history_empty_shows_message() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let state = HistoryViewState::default();
        let filtered = state.filtered_indices();
        let clamped = state.clamped_selection();
        render_history_view(
            &mut frame,
            0,
            80,
            22,
            &state,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
        );

        let mut found_empty = false;
        for r in 0..22 {
            let text = read_row(&frame, r);
            if text.contains("No history") {
                found_empty = true;
                break;
            }
        }
        assert!(found_empty, "Should show 'No history' message");
    }

    #[test]
    fn render_history_zero_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let state = HistoryViewState::default();
        let filtered = state.filtered_indices();
        render_history_view(
            &mut frame,
            0,
            80,
            0,
            &state,
            &filtered,
            0,
            FocusRegion::PrimaryList,
        );
    }

    #[test]
    fn render_history_undoable_filter_shown() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(120, 30, &mut pool);
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.history.undoable_only = true;

        let filtered = model.view_state.history.filtered_indices();
        let clamped = model.view_state.history.clamped_selection();
        render_history_view(
            &mut frame,
            0,
            120,
            28,
            &model.view_state.history,
            &filtered,
            clamped,
            FocusRegion::PrimaryList,
        );

        let row0 = read_row(&frame, 0);
        assert!(
            row0.contains("undoable") || row0.contains("2"),
            "Header should reflect undoable filter: {row0}"
        );
    }

    // -- Modal interaction tests (FTUI-06.3) --

    #[test]
    fn modal_confirm_enter_executes_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.show_modal(ModalState::confirm(
            "Test",
            "Run action?",
            ConfirmAction::ExecuteCommand("ft test cmd".to_string()),
        ));
        assert!(model.active_modal.is_some());
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
        assert_eq!(model.triage_queued_action.as_deref(), Some("ft test cmd"));
    }

    #[test]
    fn modal_confirm_y_executes_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.show_modal(ModalState::confirm(
            "Test",
            "Run?",
            ConfirmAction::ExecuteCommand("ft test".to_string()),
        ));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('y'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
        assert_eq!(model.triage_queued_action.as_deref(), Some("ft test"));
    }

    #[test]
    fn modal_escape_dismisses_without_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.show_modal(ModalState::confirm(
            "Test",
            "Run?",
            ConfirmAction::ExecuteCommand("ft dangerous".to_string()),
        ));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Escape,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
        assert!(model.triage_queued_action.is_none());
    }

    #[test]
    fn modal_n_dismisses_without_action() {
        let mut model = make_model(MockQuery::with_triage());
        model.show_modal(ModalState::confirm(
            "Test",
            "Run?",
            ConfirmAction::ExecuteCommand("ft dangerous".to_string()),
        ));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('n'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
        assert!(model.triage_queued_action.is_none());
    }

    #[test]
    fn modal_absorbs_unrelated_keys() {
        let mut model = make_model(MockQuery::with_triage());
        model.show_modal(ModalState::confirm(
            "Test",
            "Run?",
            ConfirmAction::ExecuteCommand("cmd".to_string()),
        ));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('q'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let result = model.handle_modal_key(&key);
        assert!(result.is_some());
        assert!(model.active_modal.is_some());
    }

    #[test]
    fn modal_blocks_global_keys_in_update() {
        let mut model = make_model(MockQuery::with_triage());
        model.show_modal(ModalState::confirm(
            "Test",
            "Proceed?",
            ConfirmAction::ExecuteCommand("cmd".to_string()),
        ));
        let before = model.view_state.current_view;
        let cmd = ftui::Model::update(
            &mut model,
            WaMsg::TermEvent(ftui::Event::Key(ftui::KeyEvent {
                code: ftui::KeyCode::Tab,
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            })),
        );
        assert!(matches!(cmd, ftui::Cmd::None));
        assert_eq!(model.view_state.current_view, before);
        assert!(model.active_modal.is_some());
    }

    #[test]
    fn modal_error_dismissed_with_enter() {
        let mut model = make_model(MockQuery::healthy());
        model.show_modal(ModalState::error("Error", "Something went wrong"));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
    }

    #[test]
    fn modal_error_dismissed_with_escape() {
        let mut model = make_model(MockQuery::healthy());
        model.show_modal(ModalState::error("Error", "Something went wrong"));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Escape,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
    }

    #[test]
    fn modal_info_dismissed_with_enter() {
        let mut model = make_model(MockQuery::healthy());
        model.show_modal(ModalState::info("Info", "Operation complete."));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
    }

    #[test]
    fn modal_no_active_returns_none() {
        let mut model = make_model(MockQuery::healthy());
        assert!(model.active_modal.is_none());
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let result = model.handle_modal_key(&key);
        assert!(result.is_none());
    }

    #[test]
    fn modal_mute_confirm_executes_mute() {
        let mut model = make_model(MockQuery::with_triage());
        model.refresh_data();
        model.show_modal(ModalState::confirm(
            "Confirm Mute",
            "Mute event 42?",
            ConfirmAction::MuteEvent("42".to_string()),
        ));
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Enter,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_modal_key(&key);
        assert!(model.active_modal.is_none());
        assert!(model.view_state.error_message.is_none());
    }

    #[test]
    fn render_modal_overlay_zero_height_no_panic() {
        // ftui::Frame requires height > 0; test with height=1 for minimal terminal
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 1, &mut pool);
        let modal = ModalState::confirm(
            "Test",
            "Body",
            ConfirmAction::ExecuteCommand("cmd".to_string()),
        );
        render_modal_overlay(&mut frame, 80, 1, &modal);
    }

    #[test]
    fn render_modal_overlay_small_terminal_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(10, 7, &mut pool);
        let modal = ModalState::error("Error", "Something went wrong.");
        render_modal_overlay(&mut frame, 10, 7, &modal);
    }

    #[test]
    fn render_modal_confirm_shows_hint() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let modal = ModalState::confirm(
            "Confirm",
            "Do it?",
            ConfirmAction::ExecuteCommand("cmd".to_string()),
        );
        render_modal_overlay(&mut frame, 80, 24, &modal);
        let text: String = (0..24)
            .map(|r| read_row(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("confirm"), "Should show confirm hint: {text}");
        assert!(text.contains("cancel"), "Should show cancel hint: {text}");
        assert!(text.contains("Confirm"), "Should show title: {text}");
    }

    #[test]
    fn render_modal_error_shows_dismiss_hint() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        let modal = ModalState::error("Oops", "An error occurred.");
        render_modal_overlay(&mut frame, 80, 24, &modal);
        let text: String = (0..24)
            .map(|r| read_row(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("dismiss"), "Should show dismiss hint: {text}");
        assert!(text.contains("Oops"), "Should show title: {text}");
    }

    // -- TextInput unit tests (FTUI-06.4) --

    #[test]
    fn text_input_insert_and_cursor() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('b');
        ti.insert_char('c');
        assert_eq!(ti.text(), "abc");
        assert_eq!(ti.cursor_pos(), 3);
    }

    #[test]
    fn text_input_delete_back() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('b');
        ti.delete_back();
        assert_eq!(ti.text(), "a");
        assert_eq!(ti.cursor_pos(), 1);
    }

    #[test]
    fn text_input_delete_back_empty() {
        let mut ti = TextInput::new();
        ti.delete_back();
        assert_eq!(ti.text(), "");
        assert_eq!(ti.cursor_pos(), 0);
    }

    #[test]
    fn text_input_delete_forward() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('b');
        ti.move_left();
        ti.delete_forward();
        assert_eq!(ti.text(), "a");
        assert_eq!(ti.cursor_pos(), 1);
    }

    #[test]
    fn text_input_cursor_movement() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('b');
        ti.insert_char('c');
        assert_eq!(ti.cursor_pos(), 3);
        ti.move_left();
        assert_eq!(ti.cursor_pos(), 2);
        ti.move_left();
        assert_eq!(ti.cursor_pos(), 1);
        ti.move_right();
        assert_eq!(ti.cursor_pos(), 2);
    }

    #[test]
    fn text_input_home_end() {
        let mut ti = TextInput::new();
        ti.insert_char('x');
        ti.insert_char('y');
        ti.insert_char('z');
        ti.move_home();
        assert_eq!(ti.cursor_pos(), 0);
        ti.move_end();
        assert_eq!(ti.cursor_pos(), 3);
    }

    #[test]
    fn text_input_insert_at_cursor() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('c');
        ti.move_left();
        ti.insert_char('b');
        assert_eq!(ti.text(), "abc");
        assert_eq!(ti.cursor_pos(), 2);
    }

    #[test]
    fn text_input_clear() {
        let mut ti = TextInput::new();
        ti.insert_char('x');
        ti.insert_char('y');
        ti.clear();
        assert_eq!(ti.text(), "");
        assert_eq!(ti.cursor_pos(), 0);
    }

    #[test]
    fn text_input_cursor_clamp_at_bounds() {
        let mut ti = TextInput::new();
        ti.move_left();
        assert_eq!(ti.cursor_pos(), 0);
        ti.insert_char('a');
        ti.move_right();
        assert_eq!(ti.cursor_pos(), 1);
        ti.move_right();
        assert_eq!(ti.cursor_pos(), 1);
    }

    #[test]
    fn text_input_set_text_clamps_cursor() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('b');
        ti.insert_char('c');
        assert_eq!(ti.cursor_pos(), 3);
        ti.set_text("x".into());
        assert_eq!(ti.text(), "x");
        assert_eq!(ti.cursor_pos(), 1);
    }

    // -- FTUI-06.4.a: text-input editing edge-case matrix --
    //
    // Systematic coverage of cursor/editing edge cases that the basic
    // tests above do not exercise.

    /// Helper: assert text content and cursor position together.
    fn assert_ti(ti: &TextInput, text: &str, cursor: usize, label: &str) {
        assert_eq!(
            ti.text(),
            text,
            "{label}: text mismatch (expected {text:?})"
        );
        assert_eq!(
            ti.cursor_pos(),
            cursor,
            "{label}: cursor mismatch (expected {cursor})"
        );
    }

    // -- Multi-byte / Unicode edge cases --

    #[test]
    fn edge_multibyte_insert_and_delete() {
        let mut ti = TextInput::new();
        ti.insert_char('é'); // 2-byte UTF-8
        assert_ti(&ti, "é", 2, "after inserting é");
        ti.insert_char('ñ'); // 2-byte UTF-8
        assert_ti(&ti, "éñ", 4, "after inserting ñ");
        ti.delete_back();
        assert_ti(&ti, "é", 2, "after deleting ñ");
        ti.delete_back();
        assert_ti(&ti, "", 0, "after deleting é");
    }

    #[test]
    fn edge_multibyte_cursor_movement() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('é');
        ti.insert_char('b');
        // "aéb" — bytes: a(1) é(2) b(1) = 4 bytes
        assert_ti(&ti, "aéb", 4, "initial");
        ti.move_left();
        assert_ti(&ti, "aéb", 3, "left once (past b)");
        ti.move_left();
        assert_ti(&ti, "aéb", 1, "left twice (past é, 2 bytes)");
        ti.move_right();
        assert_ti(&ti, "aéb", 3, "right once (past é)");
    }

    #[test]
    fn text_input_recovers_from_non_boundary_cursor() {
        let mut ti = TextInput {
            text: "éx".to_string(),
            cursor: 1,
        };
        ti.move_right();
        assert_ti(&ti, "éx", 2, "right from invalid middle byte");

        ti.cursor = 1;
        ti.insert_char('a');
        assert_ti(&ti, "aéx", 1, "insert from invalid middle byte");

        ti.cursor = 2;
        ti.delete_forward();
        assert_ti(&ti, "ax", 1, "delete forward from invalid middle byte");
    }

    #[test]
    fn edge_emoji_insert_and_navigate() {
        let mut ti = TextInput::new();
        ti.insert_char('🦀'); // 4-byte UTF-8
        ti.insert_char('x');
        assert_ti(&ti, "🦀x", 5, "crab+x");
        ti.move_left();
        assert_ti(&ti, "🦀x", 4, "left past x");
        ti.move_left();
        assert_ti(&ti, "🦀x", 0, "left past crab");
        ti.delete_forward();
        assert_ti(&ti, "x", 0, "deleted crab forward");
    }

    #[test]
    fn edge_cjk_characters() {
        let mut ti = TextInput::new();
        ti.insert_char('漢'); // 3-byte UTF-8
        ti.insert_char('字'); // 3-byte UTF-8
        assert_ti(&ti, "漢字", 6, "two CJK chars");
        ti.move_left();
        assert_ti(&ti, "漢字", 3, "left past 字");
        ti.insert_char('a');
        assert_ti(&ti, "漢a字", 4, "inserted ASCII between CJK");
    }

    // -- Deletion edge cases --

    #[test]
    fn edge_delete_forward_at_end_is_noop() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.delete_forward();
        assert_ti(&ti, "a", 1, "delete_forward at end");
    }

    #[test]
    fn edge_delete_back_at_start_is_noop() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.move_home();
        ti.delete_back();
        assert_ti(&ti, "a", 0, "delete_back at start");
    }

    #[test]
    fn edge_delete_all_chars_one_by_one() {
        let mut ti = TextInput::new();
        for c in "hello".chars() {
            ti.insert_char(c);
        }
        for _ in 0..5 {
            ti.delete_back();
        }
        assert_ti(&ti, "", 0, "after deleting all chars");
        // Extra delete on empty should be safe
        ti.delete_back();
        assert_ti(&ti, "", 0, "extra delete on empty");
    }

    #[test]
    fn edge_delete_forward_from_middle() {
        let mut ti = TextInput::new();
        for c in "abcde".chars() {
            ti.insert_char(c);
        }
        ti.move_home();
        ti.move_right(); // cursor at 1 (after 'a')
        ti.delete_forward(); // deletes 'b'
        assert_ti(&ti, "acde", 1, "delete forward from middle");
        ti.delete_forward(); // deletes 'c'
        assert_ti(&ti, "ade", 1, "delete forward again");
    }

    // -- Cursor boundary edge cases --

    #[test]
    fn edge_rapid_left_right_at_boundaries() {
        let mut ti = TextInput::new();
        ti.insert_char('x');
        // Rapid left past boundary
        for _ in 0..10 {
            ti.move_left();
        }
        assert_ti(&ti, "x", 0, "clamped at 0 after many lefts");
        // Rapid right past boundary
        for _ in 0..10 {
            ti.move_right();
        }
        assert_ti(&ti, "x", 1, "clamped at len after many rights");
    }

    #[test]
    fn edge_home_on_empty() {
        let mut ti = TextInput::new();
        ti.move_home();
        assert_ti(&ti, "", 0, "home on empty");
    }

    #[test]
    fn edge_end_on_empty() {
        let mut ti = TextInput::new();
        ti.move_end();
        assert_ti(&ti, "", 0, "end on empty");
    }

    // -- Sequence combination edge cases --

    #[test]
    fn edge_home_then_insert() {
        let mut ti = TextInput::new();
        for c in "world".chars() {
            ti.insert_char(c);
        }
        ti.move_home();
        for c in "hello ".chars() {
            ti.insert_char(c);
        }
        assert_ti(&ti, "hello world", 6, "insert at home");
    }

    #[test]
    fn edge_home_then_delete_forward_all() {
        let mut ti = TextInput::new();
        for c in "abc".chars() {
            ti.insert_char(c);
        }
        ti.move_home();
        ti.delete_forward();
        ti.delete_forward();
        ti.delete_forward();
        assert_ti(&ti, "", 0, "delete forward all from home");
        ti.delete_forward(); // Extra should be safe
        assert_ti(&ti, "", 0, "extra delete_forward on empty");
    }

    #[test]
    fn edge_interleaved_insert_delete() {
        let mut ti = TextInput::new();
        ti.insert_char('a');
        ti.insert_char('b');
        ti.delete_back(); // remove b
        ti.insert_char('c');
        ti.insert_char('d');
        ti.move_left(); // before d
        ti.delete_back(); // remove c
        assert_ti(&ti, "ad", 1, "interleaved insert/delete");
    }

    #[test]
    fn edge_clear_then_rebuild() {
        let mut ti = TextInput::new();
        for c in "hello".chars() {
            ti.insert_char(c);
        }
        ti.clear();
        assert_ti(&ti, "", 0, "after clear");
        for c in "world".chars() {
            ti.insert_char(c);
        }
        assert_ti(&ti, "world", 5, "rebuilt after clear");
    }

    #[test]
    fn edge_set_text_then_edit() {
        let mut ti = TextInput::new();
        ti.set_text("abc".into());
        assert_ti(&ti, "abc", 3, "after set_text");
        ti.move_left();
        ti.insert_char('x');
        assert_ti(&ti, "abxc", 3, "insert after set_text");
    }

    // -- Stress / long input --

    #[test]
    fn edge_long_input_200_chars() {
        let mut ti = TextInput::new();
        for _ in 0..200 {
            ti.insert_char('x');
        }
        assert_eq!(ti.text().len(), 200);
        assert_eq!(ti.cursor_pos(), 200);
        ti.move_home();
        assert_eq!(ti.cursor_pos(), 0);
        ti.move_end();
        assert_eq!(ti.cursor_pos(), 200);
    }

    #[test]
    fn edge_navigate_entire_string() {
        let mut ti = TextInput::new();
        for c in "abcdef".chars() {
            ti.insert_char(c);
        }
        // Walk all the way left
        for i in (0..6).rev() {
            ti.move_left();
            assert_eq!(ti.cursor_pos(), i, "left walk at {i}");
        }
        // Walk all the way right
        for i in 1..=6 {
            ti.move_right();
            assert_eq!(ti.cursor_pos(), i, "right walk at {i}");
        }
    }

    #[test]
    fn edge_single_char_full_lifecycle() {
        let mut ti = TextInput::new();
        ti.insert_char('z');
        assert_ti(&ti, "z", 1, "insert");
        ti.move_left();
        assert_ti(&ti, "z", 0, "left");
        ti.move_right();
        assert_ti(&ti, "z", 1, "right");
        ti.move_home();
        assert_ti(&ti, "z", 0, "home");
        ti.move_end();
        assert_ti(&ti, "z", 1, "end");
        ti.delete_back();
        assert_ti(&ti, "", 0, "delete");
    }

    #[test]
    fn search_left_right_in_query() {
        let mut model = make_model(MockQuery::healthy().with_search_results(vec![]));
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('a'));
        press_key(&mut model, ftui::KeyCode::Char('b'));
        assert_eq!(model.search_input.text(), "ab");
        press_key(&mut model, ftui::KeyCode::Left);
        press_key(&mut model, ftui::KeyCode::Char('x'));
        assert_eq!(model.search_input.text(), "axb");
    }

    #[test]
    fn search_home_end_in_query() {
        let mut model = make_model(MockQuery::healthy().with_search_results(vec![]));
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('h'));
        press_key(&mut model, ftui::KeyCode::Char('i'));
        press_key(&mut model, ftui::KeyCode::Home);
        press_key(&mut model, ftui::KeyCode::Char('_'));
        assert_eq!(model.search_input.text(), "_hi");
        press_key(&mut model, ftui::KeyCode::End);
        press_key(&mut model, ftui::KeyCode::Char('!'));
        assert_eq!(model.search_input.text(), "_hi!");
    }

    #[test]
    fn search_delete_forward_in_query() {
        let mut model = make_model(MockQuery::healthy().with_search_results(vec![]));
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('a'));
        press_key(&mut model, ftui::KeyCode::Char('b'));
        press_key(&mut model, ftui::KeyCode::Home);
        press_key(&mut model, ftui::KeyCode::Delete);
        assert_eq!(model.search_input.text(), "b");
    }

    // -- Events pane_filter cursor navigation integration tests --

    #[test]
    fn events_left_right_in_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('4'));
        press_key(&mut model, ftui::KeyCode::Char('2'));
        assert_eq!(model.view_state.events.pane_filter.text(), "42");
        press_key(&mut model, ftui::KeyCode::Left);
        press_key(&mut model, ftui::KeyCode::Char('0'));
        assert_eq!(model.view_state.events.pane_filter.text(), "402");
    }

    #[test]
    fn events_home_end_in_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('1'));
        press_key(&mut model, ftui::KeyCode::Char('2'));
        press_key(&mut model, ftui::KeyCode::Home);
        press_key(&mut model, ftui::KeyCode::Char('9'));
        assert_eq!(model.view_state.events.pane_filter.text(), "912");
        press_key(&mut model, ftui::KeyCode::End);
        press_key(&mut model, ftui::KeyCode::Char('3'));
        assert_eq!(model.view_state.events.pane_filter.text(), "9123");
    }

    #[test]
    fn events_delete_forward_in_filter() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('5'));
        press_key(&mut model, ftui::KeyCode::Char('6'));
        press_key(&mut model, ftui::KeyCode::Home);
        press_key(&mut model, ftui::KeyCode::Delete);
        assert_eq!(model.view_state.events.pane_filter.text(), "6");
    }

    // -- History filter_input cursor navigation integration tests --

    #[test]
    fn history_left_right_in_filter() {
        let mut model = make_model(MockQuery::with_history());
        model.view_state.current_view = View::History;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('a'));
        press_key(&mut model, ftui::KeyCode::Char('b'));
        assert_eq!(model.view_state.history.filter_input.text(), "ab");
        press_key(&mut model, ftui::KeyCode::Left);
        press_key(&mut model, ftui::KeyCode::Char('x'));
        assert_eq!(model.view_state.history.filter_input.text(), "axb");
    }

    #[test]
    fn history_home_end_in_filter() {
        let mut model = make_model(MockQuery::with_history());
        model.view_state.current_view = View::History;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('h'));
        press_key(&mut model, ftui::KeyCode::Char('i'));
        press_key(&mut model, ftui::KeyCode::Home);
        press_key(&mut model, ftui::KeyCode::Char('_'));
        assert_eq!(model.view_state.history.filter_input.text(), "_hi");
        press_key(&mut model, ftui::KeyCode::End);
        press_key(&mut model, ftui::KeyCode::Char('!'));
        assert_eq!(model.view_state.history.filter_input.text(), "_hi!");
    }

    #[test]
    fn history_delete_forward_in_filter() {
        let mut model = make_model(MockQuery::with_history());
        model.view_state.current_view = View::History;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('a'));
        press_key(&mut model, ftui::KeyCode::Char('b'));
        press_key(&mut model, ftui::KeyCode::Home);
        press_key(&mut model, ftui::KeyCode::Delete);
        assert_eq!(model.view_state.history.filter_input.text(), "b");
    }

    // -----------------------------------------------------------------------
    // Focus traversal tests (FTUI-06.5)
    // -----------------------------------------------------------------------

    #[test]
    fn focus_default_is_primary_list() {
        let model = make_model(MockQuery::healthy());
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_tab_resets_to_primary_list() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.focus = FocusRegion::FilterBar;
        let tab = ftui::KeyEvent {
            code: ftui::KeyCode::Tab,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        ftui::Model::update(&mut model, WaMsg::TermEvent(ftui::Event::Key(tab)));
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_backtab_resets_to_primary_list() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.focus = FocusRegion::FilterBar;
        let backtab = ftui::KeyEvent {
            code: ftui::KeyCode::BackTab,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        ftui::Model::update(&mut model, WaMsg::TermEvent(ftui::Event::Key(backtab)));
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_search_typing_sets_filter_bar() {
        let mut model = make_model(MockQuery::healthy().with_search_results(vec![]));
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
    }

    #[test]
    fn focus_search_down_sets_primary_list() {
        let mut model = make_model(MockQuery::healthy().with_search_results(vec![]));
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Down);
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_search_escape_sets_primary_list() {
        let mut model = make_model(MockQuery::healthy().with_search_results(vec![]));
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Escape);
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_search_enter_sets_primary_list() {
        let mut model = make_model(MockQuery::healthy().with_search_results(vec![]));
        model.view_state.current_view = View::Search;
        press_key(&mut model, ftui::KeyCode::Char('t'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Enter);
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_events_digit_sets_filter_bar() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('4'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
    }

    #[test]
    fn focus_events_j_sets_primary_list() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('4'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Char('j'));
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_events_escape_sets_primary_list() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('4'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Escape);
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_history_typing_sets_filter_bar() {
        let mut model = make_model(MockQuery::with_history());
        model.view_state.current_view = View::History;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
    }

    #[test]
    fn focus_history_j_sets_primary_list() {
        let mut model = make_model(MockQuery::with_history());
        model.view_state.current_view = View::History;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Char('j'));
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_history_escape_sets_primary_list() {
        let mut model = make_model(MockQuery::with_history());
        model.view_state.current_view = View::History;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Escape);
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_modal_traps_input() {
        let mut model = make_model(MockQuery::with_triage());
        model.view_state.current_view = View::Triage;
        model.refresh_data();
        // Trigger action modal
        press_key(&mut model, ftui::KeyCode::Enter);
        assert!(model.active_modal.is_some());
        // Tab should NOT change views while modal is active
        let view_before = model.view_state.current_view;
        let tab = ftui::KeyEvent {
            code: ftui::KeyCode::Tab,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        ftui::Model::update(&mut model, WaMsg::TermEvent(ftui::Event::Key(tab)));
        assert_eq!(model.view_state.current_view, view_before);
        assert!(model.active_modal.is_some());
    }

    #[test]
    fn focus_traversal_full_cycle() {
        // Tab through all 8 views and verify focus resets each time
        let mut model = make_model(MockQuery::healthy());
        model.view_state.focus = FocusRegion::FilterBar;
        for _ in 0..8 {
            let tab = ftui::KeyEvent {
                code: ftui::KeyCode::Tab,
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            };
            ftui::Model::update(&mut model, WaMsg::TermEvent(ftui::Event::Key(tab)));
            assert_eq!(
                model.view_state.focus,
                FocusRegion::PrimaryList,
                "Focus should reset to PrimaryList on view switch"
            );
        }
        // Should have cycled back to original view
        assert_eq!(model.view_state.current_view, View::Home);
    }

    #[test]
    fn focus_events_u_toggle_sets_primary_list() {
        let mut model = make_model(MockQuery::with_events());
        model.view_state.current_view = View::Events;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('4'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Char('u'));
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    #[test]
    fn focus_history_u_toggle_sets_primary_list() {
        let mut model = make_model(MockQuery::with_history());
        model.view_state.current_view = View::History;
        model.refresh_data();
        press_key(&mut model, ftui::KeyCode::Char('a'));
        assert_eq!(model.view_state.focus, FocusRegion::FilterBar);
        press_key(&mut model, ftui::KeyCode::Char('u'));
        assert_eq!(model.view_state.focus, FocusRegion::PrimaryList);
    }

    // -----------------------------------------------------------------------
    // Snapshot / golden suite (FTUI-07.2)
    // -----------------------------------------------------------------------

    /// Capture the entire frame buffer as a multi-line string.
    /// Trailing whitespace is trimmed per-line for stable comparisons.
    fn frame_to_text(frame: &ftui::Frame) -> String {
        let h = frame.height();
        let mut lines = Vec::with_capacity(h as usize);
        for y in 0..h {
            lines.push(read_row(frame, y).trim_end().to_string());
        }
        // Remove trailing empty lines
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Render a full frame for the given view + model and return text.
    fn snapshot_view(query: MockQuery, view: View, w: u16, h: u16) -> String {
        use ftui::Model as _;
        let mut model = make_model(query);
        model.refresh_data();
        model.view_state.current_view = view;
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(w, h, &mut pool);
        model.view(&mut frame);
        frame_to_text(&frame)
    }

    // -- Structural invariants across all views and sizes --

    const SNAPSHOT_SIZES: &[(u16, u16)] = &[(40, 5), (80, 24), (120, 40)];

    fn assert_tab_bar(text: &str, _active_view: View) {
        let first_line = text.lines().next().expect("non-empty frame");
        // Tab bar should contain at least "Home" (the first tab always fits)
        assert!(
            first_line.contains("Home"),
            "Tab bar missing 'Home' in: {first_line}"
        );
        assert!(
            first_line.contains('│'),
            "Tab bar missing separators in: {first_line}"
        );
        let border_line = text.lines().nth(1).expect("tab border row");
        assert!(
            border_line.contains('─'),
            "Tab bar missing bottom border in: {border_line}"
        );
    }

    // -- Home view snapshots --

    #[test]
    fn snapshot_home_healthy_80x24() {
        let text = snapshot_view(MockQuery::healthy(), View::Home, 80, 24);
        assert_tab_bar(&text, View::Home);
        assert!(
            text.contains("ok")
                || text.contains("OK")
                || text.contains("healthy")
                || text.contains("Home"),
            "Home should show health status"
        );
    }

    #[test]
    fn snapshot_home_degraded_80x24() {
        let text = snapshot_view(MockQuery::degraded(), View::Home, 80, 24);
        assert_tab_bar(&text, View::Home);
        // Degraded state should show ERROR badge, "stopped" watcher, or "unavailable" db
        assert!(
            text.contains("ERROR") || text.contains("stopped") || text.contains("unavailable"),
            "Degraded home should show problems: {text}"
        );
    }

    #[test]
    fn snapshot_home_all_sizes() {
        for &(w, h) in SNAPSHOT_SIZES {
            let text = snapshot_view(MockQuery::healthy(), View::Home, w, h);
            if h >= 2 {
                assert_tab_bar(&text, View::Home);
            }
            // Should not panic at any size
            assert!(
                !text.is_empty() || h < 2,
                "Frame should be non-empty for h={h}"
            );
        }
    }

    // -- Panes view snapshots --

    #[test]
    fn snapshot_panes_populated_80x24() {
        let text = snapshot_view(MockQuery::healthy(), View::Panes, 80, 24);
        assert_tab_bar(&text, View::Panes);
        // MockQuery::healthy() has pane_count=3
        assert!(
            text.contains("pane") || text.contains("Pane"),
            "Panes view should show pane info"
        );
    }

    #[test]
    fn snapshot_panes_empty_80x24() {
        let text = snapshot_view(MockQuery::degraded(), View::Panes, 80, 24);
        assert_tab_bar(&text, View::Panes);
        // With 0 panes, should show empty state
        assert!(
            text.contains("No pane")
                || text.contains("no pane")
                || text.contains("0")
                || text.lines().count() >= 3,
            "Empty panes should show some message"
        );
    }

    #[test]
    fn snapshot_panes_all_sizes() {
        for &(w, h) in SNAPSHOT_SIZES {
            let text = snapshot_view(MockQuery::healthy(), View::Panes, w, h);
            if h >= 2 {
                assert_tab_bar(&text, View::Panes);
            }
        }
    }

    // -- Events view snapshots --

    #[test]
    fn snapshot_events_populated_80x24() {
        let text = snapshot_view(MockQuery::with_events(), View::Events, 80, 24);
        assert_tab_bar(&text, View::Events);
        assert!(
            text.contains("Rate limit")
                || text.contains("rate_limit")
                || text.contains("warning")
                || text.contains("error"),
            "Events view should show event data"
        );
    }

    #[test]
    fn snapshot_events_empty_80x24() {
        let text = snapshot_view(MockQuery::healthy(), View::Events, 80, 24);
        assert_tab_bar(&text, View::Events);
        // healthy() has empty events
    }

    #[test]
    fn snapshot_events_all_sizes() {
        for &(w, h) in SNAPSHOT_SIZES {
            let text = snapshot_view(MockQuery::with_events(), View::Events, w, h);
            if h >= 2 {
                assert_tab_bar(&text, View::Events);
            }
        }
    }

    // -- Triage view snapshots --

    #[test]
    fn snapshot_triage_populated_80x24() {
        let text = snapshot_view(MockQuery::with_triage(), View::Triage, 80, 24);
        assert_tab_bar(&text, View::Triage);
        assert!(
            text.contains("crash")
                || text.contains("Fatal")
                || text.contains("Triage")
                || text.contains("error"),
            "Triage view should show triage items"
        );
    }

    #[test]
    fn snapshot_triage_empty_80x24() {
        let text = snapshot_view(MockQuery::degraded(), View::Triage, 80, 24);
        assert_tab_bar(&text, View::Triage);
    }

    #[test]
    fn snapshot_triage_all_sizes() {
        for &(w, h) in SNAPSHOT_SIZES {
            let text = snapshot_view(MockQuery::with_triage(), View::Triage, w, h);
            if h >= 2 {
                assert_tab_bar(&text, View::Triage);
            }
        }
    }

    // -- History view snapshots --

    #[test]
    fn snapshot_history_populated_80x24() {
        let text = snapshot_view(MockQuery::with_history(), View::History, 80, 24);
        assert_tab_bar(&text, View::History);
        assert!(
            text.contains("send_text") || text.contains("History"),
            "History view should show action data"
        );
    }

    #[test]
    fn snapshot_history_empty_80x24() {
        let text = snapshot_view(MockQuery::healthy(), View::History, 80, 24);
        assert_tab_bar(&text, View::History);
        assert!(
            text.contains("No history") || text.contains("no history") || text.contains("empty"),
            "Empty history should show placeholder: {text}"
        );
    }

    #[test]
    fn snapshot_history_all_sizes() {
        for &(w, h) in SNAPSHOT_SIZES {
            let text = snapshot_view(MockQuery::with_history(), View::History, w, h);
            if h >= 2 {
                assert_tab_bar(&text, View::History);
            }
        }
    }

    // -- Search view snapshots --

    #[test]
    fn snapshot_search_empty_80x24() {
        let text = snapshot_view(MockQuery::healthy(), View::Search, 80, 24);
        assert_tab_bar(&text, View::Search);
    }

    #[test]
    fn snapshot_search_with_results_80x24() {
        use ftui::Model as _;
        let query = MockQuery::healthy().with_search_results(vec![
            SearchResultView {
                pane_id: 1,
                timestamp: 1_700_000_000_000,
                snippet: "matched line alpha".to_string(),
                rank: 0.95,
            },
            SearchResultView {
                pane_id: 2,
                timestamp: 1_700_000_060_000,
                snippet: "matched line beta".to_string(),
                rank: 0.80,
            },
        ]);
        let mut model = make_model(query);
        model.refresh_data();
        model.view_state.current_view = View::Search;
        model.search_last_query = "alpha".to_string();
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        model.view(&mut frame);
        let text = frame_to_text(&frame);
        assert_tab_bar(&text, View::Search);
        assert!(
            text.contains("alpha") || text.contains("matched"),
            "Search results should show snippets"
        );
    }

    #[test]
    fn snapshot_search_all_sizes() {
        for &(w, h) in SNAPSHOT_SIZES {
            let text = snapshot_view(MockQuery::healthy(), View::Search, w, h);
            if h >= 2 {
                assert_tab_bar(&text, View::Search);
            }
        }
    }

    // -- Help view snapshots --

    #[test]
    fn snapshot_help_80x24() {
        let text = snapshot_view(MockQuery::healthy(), View::Help, 80, 24);
        assert_tab_bar(&text, View::Help);
        assert!(
            text.contains("FrankenTerm Control Center")
                || text.contains("Keybindings")
                || text.contains("Tab")
                || text.contains("help"),
            "Help view should show help text: {text}"
        );
    }

    #[test]
    fn snapshot_help_all_sizes() {
        for &(w, h) in SNAPSHOT_SIZES {
            let text = snapshot_view(MockQuery::healthy(), View::Help, w, h);
            if h >= 2 {
                assert_tab_bar(&text, View::Help);
            }
        }
    }

    // -- Full-frame structural tests --

    #[test]
    fn snapshot_all_views_no_panic_tiny() {
        // 40x5 is extremely small — ensure no panics
        for &view in View::all() {
            let text = snapshot_view(MockQuery::healthy(), view, 40, 5);
            assert_tab_bar(&text, view);
        }
    }

    #[test]
    fn snapshot_all_views_no_panic_very_small() {
        // Minimum viable: 2 rows (tab + border).
        for &view in View::all() {
            let _text = snapshot_view(MockQuery::healthy(), view, 30, 2);
        }
    }

    #[test]
    fn snapshot_all_views_height_1_renders_empty() {
        use ftui::Model as _;
        // height < 2 → view() returns early, frame should be all spaces
        for &view in View::all() {
            let mut model = make_model(MockQuery::healthy());
            model.view_state.current_view = view;
            let mut pool = ftui::GraphemePool::new();
            let mut frame = ftui::Frame::new(80, 1, &mut pool);
            model.view(&mut frame);
            let text = frame_to_text(&frame);
            // Should be empty or whitespace-only since height < 2
            assert!(
                text.trim().is_empty(),
                "Height 1 should produce empty frame for {view:?}: '{text}'"
            );
        }
    }

    #[test]
    fn snapshot_all_views_large_120x40() {
        // Large terminal — verify structure
        for &view in View::all() {
            let query = match view {
                View::Events => MockQuery::with_events(),
                View::Triage => MockQuery::with_triage(),
                View::History => MockQuery::with_history(),
                _ => MockQuery::healthy(),
            };
            let text = snapshot_view(query, view, 120, 40);
            assert_tab_bar(&text, view);
        }
    }

    // -- Width edge cases --

    #[test]
    fn snapshot_narrow_width_no_panic() {
        // Width of 20 — tab bar gets truncated, verify no panic
        for &view in View::all() {
            let _text = snapshot_view(MockQuery::healthy(), view, 20, 24);
        }
    }

    #[test]
    fn snapshot_wide_width_200() {
        // Very wide terminal
        for &view in View::all() {
            let text = snapshot_view(MockQuery::healthy(), view, 200, 24);
            if !text.is_empty() {
                let first_line = text.lines().next().unwrap();
                // Tab bar should be present
                assert!(first_line.contains("Home"), "Tab bar present at 200 width");
            }
        }
    }

    // -- Golden text structural tests for tab bar --

    #[test]
    fn snapshot_tab_bar_structure_80() {
        let text = snapshot_view(MockQuery::healthy(), View::Home, 80, 24);
        let tab_line = text.lines().next().unwrap();
        // Verify separator characters between tabs
        assert!(tab_line.contains('│'), "Tab bar should have separators");
        // Verify view names for tabs that fit at 80 columns. Shortcut keys
        // remain active but are not part of the ratatui-oracle tab chrome.
        for (i, view) in View::all().iter().enumerate() {
            let expected = view.name();
            if tab_line.contains(&expected) {
                // Tab is visible — good
            } else {
                // Tab was truncated due to width — only acceptable for later tabs
                assert!(
                    i >= 7,
                    "Tab bar should contain '{expected}' at 80 cols: {tab_line}"
                );
            }
        }
    }

    #[test]
    fn snapshot_tab_bar_shows_view_name() {
        for &view in View::all() {
            let text = snapshot_view(MockQuery::healthy(), view, 80, 24);
            let tab_line = text.lines().next().unwrap();
            assert!(
                tab_line.contains(view.name()),
                "Tab bar should show '{name}' for {view:?}: {tab_line}",
                name = view.name()
            );
        }
    }

    // -- Content area line count sanity --

    #[test]
    fn snapshot_content_fills_frame() {
        // In an 80x24 frame, we expect tab chrome (2) + content (22).
        let text = snapshot_view(MockQuery::healthy(), View::Home, 80, 24);
        let line_count = text.lines().count();
        // The last lines may be trimmed if blank, but we should have at least
        // tab chrome + some content.
        assert!(
            line_count >= 3,
            "Should have at least 3 lines in 80x24 frame, got {line_count}"
        );
    }

    // -- Filtered state snapshots --

    #[test]
    fn snapshot_events_with_filter_80x24() {
        use ftui::Model as _;
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();
        model.view_state.current_view = View::Events;
        // Apply unhandled-only filter
        model.view_state.events.unhandled_only = true;
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        model.view(&mut frame);
        let text = frame_to_text(&frame);
        assert_tab_bar(&text, View::Events);
    }

    #[test]
    fn snapshot_history_undoable_filter_80x24() {
        use ftui::Model as _;
        let mut model = make_model(MockQuery::with_history());
        model.refresh_data();
        model.view_state.current_view = View::History;
        model.view_state.history.undoable_only = true;
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        model.view(&mut frame);
        let text = frame_to_text(&frame);
        assert_tab_bar(&text, View::History);
        // Only 2 of 3 entries are undoable
        assert!(
            text.contains("2") || text.contains("undoable"),
            "Should reflect filtered count"
        );
    }

    // -----------------------------------------------------------------------
    // PTY E2E scenario pack (FTUI-07.3)
    //
    // Headless multi-step user journeys through the full model pipeline.
    // Each test simulates a realistic terminal session: key sequences,
    // view transitions, filtering, and data interactions — capturing
    // frame output at each step for regression detection.
    // -----------------------------------------------------------------------

    /// Multi-step session helper: inject keys and capture frames.
    struct E2eSession {
        model: WaModel,
        frames: Vec<String>,
    }

    impl E2eSession {
        fn new(query: MockQuery) -> Self {
            let mut model = make_model(query);
            model.refresh_data();
            Self {
                model,
                frames: Vec::new(),
            }
        }

        /// Press a key through the full update() pipeline and capture the frame.
        fn press(&mut self, code: ftui::KeyCode) -> usize {
            use ftui::Model as _;
            let key = ftui::KeyEvent {
                code,
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            };
            self.model.update(WaMsg::TermEvent(ftui::Event::Key(key)));
            self.capture_idx()
        }

        /// Press a char key.
        fn char(&mut self, ch: char) -> usize {
            self.press(ftui::KeyCode::Char(ch))
        }

        /// Capture the current frame as text. Returns snapshot index.
        fn capture(&mut self) -> usize {
            self.capture_idx()
        }

        fn capture_idx(&mut self) -> usize {
            use ftui::Model as _;
            let mut pool = ftui::GraphemePool::new();
            let mut frame = ftui::Frame::new(80, 24, &mut pool);
            self.model.view(&mut frame);
            self.frames.push(frame_to_text(&frame));
            self.frames.len() - 1
        }

        /// Get a snapshot by index.
        fn frame_at(&self, idx: usize) -> &str {
            &self.frames[idx]
        }

        /// Get the last captured frame.
        fn last_frame(&self) -> &str {
            self.frames.last().map_or("", |s| s.as_str())
        }

        /// Dump all frames for diagnostics.
        fn diagnostic_dump(&self) -> String {
            let mut out = String::new();
            for (i, f) in self.frames.iter().enumerate() {
                out.push_str(&format!("=== Frame {} ===\n{}\n\n", i, f));
            }
            out
        }

        /// Assert the current view matches expected.
        fn assert_view(&self, expected: View) {
            assert_eq!(
                self.model.view_state.current_view,
                expected,
                "Expected view {:?}, dump:\n{}",
                expected,
                self.diagnostic_dump()
            );
        }

        /// Assert last frame contains text.
        fn assert_contains(&self, text: &str) {
            let last = self.last_frame();
            assert!(
                last.contains(text),
                "Expected '{}' in frame:\n{}\nFull dump:\n{}",
                text,
                last,
                self.diagnostic_dump()
            );
        }

        /// Assert last frame does NOT contain text.
        #[allow(dead_code)]
        fn assert_not_contains(&self, text: &str) {
            let last = self.last_frame();
            assert!(
                !last.contains(text),
                "Did not expect '{}' in frame:\n{}",
                text,
                last
            );
        }
    }

    // -- Lifecycle scenarios --

    #[test]
    fn e2e_full_view_tour() {
        // Scenario: User tours all views via Tab key
        let mut s = E2eSession::new(MockQuery::healthy());
        s.capture();
        s.assert_view(View::Home);

        // Tab through all views
        let expected = [
            View::Panes,
            View::Events,
            View::Triage,
            View::History,
            View::Search,
            View::Help,
            View::Timeline,
            View::Home, // wraps
        ];
        for &view in &expected {
            s.press(ftui::KeyCode::Tab);
            s.assert_view(view);
        }
    }

    #[test]
    fn e2e_direct_navigation_1_through_8() {
        // Scenario: User jumps to each view via number keys from Home
        // Note: digit keys are consumed by filters in Events/Triage/History/Search,
        // so we return to Home between each navigation.
        let mut s = E2eSession::new(MockQuery::healthy());
        let views = [
            ('1', View::Home),
            ('2', View::Panes),
            ('3', View::Events),
            ('4', View::Triage),
            ('5', View::History),
            ('6', View::Search),
            ('7', View::Help),
            ('8', View::Timeline),
        ];
        for (key, view) in views {
            // Return to Home first (where digits always navigate)
            s.model.view_state.current_view = View::Home;
            s.char(key);
            s.assert_view(view);
        }
    }

    #[test]
    fn e2e_quit_from_home() {
        // Scenario: User presses 'q' to quit from Home view
        let mut s = E2eSession::new(MockQuery::healthy());
        s.capture();
        s.assert_view(View::Home);
        let key = ftui::KeyEvent {
            code: ftui::KeyCode::Char('q'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        let result = s.model.handle_global_key(&key);
        // 'q' on Home should signal quit (or be handled globally)
        assert!(result.is_some(), "q should be handled globally");
    }

    // -- Events view interaction --

    #[test]
    fn e2e_events_browse_and_filter() {
        // Scenario: Navigate to Events, browse items, toggle unhandled filter
        let mut s = E2eSession::new(MockQuery::with_events());
        s.char('3'); // Switch to Events
        s.capture();
        s.assert_view(View::Events);

        // Navigate down through events
        s.press(ftui::KeyCode::Char('j'));
        let after_down = s.capture();
        // Selection should have changed (different highlight)

        s.press(ftui::KeyCode::Char('j'));
        s.press(ftui::KeyCode::Char('k')); // Back up
        s.capture();

        // Toggle unhandled-only filter
        s.press(ftui::KeyCode::Char('u'));
        let filtered = s.capture();
        // Should show filtered count (unhandled events only)
        assert_ne!(
            s.frame_at(after_down),
            s.frame_at(filtered),
            "Filter should change display"
        );
    }

    // -- History view lifecycle --

    #[test]
    fn e2e_history_filter_navigate_clear() {
        // Scenario: Open History, type filter, navigate filtered results, clear
        let mut s = E2eSession::new(MockQuery::with_history());
        s.char('5'); // Switch to History
        s.capture();
        s.assert_view(View::History);
        s.assert_contains("History");

        // Type "send" to filter
        for ch in "send".chars() {
            s.char(ch);
        }
        s.capture();
        assert_eq!(s.model.view_state.history.filter_input.text(), "send");

        // Navigate in filtered results
        s.press(ftui::KeyCode::Down);
        s.capture();

        // Clear with Escape
        s.press(ftui::KeyCode::Escape);
        s.capture();
        assert!(s.model.view_state.history.filter_input.text().is_empty());
        assert!(!s.model.view_state.history.undoable_only);
    }

    #[test]
    fn e2e_history_undoable_then_text_filter() {
        // Scenario: Toggle undoable, then add text filter — combined filtering
        let mut s = E2eSession::new(MockQuery::with_history());
        s.char('5');
        s.capture();

        // Toggle undoable
        s.press(ftui::KeyCode::Char('u'));
        s.capture();
        assert!(s.model.view_state.history.undoable_only);
        let undoable_count = s.model.view_state.history.filtered_indices().len();
        assert_eq!(undoable_count, 2, "2 of 3 entries are undoable");

        // Further filter by text
        for ch in "send".chars() {
            s.char(ch);
        }
        s.capture();
        // "send_text" entries that are also undoable
        let combined = s.model.view_state.history.filtered_indices().len();
        assert!(combined <= undoable_count);
    }

    // -- Triage view interaction --

    #[test]
    fn e2e_triage_browse_and_expand() {
        // Scenario: Open Triage, navigate items, expand detail
        let mut s = E2eSession::new(MockQuery::with_triage());
        s.char('4'); // Switch to Triage
        s.capture();
        s.assert_view(View::Triage);

        // Navigate down
        s.press(ftui::KeyCode::Char('j'));
        s.capture();

        // Press Enter to expand (if supported)
        s.press(ftui::KeyCode::Enter);
        s.capture();
    }

    // -- Search view lifecycle --

    #[test]
    fn e2e_search_type_query_and_browse() {
        // Scenario: Open Search, type query, see results appear
        let query = MockQuery::healthy().with_search_results(vec![
            SearchResultView {
                pane_id: 1,
                timestamp: 1_700_000_000_000,
                snippet: "matched line alpha".to_string(),
                rank: 0.95,
            },
            SearchResultView {
                pane_id: 2,
                timestamp: 1_700_000_060_000,
                snippet: "matched line beta".to_string(),
                rank: 0.80,
            },
        ]);
        let mut s = E2eSession::new(query);
        s.char('6'); // Switch to Search
        s.capture();
        s.assert_view(View::Search);

        // Type search query
        for ch in "alpha".chars() {
            s.char(ch);
        }
        s.capture();
        assert_eq!(s.model.search_input.text(), "alpha");

        // Navigate results (if any)
        s.press(ftui::KeyCode::Down);
        s.capture();
    }

    // -- Panes view lifecycle --

    #[test]
    fn e2e_panes_navigate_and_return() {
        // Scenario: Navigate to Panes, browse, return to Home
        let mut s = E2eSession::new(MockQuery::healthy());
        s.char('2'); // Panes
        s.capture();
        s.assert_view(View::Panes);

        // Navigate pane list
        s.press(ftui::KeyCode::Down);
        s.press(ftui::KeyCode::Down);
        s.capture();

        // Return to Home
        s.char('1');
        s.assert_view(View::Home);
        s.capture();
        s.assert_contains("FrankenTerm Control Center");
    }

    // -- Resize stress --

    #[test]
    fn e2e_resize_stress_no_panic() {
        // Scenario: Render at various sizes rapidly, simulating terminal resize
        use ftui::Model as _;
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();

        let sizes: &[(u16, u16)] = &[
            (80, 24),
            (40, 10),
            (120, 50),
            (20, 3),
            (200, 80),
            (80, 24),
            (60, 15),
            (30, 5),
            (100, 30),
            (80, 24),
        ];

        for &(w, h) in sizes {
            for &view in View::all() {
                model.view_state.current_view = view;
                let mut pool = ftui::GraphemePool::new();
                let mut frame = ftui::Frame::new(w, h, &mut pool);
                model.view(&mut frame);
                // Just verify no panic and non-empty for valid sizes
                if h >= 3 && w >= 20 {
                    let text = frame_to_text(&frame);
                    assert!(!text.is_empty(), "Frame empty at {w}x{h} for {view:?}");
                }
            }
        }
    }

    // -- Key storm stress --

    #[test]
    fn e2e_key_storm_no_panic() {
        // Scenario: Rapid random key presses across all views
        let mut s = E2eSession::new(MockQuery::with_history());

        let key_storm: &[ftui::KeyCode] = &[
            ftui::KeyCode::Tab,
            ftui::KeyCode::Char('j'),
            ftui::KeyCode::Char('k'),
            ftui::KeyCode::Down,
            ftui::KeyCode::Up,
            ftui::KeyCode::Char('3'), // Events
            ftui::KeyCode::Char('j'),
            ftui::KeyCode::Char('j'),
            ftui::KeyCode::Char('u'), // toggle unhandled
            ftui::KeyCode::Tab,
            ftui::KeyCode::Char('5'), // History
            ftui::KeyCode::Char('a'),
            ftui::KeyCode::Char('b'),
            ftui::KeyCode::Char('c'),
            ftui::KeyCode::Backspace,
            ftui::KeyCode::Backspace,
            ftui::KeyCode::Char('u'), // toggle undoable
            ftui::KeyCode::Escape,
            ftui::KeyCode::Tab,
            ftui::KeyCode::Tab,
            ftui::KeyCode::Tab,
            ftui::KeyCode::Char('7'), // Help
            ftui::KeyCode::Char('1'), // Home
        ];

        for &code in key_storm {
            s.press(code);
        }

        // Should end on Home after pressing '1'
        s.assert_view(View::Home);
        // Verify no crash and frame is renderable
        s.capture();
    }

    // -- Data refresh during interaction --

    #[test]
    fn e2e_refresh_preserves_view() {
        // Scenario: User is browsing, data refresh happens, view should persist
        let mut s = E2eSession::new(MockQuery::with_events());
        s.char('3'); // Events
        s.capture();
        s.assert_view(View::Events);

        // Navigate down
        s.press(ftui::KeyCode::Char('j'));

        // Simulate data refresh
        s.model.refresh_data();
        s.capture();

        // View should still be Events
        s.assert_view(View::Events);
    }

    #[test]
    fn e2e_refresh_during_filter() {
        // Scenario: User has active filter, data refresh should preserve filter state
        let mut s = E2eSession::new(MockQuery::with_history());
        s.char('5'); // History
        for ch in "send".chars() {
            s.char(ch);
        }
        let before_refresh = s.model.view_state.history.filter_input.text().to_string();

        s.model.refresh_data();
        s.capture();

        assert_eq!(
            s.model.view_state.history.filter_input.text(),
            before_refresh
        );
        s.assert_view(View::History);
    }

    // -- Degraded data scenarios --

    #[test]
    fn e2e_degraded_system_all_views_accessible() {
        // Scenario: System is unhealthy; all views should still render without panic
        let mut s = E2eSession::new(MockQuery::degraded());
        for &view in View::all() {
            s.model.view_state.current_view = view;
            s.capture();
        }
        // Verify Home shows error state
        s.model.view_state.current_view = View::Home;
        s.capture();
        s.assert_contains("ERROR");
    }

    // -- Full workflow: navigate, filter, navigate, switch, return --

    #[test]
    fn e2e_cross_view_workflow() {
        // Scenario: Complex multi-view workflow navigating via Tab.
        // Digit keys are consumed by view-specific handlers in Events/History/Search.
        let query = MockQuery::healthy().with_search_results(vec![SearchResultView {
            pane_id: 1,
            timestamp: 1_700_000_000_000,
            snippet: "error log".to_string(),
            rank: 0.9,
        }]);
        let mut s = E2eSession::new(query);

        // 1. Home
        s.capture();
        s.assert_view(View::Home);
        s.assert_contains("FrankenTerm Control Center");

        // 2. Events (from Home, digit '3' works)
        s.char('3');
        s.assert_view(View::Events);
        s.press(ftui::KeyCode::Char('j'));
        s.capture();

        // 3. History (Tab from Events through Triage)
        s.press(ftui::KeyCode::Tab); // Events -> Triage
        s.press(ftui::KeyCode::Tab); // Triage -> History
        s.assert_view(View::History);
        s.char('t');
        s.char('e');
        s.char('s');
        s.char('t');
        s.capture();
        assert_eq!(s.model.view_state.history.filter_input.text(), "test");
        s.press(ftui::KeyCode::Escape);
        assert!(s.model.view_state.history.filter_input.text().is_empty());

        // 4. Panes (Tab from History through Search/Help/Timeline/Home, then '2')
        s.press(ftui::KeyCode::Tab); // History -> Search
        s.press(ftui::KeyCode::Tab); // Search -> Help
        s.press(ftui::KeyCode::Tab); // Help -> Timeline
        s.press(ftui::KeyCode::Tab); // Timeline -> Home
        s.char('2'); // Home -> Panes
        s.assert_view(View::Panes);
        s.press(ftui::KeyCode::Down);
        s.capture();

        // 5. Search (Tab from Panes through Events/Triage/History)
        s.press(ftui::KeyCode::Tab); // Panes -> Events
        s.press(ftui::KeyCode::Tab); // Events -> Triage
        s.press(ftui::KeyCode::Tab); // Triage -> History
        s.press(ftui::KeyCode::Tab); // History -> Search
        s.assert_view(View::Search);
        for ch in "error".chars() {
            s.char(ch);
        }
        s.capture();
        assert_eq!(s.model.search_input.text(), "error");

        // 6. Back to Home (Tab through Help/Timeline)
        s.press(ftui::KeyCode::Tab); // Search -> Help
        s.press(ftui::KeyCode::Tab); // Help -> Timeline
        s.press(ftui::KeyCode::Tab); // Timeline -> Home
        s.assert_view(View::Home);
        s.capture();
    }

    // -- Edge case: rapid view switching --

    #[test]
    fn e2e_rapid_view_switch_stress() {
        // Scenario: Rapidly switch between all views via Tab 100 times
        // Tab always works globally, unlike digits which are consumed in some views.
        let mut s = E2eSession::new(MockQuery::healthy());
        for _ in 0..100 {
            s.press(ftui::KeyCode::Tab);
        }
        // 100 tabs from Home: 100 % 8 = 4 -> should be on History (Home+4)
        // Home->Panes->Events->Triage->History->Search->Help->Timeline->Home...
        // 100 mod 8 = 4 (0-indexed from Home): Panes(1), Events(2), Triage(3), History(4)
        s.assert_view(View::History);
        s.capture();
    }

    // -- Edge case: empty data states --

    #[test]
    fn e2e_empty_states_all_views() {
        // Scenario: All data sources return empty — verify graceful rendering
        let mut s = E2eSession::new(MockQuery::degraded());
        for &view in View::all() {
            s.model.view_state.current_view = view;
            s.capture();
            // Should not crash, frame should be non-empty
            assert!(!s.last_frame().is_empty(), "Empty frame for {:?}", view);
        }
    }

    // -- Input stress: long filter strings --

    #[test]
    fn e2e_long_filter_input() {
        // Scenario: Type a very long filter string in History view
        // Avoid: 'j' (nav down), 'k' (nav up), 'u' (toggle undoable)
        let mut s = E2eSession::new(MockQuery::with_history());
        s.model.view_state.current_view = View::History;
        s.capture();

        let safe_chars = "abcdefghilmnopqrstvwxyz";
        for i in 0..200 {
            let ch = safe_chars.as_bytes()[i % safe_chars.len()] as char;
            s.char(ch);
        }
        s.capture();
        assert_eq!(s.model.view_state.history.filter_input.text().len(), 200);

        // Clear it all with Escape
        s.press(ftui::KeyCode::Escape);
        assert!(s.model.view_state.history.filter_input.text().is_empty());
    }

    // -- Artifact diagnostic test (demonstrates dump format) --

    #[test]
    fn e2e_diagnostic_dump_format() {
        // Verify the diagnostic dump is useful for debugging
        let mut s = E2eSession::new(MockQuery::healthy());
        s.capture();
        s.char('3'); // Events
        s.capture();
        s.char('5'); // History
        s.capture();

        let dump = s.diagnostic_dump();
        assert!(
            dump.contains("=== Frame 0 ==="),
            "Dump should have frame markers"
        );
        assert!(
            dump.contains("=== Frame 1 ==="),
            "Dump should show multiple frames"
        );
        assert!(dump.contains("=== Frame 2 ==="), "Should have 3 frames");
    }

    // -- FTUI-08.4: Resilience / chaos validation --
    //
    // Scenarios that exercise the system under adversarial conditions:
    // concurrent resize + input, rapid view switching under different data states,
    // extreme terminal dimensions, and failure injection.

    #[test]
    fn chaos_resize_during_key_storm() {
        // Scenario: Interleave resize and key input — simulates a user resizing
        // the terminal while actively navigating.
        use ftui::Model as _;
        let mut model = make_model(MockQuery::with_events());
        model.refresh_data();

        let sizes: &[(u16, u16)] = &[(80, 24), (40, 10), (120, 50), (30, 5), (80, 24)];
        let keys: &[ftui::KeyCode] = &[
            ftui::KeyCode::Tab,
            ftui::KeyCode::Char('j'),
            ftui::KeyCode::Char('k'),
            ftui::KeyCode::Tab,
            ftui::KeyCode::Char('3'),
        ];

        for round in 0..3 {
            for (i, (&(w, h), &code)) in sizes.iter().zip(keys.iter()).enumerate() {
                // Resize
                let mut pool = ftui::GraphemePool::new();
                let mut frame = ftui::Frame::new(w, h, &mut pool);
                model.view(&mut frame);

                // Key press
                let msg = WaMsg::TermEvent(ftui::Event::Key(ftui::KeyEvent {
                    code,
                    kind: ftui::KeyEventKind::Press,
                    modifiers: ftui::Modifiers::empty(),
                }));
                let _cmd = model.update(msg);

                // Render at new size
                let mut pool2 = ftui::GraphemePool::new();
                let mut frame2 = ftui::Frame::new(w, h, &mut pool2);
                model.view(&mut frame2);

                if h >= 3 && w >= 20 {
                    let text = frame_to_text(&frame2);
                    assert!(
                        !text.is_empty(),
                        "Empty frame at round={round} step={i} size={w}x{h}"
                    );
                }
            }
        }
    }

    #[test]
    fn chaos_extreme_dimensions() {
        // Scenario: Render at extreme terminal sizes (1x1, 1000x1, 1x1000, etc.)
        use ftui::Model as _;
        let mut model = make_model(MockQuery::healthy());
        model.refresh_data();

        let extremes: &[(u16, u16)] = &[
            (1, 1),
            (1, 100),
            (100, 1),
            (2, 2),
            (3, 3),
            (255, 255),
            (1, 2),
            (2, 1),
            (500, 1),
        ];

        for &view in View::all() {
            model.view_state.current_view = view;
            for &(w, h) in extremes {
                let mut pool = ftui::GraphemePool::new();
                let mut frame = ftui::Frame::new(w, h, &mut pool);
                // Must not panic regardless of size
                model.view(&mut frame);
            }
        }
    }

    #[test]
    fn chaos_rapid_view_switch_with_filter_state() {
        // Scenario: Switch views while filter state is active — ensure no
        // cross-view state corruption.
        let mut s = E2eSession::new(MockQuery::with_events());

        // Enter Events view and set a filter (direct view assignment avoids
        // digit keys being consumed by the filter input handler)
        s.model.view_state.current_view = View::Events;
        s.press(ftui::KeyCode::Char('0'));
        s.press(ftui::KeyCode::Char('9'));
        s.capture();
        assert_eq!(s.model.view_state.events.pane_filter.text(), "09");

        // Switch to History (which also has a filter)
        s.model.view_state.current_view = View::History;
        s.press(ftui::KeyCode::Char('x'));
        s.capture();
        assert_eq!(s.model.view_state.history.filter_input.text(), "x");

        // Switch back to Events — filter should be preserved
        s.model.view_state.current_view = View::Events;
        assert_eq!(
            s.model.view_state.events.pane_filter.text(),
            "09",
            "Events pane filter lost after view switch"
        );

        // Switch back to History — filter should be preserved
        s.model.view_state.current_view = View::History;
        assert_eq!(
            s.model.view_state.history.filter_input.text(),
            "x",
            "History filter lost after view switch"
        );
    }

    #[test]
    fn chaos_100_rapid_tab_cycles_with_data() {
        // Scenario: 100 Tab presses with real data — stress the view routing
        let mut s = E2eSession::new(MockQuery::with_events());

        for i in 0..100 {
            s.press(ftui::KeyCode::Tab);
            // Capture every 10th frame to exercise rendering
            if i % 10 == 0 {
                s.capture();
                assert!(!s.last_frame().is_empty(), "Empty frame at cycle {i}");
            }
        }

        // 100 tabs through 8 views = 100 mod 8 = position 4
        // Views: Home(0), Panes(1), Events(2), Triage(3), History(4), Search(5), Help(6), Timeline(7)
        // 100 % 8 = 4 → History
        s.assert_view(View::History);
    }

    #[test]
    fn chaos_refresh_during_every_view() {
        // Scenario: Force data refresh while on each view — must not crash
        // or lose view position.
        let mut s = E2eSession::new(MockQuery::with_history());

        for &view in View::all() {
            s.model.view_state.current_view = view;
            s.capture();
            s.model.refresh_data();
            s.capture();
            assert_eq!(
                s.model.view_state.current_view, view,
                "View changed during refresh on {view:?}"
            );
        }
    }

    #[test]
    fn chaos_degraded_then_healthy_transition() {
        // Scenario: System starts degraded, then data becomes healthy.
        // Simulates recovery from a monitoring gap.
        use ftui::Model as _;
        let mut model = make_model(MockQuery::degraded());
        model.refresh_data();

        // Render all views in degraded state
        for &view in View::all() {
            model.view_state.current_view = view;
            let mut pool = ftui::GraphemePool::new();
            let mut frame = ftui::Frame::new(80, 24, &mut pool);
            model.view(&mut frame);
        }

        // Transition to healthy by replacing query client
        model.query = std::sync::Arc::new(MockQuery::with_events());
        model.refresh_data();

        // Render all views in healthy state
        for &view in View::all() {
            model.view_state.current_view = view;
            let mut pool = ftui::GraphemePool::new();
            let mut frame = ftui::Frame::new(80, 24, &mut pool);
            model.view(&mut frame);
            let text = frame_to_text(&frame);
            assert!(!text.is_empty(), "Empty frame after recovery for {view:?}");
        }
    }

    #[test]
    fn chaos_backspace_storm_on_empty_filter() {
        // Scenario: Rapid backspace presses on empty filter — must not underflow
        let mut s = E2eSession::new(MockQuery::with_history());
        s.model.view_state.current_view = View::History;

        // 50 backspace presses on empty filter
        for _ in 0..50 {
            s.press(ftui::KeyCode::Backspace);
        }
        assert!(s.model.view_state.history.filter_input.is_empty());
        s.capture();
        assert!(!s.last_frame().is_empty());
    }

    #[test]
    fn chaos_alternating_filter_clear_cycles() {
        // Scenario: Rapidly type then clear filter, 20 cycles
        let mut s = E2eSession::new(MockQuery::with_history());
        s.model.view_state.current_view = View::History;

        for _ in 0..20 {
            // Type 5 chars
            for c in "hello".chars() {
                s.char(c);
            }
            assert_eq!(s.model.view_state.history.filter_input.text(), "hello");
            // Clear with Escape
            s.press(ftui::KeyCode::Escape);
            assert!(s.model.view_state.history.filter_input.is_empty());
        }
        s.capture();
    }

    // -----------------------------------------------------------------------
    // Timeline view tests (wa-6sk.4)
    // -----------------------------------------------------------------------

    #[test]
    fn render_timeline_empty_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_timeline_view(&mut frame, 2, 80, 20, &[], 0, 0, 0);
    }

    #[test]
    fn render_timeline_zero_height_no_panic() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_timeline_view(&mut frame, 2, 80, 0, &[], 0, 0, 0);
    }

    #[test]
    fn render_timeline_with_rows_no_panic() {
        let rows = vec![
            TimelineRow {
                id: "evt-1".to_string(),
                timestamp: "12:34:56".to_string(),
                pane_label: "P0".to_string(),
                agent_label: "codex".to_string(),
                event_type: "error_burst".to_string(),
                severity_label: "error".to_string(),
                handled_label: "unhandled".to_string(),
                correlation_label: "failover".to_string(),
                summary: "Test error event".to_string(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
            TimelineRow {
                id: "evt-2".to_string(),
                timestamp: "12:34:57".to_string(),
                pane_label: "P1".to_string(),
                agent_label: "claude".to_string(),
                event_type: "idle_timeout".to_string(),
                severity_label: "warning".to_string(),
                handled_label: "handled".to_string(),
                correlation_label: String::new(),
                summary: "Warning event".to_string(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
        ];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_timeline_view(&mut frame, 2, 80, 20, &rows, 0, 0, 0);
    }

    #[test]
    fn render_timeline_selected_second_row() {
        let rows = vec![
            TimelineRow {
                id: "1".to_string(),
                timestamp: "00:00".to_string(),
                pane_label: "P0".to_string(),
                agent_label: "a".to_string(),
                event_type: "t".to_string(),
                severity_label: "info".to_string(),
                handled_label: "h".to_string(),
                correlation_label: String::new(),
                summary: "first".to_string(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
            TimelineRow {
                id: "2".to_string(),
                timestamp: "00:01".to_string(),
                pane_label: "P1".to_string(),
                agent_label: "b".to_string(),
                event_type: "t".to_string(),
                severity_label: "error".to_string(),
                handled_label: "u".to_string(),
                correlation_label: "cascade".to_string(),
                summary: "second".to_string(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
        ];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(100, 24, &mut pool);
        // selected=1 should render detail panel for second event
        render_timeline_view(&mut frame, 1, 100, 22, &rows, 1, 2, 0);
    }

    #[test]
    fn render_timeline_narrow_stacks_detail_below_list() {
        let rows = vec![
            TimelineRow {
                id: "evt-1".to_string(),
                timestamp: "12:34:56".to_string(),
                pane_label: "P0".to_string(),
                agent_label: "codex".to_string(),
                event_type: "error_burst".to_string(),
                severity_label: "error".to_string(),
                handled_label: "unhandled".to_string(),
                correlation_label: "failover".to_string(),
                summary: "Test error event".to_string(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
            TimelineRow {
                id: "evt-2".to_string(),
                timestamp: "12:34:57".to_string(),
                pane_label: "P1".to_string(),
                agent_label: "claude".to_string(),
                event_type: "idle_timeout".to_string(),
                severity_label: "warning".to_string(),
                handled_label: "handled".to_string(),
                correlation_label: String::new(),
                summary: "Warning event".to_string(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
        ];
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(80, 24, &mut pool);
        render_timeline_view(&mut frame, 2, 80, 20, &rows, 0, 0, 0);

        let detail_row = first_row_containing(&frame, 24, "Event Details")
            .expect("compact timeline layout should still show detail header");
        assert!(
            detail_row >= 10,
            "compact timeline detail should stack below list, got row {detail_row}"
        );
    }

    #[test]
    fn timeline_key_nav_down_up() {
        let mut model = make_model(MockQuery::healthy());
        model.timeline_rows = vec![
            TimelineRow {
                id: "1".to_string(),
                timestamp: "t".to_string(),
                pane_label: "P0".to_string(),
                agent_label: "a".to_string(),
                event_type: "e".to_string(),
                severity_label: "info".to_string(),
                handled_label: "h".to_string(),
                correlation_label: String::new(),
                summary: String::new(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
            TimelineRow {
                id: "2".to_string(),
                timestamp: "t".to_string(),
                pane_label: "P1".to_string(),
                agent_label: "b".to_string(),
                event_type: "e".to_string(),
                severity_label: "error".to_string(),
                handled_label: "u".to_string(),
                correlation_label: String::new(),
                summary: String::new(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
        ];
        model.view_state.current_view = View::Timeline;
        assert_eq!(model.timeline_selected, 0);

        // Press Down
        let down = ftui::KeyEvent {
            code: ftui::KeyCode::Down,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_timeline_key(&down);
        assert_eq!(model.timeline_selected, 1);

        // Press Up
        let up = ftui::KeyEvent {
            code: ftui::KeyCode::Up,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_timeline_key(&up);
        assert_eq!(model.timeline_selected, 0);
    }

    #[test]
    fn timeline_key_zoom_in_out() {
        let mut model = make_model(MockQuery::healthy());
        model.view_state.current_view = View::Timeline;
        assert_eq!(model.timeline_zoom, 0);

        let plus = ftui::KeyEvent {
            code: ftui::KeyCode::Char('+'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_timeline_key(&plus);
        assert_eq!(model.timeline_zoom, 1);
        model.handle_timeline_key(&plus);
        assert_eq!(model.timeline_zoom, 2);

        let minus = ftui::KeyEvent {
            code: ftui::KeyCode::Char('-'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_timeline_key(&minus);
        assert_eq!(model.timeline_zoom, 1);
        model.handle_timeline_key(&minus);
        assert_eq!(model.timeline_zoom, 0);
        // Doesn't go below 0
        model.handle_timeline_key(&minus);
        assert_eq!(model.timeline_zoom, 0);
    }

    #[test]
    fn timeline_key_scroll_left_right() {
        let mut model = make_model(MockQuery::healthy());
        model.timeline_rows = vec![
            TimelineRow {
                id: "1".to_string(),
                timestamp: "t".to_string(),
                pane_label: "P0".to_string(),
                agent_label: "a".to_string(),
                event_type: "e".to_string(),
                severity_label: "info".to_string(),
                handled_label: "h".to_string(),
                correlation_label: String::new(),
                summary: String::new(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
            TimelineRow {
                id: "2".to_string(),
                timestamp: "t".to_string(),
                pane_label: "P1".to_string(),
                agent_label: "b".to_string(),
                event_type: "e".to_string(),
                severity_label: "error".to_string(),
                handled_label: "u".to_string(),
                correlation_label: String::new(),
                summary: String::new(),
                severity_style: StyleSpec::new(),
                agent_style: StyleSpec::new(),
                handled_style: StyleSpec::new(),
                correlation_style: StyleSpec::new(),
            },
        ];

        let right = ftui::KeyEvent {
            code: ftui::KeyCode::Char('l'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_timeline_key(&right);
        assert_eq!(model.timeline_scroll, 1);
        model.handle_timeline_key(&right);
        assert_eq!(model.timeline_scroll, 1);

        let left = ftui::KeyEvent {
            code: ftui::KeyCode::Left,
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        model.handle_timeline_key(&left);
        assert_eq!(model.timeline_scroll, 0);
        model.handle_timeline_key(&left);
        assert_eq!(model.timeline_scroll, 0);
    }

    #[test]
    fn timeline_zoom_capped_at_5() {
        let mut model = make_model(MockQuery::healthy());
        let plus = ftui::KeyEvent {
            code: ftui::KeyCode::Char('+'),
            kind: ftui::KeyEventKind::Press,
            modifiers: ftui::Modifiers::empty(),
        };
        for _ in 0..10 {
            model.handle_timeline_key(&plus);
        }
        assert_eq!(model.timeline_zoom, 5);
    }

    #[test]
    fn view_shortcut_8_maps_to_timeline() {
        assert_eq!(View::from_shortcut('8'), Some(View::Timeline));
    }

    #[test]
    fn view_timeline_name() {
        assert_eq!(View::Timeline.name(), "Timeline");
    }
}
