//! The fleet viewer: a FrankenTUI model of panes and detections.
//!
//! The host (browser JavaScript) owns all network I/O. It fetches `/panes` and
//! `/events`, follows `/stream/events`, and hands the bodies to the model;
//! the model only decodes, keeps bounded state and renders. It is read-only:
//! no key sends anything to a pane.

use std::collections::VecDeque;

use ftui_core::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ftui_render::cell::{Cell, CellAttrs, PackedRgba, StyleFlags};
use ftui_render::frame::Frame;
use ftui_runtime::program::{Cmd, Model};

use crate::wire::{self, Detection, Pane, StreamUpdate};

/// Detections kept for display; older ones fall off the end.
pub const MAX_EVENTS: usize = 500;
/// Panes kept; a larger fleet is shown truncated with a visible count.
pub const MAX_PANES: usize = 4096;
/// Data older than this is labelled stale.
pub const STALE_AFTER_MS: i64 = 30_000;

/// Which list the viewer shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Panes,
    Events,
}

/// State of the live `/stream/events` subscription, as the host reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Not yet connected, or the host closed it.
    Disconnected,
    /// Connecting or reconnecting.
    Connecting,
    /// The server sent `ready`; frames are flowing.
    Live,
}

/// Messages: terminal-style input from the renderer.
#[derive(Debug)]
pub enum FleetMsg {
    Input(Event),
}

impl From<Event> for FleetMsg {
    fn from(event: Event) -> Self {
        Self::Input(event)
    }
}

/// The browser fleet viewer.
#[derive(Debug)]
pub struct FleetModel {
    view: View,
    panes: Vec<Pane>,
    panes_truncated: usize,
    panes_loaded_at_ms: Option<i64>,
    events: VecDeque<Detection>,
    events_loaded_at_ms: Option<i64>,
    stream: StreamState,
    last_frame_at_ms: Option<i64>,
    lag_frames: u64,
    gap_panes: Vec<u64>,
    selected: usize,
    now_ms: i64,
    last_error: Option<String>,
    refresh_requested: bool,
}

impl Default for FleetModel {
    fn default() -> Self {
        Self::new()
    }
}

