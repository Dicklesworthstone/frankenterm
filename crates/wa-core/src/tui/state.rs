//! Deterministic UI state reducer for the TUI.
//!
//! This module defines a pure reducer function that maps `(UiState, UiAction)`
//! to `(UiState, Vec<Effect>)`, separating state transitions from I/O.
//!
//! # Design principles
//!
//! 1. **Deterministic**: identical inputs produce identical outputs. No `Instant::now()`,
//!    no QueryClient calls, no filesystem access inside the reducer.
//! 2. **Side-effect free**: all I/O is expressed as `Effect` values returned alongside
//!    the new state. The caller (event loop) is responsible for executing effects.
//! 3. **Composable**: each view's state logic is a separate function, testable in
//!    isolation without mocking.
//!
//! # Refresh cadence
//!
//! The reducer itself does not track time. The event loop is responsible for:
//! - Sending `UiAction::Tick` at a fixed cadence (e.g., every 100ms).
//! - Sending `UiAction::RefreshNeeded` when the refresh interval has elapsed.
//! - Sending `UiAction::DataRefreshed(snapshot)` when data arrives.
//!
//! This decouples timer behavior from state logic, making refresh testable
//! with synthetic tick sequences.
//!
//! # Selection clamping
//!
//! After data or filter changes, selection indices are clamped to `[0, len)`
//! (or 0 if empty). Clamping is deterministic: it runs inside the reducer
//! as part of the `DataRefreshed` or filter-change handlers.
//!
//! # Deletion criterion
//!
//! This module is permanent infrastructure. It replaces the ad-hoc state
//! mutation in `app.rs::handle_key_event()` and survives the migration.

// ---------------------------------------------------------------------------
// View — navigation state machine
// ---------------------------------------------------------------------------

/// Available views in the TUI.
///
/// This duplicates `views::View` from the ratatui backend so the reducer
/// can be framework-agnostic. When the `tui` feature is dropped, this
/// becomes the single source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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
    pub const ALL: &'static [Self] = &[
        Self::Home,
        Self::Panes,
        Self::Events,
        Self::Triage,
        Self::History,
        Self::Search,
        Self::Help,
        Self::Timeline,
    ];

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
    pub fn next(self) -> Self {
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

    #[must_use]
    pub fn prev(self) -> Self {
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

    #[must_use]
    pub fn from_index(i: usize) -> Option<Self> {
        Self::ALL.get(i).copied()
    }
}

// ---------------------------------------------------------------------------
// UiState — the complete, deterministic state snapshot
// ---------------------------------------------------------------------------

/// Complete UI state.
///
/// Every field is deterministic: no `Instant`, no opaque handles, no
/// interior mutability. State is `Clone` so snapshots can be captured
/// for diff-based testing.
#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub active_view: View,
    pub should_quit: bool,
    pub error: Option<String>,

    // -- per-view selection + filter state --
    pub panes: ListState,
    pub panes_filter: String,
    pub panes_unhandled_only: bool,
    pub panes_bookmarked_only: bool,
    pub panes_agent_filter: Option<String>,
    pub panes_domain_filter: Option<String>,
    pub panes_profile_index: usize,

    pub events: ListState,
    pub events_unhandled_only: bool,
    pub events_pane_filter: String,

    pub triage: ListState,
    pub triage_expanded: Option<usize>,

    pub history: ListState,
    pub history_filter: String,
    pub history_undoable_only: bool,

    pub search_query: String,
    pub search_last_query: String,
    pub search_results: ListState,
    pub saved_search_index: usize,

    pub timeline: ListState,
    /// Zoom level for timeline view (0 = widest, higher = more detail).
    pub timeline_zoom: u8,
    /// Horizontal scroll offset in the timeline (number of events scrolled past).
    pub timeline_scroll: usize,

    // -- data item counts (for clamping) --
    pub panes_count: usize,
    pub panes_filtered_count: usize,
    pub events_count: usize,
    pub events_filtered_count: usize,
    pub triage_count: usize,
    pub history_count: usize,
    pub history_filtered_count: usize,
    pub search_results_count: usize,
    pub saved_searches_count: usize,
    pub profiles_count: usize,
    pub timeline_count: usize,
}

/// Selection state for a list view.
///
/// Tracks the selected index and total item count for clamping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListState {
    pub selected: usize,
}

impl ListState {
    /// Clamp selection to `[0, count)`, or 0 if empty.
    pub fn clamp(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Move selection down, wrapping at `count`.
    pub fn select_next(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.selected = (self.selected + 1) % count;
    }

    /// Move selection up, wrapping at `count`.
    pub fn select_prev(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        if self.selected == 0 {
            self.selected = count - 1;
        } else {
            self.selected -= 1;
        }
    }
}

// ---------------------------------------------------------------------------
// UiAction — all possible inputs to the reducer
// ---------------------------------------------------------------------------

/// Actions that trigger state transitions.
///
/// The event loop maps user input, timer ticks, and I/O results into
/// `UiAction` values and feeds them to `reduce()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    // -- navigation --
    SwitchView(View),
    NextView,
    PrevView,
    Quit,

