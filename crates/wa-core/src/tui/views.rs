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

use super::query::{EventView, HealthStatus, PaneView, TriageItemView};
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
    /// Search interface
    Search,
    /// Help screen
    Help,
}

impl View {
    /// Get the display name for this view
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Panes => "Panes",
            Self::Events => "Events",
            Self::Triage => "Triage",
            Self::Search => "Search",
            Self::Help => "Help",
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
            Self::Search,
            Self::Help,
        ]
    }

    /// Get the index of this view in the tab order
    #[must_use]
    pub fn index(&self) -> usize {
        match self {
            Self::Home => 0,
            Self::Panes => 1,
            Self::Events => 2,
            Self::Triage => 3,
            Self::Search => 4,
            Self::Help => 5,
        }
    }

    /// Get the next view (wraps around)
    #[must_use]
    pub fn next(&self) -> Self {
        match self {
            Self::Home => Self::Panes,
            Self::Panes => Self::Events,
            Self::Events => Self::Triage,
            Self::Triage => Self::Search,
            Self::Search => Self::Help,
            Self::Help => Self::Home,
        }
    }

    /// Get the previous view (wraps around)
    #[must_use]
    pub fn prev(&self) -> Self {
        match self {
            Self::Home => Self::Help,
            Self::Panes => Self::Home,
            Self::Events => Self::Panes,
            Self::Triage => Self::Events,
            Self::Search => Self::Triage,
            Self::Help => Self::Search,
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

/// Render the home/dashboard view
pub fn render_home_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title
            Constraint::Length(7), // Health status
            Constraint::Min(5),    // Quick stats
            Constraint::Length(3), // Footer
        ])
        .split(area);

    // Title
    let title = Paragraph::new("WezTerm Automata")
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::NONE));
    title.render(chunks[0], buf);

    // Health status
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
                        format!("OPEN ({} ms)", remaining),
                        Style::default().fg(Color::Red),
                    )
                }
            };

            vec![
                Line::from(vec![Span::raw("Watcher: "), watcher_status]),
                Line::from(vec![Span::raw("Database: "), db_status]),
                Line::from(vec![Span::raw("WezTerm: "), wezterm_status]),
                Line::from(vec![Span::raw("Circuit: "), circuit_status]),
                Line::from(Span::raw(format!("Panes: {}", health.pane_count))),
            ]
        },
    );

    let health_block = Paragraph::new(health_text).block(
        Block::default()
            .title("System Status")
            .borders(Borders::ALL),
    );
    health_block.render(chunks[1], buf);

    // Instructions
    let instructions = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Navigation:",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab / Shift+Tab: Switch views"),
        Line::from("  q: Quit"),
        Line::from("  r: Refresh data"),
        Line::from("  ?: Help"),
    ])
    .block(Block::default().title("Quick Help").borders(Borders::ALL));
    instructions.render(chunks[2], buf);

    // Footer with error if any
    if let Some(ref error) = state.error_message {
        let error_widget = Paragraph::new(Span::styled(
            error.as_str(),
            Style::default().fg(Color::Red),
        ))
        .block(Block::default().borders(Borders::TOP));
        error_widget.render(chunks[3], buf);
    }
}

/// Render the panes list view
pub fn render_panes_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(area);

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

    let filter_summary = format!(
        "filter='{}'  unhandled_only={}  agent={}  domain={}",
        state.panes_filter_query,
        state.panes_unhandled_only,
        state.panes_agent_filter.as_deref().unwrap_or("all"),
        state.panes_domain_filter.as_deref().unwrap_or("all")
    );
    Paragraph::new(vec![
        Line::from("id  agent    state          unhandled  title"),
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
                    "{:>3} {:8} {:12} {:>9}  {}",
                    pane.pane_id,
                    truncate_str(agent, 8),
                    truncate_str(&pane.pane_state, 12),
                    pane.unhandled_event_count,
                    truncate_str(&pane.title, 30)
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
        let last_activity = pane
            .last_activity_ts
            .map_or_else(|| "unknown".to_string(), |ts| ts.to_string());
        let next_action = if pane.unhandled_event_count > 0 {
            format!("Run: wa workflow list --pane {}", pane.pane_id)
        } else {
            format!("Inspect: wa get-text {} --tail 120", pane.pane_id)
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
            Line::from(""),
            Line::from(Span::styled(
                "Next best action:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(next_action),
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
        let handled_label = if event.handled { "handled" } else { "UNHANDLED" };
        let handled_style = if event.handled {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD)
        };

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

/// Render the search view
pub fn render_search_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Search input
            Constraint::Min(5),    // Results
        ])
        .split(area);

    // Search input
    let search_input = Paragraph::new(state.search_query.as_str()).block(
        Block::default()
            .title("Search (FTS5)")
            .borders(Borders::ALL),
    );
    search_input.render(chunks[0], buf);

    // Placeholder for results
    let results = Paragraph::new(Span::styled(
        "Type a query and press Enter to search captured output.",
        Style::default().fg(Color::Gray),
    ))
    .block(Block::default().title("Results").borders(Borders::ALL));
    results.render(chunks[1], buf);
}

/// Render the triage view
pub fn render_triage_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // Triage list
            Constraint::Length(6), // Details + actions
        ])
        .split(area);

    let block = Block::default()
        .title("Triage (prioritized)")
        .borders(Borders::ALL);
    let inner = block.inner(chunks[0]);
    block.render(chunks[0], buf);

    if state.triage_items.is_empty() {
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

    // Details + actions panel
    let detail_block = Block::default()
        .title("Details / Actions (Enter or 1-9 to run, m to mute)")
        .borders(Borders::ALL);
    let detail_inner = detail_block.inner(chunks[1]);
    detail_block.render(chunks[1], buf);

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
        Line::from("  1-5        Jump to view by number"),
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
        Line::from("  [Panes] u=unhandled-only, a=agent filter, d=domain filter"),
        Line::from("  [Events] type digits to filter by pane/rule, u=unhandled-only"),
        Line::from(""),
        Line::from(Span::styled(
            "Views:",
            Style::default().add_modifier(Modifier::UNDERLINED),
        )),
        Line::from("  1. Home    System overview and health"),
        Line::from("  2. Panes   List all WezTerm panes"),
        Line::from("  3. Events  Recent detection events"),
        Line::from("  4. Triage  Prioritized issues + actions"),
        Line::from("  5. Search  Full-text search"),
        Line::from("  6. Help    This screen"),
    ];

    let help =
        Paragraph::new(help_text).block(Block::default().title("Help").borders(Borders::ALL));
    help.render(area, buf);
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
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn view_navigation_wraps() {
        assert_eq!(View::Home.next(), View::Panes);
        assert_eq!(View::Help.next(), View::Home);
        assert_eq!(View::Home.prev(), View::Help);
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
        let clamped = state.events_selected_index.min(filtered.len().saturating_sub(1));
        assert_eq!(clamped, 1); // Clamped to last index
    }
}
