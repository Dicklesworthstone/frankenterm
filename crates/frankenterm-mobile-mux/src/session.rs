//! One connection to one machine's mux server, as a state machine: the app
//! passes in what the channel delivered, sends what [`Session::take_outgoing`]
//! returns, and reacts to [`Event`]s.

use crate::frame::{self, Frame, FrameReader};
use crate::input::{PaneInput, Piece};
use crate::mirror::PaneMirror;
use crate::search::{ContextLine, SearchJob, SearchMatch, SearchQuery};
use crate::wire::{
    self, CodecVersion, Dialect, GetCodecVersion, GetLines, GetPaneRenderChanges, LinesResponse,
    ListPanes, LivenessResponse, MouseButton, MouseEvent, MouseEventKind, PaneList, PaneRemoved,
    RenderChanges, RenderHead, Resize, SearchResults, SearchScrollback, SendKeyDown,
    SendMouseEvent, SendPaste, SendPasteCurrent, StableRowIndex, TerminalSize, WriteToPane, ident,
};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use termwiz::surface::Line;

/// One pane of the machine's mux, in the order the server lists them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PaneInfo {
    pub pane_id: usize,
    pub tab_id: usize,
    pub window_id: usize,
    pub tab_title: String,
    pub title: String,
    pub workspace: String,
    pub working_dir: Option<String>,
    pub size: TerminalSize,
    pub is_active: bool,
    pub is_zoomed: bool,
    pub top_row: usize,
    pub left_col: usize,
    /// The stable row at the top of the pane's screen when listed.
    pub physical_top: StableRowIndex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The server speaks this client's protocol; the pane list follows.
    Ready {
        server_version: String,
    },
    /// The pane list, again whenever it may have changed.
    Panes(Vec<PaneInfo>),
    /// Bytes for the pane's terminal, which must be `rows` by `cols` first.
    Output {
        pane_id: usize,
        rows: usize,
        cols: usize,
        bytes: Vec<u8>,
    },
    PaneTitle {
        pane_id: usize,
        title: String,
    },
    PaneGone {
        pane_id: usize,
    },
    /// The answer to [`Session::search`]: the newest matches, newest first,
    /// over the newest `searched_rows` rows; or why there are none.
    SearchResults {
        search: u64,
        pane_id: usize,
        matches: Vec<SearchMatch>,
        searched_rows: usize,
        error: Option<String>,
    },
    /// The answer to [`Session::context`]: the asked-for rows that exist,
    /// in order.
    Context {
        request: u64,
        pane_id: usize,
        lines: Vec<ContextLine>,
    },
    /// The phone's size (`rows` by `cols`) is now in effect for the pane and
    /// `desktop` is the size to give back; or, with no `desktop`, it no longer
    /// is. Kept across connections, it lets the next session give a pane its
    /// size back when this one ended without doing so
    /// ([`Session::adopt_phone_size`]).
    PhoneSize {
        pane_id: usize,
        desktop: Option<TerminalSize>,
        rows: usize,
        cols: usize,
    },
    /// The pane's program asked for the mouse, or stopped: while it has it,
    /// a tap is a click ([`Session::click`]).
    MouseGrab {
        pane_id: usize,
        grabbed: bool,
    },
    /// The session is over; close the channel.
    Failed {
        reason: String,
    },
}

#[derive(Debug)]
enum Request {
    Version,
    ListPanes,
    Subscribe {
        pane_id: usize,
    },
    Lines {
        pane_id: usize,
        ranges: Vec<Range<StableRowIndex>>,
    },
    /// A resize of a shown pane, whose drawing waits for the answer.
    Resize {
        pane_id: usize,
    },
    /// Asks for the pane's changes once a resize is answered, so its new
    /// size arrives even when the program has not redrawn.
    Poll {
        pane_id: usize,
    },
    /// One range of a search walking back through a pane's scrollback.
    Search {
        search: u64,
    },
    /// The text of a finished search's matched rows.
    SearchLines {
        search: u64,
    },
    Context {
        request: u64,
        pane_id: usize,
        rows: Range<StableRowIndex>,
    },
    Action,
}

/// The most wheel notches one [`Session::wheel`] call sends.
pub const MAX_WHEEL_STEPS: u32 = 64;

/// The pane id that opens PDUs 25, 30 and 37.
#[derive(Deserialize)]
struct PanePrefix {
    pane_id: usize,
}

