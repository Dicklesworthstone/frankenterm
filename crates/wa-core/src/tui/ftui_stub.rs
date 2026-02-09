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
/// Full state management will be implemented in FTUI-04.3 (deterministic UI
/// state reducer).  For now this holds the minimum needed by the app shell.
#[derive(Debug, Default)]
pub struct ViewState {
    pub current_view: View,
    pub error_message: Option<String>,
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
    _query: Arc<dyn QueryClient + Send + Sync>,
}

impl WaModel {
    fn new(query: Arc<dyn QueryClient + Send + Sync>, config: AppConfig) -> Self {
        Self {
            view_state: ViewState::default(),
            config,
            last_refresh: Instant::now(),
            _query: query,
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

        match key.code {
            KeyCode::Char('q') => Some(ftui::Cmd::Quit),
            KeyCode::Tab => {
                self.view_state.current_view = self.view_state.current_view.next();
                Some(ftui::Cmd::None)
            }
            KeyCode::BackTab => {
                self.view_state.current_view = self.view_state.current_view.prev();
                Some(ftui::Cmd::None)
            }
            KeyCode::Char('?') => {
                self.view_state.current_view = View::Help;
                Some(ftui::Cmd::None)
            }
            KeyCode::Char(ch @ '1'..='7') => {
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
        // Schedule the first data refresh tick.
        ftui::Cmd::Tick(self.config.refresh_interval)
    }

    fn update(&mut self, msg: WaMsg) -> ftui::Cmd<WaMsg> {
        match msg {
            WaMsg::TermEvent(ftui::Event::Key(ref key)) => {
                if let Some(cmd) = self.handle_global_key(key) {
                    return cmd;
                }
                // TODO(FTUI-05.2..05.7): forward to active view handler
                ftui::Cmd::None
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
                // TODO(FTUI-05.2): issue Cmd::Task to refresh data from QueryClient
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
        render_view_placeholder(
            frame,
            content_y,
            width,
            content_h,
            self.view_state.current_view,
        );

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
}