impl FleetModel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            view: View::Panes,
            panes: Vec::new(),
            panes_truncated: 0,
            panes_loaded_at_ms: None,
            events: VecDeque::new(),
            events_loaded_at_ms: None,
            stream: StreamState::Disconnected,
            last_frame_at_ms: None,
            lag_frames: 0,
            gap_panes: Vec::new(),
            selected: 0,
            now_ms: 0,
            last_error: None,
            refresh_requested: false,
        }
    }

    /// Wall-clock time from the host, used only for freshness labels.
    pub fn set_now_ms(&mut self, now_ms: i64) {
        self.now_ms = now_ms;
    }

    /// Replace the pane inventory with a `/panes` body.
    pub fn ingest_panes(&mut self, body: &str) -> Result<usize, wire::WireError> {
        match wire::decode_panes(body) {
            Ok(mut panes) => {
                panes.sort_by_key(|p| p.pane_id);
                self.panes_truncated = panes.len().saturating_sub(MAX_PANES);
                panes.truncate(MAX_PANES);
                let count = panes.len();
                self.panes = panes;
                self.panes_loaded_at_ms = Some(self.now_ms);
                self.last_error = None;
                self.clamp_selection();
                Ok(count)
            }
            Err(err) => {
                self.last_error = Some(format!("panes: {err}"));
                Err(err)
            }
        }
    }

    /// Replace the detection history with an `/events` body (newest first).
    pub fn ingest_events(&mut self, body: &str) -> Result<usize, wire::WireError> {
        match wire::decode_events(body) {
            Ok(mut events) => {
                events.sort_by_key(|e| std::cmp::Reverse(e.detected_at_ms));
                events.truncate(MAX_EVENTS);
                let count = events.len();
                self.events = events.into();
                self.events_loaded_at_ms = Some(self.now_ms);
                self.last_error = None;
                self.clamp_selection();
                Ok(count)
            }
            Err(err) => {
                self.last_error = Some(format!("events: {err}"));
                Err(err)
            }
        }
    }

    /// Apply one `/stream/events` frame payload.
    pub fn ingest_stream_frame(&mut self, payload: &str) -> Result<(), wire::WireError> {
        let update = match wire::decode_stream_frame(payload) {
            Ok(update) => update,
            Err(err) => {
                self.last_error = Some(format!("stream: {err}"));
                return Err(err);
            }
        };
        self.last_frame_at_ms = Some(self.now_ms);
        match update {
            StreamUpdate::Ready => self.stream = StreamState::Live,
            StreamUpdate::Detection(detection) => self.push_detection(detection),
            StreamUpdate::PaneDiscovered {
                pane_id,
                domain,
                title,
            } => {
                if let Some(pane) = self.panes.iter_mut().find(|p| p.pane_id == pane_id) {
                    pane.title = Some(title);
                    pane.domain = domain;
                    pane.last_seen_at = self.now_ms;
                } else if self.panes.len() < MAX_PANES {
                    self.panes.push(Pane {
                        pane_id,
                        domain,
                        title: Some(title),
                        cwd: None,
                        last_seen_at: self.now_ms,
                    });
                    self.panes.sort_by_key(|p| p.pane_id);
                } else {
                    self.panes_truncated += 1;
                }
            }
            StreamUpdate::PaneDisappeared { pane_id } => {
                self.panes.retain(|p| p.pane_id != pane_id);
                self.clamp_selection();
            }
            StreamUpdate::Gap { pane_id } => {
                if !self.gap_panes.contains(&pane_id) {
                    self.gap_panes.push(pane_id);
                }
            }
            StreamUpdate::Lag => self.lag_frames = self.lag_frames.saturating_add(1),
            StreamUpdate::Other => {}
        }
        Ok(())
    }

    /// Host-side stream lifecycle (open, error, reconnect).
    pub fn set_stream_state(&mut self, state: StreamState) {
        self.stream = state;
    }

    /// The host shows a fetch failure (network, HTTP status) it could not decode.
    pub fn report_error(&mut self, message: &str) {
        self.last_error = Some(message.chars().take(200).collect());
    }

    /// True once after the operator pressed `r`; the host then refetches.
    pub fn take_refresh_request(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    #[must_use]
    pub fn active_view(&self) -> View {
        self.view
    }

    #[must_use]
    pub fn panes(&self) -> &[Pane] {
        &self.panes
    }

    #[must_use]
    pub fn events(&self) -> &VecDeque<Detection> {
        &self.events
    }

    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    fn push_detection(&mut self, detection: Detection) {
        // A reconnect replays persisted events the history already holds.
        if detection.id.is_some() && self.events.iter().any(|e| e.id == detection.id) {
            return;
        }
        self.events.push_front(detection);
        self.events.truncate(MAX_EVENTS);
    }

    fn rows_in_view(&self) -> usize {
        match self.view {
            View::Panes => self.panes.len(),
            View::Events => self.events.len(),
        }
    }

    fn clamp_selection(&mut self) {
        let rows = self.rows_in_view();
        if self.selected >= rows {
            self.selected = rows.saturating_sub(1);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let rows = self.rows_in_view();
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.view = match self.view {
                    View::Panes => View::Events,
                    View::Events => View::Panes,
                };
                self.selected = 0;
            }
            KeyCode::Char('1') => {
                self.view = View::Panes;
                self.selected = 0;
            }
            KeyCode::Char('2') => {
                self.view = View::Events;
                self.selected = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < rows {
                    self.selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = rows.saturating_sub(1),
            KeyCode::Char('r') => self.refresh_requested = true,
            _ => {}
        }
    }

    fn age_label(&self, at_ms: Option<i64>) -> String {
        match at_ms {
            None => "never".to_string(),
            Some(at) => {
                let age = (self.now_ms - at).max(0) / 1000;
                if age < 60 {
                    format!("{age}s ago")
                } else if age < 3600 {
                    format!("{}m ago", age / 60)
                } else {
                    format!("{}h ago", age / 3600)
                }
            }
        }
    }

    fn is_stale(&self, at_ms: Option<i64>) -> bool {
        at_ms.is_none_or(|at| self.now_ms - at > STALE_AFTER_MS)
    }

    fn status_text(&self) -> (String, PackedRgba) {
        let (label, color) = match self.stream {
            StreamState::Live => ("live", GREEN),
            StreamState::Connecting => ("connecting", YELLOW),
            StreamState::Disconnected => ("offline", RED),
        };
        let mut text = format!(
            "{label} · {} panes · {} events · data {}",
            self.panes.len(),
            self.events.len(),
            self.age_label(self.panes_loaded_at_ms.max(self.last_frame_at_ms))
        );
        if self.lag_frames > 0 {
            text.push_str(&format!(" · {} lag", self.lag_frames));
        }
        let stale = self.stream != StreamState::Live && self.is_stale(self.panes_loaded_at_ms);
        (text, if stale { YELLOW } else { color })
    }
}

const FG: PackedRgba = PackedRgba::rgb(0xCC, 0xCC, 0xCC);
const DIM: PackedRgba = PackedRgba::rgb(0x80, 0x80, 0x80);
const BAR_BG: PackedRgba = PackedRgba::rgb(0x20, 0x24, 0x2C);
const SELECTED_FG: PackedRgba = PackedRgba::rgb(0x0A, 0x0A, 0x0A);
const ACCENT: PackedRgba = PackedRgba::rgb(0x5F, 0xAF, 0xFF);
const GREEN: PackedRgba = PackedRgba::rgb(0x5F, 0xD7, 0x87);
const YELLOW: PackedRgba = PackedRgba::rgb(0xFF, 0xD7, 0x5F);
const RED: PackedRgba = PackedRgba::rgb(0xFF, 0x5F, 0x5F);

#[derive(Clone, Copy)]
struct Style {
    fg: PackedRgba,
    bg: Option<PackedRgba>,
    flags: StyleFlags,
}

impl Style {
    const fn fg(fg: PackedRgba) -> Self {
        Self {
            fg,
            bg: None,
            flags: StyleFlags::empty(),
        }
    }

    const fn bg(mut self, bg: PackedRgba) -> Self {
        self.bg = Some(bg);
        self
    }

    fn bold(mut self) -> Self {
        self.flags |= StyleFlags::BOLD;
        self
    }

    /// Highlight with explicit colors. `StyleFlags::REVERSE` is not used:
    /// with no explicit background the web renderer swaps in a transparent
    /// one, so reversed text vanishes.
    const fn selected(self) -> Self {
        Self {
            fg: SELECTED_FG,
            bg: Some(self.fg),
            flags: self.flags,
        }
    }
}

/// Write `text` at (col, row), clipped to `max_cols` and the frame.
fn put(frame: &mut Frame, col: u16, row: u16, text: &str, max_cols: u16, style: Style) {
    let buffer = &mut frame.buffer;
    if row >= buffer.height() {
        return;
    }
    let end = col.saturating_add(max_cols).min(buffer.width());
    for (x, ch) in (col..end).zip(text.chars()) {
        let mut cell = Cell::from_char(ch)
            .with_fg(style.fg)
            .with_attrs(CellAttrs::new(style.flags, 0));
        if let Some(bg) = style.bg {
            cell = cell.with_bg(bg);
        }
        buffer.set(x, row, cell);
    }
}

/// Paint a full-width bar background on `row`.
fn fill_row(frame: &mut Frame, row: u16, bg: PackedRgba) {
    let width = frame.buffer.width();
    let blank = " ".repeat(usize::from(width));
    put(frame, 0, row, &blank, width, Style::fg(FG).bg(bg));
}

fn severity_color(severity: &str) -> PackedRgba {
    match severity {
        "critical" => RED,
        "warning" => YELLOW,
        _ => FG,
    }
}

fn format_clock(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let day_secs = secs.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        day_secs / 3600,
        (day_secs / 60) % 60,
        day_secs % 60
    )
}

