//! TUI views and screen definitions
//!
//! Each view represents a distinct screen in the TUI with its own
//! state, keybindings, and rendering logic.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs, Widget},
};
use std::collections::{HashMap, HashSet};

use super::query::{
    EventView, HealthStatus, HistoryEntryView, PaneBookmarkView, PaneView, RulesetProfileState,
    SavedSearchView, SearchResultView, TriageItemView, WorkflowProgressView,
};
use crate::circuit_breaker::CircuitStateKind;

/// Available views in the TUI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Home/dashboard view showing system overview
    #[default]
    Home,
    /// List of panes with status
    Panes,
    /// Event feed
    Events,
    /// Triage view (prioritized issues + quick actions)
    Triage,
    /// Action history view (audit + undo metadata)
    History,
    /// Search interface
    Search,
    /// Help screen
    Help,
    /// Unified event timeline with cross-pane correlations (wa-6sk.4)
    Timeline,
}

impl View {
    /// Get the display name for this view
    #[must_use]
    pub const fn name(self) -> &'static str {
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

    /// Get all views in tab order
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

    /// Get the index of this view in the tab order
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Home => 0,
            Self::Panes => 1,
            Self::Events => 2,
            Self::Triage => 3,
            Self::History => 4,
            Self::Search => 5,
            Self::Help => 6,
            Self::Timeline => 7,
        }
    }

    /// Get the next view (wraps around)
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

    /// Get the previous view (wraps around)
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
}

/// State for each view
#[derive(Debug, Default)]
pub struct ViewState {
    /// Panes list for display
    pub panes: Vec<PaneView>,
    /// Events list for display
    pub events: Vec<EventView>,
    /// Action history entries for display
    pub history_entries: Vec<HistoryEntryView>,
    /// Triage items for display
    pub triage_items: Vec<TriageItemView>,
    /// Current health status
    pub health: Option<HealthStatus>,
    /// Search query input
    pub search_query: String,
    /// Free-text pane filter (matches title/cwd/domain/pane id)
    pub panes_filter_query: String,
    /// Only show panes with unhandled events
    pub panes_unhandled_only: bool,
    /// Optional agent filter (codex/claude/gemini/unknown)
    pub panes_agent_filter: Option<String>,
    /// Optional domain filter (e.g., local/ssh)
    pub panes_domain_filter: Option<String>,
    /// Error message to display (if any)
    pub error_message: Option<String>,
    /// Selected index in list views
    pub selected_index: usize,
    /// Selected index in triage view
    pub triage_selected_index: usize,
    /// Events: show only unhandled events
    pub events_unhandled_only: bool,
    /// Events: filter by pane id (text)
    pub events_pane_filter: String,
    /// Events: selected index (separate from panes)
    pub events_selected_index: usize,
    /// History: selected index
    pub history_selected_index: usize,
    /// History: free-text filter (pane/workflow/action/audit id)
    pub history_filter_query: String,
    /// History: show only currently undoable actions
    pub history_undoable_only: bool,
    /// Search: last executed query (for display)
    pub search_last_query: String,
    /// Search: results from last query
    pub search_results: Vec<SearchResultView>,
    /// Search: selected result index
    pub search_selected_index: usize,
    /// Saved searches for search view.
    pub saved_searches: Vec<SavedSearchView>,
    /// Selected saved search index.
    pub saved_search_selected_index: usize,
    /// Active workflows for progress display
    pub workflows: Vec<WorkflowProgressView>,
    /// Expanded workflow index in triage view (None = collapsed)
    pub triage_expanded: Option<usize>,
    /// Bookmark records for panes.
    pub pane_bookmarks: Vec<PaneBookmarkView>,
    /// Optional ruleset profile state.
    pub ruleset_profile_state: Option<RulesetProfileState>,
    /// Selected profile index in the profile selector.
    pub selected_ruleset_profile_index: usize,
    /// Only show panes that have at least one bookmark.
    pub panes_bookmarked_only: bool,
    /// Search: inline suggestions based on current query input.
    pub search_suggestions: Vec<crate::storage::SearchSuggestion>,
}

impl ViewState {
    /// Clear any error message
    pub fn clear_error(&mut self) {
        self.error_message = None;
    }

    /// Set an error message
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error_message = Some(msg.into());
    }
}

/// Return pane indices that match active pane filters.
#[must_use]
pub fn filtered_pane_indices(state: &ViewState) -> Vec<usize> {
    let query = state.panes_filter_query.trim().to_ascii_lowercase();
    let bookmarked_panes: HashSet<u64> = state.pane_bookmarks.iter().map(|b| b.pane_id).collect();
    state
        .panes
        .iter()
        .enumerate()
        .filter(|(_, pane)| {
            if state.panes_unhandled_only && pane.unhandled_event_count == 0 {
                return false;
            }

            if let Some(agent_filter) = &state.panes_agent_filter {
                let agent = pane.agent_type.as_deref().unwrap_or("unknown");
                if !agent.eq_ignore_ascii_case(agent_filter) {
                    return false;
                }
            }

            if let Some(domain_filter) = &state.panes_domain_filter {
                let domain = pane.domain.to_ascii_lowercase();
                let filter = domain_filter.to_ascii_lowercase();
                if filter == "ssh" {
                    if !domain.contains("ssh") {
                        return false;
                    }
                } else if !domain.contains(&filter) {
                    return false;
                }
            }

            if state.panes_bookmarked_only && !bookmarked_panes.contains(&pane.pane_id) {
                return false;
            }

            if query.is_empty() {
                return true;
            }

            let pane_id = pane.pane_id.to_string();
            let title = pane.title.to_ascii_lowercase();
            let domain = pane.domain.to_ascii_lowercase();
            let cwd = pane.cwd.as_deref().unwrap_or("").to_ascii_lowercase();
            pane_id.contains(&query)
                || title.contains(&query)
                || domain.contains(&query)
                || cwd.contains(&query)
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// Return event indices that match active event filters.
#[must_use]
pub fn filtered_event_indices(state: &ViewState) -> Vec<usize> {
    let pane_query = state.events_pane_filter.trim();
    state
        .events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            if state.events_unhandled_only && event.handled {
                return false;
            }
            if !pane_query.is_empty() {
                let pane_str = event.pane_id.to_string();
                if !pane_str.contains(pane_query) && !event.rule_id.contains(pane_query) {
                    return false;
                }
            }
            true
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// Return action-history indices that match active history filters.
#[must_use]
pub fn filtered_history_indices(state: &ViewState) -> Vec<usize> {
    let query = state.history_filter_query.trim().to_ascii_lowercase();
    state
        .history_entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            if state.history_undoable_only && !entry.undoable {
                return false;
            }

            if query.is_empty() {
                return true;
            }

            let pane = entry
                .pane_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string());
            let workflow = entry.workflow_id.as_deref().unwrap_or("-");
            let step = entry.step_name.as_deref().unwrap_or("-");
            let rule = entry.rule_id.as_deref().unwrap_or("-");
            let audit = entry.audit_id.to_string();
            let haystack = format!(
                "{pane} {workflow} {} {} {} {step} {rule} {audit}",
                entry.action_kind, entry.result, entry.actor_kind
            )
            .to_ascii_lowercase();
            haystack.contains(&query)
        })
        .map(|(idx, _)| idx)
        .collect()
}

/// Render the navigation tabs at the top
pub fn render_tabs(current_view: View, area: Rect, buf: &mut Buffer) {
    let titles: Vec<Line> = View::all()
        .iter()
        .map(|v| {
            let style = if *v == current_view {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(v.name(), style))
        })
        .collect();

    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::BOTTOM))
        .select(current_view.index())
        .highlight_style(Style::default().fg(Color::Yellow));

    tabs.render(area, buf);
}