    // -- selection --
    SelectNext,
    SelectPrev,

    // -- panes filters --
    PushPanesFilterChar(char),
    PopPanesFilterChar,
    ClearPanesFilters,
    ToggleUnhandledOnly,
    ToggleBookmarkedOnly,
    CycleAgentFilter,
    CycleDomainFilter,
    CycleProfile,

    // -- events filters --
    PushEventsFilterChar(char),
    PopEventsFilterChar,
    ClearEventsFilter,
    ToggleEventsUnhandled,

    // -- history filters --
    PushHistoryFilterChar(char),
    PopHistoryFilterChar,
    ClearHistoryFilter,
    ToggleHistoryUndoable,

    // -- triage --
    ToggleTriageExpanded,

    // -- search --
    PushSearchChar(char),
    PopSearchChar,
    ClearSearch,
    SubmitSearch,
    SearchCompleted {
        query: String,
        result_count: usize,
    },
    CycleSavedSearchNext,
    CycleSavedSearchPrev,

    // -- timeline --
    TimelineZoomIn,
    TimelineZoomOut,
    TimelineScrollLeft,
    TimelineScrollRight,

    // -- data lifecycle --
    /// Data snapshot arrived (counts for clamping).
    DataRefreshed {
        panes_count: usize,
        panes_filtered_count: usize,
        events_count: usize,
        events_filtered_count: usize,
        triage_count: usize,
        history_count: usize,
        history_filtered_count: usize,
        saved_searches_count: usize,
        profiles_count: usize,
        timeline_count: usize,
    },
    DataError(String),
    ClearError,

    // -- commands --
    QueueCommand(String),
}

// ---------------------------------------------------------------------------
// Effect — side effects to execute
// ---------------------------------------------------------------------------

/// Side effects emitted by the reducer.
///
/// The reducer never executes these directly. The event loop reads the
/// returned effects and dispatches them (run command, fetch data, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Exit the application.
    Quit,
    /// Trigger a data refresh from QueryClient.
    RefreshData,
    /// Execute an external command (suspend TUI first).
    RunCommand(String),
    /// Execute a search query via QueryClient.
    ExecuteSearch(String),
}

// ---------------------------------------------------------------------------
// Reducer — the pure state transition function
// ---------------------------------------------------------------------------