impl FleetModel {
    fn render_header(&self, frame: &mut Frame) {
        let width = frame.buffer.width();
        fill_row(frame, 0, BAR_BG);
        let title = " FrankenTerm · fleet viewer (read-only) ";
        put(
            frame,
            0,
            0,
            title,
            width,
            Style::fg(ACCENT).bg(BAR_BG).bold(),
        );
        let (status, color) = self.status_text();
        let status_cols = status.chars().count() as u16 + 1;
        if width > status_cols + title.chars().count() as u16 {
            put(
                frame,
                width - status_cols,
                0,
                &status,
                status_cols,
                Style::fg(color).bg(BAR_BG),
            );
        }

        let mut col = 1;
        for (label, view) in [(" 1 Panes ", View::Panes), (" 2 Events ", View::Events)] {
            let style = if view == self.view {
                Style::fg(FG).bold().selected()
            } else {
                Style::fg(DIM)
            };
            put(frame, col, 1, label, width.saturating_sub(col), style);
            col += label.chars().count() as u16 + 1;
        }
    }

    fn render_footer(&self, frame: &mut Frame) {
        let width = frame.buffer.width();
        let row = frame.buffer.height().saturating_sub(1);
        fill_row(frame, row, BAR_BG);
        if let Some(error) = &self.last_error {
            put(
                frame,
                1,
                row,
                error,
                width.saturating_sub(2),
                Style::fg(RED).bg(BAR_BG),
            );
        } else {
            let hint = "Tab switch view · j/k move · g/G top/bottom · r refresh";
            put(
                frame,
                1,
                row,
                hint,
                width.saturating_sub(2),
                Style::fg(DIM).bg(BAR_BG),
            );
        }
    }