/// Compute aggregate health status indicator from `HealthStatus`.
fn aggregate_health_indicator(health: &HealthStatus) -> (&'static str, Style) {
    let has_error = !health.watcher_running
        || !health.db_accessible
        || matches!(health.wezterm_circuit.state, CircuitStateKind::Open);
    let has_warning = !health.wezterm_accessible
        || matches!(health.wezterm_circuit.state, CircuitStateKind::HalfOpen);

    if has_error {
        (
            "ERROR",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if has_warning {
        ("WARNING", Style::default().fg(Color::Yellow))
    } else {
        ("OK", Style::default().fg(Color::Green))
    }
}

/// Render the home/dashboard view
pub fn render_home_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title + aggregate health
            Constraint::Length(9), // Health status detail
            Constraint::Length(7), // Metrics snapshot
            Constraint::Min(3),    // Quick help
            Constraint::Length(3), // Footer
        ])
        .split(area);

    // Title + aggregate status
    let (aggregate_label, aggregate_style) = state.health.as_ref().map_or_else(
        || ("LOADING", Style::default().fg(Color::Yellow)),
        |h| aggregate_health_indicator(h),
    );
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "WezTerm Automata  ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(aggregate_label, aggregate_style),
    ]))
    .block(Block::default().borders(Borders::NONE));
    title.render(chunks[0], buf);

    // Health status detail
    let health_text = state.health.as_ref().map_or_else(
        || {
            vec![Line::from(Span::styled(
                "Loading...",
                Style::default().fg(Color::Yellow),
            ))]
        },
        |health| {
            let watcher_status = if health.watcher_running {
                Span::styled("RUNNING", Style::default().fg(Color::Green))
            } else {
                Span::styled("STOPPED", Style::default().fg(Color::Red))
            };
            let db_status = if health.db_accessible {
                Span::styled("OK", Style::default().fg(Color::Green))
            } else {
                Span::styled("NOT FOUND", Style::default().fg(Color::Red))
            };
            let wezterm_status = if health.wezterm_accessible {
                Span::styled("OK", Style::default().fg(Color::Green))
            } else {
                Span::styled("ERROR", Style::default().fg(Color::Red))
            };
            let circuit_status = match health.wezterm_circuit.state {
                CircuitStateKind::Closed => {
                    Span::styled("CLOSED", Style::default().fg(Color::Green))
                }
                CircuitStateKind::HalfOpen => {
                    Span::styled("HALF-OPEN", Style::default().fg(Color::Yellow))
                }
                CircuitStateKind::Open => {
                    let remaining = health.wezterm_circuit.cooldown_remaining_ms.unwrap_or(0);
                    Span::styled(
                        format!("OPEN ({remaining} ms cooldown)"),
                        Style::default().fg(Color::Red),
                    )
                }
            };

            let capture_lag = health.last_capture_ts.map_or_else(
                || Span::styled("no captures yet", Style::default().fg(Color::Gray)),
                |ts| {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .and_then(|d| i64::try_from(d.as_millis()).ok())
                        .unwrap_or(0);
                    let lag_ms = now_ms.saturating_sub(ts);
                    if lag_ms > 10_000 {
                        Span::styled(format!("{lag_ms} ms"), Style::default().fg(Color::Yellow))
                    } else {
                        Span::styled(format!("{lag_ms} ms"), Style::default().fg(Color::Green))
                    }
                },
            );

            vec![
                Line::from(vec![Span::raw("  Watcher:       "), watcher_status]),
                Line::from(vec![Span::raw("  Database:      "), db_status]),
                Line::from(vec![Span::raw("  WezTerm CLI:   "), wezterm_status]),
                Line::from(vec![Span::raw("  Circuit:       "), circuit_status]),
                Line::from(vec![Span::raw("  Capture lag:   "), capture_lag]),
                Line::from(vec![
                    Span::raw("  Failures:      "),
                    Span::raw(format!(
                        "{}/{}",
                        health.wezterm_circuit.consecutive_failures,
                        health.wezterm_circuit.failure_threshold
                    )),
                ]),
            ]
        },
    );

    let health_block = Paragraph::new(health_text).block(
        Block::default()
            .title("System Status")
            .borders(Borders::ALL),
    );
    health_block.render(chunks[1], buf);

    // Metrics snapshot
    let metrics_text = state.health.as_ref().map_or_else(
        || {
            vec![Line::from(Span::styled(
                "...",
                Style::default().fg(Color::Gray),
            ))]
        },
        |health| {
            let pane_count_style = if health.pane_count == 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Green)
            };
            let event_count_style = if health.event_count > 100 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Green)
            };
            let unhandled = state.events.iter().filter(|e| !e.handled).count();
            let unhandled_style = if unhandled > 0 {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };
            let triage_count = state.triage_items.len();
            let triage_style = if triage_count > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Green)
            };

            vec![
                Line::from(vec![
                    Span::raw("  Panes:         "),
                    Span::styled(health.pane_count.to_string(), pane_count_style),
                ]),
                Line::from(vec![
                    Span::raw("  Events:        "),
                    Span::styled(health.event_count.to_string(), event_count_style),
                ]),
                Line::from(vec![
                    Span::raw("  Unhandled:     "),
                    Span::styled(unhandled.to_string(), unhandled_style),
                ]),
                Line::from(vec![
                    Span::raw("  Triage items:  "),
                    Span::styled(triage_count.to_string(), triage_style),
                ]),
            ]
        },
    );
    let metrics_block =
        Paragraph::new(metrics_text).block(Block::default().title("Metrics").borders(Borders::ALL));
    metrics_block.render(chunks[2], buf);

    // Quick help
    let instructions = Paragraph::new(vec![
        Line::from(Span::styled(
            "Navigation:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / Shift+Tab: Switch views   q: Quit   r: Refresh   ?: Help"),
    ])
    .block(Block::default().title("Quick Help").borders(Borders::ALL));
    instructions.render(chunks[3], buf);

    // Footer with error if any
    if let Some(ref error) = state.error_message {
        let error_widget = Paragraph::new(Span::styled(
            error.as_str(),
            Style::default().fg(Color::Red),
        ))
        .block(Block::default().borders(Borders::TOP));
        error_widget.render(chunks[4], buf);
    }
}