/// A pane's size as the phone found it, to give back when it looks away.
#[derive(Debug, Clone, Copy)]
struct PaneSizing {
    /// The size the desktop gave the pane.
    desktop: TerminalSize,
    /// While the phone's size may be in effect: until the restore with
    /// this serial is answered, a listed size may still be the phone's
    /// (`u64::MAX`: not restored yet).
    phone_until: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Session {
    next_serial: u64,
    reader: FrameReader,
    outgoing: Vec<u8>,
    ready: bool,
    dialect: Dialect,
    failed: bool,
    requests: HashMap<u64, Request>,
    panes: Vec<PaneInfo>,
    mirrors: HashMap<usize, PaneMirror>,
    sizing: HashMap<usize, PaneSizing>,
    /// Whether each mirrored pane's program has asked for the mouse.
    grabs: HashMap<usize, bool>,
    events: VecDeque<Event>,
    searches: HashMap<u64, SearchJob>,
    /// Searches waiting for a pane list asked for after they began (the
    /// serial before it), which says where each pane's newest row is.
    unstarted: Vec<(u64, u64)>,
    listing: bool,
    list_again: bool,
    next_lookup: u64,
    /// Incomplete paste state belongs to its original observed registration,
    /// never the most recently written or currently visible pane.
    input: PaneInput,
}

impl Session {
    /// A session whose first request, the protocol check, is already queued.
    #[must_use]
    pub fn new() -> Self {
        let mut session = Self::default();
        session.send(
            ident::GET_CODEC_VERSION,
            &GetCodecVersion {},
            Request::Version,
        );
        session
    }

    pub fn is_ready(&self) -> bool {
        self.ready && !self.failed
    }

    pub fn panes(&self) -> &[PaneInfo] {
        &self.panes
    }

    /// Bytes to write to the channel.
    pub fn take_outgoing(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outgoing)
    }

    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Bytes the channel delivered.
    pub fn receive(&mut self, bytes: &[u8]) {
        if self.failed {
            return;
        }
        self.reader.push(bytes);
        loop {
            match self.reader.next_frame() {
                Ok(Some(frame)) => {
                    if let Err(error) = self.handle(frame) {
                        self.fail(error.to_string());
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    self.fail(error.to_string());
                    return;
                }
            }
        }
    }

    /// The channel closed.
    pub fn closed(&mut self) {
        self.fail("the connection to the mux server closed".to_owned());
    }

    /// Asks for the pane list again; an [`Event::Panes`] answers. While one
    /// list is on its way, a second is asked for once it arrives.
    pub fn refresh_panes(&mut self) {
        if !self.is_ready() {
            return;
        }
        if self.listing {
            self.list_again = true;
        } else {
            self.listing = true;
            self.send(ident::LIST_PANES, &ListPanes {}, Request::ListPanes);
        }
    }

    /// Mirrors the pane onto its terminal, drawing it in full first.
    pub fn show(&mut self, pane_id: usize) {
        if !self.is_ready() {
            return;
        }
        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.show();
            self.advance(pane_id);
        } else {
            // The server tracks a pane for this connection from its first
            // GetPaneRenderChanges on, and pushes every later change.
            self.mirrors.insert(pane_id, PaneMirror::new(true));
            self.send(
                ident::GET_PANE_RENDER_CHANGES,
                &GetPaneRenderChanges { pane_id },
                Request::Subscribe { pane_id },
            );
        }
    }