    fn render_panes(&self, frame: &mut Frame, top: u16, rows: u16) {
        let width = frame.buffer.width();
        put(
            frame,
            1,
            top,
            "PANE  TITLE                          DOMAIN        LAST SEEN  CWD",
            width.saturating_sub(1),
            Style::fg(DIM).bold(),
        );
        if self.panes.is_empty() {
            let message = if self.panes_loaded_at_ms.is_some() {
                "No panes recorded. Start `ft watch` so captured panes appear here."
            } else {
                "Loading panes..."
            };
            put(
                frame,
                1,
                top + 2,
                message,
                width.saturating_sub(2),
                Style::fg(DIM),
            );
            return;
        }
        let visible = usize::from(rows.saturating_sub(1));
        let first = self.selected.saturating_sub(visible.saturating_sub(1));
        for (i, pane) in self.panes.iter().enumerate().skip(first).take(visible) {
            let row = top + 1 + (i - first) as u16;
            let gap = if self.gap_panes.contains(&pane.pane_id) {
                "!"
            } else {
                " "
            };
            let line = format!(
                "{gap}{:<4}  {:<29}  {:<12}  {:>9}  {}",
                pane.pane_id,
                truncate(pane.title.as_deref().unwrap_or("-"), 29),
                truncate(&pane.domain, 12),
                self.age_label(Some(pane.last_seen_at)),
                pane.cwd
                    .as_deref()
                    .map_or_else(|| "-".to_string(), display_cwd),
            );
            let style = if i == self.selected {
                Style::fg(FG).selected()
            } else {
                Style::fg(FG)
            };
            put(frame, 0, row, &line, width, style);
        }
        if self.panes_truncated > 0 {
            let note = format!("+{} more panes not shown", self.panes_truncated);
            put(
                frame,
                1,
                top + rows,
                &note,
                width.saturating_sub(2),
                Style::fg(YELLOW),
            );
        }
    }

    fn render_events(&self, frame: &mut Frame, top: u16, rows: u16) {
        let width = frame.buffer.width();
        put(
            frame,
            1,
            top,
            "TIME      SEVERITY  PANE  RULE",
            width.saturating_sub(1),
            Style::fg(DIM).bold(),
        );
        if self.events.is_empty() {
            let message = if self.events_loaded_at_ms.is_some() {
                "No detections yet."
            } else {
                "Loading detections..."
            };
            put(
                frame,
                1,
                top + 2,
                message,
                width.saturating_sub(2),
                Style::fg(DIM),
            );
            return;
        }
        let visible = usize::from(rows.saturating_sub(1));
        let first = self.selected.saturating_sub(visible.saturating_sub(1));
        for (i, event) in self.events.iter().enumerate().skip(first).take(visible) {
            let row = top + 1 + (i - first) as u16;
            let line = format!(
                " {}  {:<8}  {:<4}  {}",
                format_clock(event.detected_at_ms),
                truncate(&event.severity, 8),
                event.pane_id,
                event.rule_id,
            );
            let mut style = Style::fg(severity_color(&event.severity));
            if i == self.selected {
                style = style.selected();
            }
            put(frame, 0, row, &line, width, style);
        }
    }
}

