//! Dashboard models for frankenterm-gui.
//!
//! `LivePaneDashboard` powers the visible pane dashboard with current mux
//! registrations, text filtering, and exact focus selection. The older agent
//! panel models below remain available for the future watcher integration;
//! agent activity, health, and events are not yet live GUI data sources.
#![allow(dead_code)]

use frankenterm_core::agent_pane_state::{AgentPaneState, AutoLayoutPolicy};
use mux::pane::PaneId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bound the operator's pane picker independently of the mux's fleet limit.
pub const MAX_LIVE_DASHBOARD_PANES: usize = 4096;
const MAX_DASHBOARD_TEXT_CHARS: usize = 256;

/// Display metadata from one exact, live mux registration. Agent state is
/// deliberately absent: the watcher has not yet published an identity-bound
/// activity snapshot to the GUI (ft-7h5da.7.10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveDashboardPane {
    pub pane_id: PaneId,
    pub registration: [u8; 16],
    pub tab_id: mux::tab::TabId,
    pub tab_position: usize,
    pub domain: String,
    pub title: String,
    pub is_focused: bool,
}

/// Keep terminal-controlled titles out of the dashboard's control syntax and
/// bound their contribution to filtering and layout.
pub fn dashboard_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_DASHBOARD_TEXT_CHARS)
        .collect()
}

impl LiveDashboardPane {
    pub fn label(&self) -> String {
        format!(
            "{}Tab {} / pane {} · {} · {}",
            if self.is_focused { "● " } else { "" },
            self.tab_position.saturating_add(1),
            self.pane_id,
            self.domain,
            self.title,
        )
    }

    fn matches(&self, query: &str) -> bool {
        let text = self.label().to_lowercase();
        query.split_whitespace().all(|word| text.contains(word))
    }
}

/// Selection is tied to the registration generation rather than a row index
/// or reusable numeric pane id. Refreshing cannot silently retarget Enter.
#[derive(Default)]
pub struct LivePaneDashboard {
    panes: Vec<LiveDashboardPane>,
    query: String,
    selected: Option<[u8; 16]>,
    initialized: bool,
    pub truncated: bool,
}

impl LivePaneDashboard {
    pub fn replace_panes(&mut self, mut panes: Vec<LiveDashboardPane>, truncated: bool) {
        self.truncated = truncated || panes.len() > MAX_LIVE_DASHBOARD_PANES;
        panes.truncate(MAX_LIVE_DASHBOARD_PANES);
        self.panes = panes;
        let matches = self.matching_indices();
        if !self.initialized {
            self.selected = matches
                .iter()
                .find(|&&index| self.panes[index].is_focused)
                .or_else(|| matches.first())
                .map(|&index| self.panes[index].registration);
            self.initialized = true;
        } else if !matches
            .iter()
            .any(|&index| Some(self.panes[index].registration) == self.selected)
        {
            self.selected = None;
        }
    }

    pub fn panes(&self) -> &[LiveDashboardPane] {
        &self.panes
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, query: String) {
        self.query = dashboard_text(&query);
        self.selected = self
            .matching_indices()
            .first()
            .map(|&index| self.panes[index].registration);
    }

    pub fn matching_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.panes
            .iter()
            .enumerate()
            .filter_map(|(index, pane)| pane.matches(&query).then_some(index))
            .collect()
    }

    pub fn selected_registration(&self) -> Option<[u8; 16]> {
        self.selected
    }

    pub fn selected_row(&self, matches: &[usize]) -> Option<usize> {
        matches
            .iter()
            .position(|&index| Some(self.panes[index].registration) == self.selected)
    }

    pub fn move_selection(&mut self, forward: bool) {
        let matches = self.matching_indices();
        if matches.is_empty() {
            self.selected = None;
            return;
        }
        let row = match self.selected_row(&matches) {
            Some(row) if forward => row.saturating_add(1).min(matches.len() - 1),
            Some(row) => row.saturating_sub(1),
            None if forward => 0,
            None => matches.len() - 1,
        };
        self.selected = Some(self.panes[matches[row]].registration);
    }
}

/// Which section of the dashboard is currently focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardSection {
    #[default]
    PaneList,
    Health,
    Events,
    Search,
}

impl DashboardSection {
    pub fn next(self) -> Self {
        match self {
            Self::PaneList => Self::Health,
            Self::Health => Self::Events,
            Self::Events => Self::Search,
            Self::Search => Self::PaneList,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::PaneList => "Panes",
            Self::Health => "Health",
            Self::Events => "Events",
            Self::Search => "Search",
        }
    }
}

/// A row in the pane list section.
#[derive(Debug, Clone)]
pub struct PaneListEntry {
    pub pane_id: PaneId,
    pub title: String,
    pub agent_state: AgentPaneState,
    pub agent_name: Option<String>,
    pub backpressure_tier: String,
    pub is_focused: bool,
}