    /// Stops drawing the pane; [`Session::show`] redraws it later.
    pub fn hide(&mut self, pane_id: usize) {
        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.hide();
        }
    }

    /// What the pane's terminal typed. Keys whose encoding depends on the
    /// program's modes go as key presses, bracketed pastes as pastes (see
    /// [`crate::input`]), the rest as the bytes they are, in order.
    pub fn write(&mut self, pane_id: usize, data: &[u8]) {
        if !self.is_ready() || data.is_empty() {
            return;
        }
        let pieces = match self.input.split(pane_id, data) {
            Ok(pieces) => pieces,
            Err(reason) => {
                // Running out of room, or the rest of a paste for a pane
                // that closed, ends the connection. Never flush an incomplete
                // command, emit a prefix from this call, or retry the bytes.
                self.fail(reason.to_owned());
                return;
            }
        };
        for piece in pieces {
            match piece {
                Piece::Bytes(data) => {
                    self.send(
                        ident::WRITE_TO_PANE,
                        &WriteToPane { pane_id, data },
                        Request::Action,
                    );
                }
                Piece::Key(event) => {
                    let press = SendKeyDown {
                        pane_id,
                        event,
                        input_serial: 0,
                    };
                    self.send(ident::SEND_KEY_DOWN, &press, Request::Action);
                }
                Piece::Paste(text) => self.send_paste(pane_id, &text),
            }
        }
    }

    /// Pasted text; the server brackets it when the pane's program asked.
    pub fn paste(&mut self, pane_id: usize, text: &str) {
        if !self.is_ready() || text.is_empty() {
            return;
        }
        if self.input.in_paste(pane_id) {
            self.fail("a direct paste cannot overtake unfinished input for its pane".to_owned());
            return;
        }
        self.send_paste(pane_id, text);
    }

    /// A splitter-produced piece is already ordered inside its destination's
    /// paste. The public typed-paste entry point must not jump ahead of it.
    fn send_paste(&mut self, pane_id: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.dialect {
            Dialect::Legacy => {
                let request = SendPaste {
                    pane_id,
                    data: text,
                };
                self.send(ident::SEND_PASTE, &request, Request::Action);
            }
            Dialect::Current => {
                let request = SendPasteCurrent {
                    pane_id,
                    data: text,
                    input_serial: 0,
                };
                self.send(ident::SEND_PASTE, &request, Request::Action);
            }
        }
    }

    /// Turns a mouse wheel `steps` notches over a cell of the pane's screen
    /// (negative: toward older output), at most [`MAX_WHEEL_STEPS`] a call.
    /// The server reports them to a program that asked for the mouse and
    /// sends a full-screen program arrow keys instead, as for the desktop's
    /// wheel; other programs ignore them.
    pub fn wheel(&mut self, pane_id: usize, steps: i32, column: usize, row: usize) {
        if !self.is_ready() || steps == 0 {
            return;
        }
        let button = if steps < 0 {
            MouseButton::WheelUp(1)
        } else {
            MouseButton::WheelDown(1)
        };
        let event = MouseEvent {
            kind: MouseEventKind::Press,
            x: column,
            y: i64::try_from(row).unwrap_or(i64::MAX),
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            button,
            modifiers: termwiz::input::Modifiers::NONE,
        };
        for _ in 0..steps.unsigned_abs().min(MAX_WHEEL_STEPS) {
            self.send(
                ident::SEND_MOUSE_EVENT,
                &SendMouseEvent { pane_id, event },
                Request::Action,
            );
        }
    }

    /// Clicks the left button on a cell of the pane's screen: a press and a
    /// release, which the server reports to a program that asked for the
    /// mouse (others ignore them).
    pub fn click(&mut self, pane_id: usize, column: usize, row: usize) {
        if !self.is_ready() {
            return;
        }
        for kind in [MouseEventKind::Press, MouseEventKind::Release] {
            let event = MouseEvent {
                kind,
                x: column,
                y: i64::try_from(row).unwrap_or(i64::MAX),
                x_pixel_offset: 0,
                y_pixel_offset: 0,
                button: MouseButton::Left,
                modifiers: termwiz::input::Modifiers::NONE,
            };
            self.send(
                ident::SEND_MOUSE_EVENT,
                &SendMouseEvent { pane_id, event },
                Request::Action,
            );
        }
    }

    /// Searches the pane's scrollback on the server, walking back from its
    /// newest rows; an [`Event::SearchResults`] with the returned id
    /// answers. The walk starts once a fresh pane list says where the
    /// pane's newest row is.
    pub fn search(&mut self, pane_id: usize, query: &SearchQuery) -> Option<u64> {
        if !self.is_ready() || query.text.is_empty() {
            return None;
        }
        self.next_lookup += 1;
        let search = self.next_lookup;
        self.searches
            .insert(search, SearchJob::new(pane_id, query.pattern()));
        self.unstarted.push((search, self.next_serial));
        self.refresh_panes();
        Some(search)
    }

    /// The text of the pane's `rows`; an [`Event::Context`] answers.
    pub fn context(&mut self, pane_id: usize, rows: Range<StableRowIndex>) -> Option<u64> {
        if !self.is_ready() || rows.is_empty() || rows.end - rows.start > 1000 {
            return None;
        }
        self.next_lookup += 1;
        let request = self.next_lookup;
        let lines = GetLines {
            pane_id,
            lines: vec![rows.clone()],
        };
        self.send(
            ident::GET_LINES,
            &lines,
            Request::Context {
                request,
                pane_id,
                rows,
            },
        );
        Some(request)
    }

    /// Starts the searches a pane list asked for at `serial` can place.
    fn start_searches(&mut self, serial: u64) {
        let (ready, waiting): (Vec<_>, Vec<_>) = self
            .unstarted
            .drain(..)
            .partition(|(_, after)| serial > *after);
        self.unstarted = waiting;
        for (search, _) in ready {
            let Some(job) = self.searches.get(&search) else {
                continue;
            };
            let pane_id = job.pane_id;
            let listed = self
                .panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .map(|pane| {
                    pane.physical_top
                        .saturating_add(StableRowIndex::try_from(pane.size.rows).unwrap_or(0))
                });
            let live = self.mirrors.get(&pane_id).and_then(PaneMirror::end_row);
            match live.max(listed) {
                Some(end) if listed.is_some() => {
                    if let Some(job) = self.searches.get_mut(&search) {
                        job.start(end);
                    }
                    self.continue_search(search);
                }
                _ => {
                    self.finish_search(
                        search,
                        &HashMap::new(),
                        Some("The pane has closed.".into()),
                    );
                }
            }
        }
    }

    /// Asks for the search's next rows, or the text of its matches.
    fn continue_search(&mut self, search: u64) {
        let Some(job) = self.searches.get_mut(&search) else {
            return;
        };
        if let Some(range) = job.next_request() {
            let request = SearchScrollback {
                pane_id: job.pane_id,
                pattern: job.pattern.clone(),
                range,
                limit: Some(u32::try_from(crate::search::WANTED).unwrap_or(u32::MAX)),
            };
            self.send(
                ident::SEARCH_SCROLLBACK,
                &request,
                Request::Search { search },
            );
            return;
        }
        let pane_id = job.pane_id;
        let rows = SearchJob::rows(&job.best());
        if rows.is_empty() {
            self.finish_search(search, &HashMap::new(), None);
        } else {
            let lines = GetLines {
                pane_id,
                lines: rows,
            };
            self.send(ident::GET_LINES, &lines, Request::SearchLines { search });
        }
    }

    fn finish_search(
        &mut self,
        search: u64,
        rows: &HashMap<StableRowIndex, RowText>,
        error: Option<String>,
    ) {
        let Some(job) = self.searches.remove(&search) else {
            return;
        };
        let matches = if error.is_some() {
            Vec::new()
        } else {
            SearchJob::matches(&job.best(), &|row| rows.get(&row))
        };
        self.events.push_back(Event::SearchResults {
            search,
            pane_id: job.pane_id,
            matches,
            searched_rows: job.searched_rows,
            error,
        });
    }

    /// Resizes the remote pane, remembering the size the desktop gave it so
    /// [`Session::restore_size`] can put that back.
    pub fn resize(&mut self, pane_id: usize, rows: usize, cols: usize) {
        if !self.is_ready() || rows == 0 || cols == 0 {
            return;
        }
        let Some(pane) = self.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        let tab_id = pane.tab_id;
        let sizing = self.sizing.entry(pane_id).or_insert(PaneSizing {
            desktop: pane.size,
            phone_until: None,
        });
        sizing.phone_until = Some(u64::MAX);
        let desktop = sizing.desktop;
        let size = TerminalSize {
            rows,
            cols,
            pixel_width: desktop.pixel_width / desktop.cols.max(1) * cols,
            pixel_height: desktop.pixel_height / desktop.rows.max(1) * rows,
            dpi: desktop.dpi,
        };
        let request = if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.expect_size(rows, cols);
            Request::Resize { pane_id }
        } else {
            Request::Action
        };
        self.send_resize(tab_id, pane_id, size, request);
        self.events.push_back(Event::PhoneSize {
            pane_id,
            desktop: Some(desktop),
            rows,
            cols,
        });
    }

    /// Takes over a pane an earlier session left at the phone's size
    /// (`rows` by `cols`, from its [`Event::PhoneSize`]) without giving the
    /// desktop's back, so [`Session::restore_size`] returns `desktop`. Only a
    /// pane still listed at exactly that size is taken over: one the desktop
    /// resized since, or another pane under a reused id, is left alone.
    pub fn adopt_phone_size(
        &mut self,
        pane_id: usize,
        desktop: TerminalSize,
        rows: usize,
        cols: usize,
    ) -> bool {
        let still_phone_sized = self.panes.iter().any(|pane| {
            pane.pane_id == pane_id && (pane.size.rows, pane.size.cols) == (rows, cols)
        });
        if !self.is_ready() || !still_phone_sized || (desktop.rows, desktop.cols) == (rows, cols) {
            return false;
        }
        self.sizing.insert(
            pane_id,
            PaneSizing {
                desktop,
                phone_until: Some(u64::MAX),
            },
        );
        true
    }

    /// Gives the pane back the size the desktop gave it, if the phone
    /// resized it.
    pub fn restore_size(&mut self, pane_id: usize) {
        if !self.is_ready() {
            return;
        }
        let Some(tab_id) = self
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .map(|pane| pane.tab_id)
        else {
            return;
        };
        let Some(sizing) = self.sizing.get(&pane_id).copied() else {
            return;
        };
        if sizing.phone_until != Some(u64::MAX) {
            return;
        }
        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.size_settled();
        }
        self.send_resize(tab_id, pane_id, sizing.desktop, Request::Action);
        let restored = self.next_serial;
        if let Some(sizing) = self.sizing.get_mut(&pane_id) {
            sizing.phone_until = Some(restored);
        }
        self.events.push_back(Event::PhoneSize {
            pane_id,
            desktop: None,
            rows: 0,
            cols: 0,
        });
    }

    fn send_resize(&mut self, tab_id: usize, pane_id: usize, size: TerminalSize, request: Request) {
        let resize = Resize {
            containing_tab_id: tab_id,
            pane_id,
            size,
        };
        self.send(ident::RESIZE, &resize, request);
    }

    fn send<T: serde::Serialize>(&mut self, pdu: u64, body: &T, request: Request) {
        self.next_serial += 1;
        let serial = self.next_serial;
        self.outgoing
            .extend_from_slice(&frame::encode(pdu, serial, &wire::encode(body)));
        self.requests.insert(serial, request);
    }

    fn fail(&mut self, reason: String) {
        if !self.failed {
            self.failed = true;
            self.input.clear();
            self.events.push_back(Event::Failed { reason });
        }
    }

    /// A new pane list, asked for with `serial`.
    fn listed(&mut self, list: &PaneList, serial: u64) {
        self.panes = pane_infos(list);
        let retained = self
            .input
            .retain(|pane_id| self.panes.iter().any(|pane| pane.pane_id == pane_id));
        if let Err(reason) = retained {
            self.fail(reason.to_owned());
            return;
        }
        // A listed size is the desktop's unless the phone's may still be in
        // effect: after a resize, until a list asked for after the restore.
        self.sizing
            .retain(|pane_id, _| self.panes.iter().any(|pane| pane.pane_id == *pane_id));
        for pane in &self.panes {
            let sizing = self.sizing.entry(pane.pane_id).or_insert(PaneSizing {
                desktop: pane.size,
                phone_until: None,
            });
            match sizing.phone_until {
                Some(until) if serial <= until => {}
                _ => {
                    sizing.desktop = pane.size;
                    sizing.phone_until = None;
                }
            }
        }
        self.events.push_back(Event::Panes(self.panes.clone()));
        self.start_searches(serial);
    }

    fn handle(&mut self, frame: Frame) -> Result<(), crate::Error> {
        let request = if frame.serial == 0 {
            None
        } else {
            self.requests.remove(&frame.serial)
        };
        if frame.oversized {
            // Only a search can draw a reply this large; fail just that one.
            return match request {
                Some(Request::Search { search } | Request::SearchLines { search }) => {
                    self.finish_search(
                        search,
                        &HashMap::new(),
                        Some("Too many matches to show. Try a longer search.".into()),
                    );
                    Ok(())
                }
                Some(Request::Context {
                    request, pane_id, ..
                }) => {
                    self.events.push_back(Event::Context {
                        request,
                        pane_id,
                        lines: Vec::new(),
                    });
                    Ok(())
                }
                _ => Err(crate::Error::Frame("a reply too large to read")),
            };
        }
        match frame.ident {
            ident::GET_CODEC_VERSION_RESPONSE => {
                let version: CodecVersion = wire::decode(&frame.body)?;
                if let Some(dialect) = Dialect::of(version.codec_vers) {
                    self.dialect = dialect;
                    self.ready = true;
                    self.events.push_back(Event::Ready {
                        server_version: version.version_string,
                    });
                    self.refresh_panes();
                } else {
                    self.fail(format!(
                        "the mux server speaks protocol {}, and this app speaks {} to {} and {} to {}",
                        version.codec_vers,
                        wire::LEGACY_VERSIONS.start(),
                        wire::LEGACY_VERSIONS.end(),
                        wire::CURRENT_VERSIONS.start(),
                        wire::CURRENT_VERSIONS.end()
                    ));
                }
            }
            ident::LIST_PANES_RESPONSE => {
                let list: PaneList = wire::decode(&frame.body)?;
                if matches!(request, Some(Request::ListPanes)) {
                    self.listing = false;
                }
                self.listed(&list, frame.serial);
                if self.list_again && !self.listing {
                    self.list_again = false;
                    self.refresh_panes();
                }
            }
            ident::GET_PANE_RENDER_CHANGES_RESPONSE => {
                let PanePrefix { pane_id } = wire::decode(&frame.body)?;
                if self.mirrors.contains_key(&pane_id) {
                    // A line this client cannot read (v46 image cells) costs
                    // that update's lines, not the connection.
                    let (head, lines) = match wire::decode::<RenderChanges>(&frame.body) {
                        Ok(changes) => (changes.head, Some(changes.bonus_lines)),
                        Err(_) => (wire::decode::<RenderHead>(&frame.body)?, None),
                    };
                    self.update_title(pane_id, &head.title);
                    let grabbed = head.mouse_grabbed;
                    if self.grabs.insert(pane_id, grabbed) != Some(grabbed) {
                        self.events.push_back(Event::MouseGrab { pane_id, grabbed });
                    }
                    if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                        mirror.apply_changes(head, lines)?;
                    }
                    self.advance(pane_id);
                }
            }
            ident::GET_LINES_RESPONSE => {
                let lines = wire::decode::<LinesResponse>(&frame.body)
                    .map(|reply| reply.lines)
                    .unwrap_or_default();
                match request {
                    Some(Request::Lines { pane_id, ranges }) => {
                        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                            mirror.apply_lines(lines, &ranges);
                        }
                        self.advance(pane_id);
                    }
                    Some(Request::SearchLines { search }) => {
                        let rows = row_texts(lines.lines);
                        self.finish_search(search, &rows, None);
                    }
                    Some(Request::Context {
                        request,
                        pane_id,
                        rows,
                    }) => {
                        // The server shifts a range that runs past either end
                        // of the pane; only the rows asked for are wanted.
                        let mut lines: Vec<ContextLine> = row_texts(lines.lines)
                            .into_iter()
                            .filter(|(row, _)| rows.contains(row))
                            .map(|(row, text)| ContextLine {
                                row,
                                text: text.text,
                            })
                            .collect();
                        lines.sort_by_key(|line| line.row);
                        self.events.push_back(Event::Context {
                            request,
                            pane_id,
                            lines,
                        });
                    }
                    _ => {}
                }
            }
            ident::SEARCH_SCROLLBACK_RESPONSE => {
                if let Some(Request::Search { search }) = request {
                    let SearchResults { results } = wire::decode(&frame.body)?;
                    if let Some(job) = self.searches.get_mut(&search) {
                        job.answer(results);
                    }
                    self.continue_search(search);
                }
            }
            ident::LIVENESS_RESPONSE => {
                let reply: LivenessResponse = wire::decode(&frame.body)?;
                if !reply.is_alive {
                    self.pane_gone(reply.pane_id);
                } else if let Some(Request::Poll { pane_id }) = request {
                    if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                        mirror.size_settled();
                    }
                    self.advance(pane_id);
                }
            }
            ident::UNIT_RESPONSE => {
                if let Some(Request::Resize { pane_id }) = request {
                    self.poll(pane_id);
                }
            }
            ident::PANE_REMOVED => {
                let PaneRemoved { pane_id } = wire::decode(&frame.body)?;
                self.pane_gone(pane_id);
                self.refresh_panes();
            }
            // A tab or split appeared or changed shape on the desktop.
            ident::TAB_RESIZED | ident::TAB_ADDED_TO_WINDOW => self.refresh_panes(),
            ident::ERROR_RESPONSE => {
                match request {
                    Some(Request::Version) => {
                        self.fail("the mux server refused the protocol check".to_owned());
                    }
                    Some(Request::ListPanes) => {
                        self.fail("the mux server could not list its panes".to_owned());
                    }
                    Some(Request::Subscribe { pane_id } | Request::Poll { pane_id }) => {
                        self.pane_gone(pane_id);
                    }
                    Some(Request::Resize { pane_id }) => self.poll(pane_id),
                    Some(Request::Lines { pane_id, ranges }) => {
                        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                            mirror.apply_lines(wire::LineEntries::default(), &ranges);
                        }
                        self.advance(pane_id);
                    }
                    // An invalid regular expression, or a pane that just closed.
                    // Only the legacy rejection carries the error's text.
                    Some(Request::Search { search } | Request::SearchLines { search }) => {
                        let text = match self.dialect {
                            Dialect::Legacy => wire::decode::<String>(&frame.body).ok(),
                            Dialect::Current => None,
                        };
                        let reason = text.map_or_else(
                        || "The mux server refused the search (is the regular expression valid?).".to_owned(),
                        |text| text.strip_prefix("Error: ").unwrap_or(&text).to_owned(),
                    );
                        self.finish_search(search, &HashMap::new(), Some(reason));
                    }
                    Some(Request::Context {
                        request, pane_id, ..
                    }) => {
                        self.events.push_back(Event::Context {
                            request,
                            pane_id,
                            lines: Vec::new(),
                        });
                    }
                    Some(Request::Action) | None => {}
                }
            }
            // Unit replies, palettes, alerts and title notices need nothing.
            _ => {}
        }
        Ok(())
    }

    fn poll(&mut self, pane_id: usize) {
        if self.mirrors.contains_key(&pane_id) {
            self.send(
                ident::GET_PANE_RENDER_CHANGES,
                &GetPaneRenderChanges { pane_id },
                Request::Poll { pane_id },
            );
        }
    }

    fn update_title(&mut self, pane_id: usize, title: &str) {
        let known = self
            .mirrors
            .get(&pane_id)
            .and_then(PaneMirror::title)
            .or_else(|| {
                self.panes
                    .iter()
                    .find(|pane| pane.pane_id == pane_id)
                    .map(|pane| pane.title.as_str())
            });
        if known != Some(title) {
            if let Some(pane) = self.panes.iter_mut().find(|pane| pane.pane_id == pane_id) {
                title.clone_into(&mut pane.title);
            }
            self.events.push_back(Event::PaneTitle {
                pane_id,
                title: title.to_owned(),
            });
        }
    }

    fn pane_gone(&mut self, pane_id: usize) {
        if let Err(reason) = self.input.remove(pane_id) {
            self.fail(reason.to_owned());
        }
        self.mirrors.remove(&pane_id);
        self.grabs.remove(&pane_id);
        let phone_sized = self
            .sizing
            .remove(&pane_id)
            .is_some_and(|sizing| sizing.phone_until.is_some());
        self.panes.retain(|pane| pane.pane_id != pane_id);
        self.events.push_back(Event::PaneGone { pane_id });
        if phone_sized {
            self.events.push_back(Event::PhoneSize {
                pane_id,
                desktop: None,
                rows: 0,
                cols: 0,
            });
        }
    }

    /// Fetches the rows the pane's mirror lacks, or draws it.
    fn advance(&mut self, pane_id: usize) {
        let Some(mirror) = self.mirrors.get_mut(&pane_id) else {
            return;
        };
        if mirror.is_fetching() {
            return;
        }
        let wanted = mirror.wanted();
        if !wanted.is_empty() {
            mirror.set_fetching();
            let request = GetLines {
                pane_id,
                lines: wanted.clone(),
            };
            self.send(
                ident::GET_LINES,
                &request,
                Request::Lines {
                    pane_id,
                    ranges: wanted,
                },
            );
        } else if let Some(output) = mirror.render() {
            self.events.push_back(Event::Output {
                pane_id,
                rows: output.rows,
                cols: output.cols,
                bytes: output.bytes,
            });
        }
    }
}