/// Render the panes list view
pub fn render_panes_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(area);

    let mut bookmarks_by_pane: HashMap<u64, Vec<&PaneBookmarkView>> = HashMap::new();
    for bookmark in &state.pane_bookmarks {
        bookmarks_by_pane
            .entry(bookmark.pane_id)
            .or_default()
            .push(bookmark);
    }

    let filtered_indices = filtered_pane_indices(state);
    let selected_filtered_index = state
        .selected_index
        .min(filtered_indices.len().saturating_sub(1));
    let selected_pane = filtered_indices
        .get(selected_filtered_index)
        .and_then(|idx| state.panes.get(*idx));

    let list_block = Block::default()
        .title(format!(
            "Panes ({}/{})",
            filtered_indices.len(),
            state.panes.len()
        ))
        .borders(Borders::ALL);
    let list_inner = list_block.inner(chunks[0]);
    list_block.render(chunks[0], buf);

    let list_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(list_inner);

    let active_profile_name = state
        .ruleset_profile_state
        .as_ref()
        .map(|s| s.active_profile.as_str())
        .unwrap_or("default");
    let selected_profile_name = state
        .ruleset_profile_state
        .as_ref()
        .and_then(|s| {
            s.profiles
                .get(
                    state
                        .selected_ruleset_profile_index
                        .min(s.profiles.len().saturating_sub(1)),
                )
                .map(|p| p.name.as_str())
        })
        .unwrap_or("default");
    let profile_count = state
        .ruleset_profile_state
        .as_ref()
        .map_or(0, |s| s.profiles.len());

    let filter_summary = format!(
        "filter='{}' unhandled={} bookmarked={} agent={} domain={} profile={} active={} ({})",
        state.panes_filter_query,
        state.panes_unhandled_only,
        state.panes_bookmarked_only,
        state.panes_agent_filter.as_deref().unwrap_or("all"),
        state.panes_domain_filter.as_deref().unwrap_or("all"),
        selected_profile_name,
        active_profile_name,
        profile_count
    );
    Paragraph::new(vec![
        Line::from("id  bm      agent    state          unhandled  title"),
        Line::from(Span::styled(
            filter_summary,
            Style::default().fg(Color::Gray),
        )),
    ])
    .render(list_chunks[0], buf);

    if filtered_indices.is_empty() {
        Paragraph::new(Span::styled(
            "No panes match the current filters.",
            Style::default().fg(Color::Yellow),
        ))
        .render(list_chunks[1], buf);
    } else {
        let mut lines: Vec<Line> = Vec::with_capacity(filtered_indices.len());
        for (pos, pane_index) in filtered_indices.iter().enumerate() {
            let pane = &state.panes[*pane_index];
            let bookmark_summary = bookmarks_by_pane.get(&pane.pane_id).map_or_else(
                || "-".to_string(),
                |bookmarks| {
                    if bookmarks.len() == 1 {
                        truncate_str(&bookmarks[0].alias, 6)
                    } else {
                        format!("{}*", bookmarks.len())
                    }
                },
            );
            let style = if pos == selected_filtered_index {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else if pane.unhandled_event_count > 0 {
                Style::default().fg(Color::Yellow)
            } else if pane.pane_state == "AltScreen" {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default()
            };
            let agent = pane.agent_type.as_deref().unwrap_or("unknown");
            lines.push(Line::styled(
                format!(
                    "{:>3} {:6} {:8} {:12} {:>9}  {}",
                    pane.pane_id,
                    bookmark_summary,
                    truncate_str(agent, 8),
                    truncate_str(&pane.pane_state, 12),
                    pane.unhandled_event_count,
                    truncate_str(&pane.title, 24)
                ),
                style,
            ));
        }
        Paragraph::new(lines).render(list_chunks[1], buf);
    }

    let detail_block = Block::default().title("Pane Details").borders(Borders::ALL);
    let detail_inner = detail_block.inner(chunks[1]);
    detail_block.render(chunks[1], buf);

    if let Some(pane) = selected_pane {
        let pane_bookmarks = bookmarks_by_pane
            .get(&pane.pane_id)
            .cloned()
            .unwrap_or_default();
        let last_activity = pane
            .last_activity_ts
            .map_or_else(|| "unknown".to_string(), |ts| ts.to_string());
        let next_action = if selected_profile_name != active_profile_name {
            format!("Apply selected profile: wa rules profile apply {selected_profile_name}")
        } else if pane.unhandled_event_count > 0 {
            format!("Run: wa workflow list --pane {}", pane.pane_id)
        } else {
            format!("Inspect: wa get-text {} --tail 120", pane.pane_id)
        };
        let bookmark_summary = if pane_bookmarks.is_empty() {
            "none".to_string()
        } else {
            pane_bookmarks
                .iter()
                .map(|b| {
                    if b.tags.is_empty() {
                        b.alias.clone()
                    } else {
                        format!("{} [{}]", b.alias, b.tags.join(","))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let details = vec![
            Line::from(format!("Pane ID: {}", pane.pane_id)),
            Line::from(format!("Title: {}", pane.title)),
            Line::from(format!("Domain: {}", pane.domain)),
            Line::from(format!(
                "Agent: {}",
                pane.agent_type.as_deref().unwrap_or("unknown")
            )),
            Line::from(format!("State: {}", pane.pane_state)),
            Line::from(format!("CWD: {}", pane.cwd.as_deref().unwrap_or("unknown"))),
            Line::from(format!("Last Activity: {}", last_activity)),
            Line::from(format!("Unhandled Events: {}", pane.unhandled_event_count)),
            Line::from(format!(
                "Bookmarks: {}",
                truncate_str(&bookmark_summary, 80)
            )),
            Line::from(format!("Ruleset Active: {}", active_profile_name)),
            Line::from(format!("Ruleset Selected: {}", selected_profile_name)),
            Line::from(""),
            Line::from(Span::styled(
                "Next best action:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(next_action),
            Line::from(""),
            Line::from(Span::styled(
                "Keys: p=cycle profile, Enter=apply selected profile, b=bookmarked only",
                Style::default().fg(Color::Gray),
            )),
        ];
        Paragraph::new(details).render(detail_inner, buf);
    } else {
        Paragraph::new(Span::styled(
            "No pane selected.",
            Style::default().fg(Color::Yellow),
        ))
        .render(detail_inner, buf);
    }
}

/// Render the events feed view
pub fn render_events_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let filtered_indices = filtered_event_indices(state);
    let selected_filtered = state
        .events_selected_index
        .min(filtered_indices.len().saturating_sub(1));
    let selected_event = filtered_indices
        .get(selected_filtered)
        .and_then(|idx| state.events.get(*idx));

    // --- Left: event list ---
    let list_block = Block::default()
        .title(format!(
            "Events ({}/{})",
            filtered_indices.len(),
            state.events.len()
        ))
        .borders(Borders::ALL);
    let list_inner = list_block.inner(chunks[0]);
    list_block.render(chunks[0], buf);

    let list_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(list_inner);

    // Filter summary header
    let filter_summary = format!(
        "unhandled_only={}  pane/rule='{}'",
        state.events_unhandled_only, state.events_pane_filter,
    );
    Paragraph::new(vec![
        Line::from("sev       pane  rule                          status"),
        Line::from(Span::styled(
            filter_summary,
            Style::default().fg(Color::Gray),
        )),
    ])
    .render(list_chunks[0], buf);

    if filtered_indices.is_empty() {
        let msg = if state.events.is_empty() {
            "No events yet. Watcher will capture pattern matches here."
        } else {
            "No events match the current filters."
        };
        Paragraph::new(Span::styled(msg, Style::default().fg(Color::Yellow)))
            .render(list_chunks[1], buf);
    } else {
        let mut lines: Vec<Line> = Vec::with_capacity(filtered_indices.len());
        for (pos, event_index) in filtered_indices.iter().enumerate() {
            let event = &state.events[*event_index];
            let severity_style = severity_color(&event.severity);
            let handled_marker = if event.handled { " " } else { "*" };

            if pos == selected_filtered {
                lines.push(Line::styled(
                    format!(
                        "[{:8}] {:>4}  {:28} {}",
                        truncate_str(&event.severity, 8),
                        event.pane_id,
                        truncate_str(&event.rule_id, 28),
                        handled_marker,
                    ),
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("[{:8}]", truncate_str(&event.severity, 8)),
                        severity_style,
                    ),
                    Span::raw(format!(
                        " {:>4}  {:28} {}",
                        event.pane_id,
                        truncate_str(&event.rule_id, 28),
                        handled_marker,
                    )),
                ]));
            }
        }
        Paragraph::new(lines).render(list_chunks[1], buf);
    }

    // --- Right: event detail panel ---
    let detail_block = Block::default()
        .title("Event Details")
        .borders(Borders::ALL);
    let detail_inner = detail_block.inner(chunks[1]);
    detail_block.render(chunks[1], buf);

    if let Some(event) = selected_event {
        let severity_style = severity_color(&event.severity);
        let handled_label = if event.handled {
            "handled"
        } else {
            "UNHANDLED"
        };
        let handled_style = if event.handled {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        };
        let triage = event.triage_state.as_deref().unwrap_or("unset");
        let labels = if event.labels.is_empty() {
            "none".to_string()
        } else {
            event.labels.join(",")
        };
        let note = event.note.as_deref().unwrap_or("none");

        let mut details = vec![
            Line::from(vec![
                Span::raw("ID: "),
                Span::styled(
                    event.id.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(format!("Pane: {}", event.pane_id)),
            Line::from(vec![
                Span::raw("Severity: "),
                Span::styled(event.severity.clone(), severity_style),
            ]),
            Line::from(vec![
                Span::raw("Status: "),
                Span::styled(handled_label, handled_style),
            ]),
            Line::from(format!("Triage: {triage}")),
            Line::from(format!("Labels: {}", truncate_str(&labels, 60))),
            Line::from(format!("Note: {}", truncate_str(note, 60))),
            Line::from(""),
            Line::from(Span::styled(
                "Rule:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  {}", event.rule_id)),
            Line::from(""),
            Line::from(Span::styled(
                "Match (redacted):",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  {}", truncate_str(&event.message, 60))),
        ];

        // Timestamp
        details.push(Line::from(""));
        details.push(Line::from(format!("Captured: {}", event.timestamp)));

        // Suggested next actions
        details.push(Line::from(""));
        details.push(Line::from(Span::styled(
            "Actions:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if !event.handled {
            details.push(Line::from(format!(
                "  wa events --pane {} --unhandled",
                event.pane_id
            )));
        }
        details.push(Line::from(format!(
            "  wa why --recent --pane {}",
            event.pane_id
        )));

        Paragraph::new(details).render(detail_inner, buf);
    } else {
        Paragraph::new(Span::styled(
            "No event selected.",
            Style::default().fg(Color::Yellow),
        ))
        .render(detail_inner, buf);
    }
}

/// Map severity string to a color style.
fn severity_color(severity: &str) -> Style {
    match severity {
        "critical" | "error" => Style::default().fg(Color::Red),
        "warning" => Style::default().fg(Color::Yellow),
        "info" => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::Gray),
    }
}

fn history_group_key(entry: &HistoryEntryView) -> String {
    if let Some(workflow_id) = &entry.workflow_id {
        format!("workflow:{workflow_id}")
    } else if let Some(pane_id) = entry.pane_id {
        format!("pane:{pane_id}")
    } else {
        "global".to_string()
    }
}

fn history_group_title(group_key: &str) -> String {
    if let Some(workflow_id) = group_key.strip_prefix("workflow:") {
        format!("Workflow {workflow_id}")
    } else if let Some(pane_id) = group_key.strip_prefix("pane:") {
        format!("Pane {pane_id}")
    } else {
        "Global".to_string()
    }
}

fn history_result_style(result: &str) -> Style {
    match result {
        "success" | "completed" => Style::default().fg(Color::Green),
        "denied" | "failed" => Style::default().fg(Color::Red),
        "timeout" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Gray),
    }
}

/// Render the action-history view.
pub fn render_history_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let filtered_indices = filtered_history_indices(state);
    let selected_filtered = state
        .history_selected_index
        .min(filtered_indices.len().saturating_sub(1));
    let selected_entry = filtered_indices
        .get(selected_filtered)
        .and_then(|idx| state.history_entries.get(*idx));

    // --- Left: grouped history list ---
    let list_block = Block::default()
        .title(format!(
            "History ({}/{})",
            filtered_indices.len(),
            state.history_entries.len()
        ))
        .borders(Borders::ALL);
    let list_inner = list_block.inner(chunks[0]);
    list_block.render(chunks[0], buf);

    let list_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(list_inner);

    let filter_summary = format!(
        "undoable_only={}  q='{}'",
        state.history_undoable_only, state.history_filter_query,
    );
    Paragraph::new(vec![
        Line::from("audit     action             result    undo  actor"),
        Line::from(Span::styled(
            filter_summary,
            Style::default().fg(Color::Gray),
        )),
    ])
    .render(list_chunks[0], buf);

    if filtered_indices.is_empty() {
        let msg = if state.history_entries.is_empty() {
            "No action history yet. Run workflows or wa send to populate audit records."
        } else {
            "No history entries match the current filters."
        };
        Paragraph::new(Span::styled(msg, Style::default().fg(Color::Yellow)))
            .render(list_chunks[1], buf);
    } else {
        let mut lines: Vec<Line> = Vec::new();
        let mut current_group: Option<String> = None;

        for (pos, entry_idx) in filtered_indices.iter().enumerate() {
            let entry = &state.history_entries[*entry_idx];
            let group_key = history_group_key(entry);
            if current_group.as_deref() != Some(group_key.as_str()) {
                current_group = Some(group_key.clone());
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("-- {} --", history_group_title(&group_key)),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }

            let undo_tag = if entry.undoable {
                "UNDO"
            } else if entry.undone {
                "done"
            } else {
                "-"
            };
            let result_style = history_result_style(&entry.result);
            let action = truncate_str(&entry.action_kind, 18);
            let actor = truncate_str(&entry.actor_kind, 8);
            let line_text = format!(
                "#{:>6} {:18} {:8} {:>5} {}",
                entry.audit_id, action, entry.result, undo_tag, actor
            );

            if pos == selected_filtered {
                lines.push(Line::styled(
                    line_text,
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ));
            } else if entry.undoable {
                lines.push(Line::styled(
                    line_text,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(format!(
                        "#{:>6} {:18} ",
                        entry.audit_id,
                        truncate_str(&entry.action_kind, 18)
                    )),
                    Span::styled(format!("{:8}", entry.result), result_style),
                    Span::raw(format!(" {:>5} {}", undo_tag, actor)),
                ]));
            }
        }

        Paragraph::new(lines).render(list_chunks[1], buf);
    }

    // --- Right: selected history detail ---
    let detail_block = Block::default()
        .title("History Details")
        .borders(Borders::ALL);
    let detail_inner = detail_block.inner(chunks[1]);
    detail_block.render(chunks[1], buf);

    if let Some(entry) = selected_entry {
        let undo_status = if entry.undoable {
            "undoable"
        } else if entry.undone {
            "undone"
        } else {
            "not-undoable"
        };
        let group = history_group_title(&history_group_key(entry));

        let mut details = vec![
            Line::from(vec![
                Span::raw("Audit ID: "),
                Span::styled(
                    format!("#{}", entry.audit_id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(format!("Group: {}", group)),
            Line::from(format!("Action: {}", entry.action_kind)),
            Line::from(format!("Result: {}", entry.result)),
            Line::from(format!("Actor: {}", entry.actor_kind)),
            Line::from(format!("Undo: {}", undo_status)),
            Line::from(format!("Timestamp: {}", entry.timestamp)),
        ];

        if let Some(pane_id) = entry.pane_id {
            details.push(Line::from(format!("Pane: {}", pane_id)));
        }
        if let Some(workflow_id) = &entry.workflow_id {
            details.push(Line::from(format!("Workflow: {}", workflow_id)));
        }
        if let Some(step_name) = &entry.step_name {
            details.push(Line::from(format!("Step: {}", step_name)));
        }
        if let Some(strategy) = &entry.undo_strategy {
            details.push(Line::from(format!("Undo Strategy: {}", strategy)));
        }
        if let Some(hint) = &entry.undo_hint {
            details.push(Line::from(format!("Undo Hint: {}", truncate_str(hint, 80))));
        }
        if !entry.summary.is_empty() {
            details.push(Line::from(format!(
                "Summary: {}",
                truncate_str(&entry.summary, 80)
            )));
        }

        details.push(Line::from(""));
        details.push(Line::from(Span::styled(
            "Quick Jumps:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if let Some(workflow_id) = &entry.workflow_id {
            details.push(Line::from(format!("  wa history --workflow {workflow_id}")));
            details.push(Line::from(format!("  wa workflow status {workflow_id}")));
        }
        if let Some(pane_id) = entry.pane_id {
            details.push(Line::from(format!(
                "  wa history --pane {pane_id} --limit 50"
            )));
            details.push(Line::from(format!(
                "  wa events --pane-id {pane_id} --limit 20"
            )));
            if let Some(rule_id) = &entry.rule_id {
                details.push(Line::from(format!(
                    "  wa events --pane-id {pane_id} --rule-id {rule_id}"
                )));
            }
        }

        Paragraph::new(details).render(detail_inner, buf);
    } else {
        Paragraph::new(Span::styled(
            "No history entry selected.",
            Style::default().fg(Color::Yellow),
        ))
        .render(detail_inner, buf);
    }
}

/// Render the search view
pub fn render_search_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let show_suggestions = !state.search_suggestions.is_empty() && state.search_results.is_empty();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if show_suggestions {
            vec![
                Constraint::Length(3), // Search input
                Constraint::Length(5), // Suggestions
                Constraint::Length(8), // Saved searches
                Constraint::Min(5),    // Results + detail
            ]
        } else {
            vec![
                Constraint::Length(3), // Search input
                Constraint::Length(0), // No suggestions
                Constraint::Length(8), // Saved searches
                Constraint::Min(5),    // Results + detail
            ]
        })
        .split(area);

    // Search input
    let cursor_indicator = if state.search_query.is_empty() {
        "Search (FTS5) — type query, Enter to search"
    } else {
        "Search (FTS5) — Enter to search, Esc to clear"
    };
    let search_input = Paragraph::new(format!("{}_", state.search_query)).block(
        Block::default()
            .title(cursor_indicator)
            .borders(Borders::ALL),
    );
    search_input.render(chunks[0], buf);

    // Inline suggestions (shown while typing, before executing a search)
    if show_suggestions {
        let mut suggestion_lines: Vec<Line> = Vec::new();
        for s in state.search_suggestions.iter().take(4) {
            let desc = s.description.as_deref().unwrap_or("");
            suggestion_lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", s.text),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {desc}"), Style::default().fg(Color::Gray)),
            ]));
        }
        let suggestions_block = Block::default().title("Suggestions").borders(Borders::ALL);
        Paragraph::new(suggestion_lines)
            .block(suggestions_block)
            .render(chunks[1], buf);
    }

    // Saved searches list
    let saved_block = Block::default()
        .title(format!("Saved Searches ({})", state.saved_searches.len()))
        .borders(Borders::ALL);
    let saved_inner = saved_block.inner(chunks[2]);
    saved_block.render(chunks[2], buf);
    if state.saved_searches.is_empty() {
        Paragraph::new(Span::styled(
            "No saved searches yet. Use `wa search save <name> <query>`.",
            Style::default().fg(Color::Gray),
        ))
        .render(saved_inner, buf);
    } else {
        let selected_saved = state
            .saved_search_selected_index
            .min(state.saved_searches.len().saturating_sub(1));
        let mut saved_lines = vec![Line::from(
            "name           ena sched(ms) pane last_run      err query",
        )];
        for (idx, saved) in state.saved_searches.iter().enumerate() {
            let enabled = if saved.enabled { "on" } else { "off" };
            let schedule = saved
                .schedule_interval_ms
                .map_or_else(|| "-".to_string(), |v| v.to_string());
            let pane = saved
                .pane_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());
            let last_run = saved
                .last_run_at
                .map_or_else(|| "-".to_string(), |ts| ts.to_string());
            let err = if saved.last_error.is_some() {
                "yes"
            } else {
                "no"
            };
            let line = format!(
                "{:14} {:3} {:9} {:4} {:12} {:3} {}",
                truncate_str(&saved.name, 14),
                enabled,
                truncate_str(&schedule, 9),
                truncate_str(&pane, 4),
                truncate_str(&last_run, 12),
                err,
                truncate_str(&saved.query, 32),
            );
            if idx == selected_saved {
                saved_lines.push(Line::styled(
                    line,
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                saved_lines.push(Line::raw(line));
            }
        }
        Paragraph::new(saved_lines).render(saved_inner, buf);
    }

    if state.search_results.is_empty() {
        let msg = if state.search_last_query.is_empty() {
            "Type a query + Enter, or Ctrl+N/Ctrl+P to pick a saved search then Ctrl+R to run."
        } else {
            "No results found. Try a different query."
        };
        let results = Paragraph::new(Span::styled(msg, Style::default().fg(Color::Gray))).block(
            Block::default()
                .title(format!(
                    "Results ({})",
                    if state.search_last_query.is_empty() {
                        "waiting"
                    } else {
                        "0 matches"
                    }
                ))
                .borders(Borders::ALL),
        );
        results.render(chunks[3], buf);
        return;
    }

    // Split results area into list + detail
    let result_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(chunks[3]);

    let selected = state
        .search_selected_index
        .min(state.search_results.len().saturating_sub(1));

    // Results list
    let list_block = Block::default()
        .title(format!(
            "Results ({} matches for '{}')",
            state.search_results.len(),
            truncate_str(&state.search_last_query, 20),
        ))
        .borders(Borders::ALL);
    let list_inner = list_block.inner(result_chunks[0]);
    list_block.render(result_chunks[0], buf);

    let mut lines: Vec<Line> = Vec::with_capacity(state.search_results.len());
    for (pos, result) in state.search_results.iter().enumerate() {
        let snippet_preview = truncate_str(&result.snippet, 40);
        if pos == selected {
            lines.push(Line::styled(
                format!(
                    "P{:>3} | {:.2} | {}",
                    result.pane_id, result.rank, snippet_preview,
                ),
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("P{:>3}", result.pane_id),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(format!(" | {:.2} | {}", result.rank, snippet_preview)),
            ]));
        }
    }
    Paragraph::new(lines).render(list_inner, buf);

    // Detail panel for selected result
    let detail_block = Block::default()
        .title("Match Context")
        .borders(Borders::ALL);
    let detail_inner = detail_block.inner(result_chunks[1]);
    detail_block.render(result_chunks[1], buf);

    if let Some(result) = state.search_results.get(selected) {
        let details = vec![
            Line::from(vec![
                Span::styled("Pane: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(result.pane_id.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Rank: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{:.4}", result.rank)),
            ]),
            Line::from(vec![
                Span::styled("Captured: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(result.timestamp.to_string()),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Snippet (redacted):",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(result.snippet.clone()),
            Line::from(""),
            Line::from(Span::styled(
                "Saved search keys:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("Ctrl+N next, Ctrl+P prev, Ctrl+R run, Ctrl+E toggle enable"),
        ];
        Paragraph::new(details).render(detail_inner, buf);
    }
}

/// Render an ASCII progress bar: `[████░░░░] 2/5`
fn render_progress_bar(current: usize, total: usize, width: usize) -> Vec<Span<'static>> {
    let bar_width = width.saturating_sub(2); // account for [ ]
    let filled = current
        .checked_mul(bar_width)
        .and_then(|value| value.checked_div(total))
        .unwrap_or(0);
    let empty = bar_width.saturating_sub(filled);

    let filled_char = "\u{2588}"; // █
    let empty_char = "\u{2591}"; // ░

    let bar_style = if current >= total {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Cyan)
    };

    vec![
        Span::raw("["),
        Span::styled(filled_char.repeat(filled), bar_style),
        Span::styled(
            empty_char.repeat(empty),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format!("] {current}/{total}")),
    ]
}

/// Color style for a workflow status string.
fn workflow_status_style(status: &str) -> Style {
    match status {
        "running" => Style::default().fg(Color::Cyan),
        "waiting" => Style::default().fg(Color::Yellow),
        "failed" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "completed" => Style::default().fg(Color::Green),
        _ => Style::default().fg(Color::Gray),
    }
}

/// Render the triage view
pub fn render_triage_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let has_workflows = !state.workflows.is_empty();
    let constraints = if has_workflows {
        vec![
            Constraint::Percentage(50), // Triage list
            Constraint::Percentage(25), // Workflow progress
            Constraint::Length(6),      // Details + actions
        ]
    } else {
        vec![
            Constraint::Min(8),    // Triage list
            Constraint::Length(6), // Details + actions
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let block = Block::default()
        .title("Triage (prioritized)")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    block.render(chunks[0], buf);

    if state.triage_items.is_empty() && !has_workflows {
        let empty_msg = Paragraph::new(Span::styled(
            "All clear. No items need attention.",
            Style::default().fg(Color::Green),
        ));
        empty_msg.render(inner, buf);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in state.triage_items.iter().enumerate() {
        let severity_style = match item.severity.as_str() {
            "error" => Style::default().fg(Color::Red),
            "warning" => Style::default().fg(Color::Yellow),
            "info" => Style::default().fg(Color::Blue),
            _ => Style::default().fg(Color::Gray),
        };
        if i == state.triage_selected_index {
            let row = format!(
                "[{:7}] {} | {}",
                truncate_str(&item.severity, 7),
                truncate_str(&item.section, 8),
                truncate_str(&item.title, 80),
            );
            lines.push(Line::styled(
                row,
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("[{:7}]", truncate_str(&item.severity, 7)),
                    severity_style,
                ),
                Span::raw(format!(
                    " {} | {}",
                    truncate_str(&item.section, 8),
                    truncate_str(&item.title, 80),
                )),
            ]));
        }
    }

    let list = Paragraph::new(lines);
    list.render(inner, buf);

    // Workflow progress panel (if workflows exist)
    let detail_chunk_idx = if has_workflows {
        let wf_block = Block::default()
            .title(format!("Active Workflows ({})", state.workflows.len()))
            .borders(Borders::ALL);
        let wf_inner = wf_block.inner(chunks[1]);
        wf_block.render(chunks[1], buf);

        let mut wf_lines: Vec<Line> = Vec::new();
        for (i, wf) in state.workflows.iter().enumerate() {
            let status_style = workflow_status_style(&wf.status);
            let is_expanded = state.triage_expanded == Some(i);
            let expand_marker = if is_expanded { "▼" } else { "▶" };

            // Main workflow line with progress bar
            let mut spans: Vec<Span> = vec![
                Span::raw(format!("{expand_marker} ")),
                Span::styled(
                    truncate_str(&wf.workflow_name, 20),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" P{} ", wf.pane_id)),
                Span::styled(format!("{:8}", truncate_str(&wf.status, 8)), status_style),
                Span::raw(" "),
            ];
            spans.extend(render_progress_bar(wf.current_step, wf.total_steps, 12));
            wf_lines.push(Line::from(spans));

            // Expanded detail: step info + error
            if is_expanded {
                wf_lines.push(Line::from(vec![
                    Span::raw("    ID: "),
                    Span::styled(&*wf.id, Style::default().fg(Color::Gray)),
                ]));
                wf_lines.push(Line::from(format!(
                    "    Step {}/{} | started {} ms ago",
                    wf.current_step + 1,
                    wf.total_steps,
                    epoch_ms_ago(wf.started_at),
                )));
                if let Some(ref error) = wf.error {
                    wf_lines.push(Line::from(Span::styled(
                        format!("    ERROR: {}", truncate_str(error, 60)),
                        Style::default().fg(Color::Red),
                    )));
                }
                wf_lines.push(Line::from(""));
            }
        }
        Paragraph::new(wf_lines).render(wf_inner, buf);
        2
    } else {
        1
    };

    // Details + actions panel
    let detail_block = Block::default()
        .title("Details / Actions (Enter or 1-9 to run, m to mute, e to expand)")
        .borders(Borders::ALL);
    let detail_inner = detail_block.inner(chunks[detail_chunk_idx]);
    detail_block.render(chunks[detail_chunk_idx], buf);

    if let Some(item) = state.triage_items.get(state.triage_selected_index) {
        let mut detail_lines: Vec<Line> = Vec::new();
        if !item.detail.is_empty() {
            detail_lines.push(Line::from(Span::raw(truncate_str(&item.detail, 120))));
        }
        if !item.actions.is_empty() {
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(Span::styled(
                "Actions:",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for (idx, action) in item.actions.iter().enumerate() {
                detail_lines.push(Line::from(Span::raw(format!(
                    "  {}. {} ({})",
                    idx + 1,
                    action.label,
                    truncate_str(&action.command, 40)
                ))));
            }
        }
        let details = Paragraph::new(detail_lines);
        details.render(detail_inner, buf);
    }
}

/// Compute how many ms ago a timestamp was (for display).
fn epoch_ms_ago(ts: i64) -> i64 {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    now_ms.saturating_sub(ts)
}

/// Render the help view
pub fn render_help_view(area: Rect, buf: &mut Buffer) {
    let help_text = vec![
        Line::from(Span::styled(
            "WezTerm Automata TUI",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Global Keybindings:",
            Style::default().add_modifier(Modifier::UNDERLINED),
        )),
        Line::from("  q          Quit"),
        Line::from("  ?          Show this help"),
        Line::from("  r          Refresh current view"),
        Line::from("  Tab        Next view"),
        Line::from("  Shift+Tab  Previous view"),
        Line::from("  1-7        Jump to view by number"),
        Line::from(""),
        Line::from(Span::styled(
            "List Navigation:",
            Style::default().add_modifier(Modifier::UNDERLINED),
        )),
        Line::from("  j / Down   Move selection down"),
        Line::from("  k / Up     Move selection up"),
        Line::from("  Enter      Run primary action (triage)"),
        Line::from("  1-9        Run action by number (triage)"),
        Line::from("  m          Mute selected event (triage)"),
        Line::from("  [Panes] type text to filter, Backspace to edit, Esc to clear"),
        Line::from("  [Panes] u=unhandled-only, b=bookmarked-only, a=agent, d=domain"),
        Line::from("  [Panes] p=cycle ruleset profile, Enter=apply selected profile"),
        Line::from("  [Events] type digits to filter by pane/rule, u=unhandled-only"),
        Line::from("  [History] type text to filter, u=undoable-only"),
        Line::from("  [Search] Ctrl+N/Ctrl+P select saved, Ctrl+R run, Ctrl+E toggle"),
        Line::from("  [Triage] e=expand/collapse workflow progress"),
        Line::from(""),
        Line::from(Span::styled(
            "Views:",
            Style::default().add_modifier(Modifier::UNDERLINED),
        )),
        Line::from("  1. Home    System overview and health"),
        Line::from("  2. Panes   List all WezTerm panes"),
        Line::from("  3. Events  Recent detection events"),
        Line::from("  4. Triage  Prioritized issues + actions"),
        Line::from("  5. History Audit action timeline"),
        Line::from("  6. Search  Full-text search"),
        Line::from("  7. Help    This screen"),
        Line::from("  8. Timeline Cross-pane event timeline"),
    ];

    let help =
        Paragraph::new(help_text).block(Block::default().title("Help").borders(Borders::ALL));
    help.render(area, buf);
}

/// Render a placeholder for the Timeline view in the ratatui backend.
///
/// Full timeline rendering is implemented in the ftui backend; this stub
/// provides basic UI so the ratatui backend compiles and displays a message.
pub fn render_timeline_placeholder(area: Rect, buf: &mut Buffer) {
    let text = vec![
        Line::from(Span::styled(
            "Timeline",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Cross-pane event timeline with correlation markers."),
        Line::from("  Full rendering available in the ftui backend."),
        Line::from(""),
        Line::from("  Keys: j/k=nav  +/-=zoom  8=jump here"),
    ];
    let block =
        Paragraph::new(text).block(Block::default().title("Timeline").borders(Borders::ALL));
    block.render(area, buf);
}

/// Truncate a string to max length, adding ellipsis if needed
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len > 3 {
        format!("{}...", &s[..max_len - 3])
    } else {
        s[..max_len].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::CircuitBreakerStatus;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn view_navigation_wraps() {
        assert_eq!(View::Home.next(), View::Panes);
        assert_eq!(View::Help.next(), View::Timeline);
        assert_eq!(View::Timeline.next(), View::Home);
        assert_eq!(View::Home.prev(), View::Timeline);
        assert_eq!(View::Timeline.prev(), View::Help);
        assert_eq!(View::Panes.prev(), View::Home);
        assert_eq!(View::Triage.prev(), View::Events);
    }

    #[test]
    fn view_index_matches_order() {
        for (i, view) in View::all().iter().enumerate() {
            assert_eq!(view.index(), i);
        }
    }

    #[test]
    fn truncate_handles_edge_cases() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 8), "hello...");
        assert_eq!(truncate_str("ab", 2), "ab");
    }

    #[test]
    fn render_triage_view_handles_empty_and_populated_state() {
        let mut state = ViewState::default();
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);

        render_triage_view(&state, area, &mut buf);

        state.triage_items = vec![TriageItemView {
            section: "events".to_string(),
            severity: "warning".to_string(),
            title: "test".to_string(),
            detail: "detail".to_string(),
            actions: vec![super::super::query::TriageAction {
                label: "Explain".to_string(),
                command: "wa why --recent --pane 0".to_string(),
            }],
            event_id: Some(1),
            pane_id: Some(0),
            workflow_id: None,
        }];

        render_triage_view(&state, area, &mut buf);
    }

    fn pane(id: u64, title: &str, agent: Option<&str>, unhandled: u32, domain: &str) -> PaneView {
        PaneView {
            pane_id: id,
            title: title.to_string(),
            domain: domain.to_string(),
            cwd: Some(format!("/tmp/{title}")),
            is_excluded: false,
            agent_type: agent.map(str::to_string),
            pane_state: "PromptActive".to_string(),
            last_activity_ts: Some(1_700_000_000_000),
            unhandled_event_count: unhandled,
        }
    }

    #[test]
    fn filtered_pane_indices_applies_query_and_toggles() {
        let mut state = ViewState::default();
        state.panes = vec![
            pane(1, "codex-main", Some("codex"), 2, "local"),
            pane(2, "claude-docs", Some("claude"), 0, "ssh:prod"),
            pane(3, "shell", None, 1, "local"),
        ];

        state.panes_filter_query = "codex".to_string();
        let filtered = filtered_pane_indices(&state);
        assert_eq!(filtered, vec![0]);

        state.panes_filter_query.clear();
        state.panes_unhandled_only = true;
        let filtered = filtered_pane_indices(&state);
        assert_eq!(filtered, vec![0, 2]);

        state.panes_unhandled_only = false;
        state.panes_agent_filter = Some("claude".to_string());
        let filtered = filtered_pane_indices(&state);
        assert_eq!(filtered, vec![1]);

        state.panes_agent_filter = None;
        state.panes_domain_filter = Some("ssh".to_string());
        let filtered = filtered_pane_indices(&state);
        assert_eq!(filtered, vec![1]);
    }

    #[test]
    fn filtered_pane_indices_is_stable_for_large_lists() {
        let mut state = ViewState::default();
        state.panes = (0..1000)
            .map(|id| pane(id, &format!("pane-{id}"), Some("codex"), 0, "local"))
            .collect();
        state.panes_filter_query = "pane-9".to_string();

        let filtered = filtered_pane_indices(&state);
        assert!(!filtered.is_empty());
        assert!(filtered.windows(2).all(|w| w[0] < w[1]));
    }

    // -----------------------------------------------------------------------
    // Events view tests (wa-nu4.3.7.3)
    // -----------------------------------------------------------------------

    fn event(id: i64, pane_id: u64, rule: &str, severity: &str, handled: bool) -> EventView {
        EventView {
            id,
            rule_id: rule.to_string(),
            pane_id,
            severity: severity.to_string(),
            message: format!("matched text for {rule}"),
            timestamp: 1_700_000_000_000 + id,
            handled,
            triage_state: None,
            labels: Vec::new(),
            note: None,
        }
    }

    #[test]
    fn filtered_event_indices_returns_all_when_no_filters() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
            event(3, 10, "core.prompt_idle", "info", false),
        ];
        let filtered = filtered_event_indices(&state);
        assert_eq!(filtered, vec![0, 1, 2]);
    }

    #[test]
    fn filtered_event_indices_unhandled_only() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
            event(3, 10, "core.prompt_idle", "info", false),
        ];
        state.events_unhandled_only = true;
        let filtered = filtered_event_indices(&state);
        assert_eq!(filtered, vec![0, 2]);
    }

    #[test]
    fn filtered_event_indices_pane_filter() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
            event(3, 10, "core.prompt_idle", "info", false),
        ];
        state.events_pane_filter = "20".to_string();
        let filtered = filtered_event_indices(&state);
        assert_eq!(filtered, vec![1]);
    }

    #[test]
    fn filtered_event_indices_rule_filter() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
            event(3, 10, "core.prompt_idle", "info", false),
        ];
        state.events_pane_filter = "codex".to_string();
        let filtered = filtered_event_indices(&state);
        assert_eq!(filtered, vec![0]);
    }

    #[test]
    fn filtered_event_indices_combined_filters() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
            event(3, 10, "core.prompt_idle", "info", false),
        ];
        state.events_unhandled_only = true;
        state.events_pane_filter = "10".to_string();
        let filtered = filtered_event_indices(&state);
        assert_eq!(filtered, vec![0, 2]);
    }

    #[test]
    fn filtered_event_indices_empty_events() {
        let state = ViewState::default();
        let filtered = filtered_event_indices(&state);
        assert!(filtered.is_empty());
    }

    #[test]
    fn render_events_view_handles_empty_state() {
        let state = ViewState::default();
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_events_view(&state, area, &mut buf);
        // Should not panic with empty events
    }

    #[test]
    fn render_events_view_handles_populated_state() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
            event(3, 10, "core.prompt_idle", "info", false),
        ];
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_events_view(&state, area, &mut buf);
        // Should render without panic
    }

    #[test]
    fn render_events_view_with_selection() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
        ];
        state.events_selected_index = 1;
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_events_view(&state, area, &mut buf);
        // Should render detail panel for second event
    }

    #[test]
    fn render_events_view_with_filters_active() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
        ];
        state.events_unhandled_only = true;
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_events_view(&state, area, &mut buf);
        // Only unhandled events should appear
    }

    #[test]
    fn severity_color_maps_correctly() {
        let critical = severity_color("critical");
        assert_eq!(critical.fg, Some(Color::Red));
        let warning = severity_color("warning");
        assert_eq!(warning.fg, Some(Color::Yellow));
        let info = severity_color("info");
        assert_eq!(info.fg, Some(Color::Blue));
        let unknown = severity_color("other");
        assert_eq!(unknown.fg, Some(Color::Gray));
        let error = severity_color("error");
        assert_eq!(error.fg, Some(Color::Red));
    }

    #[test]
    fn events_selected_index_clamps_to_filtered() {
        let mut state = ViewState::default();
        state.events = vec![
            event(1, 10, "codex.usage_reached", "warning", false),
            event(2, 20, "claude.error", "critical", true),
        ];
        state.events_selected_index = 99; // Beyond range
        let filtered = filtered_event_indices(&state);
        let clamped = state
            .events_selected_index
            .min(filtered.len().saturating_sub(1));
        assert_eq!(clamped, 1); // Clamped to last index
    }

    // -----------------------------------------------------------------------
    // History view tests (wa-5em.3)
    // -----------------------------------------------------------------------

    fn history_entry(
        id: i64,
        pane_id: Option<u64>,
        workflow_id: Option<&str>,
        action_kind: &str,
        undoable: bool,
        undone: bool,
    ) -> HistoryEntryView {
        HistoryEntryView {
            audit_id: id,
            timestamp: 1_700_000_000_000 + id,
            pane_id,
            workflow_id: workflow_id.map(str::to_string),
            action_kind: action_kind.to_string(),
            result: "success".to_string(),
            actor_kind: "workflow".to_string(),
            step_name: Some("step".to_string()),
            undoable,
            undone,
            undo_strategy: Some("manual".to_string()),
            undo_hint: Some("run manual rollback".to_string()),
            rule_id: Some("codex.usage".to_string()),
            summary: "redacted summary".to_string(),
        }
    }

    #[test]
    fn filtered_history_indices_without_filters_returns_all() {
        let mut state = ViewState::default();
        state.history_entries = vec![
            history_entry(1, Some(10), None, "send_text", true, false),
            history_entry(2, Some(10), Some("wf-1"), "workflow_step", false, false),
            history_entry(3, Some(20), None, "workflow_completed", false, true),
        ];
        let filtered = filtered_history_indices(&state);
        assert_eq!(filtered, vec![0, 1, 2]);
    }

    #[test]
    fn filtered_history_indices_applies_query_and_undoable_filter() {
        let mut state = ViewState::default();
        state.history_entries = vec![
            history_entry(1, Some(10), None, "send_text", true, false),
            history_entry(2, Some(10), Some("wf-1"), "workflow_step", false, false),
            history_entry(3, Some(20), None, "workflow_completed", false, true),
        ];

        state.history_filter_query = "wf-1".to_string();
        let filtered = filtered_history_indices(&state);
        assert_eq!(filtered, vec![1]);

        state.history_filter_query.clear();
        state.history_undoable_only = true;
        let filtered = filtered_history_indices(&state);
        assert_eq!(filtered, vec![0]);
    }

    #[test]
    fn render_history_view_handles_empty_and_populated_state() {
        let mut state = ViewState::default();
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_history_view(&state, area, &mut buf);

        state.history_entries = vec![
            history_entry(1, Some(10), Some("wf-1"), "workflow_step", true, false),
            history_entry(2, Some(10), Some("wf-1"), "workflow_completed", false, true),
            history_entry(3, Some(20), None, "send_text", false, false),
        ];
        state.history_selected_index = 1;
        render_history_view(&state, area, &mut buf);
    }

    // -----------------------------------------------------------------------
    // Search view rendering tests (wa-nu4.3.7.4)
    // -----------------------------------------------------------------------

    fn search_result(pane_id: u64, snippet: &str, rank: f64) -> SearchResultView {
        SearchResultView {
            pane_id,
            timestamp: 1_700_000_000_000,
            snippet: snippet.to_string(),
            rank,
        }
    }

    #[test]
    fn render_search_view_empty_no_query() {
        let state = ViewState::default();
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Should not panic; shows "type a query" message
    }

    #[test]
    fn render_search_view_empty_with_prior_query() {
        let mut state = ViewState::default();
        state.search_last_query = "nonexistent".to_string();
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Shows "no results" message
    }

    #[test]
    fn render_search_view_with_results() {
        let mut state = ViewState::default();
        state.search_last_query = "test".to_string();
        state.search_results = vec![
            search_result(10, ">>matched<< text for test", 0.95),
            search_result(20, "another >>match<< here", 0.75),
        ];
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Should render results list + detail panel
    }

    #[test]
    fn render_search_view_with_selection() {
        let mut state = ViewState::default();
        state.search_last_query = "test".to_string();
        state.search_results = vec![
            search_result(10, "first result", 0.95),
            search_result(20, "second result", 0.75),
        ];
        state.search_selected_index = 1;
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Detail panel shows second result
    }

    #[test]
    fn render_search_view_query_with_cursor() {
        let mut state = ViewState::default();
        state.search_query = "hello".to_string();
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Should show "hello_" in the input area
    }

    #[test]
    fn render_search_view_shows_suggestions() {
        let mut state = ViewState::default();
        state.search_query = "err".to_string();
        state.search_suggestions = vec![crate::storage::SearchSuggestion {
            text: "error".to_string(),
            description: Some("Common errors".to_string()),
        }];
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Should not panic with suggestions rendered
    }

    #[test]
    fn render_search_view_hides_suggestions_with_results() {
        let mut state = ViewState::default();
        state.search_query = "err".to_string();
        state.search_suggestions = vec![crate::storage::SearchSuggestion {
            text: "error".to_string(),
            description: Some("Common errors".to_string()),
        }];
        // Add a result — suggestions should be hidden
        state.search_results = vec![crate::tui::query::SearchResultView {
            pane_id: 1,
            timestamp: 1_735_689_600_000,
            snippet: "some error text".to_string(),
            rank: 1.0,
        }];
        state.search_last_query = "err".to_string();
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Should not panic; suggestions hidden when results present
    }

    // -----------------------------------------------------------------------
    // Health metrics panel tests (wa-nu4.3.7.6)
    // -----------------------------------------------------------------------

    fn make_health(watcher: bool, db: bool, wezterm: bool) -> HealthStatus {
        HealthStatus {
            watcher_running: watcher,
            db_accessible: db,
            wezterm_accessible: wezterm,
            wezterm_circuit: CircuitBreakerStatus::default(),
            pane_count: 3,
            event_count: 10,
            last_capture_ts: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn aggregate_health_ok_when_all_healthy() {
        let health = make_health(true, true, true);
        let (label, _) = aggregate_health_indicator(&health);
        assert_eq!(label, "OK");
    }

    #[test]
    fn aggregate_health_error_when_watcher_stopped() {
        let health = make_health(false, true, true);
        let (label, _) = aggregate_health_indicator(&health);
        assert_eq!(label, "ERROR");
    }

    #[test]
    fn aggregate_health_error_when_db_inaccessible() {
        let health = make_health(true, false, true);
        let (label, _) = aggregate_health_indicator(&health);
        assert_eq!(label, "ERROR");
    }

    #[test]
    fn aggregate_health_warning_when_wezterm_inaccessible() {
        let health = make_health(true, true, false);
        let (label, _) = aggregate_health_indicator(&health);
        assert_eq!(label, "WARNING");
    }

    #[test]
    fn aggregate_health_error_when_circuit_open() {
        let mut health = make_health(true, true, true);
        health.wezterm_circuit.state = CircuitStateKind::Open;
        let (label, _) = aggregate_health_indicator(&health);
        assert_eq!(label, "ERROR");
    }

    #[test]
    fn aggregate_health_warning_when_circuit_half_open() {
        let mut health = make_health(true, true, true);
        health.wezterm_circuit.state = CircuitStateKind::HalfOpen;
        let (label, _) = aggregate_health_indicator(&health);
        assert_eq!(label, "WARNING");
    }

    #[test]
    fn render_home_view_healthy() {
        let mut state = ViewState::default();
        state.health = Some(make_health(true, true, true));
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
        // Should render without panic, show OK status
    }

    #[test]
    fn render_home_view_degraded() {
        let mut state = ViewState::default();
        let mut health = make_health(true, true, false);
        health.wezterm_circuit.state = CircuitStateKind::HalfOpen;
        state.health = Some(health);
        state.events = vec![event(1, 10, "codex.error", "critical", false)];
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
        // Should show WARNING aggregate with unhandled count
    }

    #[test]
    fn render_home_view_no_health() {
        let state = ViewState::default();
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
        // Should show "Loading..." gracefully
    }

    #[test]
    fn render_home_view_with_error_message() {
        let mut state = ViewState::default();
        state.health = Some(make_health(true, true, true));
        state.set_error("Connection lost");
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
        // Should render error footer
    }

    // -----------------------------------------------------------------------
    // Workflow progress panel tests (wa-nu4.3.7.5)
    // -----------------------------------------------------------------------

    fn workflow(
        id: &str,
        name: &str,
        pane: u64,
        step: usize,
        total: usize,
        status: &str,
    ) -> WorkflowProgressView {
        WorkflowProgressView {
            id: id.to_string(),
            workflow_name: name.to_string(),
            pane_id: pane,
            current_step: step,
            total_steps: total,
            status: status.to_string(),
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_001_000,
        }
    }

    #[test]
    fn progress_bar_renders_correctly() {
        let spans = render_progress_bar(2, 5, 12);
        // Should produce [, filled, empty, ] N/M
        assert_eq!(spans.len(), 4);
        // First span is "["
        assert_eq!(spans[0].content.as_ref(), "[");
        // Last span contains "] 2/5"
        assert!(spans[3].content.contains("2/5"));
    }

    #[test]
    fn progress_bar_full() {
        let spans = render_progress_bar(5, 5, 12);
        assert!(spans[3].content.contains("5/5"));
    }

    #[test]
    fn progress_bar_zero_total() {
        let spans = render_progress_bar(0, 0, 12);
        assert!(spans[3].content.contains("0/0"));
    }

    #[test]
    fn workflow_status_style_maps_correctly() {
        let running = workflow_status_style("running");
        assert_eq!(running.fg, Some(Color::Cyan));
        let waiting = workflow_status_style("waiting");
        assert_eq!(waiting.fg, Some(Color::Yellow));
        let failed = workflow_status_style("failed");
        assert_eq!(failed.fg, Some(Color::Red));
        let completed = workflow_status_style("completed");
        assert_eq!(completed.fg, Some(Color::Green));
        let unknown = workflow_status_style("other");
        assert_eq!(unknown.fg, Some(Color::Gray));
    }

    #[test]
    fn render_triage_view_with_workflows() {
        let mut state = ViewState::default();
        state.triage_items = vec![TriageItemView {
            section: "events".to_string(),
            severity: "warning".to_string(),
            title: "test event".to_string(),
            detail: "detail".to_string(),
            actions: vec![],
            event_id: Some(1),
            pane_id: Some(0),
            workflow_id: None,
        }];
        state.workflows = vec![
            workflow("wf-1", "notify_user", 10, 1, 3, "running"),
            workflow("wf-2", "restart_agent", 20, 0, 2, "waiting"),
        ];
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_triage_view(&state, area, &mut buf);
        // Should render without panic, showing workflow panel
    }

    #[test]
    fn render_triage_view_with_expanded_workflow() {
        let mut state = ViewState::default();
        state.workflows = vec![workflow("wf-1", "notify_user", 10, 2, 4, "running")];
        state.triage_expanded = Some(0);
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_triage_view(&state, area, &mut buf);
        // Should show expanded details for workflow
    }

    #[test]
    fn render_triage_view_with_failed_workflow() {
        let mut state = ViewState::default();
        let mut wf = workflow("wf-err", "deploy_check", 5, 1, 3, "failed");
        wf.error = Some("Connection refused to remote host".to_string());
        state.workflows = vec![wf];
        state.triage_expanded = Some(0);
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_triage_view(&state, area, &mut buf);
        // Should show error in red when expanded
    }

    #[test]
    fn render_triage_view_no_workflows() {
        let mut state = ViewState::default();
        state.triage_items = vec![TriageItemView {
            section: "events".to_string(),
            severity: "warning".to_string(),
            title: "test".to_string(),
            detail: "detail".to_string(),
            actions: vec![],
            event_id: Some(1),
            pane_id: Some(0),
            workflow_id: None,
        }];
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_triage_view(&state, area, &mut buf);
        // Should render without workflow panel (original layout)
    }

    #[test]
    fn render_triage_view_only_workflows_no_triage() {
        let mut state = ViewState::default();
        state.workflows = vec![workflow("wf-1", "notify_user", 10, 1, 3, "running")];
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_triage_view(&state, area, &mut buf);
        // Should not panic; shows empty triage + workflow panel
    }

    // -----------------------------------------------------------------------
    // Comprehensive TUI tests (wa-nu4.3.7.7)
    // -----------------------------------------------------------------------

    // --- View state transition tests ---

    #[test]
    fn view_state_default_is_clean() {
        let state = ViewState::default();
        assert!(state.panes.is_empty());
        assert!(state.events.is_empty());
        assert!(state.history_entries.is_empty());
        assert!(state.triage_items.is_empty());
        assert!(state.workflows.is_empty());
        assert!(state.health.is_none());
        assert!(state.search_query.is_empty());
        assert!(state.history_filter_query.is_empty());
        assert!(state.error_message.is_none());
        assert_eq!(state.selected_index, 0);
        assert_eq!(state.triage_selected_index, 0);
        assert_eq!(state.history_selected_index, 0);
        assert!(!state.panes_unhandled_only);
        assert!(!state.events_unhandled_only);
        assert!(!state.history_undoable_only);
        assert!(state.triage_expanded.is_none());
    }

    #[test]
    fn view_state_error_set_and_clear() {
        let mut state = ViewState::default();
        assert!(state.error_message.is_none());

        state.set_error("something broke");
        assert_eq!(state.error_message.as_deref(), Some("something broke"));

        state.clear_error();
        assert!(state.error_message.is_none());
    }

    #[test]
    fn view_all_returns_eight_views() {
        assert_eq!(View::all().len(), 8);
    }

    #[test]
    fn view_next_prev_are_inverse() {
        for view in View::all() {
            assert_eq!(view.next().prev(), *view);
            assert_eq!(view.prev().next(), *view);
        }
    }

    #[test]
    fn view_name_non_empty() {
        for view in View::all() {
            assert!(!view.name().is_empty());
        }
    }

    // --- Truncation edge cases ---

    #[test]
    fn truncate_handles_unicode_boundary() {
        // If truncation hits a multi-byte char boundary, it should not panic
        let result = truncate_str("héllo wörld", 7);
        assert!(!result.is_empty());
    }

    #[test]
    fn truncate_exact_max() {
        assert_eq!(truncate_str("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_one_over() {
        assert_eq!(truncate_str("abcdef", 5), "ab...");
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn truncate_max_three() {
        // When max_len == 3, should truncate without ellipsis
        assert_eq!(truncate_str("abcdef", 3), "abc");
    }

    // --- Pane rendering edge cases ---

    #[test]
    fn render_panes_view_empty_panes() {
        let state = ViewState::default();
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_panes_view(&state, area, &mut buf);
        // Should render "No panes match" gracefully
    }

    #[test]
    fn render_panes_view_with_selection_out_of_bounds() {
        let mut state = ViewState::default();
        state.panes = vec![pane(1, "test", Some("codex"), 0, "local")];
        state.selected_index = 99; // Way out of bounds
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_panes_view(&state, area, &mut buf);
        // Should clamp and render without panic
    }

    #[test]
    fn render_panes_view_alt_screen_pane() {
        let mut state = ViewState::default();
        let mut p = pane(1, "vim", None, 0, "local");
        p.pane_state = "AltScreen".to_string();
        state.panes = vec![p];
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_panes_view(&state, area, &mut buf);
    }

    #[test]
    fn render_panes_view_with_all_filters() {
        let mut state = ViewState::default();
        state.panes = vec![
            pane(1, "codex-main", Some("codex"), 2, "local"),
            pane(2, "claude-docs", Some("claude"), 0, "ssh:prod"),
        ];
        state.panes_filter_query = "codex".to_string();
        state.panes_unhandled_only = true;
        state.panes_agent_filter = Some("codex".to_string());
        state.panes_domain_filter = Some("local".to_string());
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_panes_view(&state, area, &mut buf);
    }

    // --- Events rendering edge cases ---

    #[test]
    fn render_events_view_selected_index_beyond_filtered() {
        let mut state = ViewState::default();
        state.events = vec![event(1, 10, "rule1", "warning", true)];
        state.events_unhandled_only = true; // Filters out the only event
        state.events_selected_index = 5;
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_events_view(&state, area, &mut buf);
        // Should render "No events match" without panic
    }

    // --- Search rendering edge cases ---

    #[test]
    fn render_search_view_selected_beyond_results() {
        let mut state = ViewState::default();
        state.search_last_query = "test".to_string();
        state.search_results = vec![search_result(10, "one result", 0.5)];
        state.search_selected_index = 99; // Way out of bounds
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_search_view(&state, area, &mut buf);
        // Should clamp and render without panic
    }

    // --- Tab rendering ---

    #[test]
    fn render_tabs_for_each_view() {
        let area = Rect::new(0, 0, 80, 2);
        for view in View::all() {
            let mut buf = Buffer::empty(area);
            render_tabs(*view, area, &mut buf);
            // Should not panic for any view
        }
    }

    // --- Help view ---

    #[test]
    fn render_help_view_does_not_panic() {
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_help_view(area, &mut buf);
    }

    // --- Triage edge cases ---

    #[test]
    fn render_triage_view_selected_beyond_items() {
        let mut state = ViewState::default();
        state.triage_items = vec![TriageItemView {
            section: "events".to_string(),
            severity: "error".to_string(),
            title: "test".to_string(),
            detail: "detail".to_string(),
            actions: vec![],
            event_id: None,
            pane_id: None,
            workflow_id: None,
        }];
        state.triage_selected_index = 99;
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_triage_view(&state, area, &mut buf);
        // Should not panic; detail panel may be empty
    }

    #[test]
    fn render_triage_view_with_multiple_actions() {
        let mut state = ViewState::default();
        state.triage_items = vec![TriageItemView {
            section: "events".to_string(),
            severity: "error".to_string(),
            title: "multi-action item".to_string(),
            detail: "multiple fixes available".to_string(),
            actions: vec![
                super::super::query::TriageAction {
                    label: "Action 1".to_string(),
                    command: "wa fix --auto".to_string(),
                },
                super::super::query::TriageAction {
                    label: "Action 2".to_string(),
                    command: "wa restart".to_string(),
                },
                super::super::query::TriageAction {
                    label: "Action 3".to_string(),
                    command: "wa why --recent".to_string(),
                },
            ],
            event_id: Some(42),
            pane_id: Some(10),
            workflow_id: None,
        }];
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        render_triage_view(&state, area, &mut buf);
    }

    // --- Home view edge cases ---

    #[test]
    fn render_home_view_zero_panes_and_events() {
        let mut state = ViewState::default();
        let health = make_health(true, true, true);
        state.health = Some(health);
        // pane_count=3, event_count=10 from make_health defaults; override
        state.health.as_mut().unwrap().pane_count = 0;
        state.health.as_mut().unwrap().event_count = 0;
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
    }

    #[test]
    fn render_home_view_high_event_count() {
        let mut state = ViewState::default();
        state.health = Some(make_health(true, true, true));
        state.health.as_mut().unwrap().event_count = 500;
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
        // Should show event count in yellow (>100)
    }

    #[test]
    fn render_home_view_no_capture_timestamp() {
        let mut state = ViewState::default();
        let mut health = make_health(true, true, true);
        health.last_capture_ts = None;
        state.health = Some(health);
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
        // Should show "no captures yet"
    }

    #[test]
    fn render_home_view_circuit_open_with_cooldown() {
        let mut state = ViewState::default();
        let mut health = make_health(true, true, true);
        health.wezterm_circuit.state = CircuitStateKind::Open;
        health.wezterm_circuit.cooldown_remaining_ms = Some(5000);
        state.health = Some(health);
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);
        render_home_view(&state, area, &mut buf);
    }

    // --- Small terminal size rendering ---

    #[test]
    fn render_all_views_at_minimum_size() {
        let area = Rect::new(0, 0, 40, 10);
        let state = ViewState::default();
        let mut buf = Buffer::empty(area);

        render_home_view(&state, area, &mut buf);
        render_panes_view(&state, area, &mut buf);
        render_events_view(&state, area, &mut buf);
        render_history_view(&state, area, &mut buf);
        render_triage_view(&state, area, &mut buf);
        render_search_view(&state, area, &mut buf);
        render_help_view(area, &mut buf);
        // None should panic at small terminal size
    }

    // --- Pane filter combinations ---

    #[test]
    fn filtered_pane_indices_empty_query_returns_all() {
        let mut state = ViewState::default();
        state.panes = vec![
            pane(1, "test", None, 0, "local"),
            pane(2, "test2", None, 0, "local"),
        ];
        let filtered = filtered_pane_indices(&state);
        assert_eq!(filtered, vec![0, 1]);
    }

    #[test]
    fn filtered_pane_indices_by_cwd() {
        let mut state = ViewState::default();
        state.panes = vec![
            pane(1, "test", None, 0, "local"),
            pane(2, "test2", None, 0, "local"),
        ];
        // cwd is "/tmp/{title}" - filter by test2
        state.panes_filter_query = "test2".to_string();
        let filtered = filtered_pane_indices(&state);
        assert_eq!(filtered, vec![1]);
    }

    #[test]
    fn filtered_pane_indices_domain_ssh() {
        let mut state = ViewState::default();
        state.panes = vec![
            pane(1, "local-shell", None, 0, "local"),
            pane(2, "remote", None, 0, "ssh:myhost"),
        ];
        state.panes_domain_filter = Some("ssh".to_string());
        let filtered = filtered_pane_indices(&state);
        assert_eq!(filtered, vec![1]);
    }

    // --- Progress bar edge cases ---

    #[test]
    fn progress_bar_single_step() {
        let spans = render_progress_bar(1, 1, 12);
        assert!(spans[3].content.contains("1/1"));
    }

    #[test]
    fn progress_bar_large_values() {
        let spans = render_progress_bar(50, 100, 22);
        assert!(spans[3].content.contains("50/100"));
    }

    #[test]
    fn progress_bar_minimum_width() {
        let spans = render_progress_bar(1, 2, 2);
        // Width 2 means bar_width = 0, should still produce valid output
        assert_eq!(spans.len(), 4);
    }
}
