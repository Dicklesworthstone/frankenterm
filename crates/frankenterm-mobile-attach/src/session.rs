//! The attach loop: one mux pane mirrored onto an ordinary terminal byte
//! stream, and that terminal's keystrokes delivered to the pane as mux input.
//!
//! The session owns no TTY.  The binary feeds it [`Event`]s (stdin bytes,
//! window-size changes, termination) and gives it the output writer, so tests
//! drive the same loop with in-memory input and output.
//!
//! The phone's screen mirrors the pane's viewport row for row: phone row `i`
//! shows stable row `top + i`.  When output scrolls the pane, the new rows are
//! streamed below the phone's last row with CRLF, so the phone's own
//! scrollback accumulates the history exactly as if the program ran locally.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use codec::{GetPaneRenderChangesResponse, MuxErrorCode};
use frankenterm_core::cx::{self, Cx};
use frankenterm_core::protocol_recovery::MuxConnectionDisposition;
use frankenterm_core::runtime_async::{CompatRuntime, Runtime, RuntimeBuilder};
use frankenterm_core::vendored::{DirectMuxClient, DirectMuxClientConfig, DirectMuxError};
use frankenterm_term::{StableRowIndex, TerminalSize};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use termwiz::input::{InputEvent, InputParser, KeyEvent};
use termwiz::surface::{CursorShape, CursorVisibility, Line};

use crate::control::{ControlMessage, ControlScanner, LONE_ESC_RELEASE_AFTER, ScanItem};
use crate::panes::{self, PaneSummary};
use crate::render::render_line;

/// Rows per `GetLines` request; small replies stay quick on a busy mux.
const LINES_PER_REQUEST: isize = 256;
/// Render poll interval right after input or output.
const ACTIVE_POLL: Duration = Duration::from_millis(16);
/// Render poll interval after a short quiet period.
const SETTLING_POLL: Duration = Duration::from_millis(50);
/// Render poll interval once the pane has been quiet for a while.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// How long a pane must stay quiet before polling slows down.
const ACTIVE_WINDOW: Duration = Duration::from_millis(500);
/// How long a pane must stay quiet before polling reaches [`IDLE_POLL`].
const SETTLING_WINDOW: Duration = Duration::from_secs(2);
/// How long an attach waits for the mux to apply the phone's size.
const RESIZE_SETTLE: Duration = Duration::from_secs(1);
/// How long the first render read of a pane may keep failing transiently.
const FIRST_RENDER_BUDGET: Duration = Duration::from_secs(5);
/// Attach attempts while panes keep disappearing under us.
const MAX_ATTACH_ATTEMPTS: usize = 8;
/// Delay before retrying a re-attach that failed transiently.
const REATTACH_RETRY: Duration = Duration::from_millis(100);

/// The phone's terminal size, from the PTY's `TIOCGWINSZ`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

/// Input to the session loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Bytes read from the phone (keystrokes, pastes, control messages).
    Input(Vec<u8>),
    /// The phone's input stream ended.
    InputClosed,
    /// The phone's terminal changed size.
    Resize(PtySize),
    /// SIGHUP/SIGTERM/SIGINT: detach and exit.
    Terminate,
}

/// Why a session ended without error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    InputClosed,
    Terminated,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("cannot start the async runtime: {0}")]
    Runtime(String),
    #[error("cannot connect to the FrankenTerm mux at {}: {source}", socket.display())]
    Connect {
        socket: PathBuf,
        #[source]
        source: DirectMuxError,
    },
    #[error("pane {0} does not exist on this mux")]
    PaneNotFound(u64),
    #[error("the mux has no panes left to attach to")]
    NoPanes,
    #[error("mux request failed: {0}")]
    Mux(#[from] DirectMuxError),
    #[error("cannot write to the terminal: {0}")]
    Output(#[from] std::io::Error),
}

/// Everything a session needs besides its event source and output.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub socket_path: PathBuf,
    /// Attach to exactly this pane first (it must exist).
    pub pane: Option<u64>,
    /// The phone's terminal size at start.
    pub size: PtySize,
    /// Scrollback rows copied into the phone's history on attach, and the
    /// most rows streamed for one burst of output.
    pub history_lines: usize,
    /// How often the pane list is re-read to notice topology changes.
    pub topology_interval: Duration,
}

impl SessionConfig {
    #[must_use]
    pub fn new(socket_path: PathBuf, pane: Option<u64>, size: PtySize) -> Self {
        Self {
            socket_path,
            pane,
            size,
            history_lines: 1000,
            topology_interval: Duration::from_secs(2),
        }
    }
}

/// Run one attach session to completion, then restore the attached pane's
/// original size (best effort) and the phone terminal's modes.
pub fn run_session(
    config: &SessionConfig,
    events: &Receiver<Event>,
    out: &mut dyn Write,
) -> Result<SessionEnd, SessionError> {
    let mux = Mux::connect(&config.socket_path)?;
    let mut session = Session::new(mux, config, out);
    let result = session.run(events);
    session.finish();
    session.mux.close();
    result
}