/// The mux reports a pane's working directory as a `file://host/path` URL;
/// show the path. Percent-escapes are decoded only when they form valid UTF-8.
fn display_cwd(cwd: &str) -> String {
    let Some(rest) = cwd.strip_prefix("file://") else {
        return cwd.to_string();
    };
    let path = rest.find('/').map_or(rest, |slash| &rest[slash..]);
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escape = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(byte) = escape {
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| path.to_string())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

impl Model for FleetModel {
    type Message = FleetMsg;

    fn update(&mut self, msg: FleetMsg) -> Cmd<FleetMsg> {
        let FleetMsg::Input(event) = msg;
        if let Event::Key(key) = event {
            self.handle_key(key);
        }
        Cmd::none()
    }

    fn view(&self, frame: &mut Frame) {
        let height = frame.buffer.height();
        if height < 4 || frame.buffer.width() < 20 {
            put(
                frame,
                0,
                0,
                "window too small",
                frame.buffer.width(),
                Style::fg(FG),
            );
            return;
        }
        self.render_header(frame);
        let top = 2;
        let rows = height - 3;
        match self.view {
            View::Panes => self.render_panes(frame, top, rows),
            View::Events => self.render_events(frame, top, rows),
        }
        self.render_footer(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ftui_core::event::Modifiers;
    use ftui_render::grapheme_pool::GraphemePool;

    const PANES: &str = r#"{"ok":true,"version":"x","data":{"total":2,"panes":[
        {"pane_id":7,"domain":"local","title":"codex worker","cwd":"/repo","first_seen_at":1000,"last_seen_at":9000},
        {"pane_id":2,"domain":"SSH:gpu","title":"claude","first_seen_at":1000,"last_seen_at":8000}]}}"#;
    const EVENTS: &str = r#"{"ok":true,"version":"x","data":{"total":1,"events":[
        {"id":5,"pane_id":7,"rule_id":"codex.usage.reached","event_type":"usage","severity":"warning","confidence":0.9,"detected_at":9500}]}}"#;

    fn key(code: KeyCode) -> FleetMsg {
        FleetMsg::Input(Event::Key(KeyEvent {
            code,
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
        }))
    }

    fn render(model: &FleetModel, width: u16, height: u16) -> Vec<String> {
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(width, height, &mut pool);
        model.view(&mut frame);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        frame
                            .buffer
                            .get(x, y)
                            .and_then(|cell| cell.content.as_char())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn panes_render_sorted_with_title_domain_and_age() {
        let mut model = FleetModel::new();
        model.set_now_ms(10_000);
        assert_eq!(model.ingest_panes(PANES), Ok(2));
        let screen = render(&model, 100, 10);
        assert!(screen[0].contains("fleet viewer (read-only)"), "{screen:?}");
        let pane2 = screen
            .iter()
            .position(|l| l.contains("claude"))
            .expect("pane 2 row");
        let pane7 = screen
            .iter()
            .position(|l| l.contains("codex worker"))
            .expect("pane 7 row");
        assert!(pane2 < pane7, "panes sort by id: {screen:?}");
        assert!(screen[pane7].contains("1s ago") && screen[pane7].contains("/repo"));
        assert!(screen[pane2].contains("SSH:gpu"));
    }

    #[test]
    fn events_view_shows_history_then_live_detections_without_replay_duplicates() {
        let mut model = FleetModel::new();
        model.set_now_ms(10_000);
        assert_eq!(model.ingest_events(EVENTS), Ok(1));
        let live = r#"{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":2,"ts_ms":9900,
            "data":{"event":{"type":"pattern_detected","pane_id":2,"event_id":6,
            "detection":{"rule_id":"claude_code.approval_needed","agent_type":"claude_code","event_type":"prompt","severity":"critical","confidence":1.0,"extracted":{},"matched_text":"x"}}}}"#;
        model.ingest_stream_frame(live).expect("live frame");
        // A reconnect replays event 5, which the history already holds.
        let replay = r#"{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":3,"ts_ms":9500,
            "data":{"replayed":true,"event":{"type":"pattern_detected","pane_id":7,"event_id":5,
            "detection":{"rule_id":"codex.usage.reached","agent_type":"codex","event_type":"usage","severity":"warning","confidence":0.9,"extracted":{},"matched_text":"x"}}}}"#;
        model.ingest_stream_frame(replay).expect("replayed frame");
        assert_eq!(model.events().len(), 2);
        model.update(key(KeyCode::Tab));
        assert_eq!(model.active_view(), View::Events);
        let screen = render(&model, 100, 10);
        let newest = screen
            .iter()
            .position(|l| l.contains("claude_code.approval_needed"))
            .expect("live detection row");
        let older = screen
            .iter()
            .position(|l| l.contains("codex.usage.reached"))
            .expect("history row");
        assert!(newest < older, "{screen:?}");
    }

    #[test]
    fn stream_state_and_errors_reach_the_screen() {
        let mut model = FleetModel::new();
        model.set_now_ms(1_000);
        assert!(render(&model, 100, 8)[0].contains("offline"));
        let ready = r#"{"schema":"ft.stream.v1","stream":"events","kind":"ready","seq":1,"ts_ms":1,"data":{}}"#;
        model.ingest_stream_frame(ready).expect("ready");
        assert!(render(&model, 100, 8)[0].contains("live"));
        assert!(
            model
                .ingest_panes(
                    r#"{"ok":false,"error":"down","error_code":"storage_error","version":"x"}"#
                )
                .is_err()
        );
        let screen = render(&model, 100, 8);
        assert!(
            screen[7].contains("panes: storage_error: down"),
            "{screen:?}"
        );
    }

    #[test]
    fn navigation_is_bounded_and_refresh_is_one_shot() {
        let mut model = FleetModel::new();
        model.ingest_panes(PANES).expect("panes");
        model.update(key(KeyCode::Char('j')));
        model.update(key(KeyCode::Char('j')));
        assert_eq!(model.selected(), 1, "selection stops at the last row");
        model.update(key(KeyCode::Char('g')));
        assert_eq!(model.selected(), 0);
        model.update(key(KeyCode::Char('r')));
        assert!(model.take_refresh_request());
        assert!(!model.take_refresh_request());
        let gone = r#"{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":1,"ts_ms":1,"data":{"event":{"type":"pane_disappeared","pane_id":7}}}"#;
        model.update(key(KeyCode::Char('G')));
        model.ingest_stream_frame(gone).expect("disappeared");
        assert_eq!(model.panes().len(), 1);
        assert_eq!(model.selected(), 0, "selection follows the shrunken list");
    }

    #[test]
    fn event_history_is_bounded() {
        let mut model = FleetModel::new();
        for id in 0..(MAX_EVENTS as i64 + 25) {
            let frame = format!(
                r#"{{"schema":"ft.stream.v1","stream":"events","kind":"event","seq":1,"ts_ms":{id},"data":{{"event":{{"type":"pattern_detected","pane_id":1,"event_id":{id},"detection":{{"rule_id":"r","severity":"info"}}}}}}}}"#
            );
            model.ingest_stream_frame(&frame).expect("frame");
        }
        assert_eq!(model.events().len(), MAX_EVENTS);
        assert_eq!(model.events()[0].id, Some(MAX_EVENTS as i64 + 24));
    }

    #[test]
    fn highlights_use_explicit_opaque_colors() {
        // A REVERSE-flagged cell with no background rendered as an empty bar in
        // the web renderer; every highlighted cell must carry its own colors.
        let mut model = FleetModel::new();
        model.ingest_panes(PANES).expect("panes");
        let mut pool = GraphemePool::new();
        let mut frame = Frame::new(100, 10, &mut pool);
        model.view(&mut frame);
        let tab_row = 1;
        let selected_row = 3;
        for (row, col) in [(tab_row, 2), (selected_row, 1)] {
            let cell = frame.buffer.get(col, row).expect("cell");
            assert!(!cell.attrs.flags().contains(StyleFlags::REVERSE));
            assert_ne!(
                cell.bg.a(),
                0,
                "row {row} highlight needs an opaque background"
            );
            assert_ne!(
                cell.fg, cell.bg,
                "row {row} text must differ from its background"
            );
        }
    }

    #[test]
    fn cwd_urls_show_as_paths() {
        assert_eq!(display_cwd("file:///Users/j/work/"), "/Users/j/work/");
        assert_eq!(
            display_cwd("file://mac-studio/Users/j/my%20repo"),
            "/Users/j/my repo"
        );
        assert_eq!(display_cwd("/already/a/path"), "/already/a/path");
        // Escapes that do not decode to UTF-8 are left as they arrived.
        assert_eq!(display_cwd("file:///bad%FF%FE"), "/bad%FF%FE");
        assert_eq!(display_cwd("file:///trailing%2"), "/trailing%2");
    }

    #[test]
    fn tiny_frames_do_not_panic() {
        let mut model = FleetModel::new();
        model.ingest_panes(PANES).expect("panes");
        for (w, h) in [(1, 1), (19, 3), (20, 4), (300, 2)] {
            let _ = render(&model, w, h);
        }
    }
}