/// One row as text, trailing blanks dropped, and where each of its cells
/// starts in that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowText {
    pub text: String,
    /// (cell column, UTF-8 offset in `text`), by column.
    columns: Vec<(usize, usize)>,
}

impl RowText {
    fn of(line: &Line) -> Self {
        let mut text = String::new();
        let mut columns = Vec::new();
        for cell in line.visible_cells() {
            columns.push((cell.cell_index(), text.len()));
            text.push_str(cell.str());
        }
        let kept = text.trim_end().len();
        text.truncate(kept);
        Self { text, columns }
    }

    /// A row of one-cell characters.
    #[cfg(test)]
    pub fn plain(text: &str) -> Self {
        let cells: Vec<(String, usize)> = text
            .chars()
            .enumerate()
            .map(|(column, c)| (c.to_string(), column))
            .collect();
        let cells: Vec<(&str, usize)> = cells.iter().map(|(s, c)| (s.as_str(), *c)).collect();
        Self::wide(&cells)
    }

    /// A row of (text, starting column) cells.
    #[cfg(test)]
    pub fn wide(cells: &[(&str, usize)]) -> Self {
        let mut text = String::new();
        let mut columns = Vec::new();
        for (cell, column) in cells {
            columns.push((*column, text.len()));
            text.push_str(cell);
        }
        Self { text, columns }
    }

