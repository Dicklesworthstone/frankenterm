//! Embedded agent swarm dashboard panel.
//!
//! Provides a side-panel/overlay within frankenterm-gui that shows:
//! - Pane list with agent state indicators
//! - Health/backpressure status per pane
//! - Event stream (recent watchdog, circuit-breaker, error events)
//! - Search across all panes (with click-to-focus results)
//!
//! Toggled via Cmd+Shift+D (macOS) or Ctrl+Shift+D (Linux).
//!
//! Data sources: mux pane state, agent_pane_state detection,
//! backpressure manager, watchdog health reports.

use frankenterm_core::agent_pane_state::{AgentPaneState, AutoLayoutPolicy};
use mux::pane::PaneId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