/// Pure reducer: maps `(state, action)` to `(mutated state, effects)`.
///
/// This function is deterministic: given the same `state` and `action`,
/// it always produces the same output. No I/O, no timing, no randomness.
pub fn reduce(state: &mut UiState, action: UiAction) -> Vec<Effect> {
    match action {
        // -- navigation --
        UiAction::SwitchView(view) => {
            state.active_view = view;
            vec![]
        }
        UiAction::NextView => {
            state.active_view = state.active_view.next();
            vec![]
        }
        UiAction::PrevView => {
            state.active_view = state.active_view.prev();
            vec![]
        }
        UiAction::Quit => {
            state.should_quit = true;
            vec![Effect::Quit]
        }

        // -- selection (dispatched per active view) --
        UiAction::SelectNext => {
            select_next_for_view(state);
            vec![]
        }
        UiAction::SelectPrev => {
            select_prev_for_view(state);
            vec![]
        }

        // -- panes filters --
        UiAction::PushPanesFilterChar(c) => {
            state.panes_filter.push(c);
            state.panes.selected = 0; // Reset selection on filter change.
            vec![Effect::RefreshData]
        }
        UiAction::PopPanesFilterChar => {
            state.panes_filter.pop();
            vec![Effect::RefreshData]
        }
        UiAction::ClearPanesFilters => {
            state.panes_filter.clear();
            state.panes_unhandled_only = false;
            state.panes_bookmarked_only = false;
            state.panes_agent_filter = None;
            state.panes_domain_filter = None;
            state.panes.selected = 0;
            vec![Effect::RefreshData]
        }
        UiAction::ToggleUnhandledOnly => {
            state.panes_unhandled_only = !state.panes_unhandled_only;
            state.panes.selected = 0;
            vec![]
        }
        UiAction::ToggleBookmarkedOnly => {
            state.panes_bookmarked_only = !state.panes_bookmarked_only;
            state.panes.selected = 0;
            vec![]
        }
        UiAction::CycleAgentFilter => {
            state.panes_agent_filter = cycle_agent_filter(state.panes_agent_filter.as_deref());
            state.panes.selected = 0;
            vec![]
        }
        UiAction::CycleDomainFilter => {
            state.panes_domain_filter = cycle_domain_filter(state.panes_domain_filter.as_deref());
            state.panes.selected = 0;
            vec![]
        }
        UiAction::CycleProfile => {
            if state.profiles_count > 0 {
                state.panes_profile_index = (state.panes_profile_index + 1) % state.profiles_count;
            }
            vec![]
        }

        // -- events filters --
        UiAction::PushEventsFilterChar(c) => {
            state.events_pane_filter.push(c);
            vec![]
        }
        UiAction::PopEventsFilterChar => {
            state.events_pane_filter.pop();
            vec![]
        }
        UiAction::ClearEventsFilter => {
            state.events_pane_filter.clear();
            state.events.selected = 0;
            vec![]
        }
        UiAction::ToggleEventsUnhandled => {
            state.events_unhandled_only = !state.events_unhandled_only;
            state.events.selected = 0;
            vec![]
        }

        // -- history filters --
        UiAction::PushHistoryFilterChar(c) => {
            state.history_filter.push(c);
            state.history.selected = 0;
            vec![]
        }
        UiAction::PopHistoryFilterChar => {
            state.history_filter.pop();
            vec![]
        }
        UiAction::ClearHistoryFilter => {
            state.history_filter.clear();
            state.history.selected = 0;
            vec![]
        }
        UiAction::ToggleHistoryUndoable => {
            state.history_undoable_only = !state.history_undoable_only;
            state.history.selected = 0;
            vec![]
        }

        // -- triage --
        UiAction::ToggleTriageExpanded => {
            let idx = state.triage.selected;
            if state.triage_expanded == Some(idx) {
                state.triage_expanded = None;
            } else {
                state.triage_expanded = Some(idx);
            }
            vec![]
        }

        // -- timeline --
        UiAction::TimelineZoomIn => {
            if state.timeline_zoom < 5 {
                state.timeline_zoom += 1;
            }
            vec![]
        }
        UiAction::TimelineZoomOut => {
            state.timeline_zoom = state.timeline_zoom.saturating_sub(1);
            vec![]
        }
        UiAction::TimelineScrollLeft => {
            state.timeline_scroll = state.timeline_scroll.saturating_sub(1);
            vec![]
        }
        UiAction::TimelineScrollRight => {
            if state.timeline_count > 0 {
                state.timeline_scroll =
                    (state.timeline_scroll + 1).min(state.timeline_count.saturating_sub(1));
            }
            vec![]
        }

        // -- search --
        UiAction::PushSearchChar(c) => {
            state.search_query.push(c);
            vec![]
        }
        UiAction::PopSearchChar => {
            state.search_query.pop();
            vec![]
        }
        UiAction::ClearSearch => {
            state.search_query.clear();
            state.search_last_query.clear();
            state.search_results = ListState::default();
            state.search_results_count = 0;
            vec![]
        }
        UiAction::SubmitSearch => {
            if state.search_query.is_empty() {
                return vec![];
            }
            let query = state.search_query.clone();
            state.search_last_query.clone_from(&query);
            vec![Effect::ExecuteSearch(query)]
        }
        UiAction::SearchCompleted {
            query,
            result_count,
        } => {
            if query == state.search_last_query {
                state.search_results_count = result_count;
                state.search_results.clamp(result_count);
            }
            vec![]
        }
        UiAction::CycleSavedSearchNext => {
            if state.saved_searches_count > 0 {
                state.saved_search_index =
                    (state.saved_search_index + 1) % state.saved_searches_count;
            }
            vec![]
        }
        UiAction::CycleSavedSearchPrev => {
            if state.saved_searches_count > 0 {
                if state.saved_search_index == 0 {
                    state.saved_search_index = state.saved_searches_count - 1;
                } else {
                    state.saved_search_index -= 1;
                }
            }
            vec![]
        }

        // -- data lifecycle --
        UiAction::DataRefreshed {
            panes_count,
            panes_filtered_count,
            events_count,
            events_filtered_count,
            triage_count,
            history_count,
            history_filtered_count,
            saved_searches_count,
            profiles_count,
            timeline_count,
        } => {
            state.panes_count = panes_count;
            state.panes_filtered_count = panes_filtered_count;
            state.events_count = events_count;
            state.events_filtered_count = events_filtered_count;
            state.triage_count = triage_count;
            state.history_count = history_count;
            state.history_filtered_count = history_filtered_count;
            state.saved_searches_count = saved_searches_count;
            state.profiles_count = profiles_count;
            state.timeline_count = timeline_count;

            // Clamp all selection indices to new bounds.
            state.panes.clamp(panes_filtered_count);
            state.events.clamp(events_filtered_count);
            state.triage.clamp(triage_count);
            state.history.clamp(history_filtered_count);
            state.timeline.clamp(timeline_count);
            // triage_expanded must also be valid.
            if let Some(idx) = state.triage_expanded {
                if idx >= triage_count {
                    state.triage_expanded = None;
                }
            }

            state.error = None;
            vec![]
        }
        UiAction::DataError(msg) => {
            state.error = Some(msg);
            vec![]
        }
        UiAction::ClearError => {
            state.error = None;
            vec![]
        }

        // -- commands --
        UiAction::QueueCommand(cmd) => {
            vec![Effect::RunCommand(cmd)]
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Dispatch SelectNext to the active view's list.
fn select_next_for_view(state: &mut UiState) {
    match state.active_view {
        View::Panes => state.panes.select_next(state.panes_filtered_count),
        View::Events => state.events.select_next(state.events_filtered_count),
        View::Triage => state.triage.select_next(state.triage_count),
        View::History => state.history.select_next(state.history_filtered_count),
        View::Search => state.search_results.select_next(state.search_results_count),
        View::Timeline => state.timeline.select_next(state.timeline_count),
        View::Home | View::Help => {}
    }
}

/// Dispatch SelectPrev to the active view's list.
fn select_prev_for_view(state: &mut UiState) {
    match state.active_view {
        View::Panes => state.panes.select_prev(state.panes_filtered_count),
        View::Events => state.events.select_prev(state.events_filtered_count),
        View::Triage => state.triage.select_prev(state.triage_count),
        View::History => state.history.select_prev(state.history_filtered_count),
        View::Search => state.search_results.select_prev(state.search_results_count),
        View::Timeline => state.timeline.select_prev(state.timeline_count),
        View::Home | View::Help => {}
    }
}

/// Cycle agent filter: None → codex → claude → gemini → unknown → None.
fn cycle_agent_filter(current: Option<&str>) -> Option<String> {
    match current {
        None => Some("codex".to_string()),
        Some("codex") => Some("claude".to_string()),
        Some("claude") => Some("gemini".to_string()),
        Some("gemini") => Some("unknown".to_string()),
        Some("unknown" | _) => None,
    }
}

/// Cycle domain filter: None → local → ssh → None.
fn cycle_domain_filter(current: Option<&str>) -> Option<String> {
    match current {
        None => Some("local".to_string()),
        Some("local") => Some("ssh".to_string()),
        Some("ssh" | _) => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_navigation_next_wraps() {
        assert_eq!(View::Timeline.next(), View::Home);
        assert_eq!(View::Home.next(), View::Panes);
        assert_eq!(View::Help.next(), View::Timeline);
    }

    #[test]
    fn view_navigation_prev_wraps() {
        assert_eq!(View::Home.prev(), View::Timeline);
        assert_eq!(View::Panes.prev(), View::Home);
        assert_eq!(View::Timeline.prev(), View::Help);
    }

    #[test]
    fn view_from_index() {
        assert_eq!(View::from_index(0), Some(View::Home));
        assert_eq!(View::from_index(6), Some(View::Help));
        assert_eq!(View::from_index(7), Some(View::Timeline));
        assert_eq!(View::from_index(8), None);
    }

    #[test]
    fn list_state_clamp_empty() {
        let mut ls = ListState { selected: 5 };
        ls.clamp(0);
        assert_eq!(ls.selected, 0);
    }

    #[test]
    fn list_state_clamp_within_bounds() {
        let mut ls = ListState { selected: 3 };
        ls.clamp(10);
        assert_eq!(ls.selected, 3); // Unchanged
    }

    #[test]
    fn list_state_clamp_out_of_bounds() {
        let mut ls = ListState { selected: 10 };
        ls.clamp(5);
        assert_eq!(ls.selected, 4); // Last valid index
    }

    #[test]
    fn list_state_select_next_wraps() {
        let mut ls = ListState { selected: 4 };
        ls.select_next(5);
        assert_eq!(ls.selected, 0); // Wrapped
    }

    #[test]
    fn list_state_select_prev_wraps() {
        let mut ls = ListState { selected: 0 };
        ls.select_prev(5);
        assert_eq!(ls.selected, 4); // Wrapped to end
    }

    #[test]
    fn list_state_select_next_empty() {
        let mut ls = ListState { selected: 0 };
        ls.select_next(0);
        assert_eq!(ls.selected, 0); // No change
    }

    #[test]
    fn reduce_quit() {
        let mut state = UiState::default();
        let effects = reduce(&mut state, UiAction::Quit);
        assert!(state.should_quit);
        assert_eq!(effects, vec![Effect::Quit]);
    }

    #[test]
    fn reduce_switch_view() {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::SwitchView(View::Panes));
        assert_eq!(state.active_view, View::Panes);
    }

    #[test]
    fn reduce_next_view() {
        let mut state = UiState::default();
        assert_eq!(state.active_view, View::Home);
        reduce(&mut state, UiAction::NextView);
        assert_eq!(state.active_view, View::Panes);
    }

    #[test]
    fn reduce_select_next_in_panes() {
        let mut state = UiState {
            active_view: View::Panes,
            panes_filtered_count: 5,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        assert_eq!(state.panes.selected, 1);
        reduce(&mut state, UiAction::SelectNext);
        assert_eq!(state.panes.selected, 2);
    }

    #[test]
    fn reduce_select_dispatches_per_view() {
        let mut state = UiState {
            active_view: View::Events,
            events_filtered_count: 3,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        assert_eq!(state.events.selected, 1);
        assert_eq!(state.panes.selected, 0); // Not touched
    }

    #[test]
    fn reduce_panes_filter_resets_selection() {
        let mut state = UiState {
            active_view: View::Panes,
            panes: ListState { selected: 5 },
            ..Default::default()
        };
        let effects = reduce(&mut state, UiAction::PushPanesFilterChar('a'));
        assert_eq!(state.panes_filter, "a");
        assert_eq!(state.panes.selected, 0); // Reset
        assert_eq!(effects, vec![Effect::RefreshData]);
    }

    #[test]
    fn reduce_clear_panes_filters() {
        let mut state = UiState {
            panes_filter: "test".to_string(),
            panes_unhandled_only: true,
            panes_bookmarked_only: true,
            panes_agent_filter: Some("codex".to_string()),
            panes_domain_filter: Some("local".to_string()),
            panes: ListState { selected: 3 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::ClearPanesFilters);
        assert!(state.panes_filter.is_empty());
        assert!(!state.panes_unhandled_only);
        assert!(!state.panes_bookmarked_only);
        assert!(state.panes_agent_filter.is_none());
        assert!(state.panes_domain_filter.is_none());
        assert_eq!(state.panes.selected, 0);
    }

    #[test]
    fn reduce_cycle_agent_filter() {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::CycleAgentFilter);
        assert_eq!(state.panes_agent_filter.as_deref(), Some("codex"));
        reduce(&mut state, UiAction::CycleAgentFilter);
        assert_eq!(state.panes_agent_filter.as_deref(), Some("claude"));
        reduce(&mut state, UiAction::CycleAgentFilter);
        assert_eq!(state.panes_agent_filter.as_deref(), Some("gemini"));
        reduce(&mut state, UiAction::CycleAgentFilter);
        assert_eq!(state.panes_agent_filter.as_deref(), Some("unknown"));
        reduce(&mut state, UiAction::CycleAgentFilter);
        assert!(state.panes_agent_filter.is_none());
    }

    #[test]
    fn reduce_cycle_domain_filter() {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::CycleDomainFilter);
        assert_eq!(state.panes_domain_filter.as_deref(), Some("local"));
        reduce(&mut state, UiAction::CycleDomainFilter);
        assert_eq!(state.panes_domain_filter.as_deref(), Some("ssh"));
        reduce(&mut state, UiAction::CycleDomainFilter);
        assert!(state.panes_domain_filter.is_none());
    }

    #[test]
    fn reduce_data_refreshed_clamps_selections() {
        let mut state = UiState {
            panes: ListState { selected: 10 },
            events: ListState { selected: 5 },
            triage: ListState { selected: 3 },
            triage_expanded: Some(8),
            ..Default::default()
        };
        reduce(
            &mut state,
            UiAction::DataRefreshed {
                panes_count: 20,
                panes_filtered_count: 5,
                events_count: 10,
                events_filtered_count: 3,
                triage_count: 2,
                history_count: 0,
                history_filtered_count: 0,
                saved_searches_count: 0,
                profiles_count: 0,
                timeline_count: 0,
            },
        );
        assert_eq!(state.panes.selected, 4); // Clamped to 5-1
        assert_eq!(state.events.selected, 2); // Clamped to 3-1
        assert_eq!(state.triage.selected, 1); // Clamped to 2-1
        assert!(state.triage_expanded.is_none()); // 8 >= 2 → cleared
        assert!(state.error.is_none()); // Cleared on refresh
    }

    #[test]
    fn reduce_data_error_sets_error() {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::DataError("db failed".to_string()));
        assert_eq!(state.error.as_deref(), Some("db failed"));
    }

    #[test]
    fn reduce_search_lifecycle() {
        let mut state = UiState::default();

        // Type query
        reduce(&mut state, UiAction::PushSearchChar('h'));
        reduce(&mut state, UiAction::PushSearchChar('i'));
        assert_eq!(state.search_query, "hi");

        // Submit
        let effects = reduce(&mut state, UiAction::SubmitSearch);
        assert_eq!(effects, vec![Effect::ExecuteSearch("hi".to_string())]);
        assert_eq!(state.search_last_query, "hi");

        // Results arrive
        reduce(
            &mut state,
            UiAction::SearchCompleted {
                query: "hi".to_string(),
                result_count: 3,
            },
        );
        assert_eq!(state.search_results_count, 3);

        // Clear
        reduce(&mut state, UiAction::ClearSearch);
        assert!(state.search_query.is_empty());
        assert!(state.search_last_query.is_empty());
        assert_eq!(state.search_results_count, 0);
    }

    #[test]
    fn reduce_submit_empty_search_is_noop() {
        let mut state = UiState::default();
        let effects = reduce(&mut state, UiAction::SubmitSearch);
        assert!(effects.is_empty());
    }

    #[test]
    fn reduce_stale_search_result_ignored() {
        let mut state = UiState::default();
        state.search_last_query = "new".to_string();

        // Result from old query arrives
        reduce(
            &mut state,
            UiAction::SearchCompleted {
                query: "old".to_string(),
                result_count: 10,
            },
        );
        assert_eq!(state.search_results_count, 0); // Not updated
    }

    #[test]
    fn reduce_queue_command() {
        let mut state = UiState::default();
        let effects = reduce(
            &mut state,
            UiAction::QueueCommand("wa rules profile apply default".to_string()),
        );
        assert_eq!(
            effects,
            vec![Effect::RunCommand(
                "wa rules profile apply default".to_string()
            )]
        );
    }

    #[test]
    fn reduce_triage_expand_toggle() {
        let mut state = UiState {
            active_view: View::Triage,
            triage_count: 5,
            ..Default::default()
        };
        // Expand
        reduce(&mut state, UiAction::ToggleTriageExpanded);
        assert_eq!(state.triage_expanded, Some(0));

        // Collapse (same index)
        reduce(&mut state, UiAction::ToggleTriageExpanded);
        assert!(state.triage_expanded.is_none());
    }

    // -- FTUI-07.1 gap-fill tests --

    #[test]
    fn reduce_prev_view() {
        let mut state = UiState::default();
        assert_eq!(state.active_view, View::Home);
        reduce(&mut state, UiAction::PrevView);
        assert_eq!(state.active_view, View::Timeline);
        reduce(&mut state, UiAction::PrevView);
        assert_eq!(state.active_view, View::Help);
    }

    #[test]
    fn list_state_select_prev_empty() {
        let mut ls = ListState { selected: 0 };
        ls.select_prev(0);
        assert_eq!(ls.selected, 0);
    }

    #[test]
    fn reduce_select_prev_per_view() {
        let mut state = UiState {
            active_view: View::Panes,
            panes_filtered_count: 5,
            panes: ListState { selected: 2 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectPrev);
        assert_eq!(state.panes.selected, 1);
    }

    #[test]
    fn reduce_select_dispatches_triage() {
        let mut state = UiState {
            active_view: View::Triage,
            triage_count: 4,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        assert_eq!(state.triage.selected, 1);
        reduce(&mut state, UiAction::SelectPrev);
        assert_eq!(state.triage.selected, 0);
    }

    #[test]
    fn reduce_select_dispatches_history() {
        let mut state = UiState {
            active_view: View::History,
            history_filtered_count: 3,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        assert_eq!(state.history.selected, 1);
        reduce(&mut state, UiAction::SelectPrev);
        assert_eq!(state.history.selected, 0);
    }

    #[test]
    fn reduce_select_dispatches_search() {
        let mut state = UiState {
            active_view: View::Search,
            search_results_count: 5,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        assert_eq!(state.search_results.selected, 1);
    }

    #[test]
    fn reduce_select_noop_home_help() {
        let mut state = UiState {
            active_view: View::Home,
            ..Default::default()
        };
        reduce(&mut state, UiAction::SelectNext);
        // Home has no list — nothing should change
        assert_eq!(state.panes.selected, 0);

        state.active_view = View::Help;
        reduce(&mut state, UiAction::SelectNext);
        assert_eq!(state.panes.selected, 0);
    }

    #[test]
    fn reduce_cycle_profile() {
        let mut state = UiState {
            profiles_count: 3,
            ..Default::default()
        };
        assert_eq!(state.panes_profile_index, 0);
        reduce(&mut state, UiAction::CycleProfile);
        assert_eq!(state.panes_profile_index, 1);
        reduce(&mut state, UiAction::CycleProfile);
        assert_eq!(state.panes_profile_index, 2);
        reduce(&mut state, UiAction::CycleProfile);
        assert_eq!(state.panes_profile_index, 0); // Wraps
    }

    #[test]
    fn reduce_cycle_profile_zero_noop() {
        let mut state = UiState::default();
        assert_eq!(state.profiles_count, 0);
        reduce(&mut state, UiAction::CycleProfile);
        assert_eq!(state.panes_profile_index, 0);
    }

    // -- events filter tests --

    #[test]
    fn reduce_events_push_filter_char() {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::PushEventsFilterChar('4'));
        reduce(&mut state, UiAction::PushEventsFilterChar('2'));
        assert_eq!(state.events_pane_filter, "42");
    }

    #[test]
    fn reduce_events_pop_filter_char() {
        let mut state = UiState {
            events_pane_filter: "42".to_string(),
            ..Default::default()
        };
        reduce(&mut state, UiAction::PopEventsFilterChar);
        assert_eq!(state.events_pane_filter, "4");
        reduce(&mut state, UiAction::PopEventsFilterChar);
        assert!(state.events_pane_filter.is_empty());
        reduce(&mut state, UiAction::PopEventsFilterChar);
        assert!(state.events_pane_filter.is_empty()); // No panic on empty
    }

    #[test]
    fn reduce_events_clear_filter() {
        let mut state = UiState {
            events_pane_filter: "abc".to_string(),
            events: ListState { selected: 5 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::ClearEventsFilter);
        assert!(state.events_pane_filter.is_empty());
        assert_eq!(state.events.selected, 0);
    }

    #[test]
    fn reduce_events_toggle_unhandled() {
        let mut state = UiState {
            events: ListState { selected: 3 },
            ..Default::default()
        };
        assert!(!state.events_unhandled_only);
        reduce(&mut state, UiAction::ToggleEventsUnhandled);
        assert!(state.events_unhandled_only);
        assert_eq!(state.events.selected, 0);
        reduce(&mut state, UiAction::ToggleEventsUnhandled);
        assert!(!state.events_unhandled_only);
    }

    // -- history filter tests --

    #[test]
    fn reduce_history_push_filter_char() {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::PushHistoryFilterChar('s'));
        reduce(&mut state, UiAction::PushHistoryFilterChar('e'));
        assert_eq!(state.history_filter, "se");
        assert_eq!(state.history.selected, 0);
    }

    #[test]
    fn reduce_history_push_resets_selection() {
        let mut state = UiState {
            history: ListState { selected: 5 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::PushHistoryFilterChar('x'));
        assert_eq!(state.history.selected, 0);
    }

    #[test]
    fn reduce_history_pop_filter_char() {
        let mut state = UiState {
            history_filter: "abc".to_string(),
            ..Default::default()
        };
        reduce(&mut state, UiAction::PopHistoryFilterChar);
        assert_eq!(state.history_filter, "ab");
    }

    #[test]
    fn reduce_history_clear_filter() {
        let mut state = UiState {
            history_filter: "test".to_string(),
            history: ListState { selected: 3 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::ClearHistoryFilter);
        assert!(state.history_filter.is_empty());
        assert_eq!(state.history.selected, 0);
    }

    #[test]
    fn reduce_history_toggle_undoable() {
        let mut state = UiState {
            history: ListState { selected: 2 },
            ..Default::default()
        };
        assert!(!state.history_undoable_only);
        reduce(&mut state, UiAction::ToggleHistoryUndoable);
        assert!(state.history_undoable_only);
        assert_eq!(state.history.selected, 0);
        reduce(&mut state, UiAction::ToggleHistoryUndoable);
        assert!(!state.history_undoable_only);
    }

    // -- saved search cycle tests --

    #[test]
    fn reduce_cycle_saved_search_next() {
        let mut state = UiState {
            saved_searches_count: 3,
            ..Default::default()
        };
        reduce(&mut state, UiAction::CycleSavedSearchNext);
        assert_eq!(state.saved_search_index, 1);
        reduce(&mut state, UiAction::CycleSavedSearchNext);
        assert_eq!(state.saved_search_index, 2);
        reduce(&mut state, UiAction::CycleSavedSearchNext);
        assert_eq!(state.saved_search_index, 0); // Wraps
    }

    #[test]
    fn reduce_cycle_saved_search_prev() {
        let mut state = UiState {
            saved_searches_count: 3,
            ..Default::default()
        };
        reduce(&mut state, UiAction::CycleSavedSearchPrev);
        assert_eq!(state.saved_search_index, 2); // Wraps to end
        reduce(&mut state, UiAction::CycleSavedSearchPrev);
        assert_eq!(state.saved_search_index, 1);
        reduce(&mut state, UiAction::CycleSavedSearchPrev);
        assert_eq!(state.saved_search_index, 0);
    }

    #[test]
    fn reduce_cycle_saved_search_empty_noop() {
        let mut state = UiState::default();
        assert_eq!(state.saved_searches_count, 0);
        reduce(&mut state, UiAction::CycleSavedSearchNext);
        assert_eq!(state.saved_search_index, 0);
        reduce(&mut state, UiAction::CycleSavedSearchPrev);
        assert_eq!(state.saved_search_index, 0);
    }

    // -- clear error test --

    #[test]
    fn reduce_clear_error() {
        let mut state = UiState {
            error: Some("something failed".to_string()),
            ..Default::default()
        };
        reduce(&mut state, UiAction::ClearError);
        assert!(state.error.is_none());
    }

    #[test]
    fn reduce_clear_error_when_none() {
        let mut state = UiState::default();
        reduce(&mut state, UiAction::ClearError);
        assert!(state.error.is_none());
    }

    // -- panes toggle tests --

    #[test]
    fn reduce_toggle_unhandled_only() {
        let mut state = UiState {
            panes: ListState { selected: 3 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::ToggleUnhandledOnly);
        assert!(state.panes_unhandled_only);
        assert_eq!(state.panes.selected, 0);
        reduce(&mut state, UiAction::ToggleUnhandledOnly);
        assert!(!state.panes_unhandled_only);
    }

    #[test]
    fn reduce_toggle_bookmarked_only() {
        let mut state = UiState {
            panes: ListState { selected: 2 },
            ..Default::default()
        };
        reduce(&mut state, UiAction::ToggleBookmarkedOnly);
        assert!(state.panes_bookmarked_only);
        assert_eq!(state.panes.selected, 0);
    }

    #[test]
    fn reduce_pop_panes_filter() {
        let mut state = UiState {
            panes_filter: "abc".to_string(),
            ..Default::default()
        };
        let effects = reduce(&mut state, UiAction::PopPanesFilterChar);
        assert_eq!(state.panes_filter, "ab");
        assert_eq!(effects, vec![Effect::RefreshData]);
    }

    // -- search backspace test --

    #[test]
    fn reduce_pop_search_char() {
        let mut state = UiState {
            search_query: "hello".to_string(),
            ..Default::default()
        };
        reduce(&mut state, UiAction::PopSearchChar);
        assert_eq!(state.search_query, "hell");
    }

    // -- DataRefreshed clears triage_expanded within range --

    #[test]
    fn reduce_data_refreshed_preserves_triage_expanded_in_range() {
        let mut state = UiState {
            triage_expanded: Some(1),
            ..Default::default()
        };
        reduce(
            &mut state,
            UiAction::DataRefreshed {
                panes_count: 0,
                panes_filtered_count: 0,
                events_count: 0,
                events_filtered_count: 0,
                triage_count: 5,
                history_count: 0,
                history_filtered_count: 0,
                saved_searches_count: 0,
                profiles_count: 0,
                timeline_count: 0,
            },
        );
        assert_eq!(state.triage_expanded, Some(1)); // Still valid
    }

    #[test]
    fn determinism_identical_inputs_produce_identical_outputs() {
        // Run the same sequence twice and verify state matches.
        let actions = vec![
            UiAction::SwitchView(View::Panes),
            UiAction::SelectNext,
            UiAction::SelectNext,
            UiAction::PushPanesFilterChar('a'),
            UiAction::ToggleUnhandledOnly,
            UiAction::NextView,
            UiAction::SelectNext,
        ];

        let mut state1 = UiState {
            panes_filtered_count: 10,
            events_filtered_count: 5,
            ..Default::default()
        };
        let mut effects1 = vec![];
        for a in &actions {
            effects1.extend(reduce(&mut state1, a.clone()));
        }

        let mut state2 = UiState {
            panes_filtered_count: 10,
            events_filtered_count: 5,
            ..Default::default()
        };
        let mut effects2 = vec![];
        for a in &actions {
            effects2.extend(reduce(&mut state2, a.clone()));
        }

        assert_eq!(state1.active_view, state2.active_view);
        assert_eq!(state1.panes.selected, state2.panes.selected);
        assert_eq!(state1.events.selected, state2.events.selected);
        assert_eq!(state1.panes_filter, state2.panes_filter);
        assert_eq!(state1.panes_unhandled_only, state2.panes_unhandled_only);
        assert_eq!(effects1, effects2);
    }

    // -- Timeline reducer tests (wa-6sk.4) --

    #[test]
    fn timeline_zoom_in_increments() {
        let mut state = UiState::default();
        assert_eq!(state.timeline_zoom, 0);
        reduce(&mut state, UiAction::TimelineZoomIn);
        assert_eq!(state.timeline_zoom, 1);
        reduce(&mut state, UiAction::TimelineZoomIn);
        assert_eq!(state.timeline_zoom, 2);
    }

    #[test]
    fn timeline_zoom_in_capped_at_5() {
        let mut state = UiState::default();
        state.timeline_zoom = 5;
        reduce(&mut state, UiAction::TimelineZoomIn);
        assert_eq!(state.timeline_zoom, 5);
    }

    #[test]
    fn timeline_zoom_out_decrements() {
        let mut state = UiState::default();
        state.timeline_zoom = 3;
        reduce(&mut state, UiAction::TimelineZoomOut);
        assert_eq!(state.timeline_zoom, 2);
    }

    #[test]
    fn timeline_zoom_out_floors_at_0() {
        let mut state = UiState::default();
        assert_eq!(state.timeline_zoom, 0);
        reduce(&mut state, UiAction::TimelineZoomOut);
        assert_eq!(state.timeline_zoom, 0);
    }

    #[test]
    fn timeline_scroll_right_increments() {
        let mut state = UiState::default();
        state.timeline_count = 10;
        reduce(&mut state, UiAction::TimelineScrollRight);
        assert_eq!(state.timeline_scroll, 1);
    }

    #[test]
    fn timeline_scroll_right_clamped() {
        let mut state = UiState::default();
        state.timeline_count = 3;
        state.timeline_scroll = 2;
        reduce(&mut state, UiAction::TimelineScrollRight);
        assert_eq!(state.timeline_scroll, 2); // clamped to count-1
    }

    #[test]
    fn timeline_scroll_right_empty() {
        let mut state = UiState::default();
        state.timeline_count = 0;
        reduce(&mut state, UiAction::TimelineScrollRight);
        assert_eq!(state.timeline_scroll, 0);
    }

    #[test]
    fn timeline_scroll_left_decrements() {
        let mut state = UiState::default();
        state.timeline_scroll = 5;
        reduce(&mut state, UiAction::TimelineScrollLeft);
        assert_eq!(state.timeline_scroll, 4);
    }

    #[test]
    fn timeline_scroll_left_floors_at_0() {
        let mut state = UiState::default();
        assert_eq!(state.timeline_scroll, 0);
        reduce(&mut state, UiAction::TimelineScrollLeft);
        assert_eq!(state.timeline_scroll, 0);
    }

    #[test]
    fn timeline_data_refreshed_clamps_selection() {
        let mut state = UiState::default();
        state.timeline.selected = 10;
        reduce(
            &mut state,
            UiAction::DataRefreshed {
                panes_count: 0,
                panes_filtered_count: 0,
                events_count: 0,
                events_filtered_count: 0,
                triage_count: 0,
                history_count: 0,
                history_filtered_count: 0,
                saved_searches_count: 0,
                profiles_count: 0,
                timeline_count: 3,
            },
        );
        assert_eq!(state.timeline_count, 3);
    }
}