fn pane_is_gone(error: &DirectMuxError) -> bool {
    matches!(
        error,
        DirectMuxError::RemoteRejection(rejection) if rejection.code == MuxErrorCode::PANE_NOT_FOUND
    )
}

fn connection_is_lost(error: &DirectMuxError) -> bool {
    error.recovery_decision().connection == MuxConnectionDisposition::Discard
}

/// Drop a runtime the way the project's harnesses do: a runtime that panics
/// while retiring its futures must not take the process (or a thread-local
/// destructor) down with it.
fn retire_runtime(runtime: Runtime) {
    let _ = std::panic::catch_unwind(AssertUnwindSafe(move || drop(runtime)));
    let _ = std::panic::catch_unwind(AssertUnwindSafe(
        frankenterm_core::runtime_async::clear_runtime_handle,
    ));
}

/// The mux connection and the runtime that drives it.
struct Mux {
    /// Declared first: the client is dropped before the runtime it uses.
    client: DirectMuxClient,
    runtime: Runtime,
    socket_path: PathBuf,
}

fn connect_client(runtime: &Runtime, socket_path: &Path) -> Result<DirectMuxClient, SessionError> {
    let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
    runtime
        .block_on(Box::pin(DirectMuxClient::connect(config)))
        .map_err(|source| SessionError::Connect {
            socket: socket_path.to_path_buf(),
            source,
        })
}

impl Mux {
    fn connect(socket_path: &Path) -> Result<Self, SessionError> {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .map_err(SessionError::Runtime)?;
        match connect_client(&runtime, socket_path) {
            Ok(client) => Ok(Self {
                client,
                runtime,
                socket_path: socket_path.to_path_buf(),
            }),
            Err(error) => {
                retire_runtime(runtime);
                Err(error)
            }
        }
    }

    fn reconnect(&mut self) -> Result<(), SessionError> {
        self.client = connect_client(&self.runtime, &self.socket_path)?;
        Ok(())
    }

    fn close(self) {
        let Self {
            client, runtime, ..
        } = self;
        drop(client);
        retire_runtime(runtime);
    }

    fn list_panes(&mut self) -> Result<Vec<PaneSummary>, DirectMuxError> {
        let list = self.runtime.block_on(Box::pin(self.client.list_panes()))?;
        Ok(panes::summarize(&list))
    }

    /// Panes other clients (the desktop GUI) focus, most recent input first.
    /// Empty when the mux cannot say.
    fn recent_focus(&mut self) -> Vec<u64> {
        let client = &mut self.client;
        let Ok(list) = self.runtime.block_on(Box::pin(async {
            let cx = Cx::current().unwrap_or_else(cx::for_request);
            client.get_client_list_with_cx(&cx).await
        })) else {
            return Vec::new();
        };
        let mut clients = list.clients;
        clients.sort_by(|a, b| b.last_input.cmp(&a.last_input));
        clients
            .into_iter()
            .filter_map(|client| client.focused_pane_id.map(|id| id as u64))
            .collect()
    }

    fn render(&mut self, pane: u64) -> Result<GetPaneRenderChangesResponse, DirectMuxError> {
        self.runtime
            .block_on(Box::pin(self.client.get_pane_render_changes(pane)))
    }

    /// The pane's lines for `rows`; rows the mux no longer has are absent.
    fn lines(
        &mut self,
        pane: u64,
        rows: Range<StableRowIndex>,
    ) -> Result<BTreeMap<StableRowIndex, Line>, DirectMuxError> {
        let mut out = BTreeMap::new();
        let mut start = rows.start;
        while start < rows.end {
            let end = start.saturating_add(LINES_PER_REQUEST).min(rows.end);
            let reply = self
                .runtime
                .block_on(Box::pin(self.client.get_lines(pane, vec![start..end])))?;
            // A reply whose internal references do not validate is treated
            // as missing rows rather than trusted.
            if let Ok((lines, _images)) = reply.lines.extract_data_checked() {
                for (row, line) in lines {
                    if (start..end).contains(&row) {
                        out.insert(row, line);
                    }
                }
            }
            start = end;
        }
        Ok(out)
    }

    fn resize(&mut self, tab: u64, pane: u64, size: TerminalSize) -> Result<(), DirectMuxError> {
        self.runtime
            .block_on(Box::pin(self.client.resize(tab, pane, size)))
            .map(|_| ())
    }

    fn key(&mut self, pane: u64, event: KeyEvent) -> Result<(), DirectMuxError> {
        let client = &mut self.client;
        self.runtime
            .block_on(Box::pin(async {
                let cx = Cx::current().unwrap_or_else(cx::for_request);
                client.send_key_down_with_cx(&cx, pane, event).await
            }))
            .map(|_| ())
    }

    fn paste(&mut self, pane: u64, text: String) -> Result<(), DirectMuxError> {
        self.runtime
            .block_on(Box::pin(self.client.send_paste(pane, text)))
            .map(|_| ())
    }
}

/// The attached pane and the size it had before the phone resized it.
#[derive(Clone, Copy, Debug)]
struct Attachment {
    pane: u64,
    tab: u64,
    original: TerminalSize,
}