    /// The offset in `text` where the cell at `column` starts.
    pub fn offset(&self, column: usize) -> usize {
        self.columns
            .iter()
            .find(|(cell, _)| *cell >= column)
            .map_or(self.text.len(), |(_, offset)| *offset)
            .min(self.text.len())
    }
}

fn row_texts(lines: Vec<(StableRowIndex, Line)>) -> HashMap<StableRowIndex, RowText> {
    lines
        .into_iter()
        .map(|(row, line)| (row, RowText::of(&line)))
        .collect()
}

fn pane_infos(list: &PaneList) -> Vec<PaneInfo> {
    let mut infos = Vec::new();
    for (index, tab) in list.tabs.iter().enumerate() {
        let tab_title = list.tab_titles.get(index).cloned().unwrap_or_default();
        let mut entries = Vec::new();
        tab.leaves(&mut entries);
        infos.extend(entries.into_iter().map(|entry| PaneInfo {
            pane_id: entry.pane_id,
            tab_id: entry.tab_id,
            window_id: entry.window_id,
            tab_title: tab_title.clone(),
            title: entry.title.clone(),
            workspace: entry.workspace.clone(),
            working_dir: entry.working_dir.clone(),
            size: entry.size,
            is_active: entry.is_active_pane,
            is_zoomed: entry.is_zoomed_pane,
            top_row: entry.top_row,
            left_col: entry.left_col,
            physical_top: entry.physical_top,
        }));
    }
    infos
}

