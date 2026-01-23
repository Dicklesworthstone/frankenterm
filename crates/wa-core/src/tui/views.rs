//! TUI views and screen definitions
//!
//! Each view represents a distinct screen in the TUI with its own
//! state, keybindings, and rendering logic.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget, Tabs},
};

use super::query::{HealthStatus, PaneView, EventView};

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
    /// Search interface
    Search,
    /// Help screen
    Help,
}

impl View {
    /// Get the display name for this view
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Panes => "Panes",
            Self::Events => "Events",
            Self::Search => "Search",
            Self::Help => "Help",
        }
    }

    /// Get all views in tab order
    pub const fn all() -> &'static [View] {
        &[View::Home, View::Panes, View::Events, View::Search, View::Help]
    }

    /// Get the index of this view in the tab order
    pub fn index(&self) -> usize {
        match self {
            Self::Home => 0,
            Self::Panes => 1,
            Self::Events => 2,
            Self::Search => 3,
            Self::Help => 4,
        }
    }

    /// Get the next view (wraps around)
    pub fn next(&self) -> Self {
        match self {
            Self::Home => Self::Panes,
            Self::Panes => Self::Events,
            Self::Events => Self::Search,
            Self::Search => Self::Help,
            Self::Help => Self::Home,
        }
    }

    /// Get the previous view (wraps around)
    pub fn prev(&self) -> Self {
        match self {
            Self::Home => Self::Help,
            Self::Panes => Self::Home,
            Self::Events => Self::Panes,
            Self::Search => Self::Events,
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
    /// Current health status
    pub health: Option<HealthStatus>,
    /// Search query input
    pub search_query: String,
    /// Error message to display (if any)
    pub error_message: Option<String>,
    /// Selected index in list views
    pub selected_index: usize,
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
            Constraint::Length(3),  // Title
            Constraint::Length(6),  // Health status
            Constraint::Min(5),     // Quick stats
            Constraint::Length(3),  // Footer
        ])
        .split(area);

    // Title
    let title = Paragraph::new("WezTerm Automata")
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::NONE));
    title.render(chunks[0], buf);

    // Health status
    let health_text = if let Some(ref health) = state.health {
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

        vec![
            Line::from(vec![Span::raw("Watcher: "), watcher_status]),
            Line::from(vec![Span::raw("Database: "), db_status]),
            Line::from(vec![Span::raw("WezTerm: "), wezterm_status]),
            Line::from(Span::raw(format!("Panes: {}", health.pane_count))),
        ]
    } else {
        vec![Line::from(Span::styled(
            "Loading...",
            Style::default().fg(Color::Yellow),
        ))]
    };

    let health_block = Paragraph::new(health_text)
        .block(Block::default().title("System Status").borders(Borders::ALL));
    health_block.render(chunks[1], buf);

    // Instructions
    let instructions = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled("Navigation:", Style::default().add_modifier(Modifier::BOLD))),
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
    let block = Block::default().title("Panes").borders(Borders::ALL);
    let inner = block.inner(area);
    block.render(area, buf);

    if state.panes.is_empty() {
        let empty_msg = Paragraph::new(Span::styled(
            "No panes found. Is WezTerm running?",
            Style::default().fg(Color::Yellow),
        ));
        empty_msg.render(inner, buf);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (i, pane) in state.panes.iter().enumerate() {
        let style = if i == state.selected_index {
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let excluded_marker = if pane.is_excluded { " [excluded]" } else { "" };
        let agent_info = pane.agent_type.as_deref().unwrap_or("unknown");

        lines.push(Line::styled(
            format!(
                "{:>3} | {:20} | {:10} | {}{}",
                pane.pane_id,
                truncate_str(&pane.title, 20),
                agent_info,
                pane.domain,
                excluded_marker
            ),
            style,
        ));
    }

    let list = Paragraph::new(lines);
    list.render(inner, buf);
}

/// Render the events feed view
pub fn render_events_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let block = Block::default().title("Events").borders(Borders::ALL);
    let inner = block.inner(area);
    block.render(area, buf);

    if state.events.is_empty() {
        let empty_msg = Paragraph::new(Span::styled(
            "No events yet. Watcher will capture pattern matches here.",
            Style::default().fg(Color::Yellow),
        ));
        empty_msg.render(inner, buf);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for event in &state.events {
        let severity_style = match event.severity.as_str() {
            "critical" | "error" => Style::default().fg(Color::Red),
            "warning" => Style::default().fg(Color::Yellow),
            "info" => Style::default().fg(Color::Blue),
            _ => Style::default().fg(Color::Gray),
        };

        let handled_marker = if event.handled { "" } else { "*" };

        lines.push(Line::from(vec![
            Span::styled(
                format!("[{:8}]", truncate_str(&event.severity, 8)),
                severity_style,
            ),
            Span::raw(format!(" Pane {} | {} {}", event.pane_id, event.rule_id, handled_marker)),
        ]));
    }

    let list = Paragraph::new(lines);
    list.render(inner, buf);
}

/// Render the search view
pub fn render_search_view(state: &ViewState, area: Rect, buf: &mut Buffer) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Search input
            Constraint::Min(5),     // Results
        ])
        .split(area);

    // Search input
    let search_input = Paragraph::new(state.search_query.as_str())
        .block(Block::default().title("Search (FTS5)").borders(Borders::ALL));
    search_input.render(chunks[0], buf);

    // Placeholder for results
    let results = Paragraph::new(Span::styled(
        "Type a query and press Enter to search captured output.",
        Style::default().fg(Color::Gray),
    ))
    .block(Block::default().title("Results").borders(Borders::ALL));
    results.render(chunks[1], buf);
}

/// Render the help view
pub fn render_help_view(area: Rect, buf: &mut Buffer) {
    let help_text = vec![
        Line::from(Span::styled("WezTerm Automata TUI", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled("Global Keybindings:", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from("  q          Quit"),
        Line::from("  ?          Show this help"),
        Line::from("  r          Refresh current view"),
        Line::from("  Tab        Next view"),
        Line::from("  Shift+Tab  Previous view"),
        Line::from("  1-5        Jump to view by number"),
        Line::from(""),
        Line::from(Span::styled("List Navigation:", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from("  j / Down   Move selection down"),
        Line::from("  k / Up     Move selection up"),
        Line::from("  Enter      Select/inspect item"),
        Line::from(""),
        Line::from(Span::styled("Views:", Style::default().add_modifier(Modifier::UNDERLINED))),
        Line::from("  1. Home    System overview and health"),
        Line::from("  2. Panes   List all WezTerm panes"),
        Line::from("  3. Events  Recent detection events"),
        Line::from("  4. Search  Full-text search"),
        Line::from("  5. Help    This screen"),
    ];

    let help = Paragraph::new(help_text)
        .block(Block::default().title("Help").borders(Borders::ALL));
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

    #[test]
    fn view_navigation_wraps() {
        assert_eq!(View::Home.next(), View::Panes);
        assert_eq!(View::Help.next(), View::Home);
        assert_eq!(View::Home.prev(), View::Help);
        assert_eq!(View::Panes.prev(), View::Home);
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
}