/// Where the pane's viewport lands on the phone's screen.
#[derive(Clone, Copy, Debug)]
struct Layout {
    /// Stable row shown on the phone's first row.
    top: StableRowIndex,
    /// Phone rows showing pane rows (the rest are blank).
    shown: usize,
    /// Columns of pane content drawn per row.
    cols: usize,
    phone_rows: usize,
    phone_cols: usize,
}

impl Layout {
    fn new(phone: PtySize, dims: &RenderableDimensions) -> Self {
        let phone_rows = usize::from(phone.rows).max(1);
        let phone_cols = usize::from(phone.cols).max(1);
        // A pane taller than the phone (its resize not yet applied, or
        // refused by a split) shows its bottom rows, where the cursor is.
        let hidden = dims.viewport_rows.saturating_sub(phone_rows);
        Self {
            top: dims
                .physical_top
                .saturating_add(isize::try_from(hidden).unwrap_or(isize::MAX)),
            shown: dims.viewport_rows.min(phone_rows),
            cols: dims.cols.min(phone_cols),
            phone_rows,
            phone_cols,
        }
    }

    fn shown_rows(&self) -> Range<StableRowIndex> {
        self.top
            ..self
                .top
                .saturating_add(isize::try_from(self.shown).unwrap_or(isize::MAX))
    }

    /// Phone (row, col) for the pane cursor, if it is on screen.
    fn cursor(&self, cursor: &StableCursorPosition) -> Option<(usize, usize)> {
        let row = usize::try_from(cursor.y.checked_sub(self.top)?).ok()?;
        if row >= self.shown || self.cols == 0 {
            return None;
        }
        Some((row, cursor.x.min(self.cols - 1)))
    }
}

/// What the phone's screen shows.
#[derive(Clone, Copy, Debug, Default)]
struct View {
    /// The screen mirrors the pane as of the last frame.
    valid: bool,
    /// The phone is in its alternate screen.
    alt: bool,
    /// Pane (cols, viewport rows) at the last frame.
    dims: (usize, usize),
    /// Stable row on the phone's first row at the last frame.
    top: StableRowIndex,
}

/// Cursor state last sent to the phone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CursorState {
    at: Option<(usize, usize)>,
    visible: bool,
}

/// How an attach attempt ended.
enum Attach {
    Attached,
    PaneGone,
}

struct Session<'w> {
    mux: Mux,
    out: &'w mut dyn Write,
    requested: Option<u64>,
    phone: PtySize,
    history_lines: usize,
    topology_interval: Duration,
    panes: Vec<PaneSummary>,
    announced: Option<Vec<u8>>,
    attached: Option<Attachment>,
    /// A pane that vanished; attach elsewhere at `reattach_at`.
    reattach_from: Option<u64>,
    reattach_at: Instant,
    view: View,
    cursor: Option<CursorState>,
    cursor_shape: Option<CursorShape>,
    title: Option<String>,
    scanner: ControlScanner,
    parser: InputParser,
    parser_pending: bool,
    last_input: Instant,
    last_activity: Instant,
    next_render: Instant,
    next_topology: Instant,
}

fn push_cup(out: &mut Vec<u8>, row: usize, col: usize) {
    out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
}

fn decscusr(shape: CursorShape) -> &'static [u8] {
    match shape {
        CursorShape::Default => b"\x1b[0 q",
        CursorShape::BlinkingBlock => b"\x1b[1 q",
        CursorShape::SteadyBlock => b"\x1b[2 q",
        CursorShape::BlinkingUnderline => b"\x1b[3 q",
        CursorShape::SteadyUnderline => b"\x1b[4 q",
        CursorShape::BlinkingBar => b"\x1b[5 q",
        CursorShape::SteadyBar => b"\x1b[6 q",
    }
}

/// Paint phone `row` (CUP, the pane row it shows, erase the rest).
fn paint_row(
    body: &mut Vec<u8>,
    layout: &Layout,
    lines: &BTreeMap<StableRowIndex, Line>,
    row: usize,
) {
    push_cup(body, row, 0);
    let mut cols = 0;
    if row < layout.shown {
        let stable = layout
            .top
            .saturating_add(isize::try_from(row).unwrap_or(isize::MAX));
        if let Some(line) = lines.get(&stable) {
            cols = render_line(line, layout.cols, body);
        }
    }
    // Never after a full-width row: EL in the pending-wrap state would
    // erase that row's last column.
    if cols < layout.phone_cols {
        body.extend_from_slice(b"\x1b[K");
    }
}

fn missing_rows(
    lines: &BTreeMap<StableRowIndex, Line>,
    rows: Range<StableRowIndex>,
) -> BTreeSet<StableRowIndex> {
    rows.filter(|row| !lines.contains_key(row)).collect()
}

/// Merge sorted rows into contiguous ranges.
fn row_runs(rows: &BTreeSet<StableRowIndex>) -> Vec<Range<StableRowIndex>> {
    let mut runs: Vec<Range<StableRowIndex>> = Vec::new();
    for &row in rows {
        match runs.last_mut() {
            Some(run) if run.end == row => run.end = row + 1,
            _ => runs.push(row..row + 1),
        }
    }
    runs
}