#[cfg(test)]
mod paste_routing_tests {
    use super::*;

    #[derive(Deserialize)]
    struct PasteBody {
        pane_id: usize,
        data: String,
    }

    #[derive(Deserialize)]
    struct BytesBody {
        pane_id: usize,
        data: Vec<u8>,
    }

    fn ready(dialect: Dialect) -> Session {
        Session {
            ready: true,
            dialect,
            ..Session::default()
        }
    }

    /// Decode the real outbound protocol bytes, not the splitter's pieces.
    /// The paste prefix is shared by both dialects; Current's serial follows it.
    fn writes(session: &mut Session) -> Vec<(u64, usize, Vec<u8>)> {
        let mut reader = FrameReader::default();
        reader.push(&session.take_outgoing());
        let mut result = Vec::new();
        while let Some(frame) = reader.next_frame().expect("valid encoded frame") {
            match frame.ident {
                ident::SEND_PASTE => {
                    let body: PasteBody = wire::decode(&frame.body).expect("valid paste");
                    result.push((frame.ident, body.pane_id, body.data.into_bytes()));
                }
                ident::WRITE_TO_PANE => {
                    let body: BytesBody = wire::decode(&frame.body).expect("valid input");
                    result.push((frame.ident, body.pane_id, body.data));
                }
                other => panic!("unexpected PDU {other}"),
            }
        }
        result
    }