/// An event entry in the events section.
#[derive(Debug, Clone)]
pub struct DashboardEvent {
    pub timestamp_ms: u64,
    pub pane_id: Option<PaneId>,
    pub severity: EventSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSeverity {
    Info,
    Warning,
    Error,
}

/// A search result entry.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub pane_id: PaneId,
    pub pane_title: String,
    pub line_number: i64,
    pub matched_text: String,
}

/// Dashboard panel state.
#[derive(Debug)]
pub struct DashboardPanel {
    /// Whether the dashboard is visible.
    pub visible: bool,
    /// Which section is focused.
    pub active_section: DashboardSection,
    /// Width of the dashboard panel in pixels (side panel mode).
    pub panel_width_px: u32,
    /// Position: left or right side.
    pub position: DashboardPosition,
    /// Pane list entries (updated each render tick).
    pub pane_entries: Vec<PaneListEntry>,
    /// Selected pane index in the list.
    pub selected_pane_idx: usize,
    /// Recent events ring buffer.
    pub events: Vec<DashboardEvent>,
    /// Maximum events to keep.
    pub max_events: usize,
    /// Search query text.
    pub search_query: String,
    /// Search results.
    pub search_results: Vec<SearchResult>,
    /// Current auto-layout policy.
    pub auto_layout: AutoLayoutPolicy,
    /// Summary stats.
    pub stats: DashboardStats,
}

/// Dashboard panel position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardPosition {
    #[default]
    Right,
    Left,
}

/// Aggregate statistics shown at the top of the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DashboardStats {
    pub total_panes: usize,
    pub agent_panes: usize,
    pub active_count: usize,
    pub thinking_count: usize,
    pub stuck_count: usize,
    pub idle_count: usize,
    pub human_count: usize,
}

impl DashboardStats {
    /// Compute stats from agent state map.
    pub fn from_states(states: &HashMap<PaneId, AgentPaneState>, total_panes: usize) -> Self {
        let mut stats = Self {
            total_panes,
            ..Default::default()
        };
        for state in states.values() {
            match state {
                AgentPaneState::Active => {
                    stats.active_count += 1;
                    stats.agent_panes += 1;
                }
                AgentPaneState::Thinking => {
                    stats.thinking_count += 1;
                    stats.agent_panes += 1;
                }
                AgentPaneState::Stuck => {
                    stats.stuck_count += 1;
                    stats.agent_panes += 1;
                }
                AgentPaneState::Idle => {
                    stats.idle_count += 1;
                    stats.agent_panes += 1;
                }
                AgentPaneState::Human => {
                    stats.human_count += 1;
                }
            }
        }
        stats
    }
}

impl Default for DashboardPanel {
    fn default() -> Self {
        Self {
            visible: false,
            active_section: DashboardSection::default(),
            panel_width_px: 320,
            position: DashboardPosition::default(),
            pane_entries: Vec::new(),
            selected_pane_idx: 0,
            events: Vec::new(),
            max_events: 200,
            search_query: String::new(),
            search_results: Vec::new(),
            auto_layout: AutoLayoutPolicy::default(),
            stats: DashboardStats::default(),
        }
    }
}

impl DashboardPanel {
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    pub fn next_section(&mut self) {
        self.active_section = self.active_section.next();
    }

    pub fn select_next_pane(&mut self) {
        if !self.pane_entries.is_empty() {
            self.selected_pane_idx = (self.selected_pane_idx + 1) % self.pane_entries.len();
        }
    }

    pub fn select_prev_pane(&mut self) {
        if !self.pane_entries.is_empty() {
            self.selected_pane_idx = self
                .selected_pane_idx
                .checked_sub(1)
                .unwrap_or(self.pane_entries.len() - 1);
        }
    }

    /// Returns the PaneId of the currently selected pane, if any.
    pub fn selected_pane_id(&self) -> Option<PaneId> {
        self.pane_entries
            .get(self.selected_pane_idx)
            .map(|e| e.pane_id)
    }