/// The size to give a pane so it fills the phone.  Pixel geometry comes from
/// the PTY when it reports one, else scales the pane's own cell size.
fn phone_terminal_size(phone: PtySize, original: TerminalSize) -> TerminalSize {
    let rows = usize::from(phone.rows);
    let cols = usize::from(phone.cols);
    let (pixel_width, pixel_height) = if phone.pixel_width > 0 && phone.pixel_height > 0 {
        (
            usize::from(phone.pixel_width),
            usize::from(phone.pixel_height),
        )
    } else if original.cols > 0 && original.rows > 0 {
        (
            original.pixel_width / original.cols * cols,
            original.pixel_height / original.rows * rows,
        )
    } else {
        (0, 0)
    };
    TerminalSize {
        rows,
        cols,
        pixel_width,
        pixel_height,
        dpi: original.dpi,
    }
}

impl<'w> Session<'w> {
    fn new(mux: Mux, config: &SessionConfig, out: &'w mut dyn Write) -> Self {
        let now = Instant::now();
        Self {
            mux,
            out,
            requested: config.pane,
            phone: config.size,
            history_lines: config.history_lines,
            topology_interval: config.topology_interval,
            panes: Vec::new(),
            announced: None,
            attached: None,
            reattach_from: None,
            reattach_at: now,
            view: View::default(),
            cursor: None,
            cursor_shape: None,
            title: None,
            scanner: ControlScanner::new(),
            parser: InputParser::new(),
            parser_pending: false,
            last_input: now,
            last_activity: now,
            next_render: now,
            next_topology: now + config.topology_interval,
        }
    }

    fn phone_known(&self) -> bool {
        self.phone.rows > 0 && self.phone.cols > 0
    }

    fn write_now(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        self.out.write_all(bytes)?;
        self.out.flush()?;
        Ok(())
    }