    #[test]
    fn interleaved_pastes_remain_whole_and_bound_in_both_dialects() {
        for dialect in [Dialect::Legacy, Dialect::Current] {
            let mut session = ready(dialect);
            session.write(10, b"\x1b[200~first ");
            assert!(writes(&mut session).is_empty());
            session.write(20, b"x");
            assert_eq!(
                writes(&mut session),
                [(ident::WRITE_TO_PANE, 20, b"x".to_vec())]
            );
            session.write(20, b"\x1b[200~second ");
            assert!(writes(&mut session).is_empty());
            session.write(10, b"pane\x1b[201~");
            session.write(20, b"pane\x1b[201~");
            assert_eq!(
                writes(&mut session),
                [
                    (ident::SEND_PASTE, 10, b"first pane".to_vec()),
                    (ident::SEND_PASTE, 20, b"second pane".to_vec()),
                ]
            );
        }
    }

    #[test]
    fn hiding_a_pane_does_not_terminate_its_paste() {
        let mut session = ready(Dialect::Legacy);
        session.show(1);
        session.take_outgoing();
        session.write(1, b"\x1b[200~before ");
        session.hide(1);
        session.write(2, b"other");
        assert_eq!(
            writes(&mut session),
            [(ident::WRITE_TO_PANE, 2, b"other".to_vec())]
        );
        session.write(1, b"after\x1b[201~");
        assert_eq!(
            writes(&mut session),
            [(ident::SEND_PASTE, 1, b"before after".to_vec())]
        );
    }