    /// Push an event, evicting oldest if at capacity.
    pub fn push_event(&mut self, event: DashboardEvent) {
        if self.events.len() >= self.max_events {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    /// Update pane list from current mux state and agent detection.
    pub fn update_pane_list(
        &mut self,
        entries: Vec<PaneListEntry>,
        agent_states: &HashMap<PaneId, AgentPaneState>,
    ) {
        self.pane_entries = entries;
        self.stats = DashboardStats::from_states(agent_states, self.pane_entries.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_pane(pane_id: PaneId, generation: u8, title: &str) -> LiveDashboardPane {
        LiveDashboardPane {
            pane_id,
            registration: [generation; 16],
            tab_id: 7,
            tab_position: 0,
            domain: "build-host".into(),
            title: title.into(),
            is_focused: false,
        }
    }

    #[test]
    fn live_refresh_preserves_exact_selection_across_reorder() {
        let first = live_pane(1, 1, "first");
        let second = live_pane(2, 2, "second");
        let mut panel = LivePaneDashboard::default();
        panel.replace_panes(vec![first.clone(), second.clone()], false);
        panel.move_selection(true);
        panel.replace_panes(vec![second, first], false);
        assert_eq!(panel.selected_registration(), Some([2; 16]));
        assert_eq!(panel.selected_row(&panel.matching_indices()), Some(0));
    }

    #[test]
    fn retired_selection_never_inherits_a_reused_pane_id() {
        let mut panel = LivePaneDashboard::default();
        panel.replace_panes(vec![live_pane(5, 1, "old")], false);
        panel.replace_panes(vec![live_pane(5, 2, "replacement")], false);
        assert_eq!(panel.selected_registration(), None);
        assert_eq!(panel.selected_row(&panel.matching_indices()), None);
        panel.move_selection(true);
        assert_eq!(panel.selected_registration(), Some([2; 16]));
        panel.replace_panes(vec![], false);
        panel.move_selection(false);
        assert_eq!(panel.selected_registration(), None);
    }

    #[test]
    fn live_dashboard_filters_all_terms_and_handles_no_matches() {
        let mut panel = LivePaneDashboard::default();
        panel.replace_panes(
            vec![live_pane(1, 1, "Compile Rust"), live_pane(2, 2, "Python")],
            false,
        );
        panel.set_query("RUST BUILD-HOST".into());
        assert_eq!(panel.matching_indices(), vec![0]);
        assert_eq!(panel.selected_registration(), Some([1; 16]));
        panel.set_query("absent".into());
        panel.move_selection(true);
        assert!(panel.matching_indices().is_empty());
        assert_eq!(panel.selected_registration(), None);
    }

    #[test]
    fn live_dashboard_bounds_rows_and_terminal_controlled_text() {
        let mut panel = LivePaneDashboard::default();
        panel.replace_panes(
            (0..MAX_LIVE_DASHBOARD_PANES + 1)
                .map(|id| live_pane(id, 1, "pane"))
                .collect(),
            false,
        );
        assert_eq!(panel.panes().len(), MAX_LIVE_DASHBOARD_PANES);
        assert!(panel.truncated);
        assert_eq!(dashboard_text("a\nb\rc\t\u{1b}"), "abc");
        assert_eq!(dashboard_text(&"λ".repeat(1024)).chars().count(), 256);
    }

    #[test]
    fn dashboard_toggle() {
        let mut panel = DashboardPanel::default();
        assert!(!panel.visible);
        panel.toggle();
        assert!(panel.visible);
        panel.toggle();
        assert!(!panel.visible);
    }

    #[test]
    fn section_cycling() {
        let mut panel = DashboardPanel::default();
        assert_eq!(panel.active_section, DashboardSection::PaneList);
        panel.next_section();
        assert_eq!(panel.active_section, DashboardSection::Health);
        panel.next_section();
        assert_eq!(panel.active_section, DashboardSection::Events);
        panel.next_section();
        assert_eq!(panel.active_section, DashboardSection::Search);
        panel.next_section();
        assert_eq!(panel.active_section, DashboardSection::PaneList);
    }

    #[test]
    fn pane_selection_wraps() {
        let mut panel = DashboardPanel::default();
        panel.pane_entries = vec![
            PaneListEntry {
                pane_id: 1,
                title: "pane1".into(),
                agent_state: AgentPaneState::Active,
                agent_name: Some("Claude".into()),
                backpressure_tier: "Green".into(),
                is_focused: true,
            },
            PaneListEntry {
                pane_id: 2,
                title: "pane2".into(),
                agent_state: AgentPaneState::Idle,
                agent_name: None,
                backpressure_tier: "Green".into(),
                is_focused: false,
            },
        ];
        assert_eq!(panel.selected_pane_idx, 0);
        panel.select_next_pane();
        assert_eq!(panel.selected_pane_idx, 1);
        panel.select_next_pane();
        assert_eq!(panel.selected_pane_idx, 0); // wraps
        panel.select_prev_pane();
        assert_eq!(panel.selected_pane_idx, 1); // wraps backward
    }

    #[test]
    fn stats_from_states() {
        let mut states = HashMap::new();
        states.insert(1, AgentPaneState::Active);
        states.insert(2, AgentPaneState::Thinking);
        states.insert(3, AgentPaneState::Stuck);
        states.insert(4, AgentPaneState::Idle);
        states.insert(5, AgentPaneState::Human);
        let stats = DashboardStats::from_states(&states, 5);
        assert_eq!(stats.total_panes, 5);
        assert_eq!(stats.agent_panes, 4);
        assert_eq!(stats.active_count, 1);
        assert_eq!(stats.thinking_count, 1);
        assert_eq!(stats.stuck_count, 1);
        assert_eq!(stats.idle_count, 1);
        assert_eq!(stats.human_count, 1);
    }

    #[test]
    fn event_ring_buffer() {
        let mut panel = DashboardPanel::default();
        panel.max_events = 3;
        for i in 0..5 {
            panel.push_event(DashboardEvent {
                timestamp_ms: i * 1000,
                pane_id: Some(i as PaneId),
                severity: EventSeverity::Info,
                message: format!("event {i}"),
            });
        }
        assert_eq!(panel.events.len(), 3);
        assert_eq!(panel.events[0].message, "event 2");
        assert_eq!(panel.events[2].message, "event 4");
    }
}