    fn run(&mut self, events: &Receiver<Event>) -> Result<SessionEnd, SessionError> {
        // Pastes arrive bracketed, so they reach the pane as one paste.
        self.write_now(b"\x1b[?2004h")?;
        let requested = self.requested;
        self.attach_somewhere(requested, None, requested.is_some())?;

        loop {
            let now = Instant::now();
            let timeout = self.next_deadline().saturating_duration_since(now);
            let mut batch = Vec::new();
            match events.recv_timeout(timeout) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(SessionEnd::InputClosed),
            }
            while let Ok(event) = events.try_recv() {
                batch.push(event);
            }
            let mut resize = None;
            for event in batch {
                match event {
                    Event::Input(bytes) => {
                        let result = self.on_input(&bytes);
                        self.recover(result)?;
                    }
                    Event::Resize(size) => resize = Some(size),
                    Event::InputClosed => return Ok(SessionEnd::InputClosed),
                    Event::Terminate => return Ok(SessionEnd::Terminated),
                }
            }
            if let Some(size) = resize {
                let result = self.on_resize(size);
                self.recover(result)?;
            }
            let result = self.settle_idle_input();
            self.recover(result)?;

            if let Some(gone) = self.reattach_from.take() {
                let result = self.attach_somewhere(None, Some(gone), false);
                if matches!(result, Err(SessionError::Mux(_))) {
                    // Transient (or a reconnect is needed): try again shortly.
                    self.reattach_from = Some(gone);
                    self.reattach_at = Instant::now() + REATTACH_RETRY;
                }
                self.recover(result)?;
            }
            if Instant::now() >= self.next_topology {
                let result = self.refresh_topology();
                self.recover(result)?;
                self.next_topology = Instant::now() + self.topology_interval;
            }
            if Instant::now() >= self.next_render {
                let result = self.poll_render();
                self.recover(result)?;
                self.next_render = Instant::now() + self.render_interval();
            }
        }
    }

    /// Handle a recoverable mux failure: a vanished pane schedules a
    /// re-attach, a broken connection is replaced, anything else is retried
    /// by the next poll.  Other errors end the session.
    fn recover(&mut self, result: Result<(), SessionError>) -> Result<(), SessionError> {
        match result {
            Err(SessionError::Mux(error)) => {
                if pane_is_gone(&error) {
                    if let Some(attached) = self.attached {
                        self.schedule_reattach(attached.pane);
                    }
                } else if connection_is_lost(&error) {
                    self.mux.reconnect()?;
                    self.view.valid = false;
                    self.announced = None;
                }
                Ok(())
            }
            other => other,
        }
    }

    fn schedule_reattach(&mut self, gone: u64) {
        self.reattach_from = Some(gone);
        self.reattach_at = Instant::now();
    }

    fn next_deadline(&self) -> Instant {
        let mut deadline = self.next_render.min(self.next_topology);
        if self.reattach_from.is_some() {
            deadline = deadline.min(self.reattach_at);
        }
        if self.parser_pending {
            deadline = deadline.min(self.last_input + LONE_ESC_RELEASE_AFTER);
        }
        if let Some(after) = self.scanner.release_after() {
            deadline = deadline.min(self.last_input + after);
        }
        deadline
    }

    fn render_interval(&self) -> Duration {
        let quiet = self.last_activity.elapsed();
        if quiet < ACTIVE_WINDOW {
            ACTIVE_POLL
        } else if quiet < SETTLING_WINDOW {
            SETTLING_POLL
        } else {
            IDLE_POLL
        }
    }

    fn mark_activity(&mut self) {
        self.last_activity = Instant::now();
        self.next_render = self.next_render.min(self.last_activity + ACTIVE_POLL);
    }

    // ----- input -----------------------------------------------------------

    fn on_input(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        self.last_input = Instant::now();
        for item in self.scanner.feed(bytes) {
            match item {
                ScanItem::Data(data) => self.parse_input(&data, true)?,
                ScanItem::Control(message) => self.on_control(&message)?,
            }
        }
        Ok(())
    }

    /// Release input held back for disambiguation once the phone went quiet.
    fn settle_idle_input(&mut self) -> Result<(), SessionError> {
        let idle = self.last_input.elapsed();
        for item in self.scanner.on_idle(idle) {
            if let ScanItem::Data(data) = item {
                self.parse_input(&data, true)?;
            }
        }
        if self.parser_pending && idle >= LONE_ESC_RELEASE_AFTER {
            self.parse_input(&[], false)?;
        }
        Ok(())
    }

    fn parse_input(&mut self, bytes: &[u8], maybe_more: bool) -> Result<(), SessionError> {
        let events = self.parser.parse_as_vec(bytes, maybe_more);
        self.parser_pending = maybe_more;
        let Some(attached) = self.attached else {
            return Ok(());
        };
        for event in events {
            match event {
                InputEvent::Key(key) => self.mux.key(attached.pane, key)?,
                InputEvent::Paste(text) => self.mux.paste(attached.pane, text)?,
                // Mouse reporting is not enabled on the phone in v1.
                InputEvent::Mouse(_)
                | InputEvent::PixelMouse(_)
                | InputEvent::Resized { .. }
                | InputEvent::Wake => continue,
            }
            self.mark_activity();
        }
        Ok(())
    }

    fn on_control(&mut self, message: &ControlMessage) -> Result<(), SessionError> {
        match message.verb.as_str() {
            "attach" => {
                let Ok(pane) = message.arg.trim().parse::<u64>() else {
                    return self.announce(true);
                };
                self.panes = self.mux.list_panes()?;
                if self.panes.iter().any(|summary| summary.id == pane) {
                    self.attach_somewhere(Some(pane), None, false)
                } else {
                    // Tell the app the current truth instead.
                    self.announce(true)
                }
            }
            "panes" => {
                self.panes = self.mux.list_panes()?;
                self.announce(true)
            }
            // Newer apps may send verbs this helper does not know.
            _ => Ok(()),
        }
    }

    fn on_resize(&mut self, size: PtySize) -> Result<(), SessionError> {
        if size == self.phone {
            return Ok(());
        }
        self.phone = size;
        self.view.valid = false;
        self.next_render = Instant::now();
        let Some(attached) = self.attached else {
            return Ok(());
        };
        if self.phone_known() {
            self.mux.resize(
                attached.tab,
                attached.pane,
                phone_terminal_size(size, attached.original),
            )?;
        }
        Ok(())
    }

    // ----- topology and attachment ----------------------------------------

    fn announce(&mut self, force: bool) -> Result<(), SessionError> {
        let Some(attached) = self.attached else {
            return Ok(());
        };
        let dcs = panes::pane_list_dcs(attached.pane, &self.panes);
        if force || self.announced.as_ref() != Some(&dcs) {
            self.write_now(&dcs)?;
            self.announced = Some(dcs);
        }
        Ok(())
    }

    fn refresh_topology(&mut self) -> Result<(), SessionError> {
        self.panes = self.mux.list_panes()?;
        if let Some(attached) = self.attached.as_mut() {
            match self.panes.iter().find(|pane| pane.id == attached.pane) {
                Some(pane) => attached.tab = pane.tab,
                None => {
                    let gone = attached.pane;
                    self.schedule_reattach(gone);
                    return Ok(());
                }
            }
        }
        self.announce(false)
    }

    /// Give the attached pane back its original size, if it still exists.
    fn release_attachment(&mut self) {
        if let Some(attached) = self.attached.take() {
            if self.panes.iter().any(|pane| pane.id == attached.pane) {
                let _ = self
                    .mux
                    .resize(attached.tab, attached.pane, attached.original);
            }
        }
    }

    /// Attach to `preferred`, else to the pane the desktop most recently
    /// used, retrying while panes disappear.  `strict` makes a missing
    /// `preferred` pane an error instead of a fallback.
    fn attach_somewhere(
        &mut self,
        preferred: Option<u64>,
        mut gone: Option<u64>,
        strict: bool,
    ) -> Result<(), SessionError> {
        let mut preferred = preferred;
        for _ in 0..MAX_ATTACH_ATTEMPTS {
            self.panes = self.mux.list_panes()?;
            let target = match preferred.filter(|id| self.panes.iter().any(|pane| pane.id == *id)) {
                Some(id) => id,
                None => {
                    if strict {
                        if let Some(id) = preferred {
                            return Err(SessionError::PaneNotFound(id));
                        }
                    }
                    let focus = self.mux.recent_focus();
                    panes::choose_pane(&self.panes, &focus, gone).ok_or(SessionError::NoPanes)?
                }
            };
            match self.attach_to(target) {
                Ok(Attach::Attached) => return Ok(()),
                Ok(Attach::PaneGone) => {}
                Err(SessionError::Mux(error)) if pane_is_gone(&error) => {}
                Err(error) => return Err(error),
            }
            gone = Some(target);
            preferred = None;
        }
        Err(SessionError::NoPanes)
    }

    fn attach_to(&mut self, pane: u64) -> Result<Attach, SessionError> {
        if self.attached.map(|attached| attached.pane) != Some(pane) {
            self.release_attachment();
        }
        let Some(summary) = self.panes.iter().find(|summary| summary.id == pane) else {
            return Ok(Attach::PaneGone);
        };
        let attached = match self.attached {
            Some(attached) if attached.pane == pane => Attachment {
                tab: summary.tab,
                ..attached
            },
            _ => Attachment {
                pane,
                tab: summary.tab,
                original: summary.size,
            },
        };
        self.attached = Some(attached);
        if self.phone_known() {
            let size = phone_terminal_size(self.phone, attached.original);
            self.mux.resize(attached.tab, pane, size)?;
            self.wait_for_size(pane, size)?;
        }
        self.panes = self.mux.list_panes()?;
        if !self.panes.iter().any(|summary| summary.id == pane) {
            return Ok(Attach::PaneGone);
        }
        self.announce(true)?;
        self.initial_paint(pane)?;
        Ok(Attach::Attached)
    }

    /// Wait (briefly) until the pane reports the size the phone asked for,
    /// so the first paint is already laid out for the phone.
    fn wait_for_size(&mut self, pane: u64, size: TerminalSize) -> Result<(), SessionError> {
        let deadline = Instant::now() + RESIZE_SETTLE;
        loop {
            match self.mux.render(pane) {
                Ok(state)
                    if state.dimensions.cols == size.cols
                        && state.dimensions.viewport_rows == size.rows =>
                {
                    return Ok(());
                }
                Ok(_) => {}
                Err(error) if pane_is_gone(&error) || connection_is_lost(&error) => {
                    return Err(error.into());
                }
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                // A split or a smaller desktop window may refuse the size;
                // the renderer clips and offsets instead.
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A render state for a freshly attached pane.  The first read of a pane
    /// on a connection can race its own delta, so transient errors retry.
    fn first_render(&mut self, pane: u64) -> Result<GetPaneRenderChangesResponse, SessionError> {
        let deadline = Instant::now() + FIRST_RENDER_BUDGET;
        loop {
            match self.mux.render(pane) {
                Ok(state) => return Ok(state),
                Err(error)
                    if pane_is_gone(&error)
                        || connection_is_lost(&error)
                        || Instant::now() >= deadline =>
                {
                    return Err(error.into());
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    // ----- painting ----------------------------------------------------------

    /// Clear the phone (screen and scrollback), copy up to `history_lines`
    /// of the pane's scrollback into the phone's history, then paint the
    /// viewport, cursor and title.
    fn initial_paint(&mut self, pane: u64) -> Result<(), SessionError> {
        let state = self.first_render(pane)?;
        let layout = Layout::new(self.phone, &state.dimensions);
        let mut body = Vec::new();
        if self.view.alt {
            body.extend_from_slice(b"\x1b[?1049l");
        }
        body.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J\x1b[3J");
        if state.alt_screen_active {
            body.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        } else {
            let history = isize::try_from(self.history_lines).unwrap_or(isize::MAX);
            let start = layout
                .top
                .saturating_sub(history)
                .max(state.dimensions.scrollback_top);
            if start < layout.top {
                let lines = self.mux.lines(pane, start..layout.top)?;
                for row in start..layout.top {
                    if let Some(line) = lines.get(&row) {
                        render_line(line, layout.cols, &mut body);
                    }
                    body.extend_from_slice(b"\r\n");
                }
                // Scroll every streamed row off the screen into the phone's
                // scrollback: from any cursor row, phone_rows - 1 more line
                // feeds leave only blank rows on screen.
                for _ in 1..layout.phone_rows {
                    body.extend_from_slice(b"\r\n");
                }
            }
        }
        let lines = self.mux.lines(pane, layout.shown_rows())?;
        for row in 0..layout.phone_rows {
            paint_row(&mut body, &layout, &lines, row);
        }
        self.view = View {
            valid: true,
            alt: state.alt_screen_active,
            dims: (state.dimensions.cols, state.dimensions.viewport_rows),
            top: layout.top,
        };
        // A new pane's cursor and title must be re-sent.
        self.cursor = None;
        self.title = None;
        self.finish_frame(body, true, &state, &layout)
    }

    fn poll_render(&mut self) -> Result<(), SessionError> {
        let Some(attached) = self.attached else {
            return Ok(());
        };
        let state = self.mux.render(attached.pane)?;
        self.apply_render(attached.pane, state)
    }

    /// Bring the phone up to date with one render delta.
    fn apply_render(
        &mut self,
        pane: u64,
        mut state: GetPaneRenderChangesResponse,
    ) -> Result<(), SessionError> {
        let dims = state.dimensions;
        let layout = Layout::new(self.phone, &dims);
        let shown = layout.shown_rows();

        let mut lines: BTreeMap<StableRowIndex, Line> = BTreeMap::new();
        if let Ok((bonus, _images)) = std::mem::take(&mut state.bonus_lines).extract_data_checked()
        {
            lines.extend(bonus);
        }
        // Rows on screen that changed: every bonus row (the server always
        // includes the cursor row) plus reported dirty ranges.
        let mut dirty: BTreeSet<StableRowIndex> = lines
            .keys()
            .copied()
            .filter(|row| shown.contains(row))
            .collect();
        for range in &state.dirty_lines {
            let start = range.start.max(shown.start);
            let end = range.end.min(shown.end);
            dirty.extend(start..end);
        }

        let mut body = Vec::new();
        let alt_changed = state.alt_screen_active != self.view.alt;
        if alt_changed {
            body.extend_from_slice(if state.alt_screen_active {
                b"\x1b[?1049h"
            } else {
                b"\x1b[?1049l"
            });
        }
        let geometry_changed =
            !self.view.valid || self.view.dims != (dims.cols, dims.viewport_rows);
        let scrolled = layout.top.saturating_sub(self.view.top);
        let can_stream = !state.alt_screen_active && dims.viewport_rows >= layout.phone_rows;

        let painted =
            if alt_changed || geometry_changed || scrolled < 0 || (scrolled > 0 && !can_stream) {
                let missing = missing_rows(&lines, shown);
                self.fetch_rows(pane, &missing, &mut lines)?;
                for row in 0..layout.phone_rows {
                    paint_row(&mut body, &layout, &lines, row);
                }
                true
            } else if scrolled > 0 {
                self.stream_scrolled(pane, &mut body, &layout, &mut lines, &dirty)?;
                true
            } else {
                let missing: BTreeSet<StableRowIndex> = dirty
                    .iter()
                    .copied()
                    .filter(|row| !lines.contains_key(row))
                    .collect();
                self.fetch_rows(pane, &missing, &mut lines)?;
                for &row in &dirty {
                    if let Ok(phone_row) = usize::try_from(row - layout.top) {
                        paint_row(&mut body, &layout, &lines, phone_row);
                    }
                }
                !dirty.is_empty()
            };

        self.view = View {
            valid: true,
            alt: state.alt_screen_active,
            dims: (dims.cols, dims.viewport_rows),
            top: layout.top,
        };
        self.finish_frame(body, painted, &state, &layout)
    }

    /// Output scrolled the pane by `layout.top - view.top` rows: stream the
    /// new rows below the phone's last row so the rows scrolling off the top
    /// land in the phone's scrollback, then repaint rows that changed in
    /// place.  A burst longer than `history_lines` keeps only its tail.
    fn stream_scrolled(
        &mut self,
        pane: u64,
        body: &mut Vec<u8>,
        layout: &Layout,
        lines: &mut BTreeMap<StableRowIndex, Line>,
        dirty: &BTreeSet<StableRowIndex>,
    ) -> Result<(), SessionError> {
        let rows = isize::try_from(layout.phone_rows).unwrap_or(isize::MAX);
        let first_new = self.view.top.saturating_add(rows);
        let end = layout.top.saturating_add(rows);
        let limit =
            isize::try_from(self.history_lines.max(layout.phone_rows)).unwrap_or(isize::MAX);
        let start = first_new.max(end.saturating_sub(limit));

        let missing = missing_rows(lines, start..end);
        self.fetch_rows(pane, &missing, lines)?;
        push_cup(body, layout.phone_rows - 1, 0);
        for row in start..end {
            body.extend_from_slice(b"\r\n");
            let cols = lines
                .get(&row)
                .map_or(0, |line| render_line(line, layout.cols, body));
            if cols < layout.phone_cols {
                body.extend_from_slice(b"\x1b[K");
            }
        }
        // Rows that were already on screen (now shifted up) and changed.
        // After a burst longer than the screen, every row was streamed.
        if start > layout.top {
            for &row in dirty.range(layout.top..start) {
                if let Ok(phone_row) = usize::try_from(row - layout.top) {
                    paint_row(body, layout, lines, phone_row);
                }
            }
        }
        Ok(())
    }

    fn fetch_rows(
        &mut self,
        pane: u64,
        rows: &BTreeSet<StableRowIndex>,
        lines: &mut BTreeMap<StableRowIndex, Line>,
    ) -> Result<(), SessionError> {
        for run in row_runs(rows) {
            lines.extend(self.mux.lines(pane, run)?);
        }
        Ok(())
    }

    /// Wrap a frame body with cursor, cursor shape and title updates and
    /// write it in one go.  Painted frames hide the cursor while drawing and
    /// use synchronized output so the phone never shows a half-drawn screen.
    fn finish_frame(
        &mut self,
        body: Vec<u8>,
        painted: bool,
        state: &GetPaneRenderChangesResponse,
        layout: &Layout,
    ) -> Result<(), SessionError> {
        let at = layout.cursor(&state.cursor_position);
        let cursor = CursorState {
            at,
            visible: at.is_some() && state.cursor_position.visibility == CursorVisibility::Visible,
        };
        let shape = state.cursor_position.shape;
        let title = panes::sanitize_title(&state.title);
        let cursor_changed = painted || self.cursor != Some(cursor);
        let shape_changed = self.cursor_shape != Some(shape);
        let title_changed = self.title.as_deref() != Some(title.as_str());
        if !(painted || cursor_changed || shape_changed || title_changed) && body.is_empty() {
            return Ok(());
        }

        let mut frame = Vec::with_capacity(body.len() + 64);
        if painted {
            frame.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        }
        frame.extend_from_slice(&body);
        if cursor_changed {
            if let Some((row, col)) = cursor.at {
                push_cup(&mut frame, row, col);
            }
            frame.extend_from_slice(if cursor.visible {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            });
        }
        if shape_changed {
            frame.extend_from_slice(decscusr(shape));
        }
        if title_changed {
            frame.extend_from_slice(b"\x1b]2;");
            frame.extend_from_slice(title.as_bytes());
            frame.extend_from_slice(b"\x1b\\");
        }
        if painted {
            frame.extend_from_slice(b"\x1b[?2026l");
        }
        self.write_now(&frame)?;

        self.cursor = Some(cursor);
        self.cursor_shape = Some(shape);
        self.title = Some(title);
        if painted || cursor_changed {
            self.mark_activity();
        }
        Ok(())
    }

    /// Detach: restore the pane's size and the phone terminal's modes.
    /// Best effort: the connection or the phone may already be gone.
    fn finish(&mut self) {
        self.release_attachment();
        let mut reset = Vec::new();
        reset.extend_from_slice(b"\x1b[?2026l");
        if self.view.alt {
            reset.extend_from_slice(b"\x1b[?1049l");
        }
        reset.extend_from_slice(b"\x1b[0m\x1b[?25h\x1b[0 q\x1b[?2004l\r\n");
        let _ = self.write_now(&reset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(cols: usize, rows: usize, top: StableRowIndex) -> RenderableDimensions {
        RenderableDimensions {
            cols,
            viewport_rows: rows,
            scrollback_rows: rows,
            physical_top: top,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 0,
            pixel_height: 0,
            reverse_video: false,
        }
    }

    fn phone(rows: u16, cols: u16) -> PtySize {
        PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[test]
    fn a_pane_taller_than_the_phone_shows_its_bottom_rows() {
        let layout = Layout::new(phone(10, 40), &dims(80, 24, 100));
        assert_eq!(layout.top, 114);
        assert_eq!(layout.shown, 10);
        assert_eq!(layout.cols, 40);
        let cursor = StableCursorPosition {
            x: 79,
            y: 123,
            ..StableCursorPosition::default()
        };
        assert_eq!(
            layout.cursor(&cursor),
            Some((9, 39)),
            "clamped to the screen"
        );
        let above = StableCursorPosition {
            y: 105,
            ..StableCursorPosition::default()
        };
        assert_eq!(layout.cursor(&above), None);
    }

    #[test]
    fn a_pane_shorter_than_the_phone_leaves_blank_rows_below() {
        let layout = Layout::new(phone(30, 100), &dims(80, 24, 7));
        assert_eq!(layout.top, 7);
        assert_eq!(layout.shown, 24);
        assert_eq!(layout.cols, 80);
        assert_eq!(layout.shown_rows(), 7..31);
    }

    #[test]
    fn row_runs_merge_contiguous_rows() {
        let rows: BTreeSet<StableRowIndex> = [1, 2, 3, 7, 9, 10].into_iter().collect();
        assert_eq!(row_runs(&rows), vec![1..4, 7..8, 9..11]);
        assert!(row_runs(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn phone_size_scales_the_panes_cell_geometry_when_the_pty_has_none() {
        let original = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 144,
        };
        assert_eq!(
            phone_terminal_size(phone(40, 50), original),
            TerminalSize {
                rows: 40,
                cols: 50,
                pixel_width: 500,
                pixel_height: 800,
                dpi: 144,
            }
        );
        let with_pixels = PtySize {
            rows: 40,
            cols: 50,
            pixel_width: 1000,
            pixel_height: 1600,
        };
        assert_eq!(phone_terminal_size(with_pixels, original).pixel_width, 1000);
    }

    #[test]
    fn cursor_shapes_map_to_decscusr() {
        assert_eq!(decscusr(CursorShape::Default), b"\x1b[0 q");
        assert_eq!(decscusr(CursorShape::SteadyBar), b"\x1b[6 q");
        assert_eq!(decscusr(CursorShape::BlinkingUnderline), b"\x1b[3 q");
    }
}