    fn failed(session: &mut Session) -> bool {
        std::iter::from_fn(|| session.next_event())
            .any(|event| matches!(event, Event::Failed { .. }))
    }

    #[test]
    fn the_rest_of_a_removed_panes_paste_ends_the_connection_unsent() {
        let mut session = ready(Dialect::Current);
        session.write(1, b"\x1b[200~make ");
        session.write(2, b"\x1b[200~keep me");
        session.pane_gone(1);
        assert!(writes(&mut session).is_empty());
        // Another pane's paste is unaffected.
        session.write(2, b"\x1b[201~");
        assert_eq!(
            writes(&mut session),
            [(ident::SEND_PASTE, 2, b"keep me".to_vec())]
        );
        assert!(!failed(&mut session));
        // Pane numbers are never reused, so this is the rest of pane 1's
        // paste. Typed, its Return would run it.
        session.write(1, b"install\r\x1b[201~");
        assert!(writes(&mut session).is_empty());
        assert!(!session.is_ready());
        assert!(failed(&mut session));
    }

    #[test]
    fn inventory_removal_discards_a_paste_without_sending_it() {
        let mut session = ready(Dialect::Legacy);
        session.write(1, b"\x1b[200~make ");
        session.listed(
            &PaneList {
                tabs: Vec::new(),
                tab_titles: Vec::new(),
                window_titles: HashMap::new(),
            },
            1,
        );
        assert!(writes(&mut session).is_empty());
        assert!(session.is_ready());
        session.write(1, b"install\r\x1b[201~");
        assert!(writes(&mut session).is_empty());
        assert!(!session.is_ready());
        assert!(failed(&mut session));
    }

    #[test]
    fn close_and_capacity_failure_never_flush_pending_text() {
        let mut session = ready(Dialect::Legacy);
        session.write(1, b"\x1b[200~not a command yet");
        session.closed();
        assert!(writes(&mut session).is_empty());
        assert!(!session.is_ready());
        session.write(1, b"\x1b[201~");
        assert!(writes(&mut session).is_empty());

        let mut session = ready(Dialect::Current);
        for pane in 0..17 {
            session.write(pane, b"\x1b[200~unfinished");
        }
        assert!(!session.is_ready());
        assert!(writes(&mut session).is_empty());
        assert!(matches!(session.next_event(), Some(Event::Failed { .. })));
        assert!(session.next_event().is_none());
    }

    #[test]
    fn direct_paste_is_independent_but_cannot_overtake_its_own_partial_input() {
        for dialect in [Dialect::Legacy, Dialect::Current] {
            let mut session = ready(dialect);
            session.write(1, b"\x1b[200~first");
            session.paste(2, "independent");
            assert_eq!(
                writes(&mut session),
                [(ident::SEND_PASTE, 2, b"independent".to_vec()),]
            );
            session.paste(1, "must not overtake");
            assert!(!session.is_ready());
            assert!(writes(&mut session).is_empty());
            assert!(matches!(session.next_event(), Some(Event::Failed { .. })));
        }
    }
}
