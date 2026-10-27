//! One connection to one machine's mux server, as a state machine: the app
//! passes in what the channel delivered, sends what [`Session::take_outgoing`]
//! returns, and reacts to [`Event`]s.

use crate::frame::{self, Frame, FrameReader};
use crate::input::{PaneInput, Piece};
use crate::mirror::PaneMirror;
use crate::search::{ContextLine, SearchJob, SearchMatch, SearchQuery};
use crate::wire::{
    self, CodecVersion, CoherentOutcome, Dialect, GetCodecVersion, GetLines, GetPaneRenderChanges,
    LinesResponse, ListPanes, ListPanesCoherent, ListPanesCoherentResponse, LivenessResponse,
    MouseButton, MouseEvent, MouseEventKind, PaneList, PaneRemoved, RenderChanges, RenderHead,
    Resize, SearchResults, SearchScrollback, SendKeyDown, SendMouseEvent, SendPaste,
    SendPasteCurrent, StableRowIndex, TerminalSize, TopologyChange, TopologyEvent, WriteToPane,
    ident,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use termwiz::input::{KeyCode, KeyEvent, Modifiers};
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
    /// Names this run of the mux server: the same for as long as it runs,
    /// new after it restarts, even when it numbers its panes as before.
    /// Only current servers say; it comes before the first pane list.
    Incarnation {
        id: [u8; 16],
    },
    /// The pane list, again whenever it may have changed.
    Panes(Vec<PaneInfo>),
    /// Bytes for the pane's terminal, which must be `rows` by `cols` first.
    /// Fed apart from any other output: one may cost all the work the
    /// phone's terminal allows a piece of output.
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
    /// The phone's size (`rows` by `cols`) may be in effect for the pane and
    /// `desktop` is the size to give back. Kept across connections, it lets
    /// the next session restore a pane after a lost connection. An absent
    /// `desktop` is not a successful restoration receipt: only
    /// [`Event::PhoneSizeRestored`] or removal of the pane retires the record.
    PhoneSize {
        pane_id: usize,
        desktop: Option<TerminalSize>,
        rows: usize,
        cols: usize,
    },
    /// The server acknowledged the exact outstanding desktop-size restore,
    /// with no newer phone resize superseding it. This retires that pane's
    /// saved recovery record; merely queueing a resize does not.
    PhoneSizeRestored {
        pane_id: usize,
    },
    /// The pane's program asked for the mouse, or stopped: while it has it,
    /// a tap is a click ([`Session::click`]).
    MouseGrab {
        pane_id: usize,
        grabbed: bool,
    },
    /// A pane not on screen has new output: a hidden pane this session has
    /// shown changed what it shows, or one it never showed scrolled or moved
    /// its cursor between two pane lists. Not repeated while one is queued.
    Activity {
        pane_id: usize,
    },
    /// The session is over; close the channel.
    Failed {
        reason: String,
    },
}

#[derive(Debug)]
enum Request {
    Version,
    RegisterClient {
        server_version: String,
    },
    PaneIdentity {
        pane_id: usize,
    },
    PaneAuthority {
        pane_id: usize,
        input_serial: u64,
    },
    /// Exactly one ordered input operation can be outstanding. The queue
    /// retains its destination, registration and original payload until its
    /// complete receipt (including any remaining byte suffix) settles.
    OrderedInput {
        pane_id: usize,
        input_serial: u64,
    },
    ListPanes,
    /// The first pane list from a current server, with its incarnation.
    CoherentPanes,
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
    /// A desktop-size restore. Only its matching successful reply can retire
    /// a saved recovery record, and only while this restore is still current.
    RestoreSize {
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
    /// Typed, pasted, clicked or scrolled input for the pane; the server
    /// answers once it has written it to the pane.
    Input {
        pane_id: usize,
    },
    Action,
}

/// The most wheel notches one [`Session::wheel`] call sends.
pub const MAX_WHEEL_STEPS: u32 = 64;
const MAX_INPUT_CALL_BYTES: usize = 64 * 1024;
const MAX_QUEUED_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_QUEUED_INPUT_OPERATIONS: usize = 256;

#[derive(Debug, Clone, Copy)]
enum PaneAuthority {
    Pending(u64),
    Available([u8; 16]),
    Unavailable,
}

#[derive(Debug)]
enum OrderedEffect {
    Bytes { data: Vec<u8>, offset: usize },
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
}

impl OrderedEffect {
    fn charge(&self) -> usize {
        match self {
            Self::Bytes { data, .. } => data.capacity(),
            Self::Paste(text) => text.capacity(),
            Self::Key(_) | Self::Mouse(_) => 128,
        }
    }

    fn serials(&self) -> usize {
        match self {
            // A writer can accept one byte per reply. Reserve every possible
            // suffix serial before admitting the original bytes, so a later
            // key cannot have a lower serial than their final suffix.
            Self::Bytes { data, .. } => data.len(),
            _ => 1,
        }
    }
}

#[derive(Debug)]
struct OrderedInput {
    pane_id: usize,
    registration: [u8; 16],
    input_serial: u64,
    last_serial: u64,
    effect: OrderedEffect,
    in_flight: bool,
}

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
    /// Only a list requested after this serial may replace `desktop`.
    /// A successful restore advances it past every already-issued request,
    /// excluding delayed snapshots captured before its acknowledgement.
    desktop_list_after: u64,
}

#[derive(Debug, Default)]
pub struct Session {
    next_serial: u64,
    reader: FrameReader,
    outgoing: Vec<u8>,
    ready: bool,
    dialect: Dialect,
    codec_version: usize,
    incarnation: Option<[u8; 16]>,
    failed: bool,
    requests: HashMap<u64, Request>,
    panes: Vec<PaneInfo>,
    /// Where each pane's screen and cursor were at the last list: the only
    /// sign of output from a pane this session never showed (no pushes).
    listed_marks: HashMap<usize, ListedMark>,
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
    authorities: HashMap<usize, PaneAuthority>,
    next_input_serial: u64,
    ordered_input: VecDeque<OrderedInput>,
    ordered_input_bytes: usize,
    /// Payloads are released on failure, but their uncertain disposition is
    /// retained for the existing Swift journal/close path.
    interrupted_input: HashSet<usize>,
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
                    if self.failed {
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

    /// After a list arrives: the one asked for while it was on its way.
    fn list_again_if_asked(&mut self) {
        if self.list_again && !self.listing {
            self.list_again = false;
            self.refresh_panes();
        }
    }

    /// Mirrors the pane onto its terminal, drawing it in full first.
    pub fn show(&mut self, pane_id: usize) {
        if !self.is_ready() {
            return;
        }
        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.show();
            if !matches!(
                self.authorities.get(&pane_id),
                Some(PaneAuthority::Pending(_))
            ) {
                self.advance(pane_id);
            }
        } else {
            // The server tracks a pane for this connection from its first
            // GetPaneRenderChanges on, and pushes every later change.
            self.mirrors.insert(pane_id, PaneMirror::new(true));
            if self.dialect == Dialect::Current {
                self.capture_pane_authority(pane_id);
            } else {
                self.subscribe(pane_id);
            }
        }
    }

    fn subscribe(&mut self, pane_id: usize) {
        self.send(
            ident::GET_PANE_RENDER_CHANGES,
            &GetPaneRenderChanges { pane_id },
            Request::Subscribe { pane_id },
        );
    }

    /// Capture server authority before subscribing/rendering, never lazily
    /// when the first key arrives after a numeric pane could have been reused.
    /// Before v71, the no-effect reliable-key probe supplies only this live
    /// registration; it does not establish a durable pane or resume identity.
    fn capture_pane_authority(&mut self, pane_id: usize) {
        if self.codec_version >= wire::PANE_IDENTITY_MIN_VERSION {
            self.send(
                ident::GET_PANE_IDENTITY,
                &wire::GetPaneIdentity { pane_id },
                Request::PaneIdentity { pane_id },
            );
        } else {
            self.probe_pane_authority(pane_id);
            return;
        }
        if !self.failed {
            self.authorities
                .insert(pane_id, PaneAuthority::Pending(self.next_serial));
        }
    }

    fn probe_pane_authority(&mut self, pane_id: usize) {
        let Some(input_serial) = self.reserve_input_serials(1) else {
            return;
        };
        self.send(
            ident::RELIABLE_KEY_EVENT,
            &wire::ReliableKeyEvent {
                pane_id,
                pane_registration: None,
                event: KeyEvent {
                    key: KeyCode::Char('\0'),
                    modifiers: Modifiers::NONE,
                },
                input_serial,
                kind: wire::ReliableKeyEventKind::KeyDown,
            },
            Request::PaneAuthority {
                pane_id,
                input_serial,
            },
        );
        if !self.failed {
            self.authorities
                .insert(pane_id, PaneAuthority::Pending(self.next_serial));
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
        if data.len() > MAX_INPUT_CALL_BYTES {
            self.interrupted_input.insert(pane_id);
            self.fail("mux input exceeds its bounded admission size".to_owned());
            return;
        }
        let pending_panes: Vec<_> = self.input.pending_panes().collect();
        let pieces = match self.input.split(pane_id, data) {
            Ok(pieces) => pieces,
            Err(reason) => {
                // Running out of room, or the rest of a paste for a pane
                // that closed, ends the connection. Never flush an incomplete
                // command, emit a prefix from this call, or retry the bytes.
                self.interrupted_input.extend(pending_panes);
                self.interrupted_input.insert(pane_id);
                self.fail(reason.to_owned());
                return;
            }
        };
        for piece in pieces {
            if self.dialect == Dialect::Current {
                match piece {
                    Piece::Bytes(data) => {
                        if self.codec_version < wire::RELIABLE_WRITE_MIN_VERSION {
                            self.interrupted_input.insert(pane_id);
                            self.fail("this mux protocol cannot provide reliable byte input; update the server".to_owned());
                            return;
                        }
                        for chunk in data.chunks(wire::MAX_RELIABLE_WRITE_BYTES) {
                            if !self.enqueue_input(
                                pane_id,
                                OrderedEffect::Bytes {
                                    data: chunk.to_vec(),
                                    offset: 0,
                                },
                            ) {
                                return;
                            }
                        }
                    }
                    Piece::Key(event) => {
                        if !self.enqueue_input(pane_id, OrderedEffect::Key(event)) {
                            return;
                        }
                    }
                    Piece::Paste(text) => {
                        if !self.enqueue_input(pane_id, OrderedEffect::Paste(text)) {
                            return;
                        }
                    }
                }
                continue;
            }
            match piece {
                Piece::Bytes(data) => {
                    self.send(
                        ident::WRITE_TO_PANE,
                        &WriteToPane { pane_id, data },
                        Request::Input { pane_id },
                    );
                }
                Piece::Key(event) => {
                    let press = SendKeyDown {
                        pane_id,
                        event,
                        input_serial: 0,
                    };
                    self.send(ident::SEND_KEY_DOWN, &press, Request::Input { pane_id });
                }
                Piece::Paste(text) => self.send_paste(pane_id, &text),
            }
        }
        self.drive_input();
    }

    /// Pasted text; the server brackets it when the pane's program asked.
    pub fn paste(&mut self, pane_id: usize, text: &str) {
        if !self.is_ready() || text.is_empty() {
            return;
        }
        if self.input.in_paste(pane_id) {
            self.interrupted_input.insert(pane_id);
            self.fail("a direct paste cannot overtake unfinished input for its pane".to_owned());
            return;
        }
        if text.len() > MAX_QUEUED_INPUT_BYTES {
            self.interrupted_input.insert(pane_id);
            self.fail("mux paste exceeds its bounded admission size".to_owned());
            return;
        }
        if self.dialect == Dialect::Current {
            if self.enqueue_input(pane_id, OrderedEffect::Paste(text.to_owned())) {
                self.drive_input();
            }
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
                self.send(ident::SEND_PASTE, &request, Request::Input { pane_id });
            }
            Dialect::Current => {
                let request = SendPasteCurrent {
                    pane_id,
                    data: text,
                    input_serial: 0,
                };
                self.send(ident::SEND_PASTE, &request, Request::Input { pane_id });
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
        if self.input.in_paste(pane_id) {
            self.fail("mouse input cannot overtake unfinished input for its pane".to_owned());
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
            if self.dialect == Dialect::Current {
                if !self.enqueue_input(pane_id, OrderedEffect::Mouse(event)) {
                    return;
                }
                continue;
            }
            self.send(
                ident::SEND_MOUSE_EVENT,
                &SendMouseEvent { pane_id, event },
                Request::Input { pane_id },
            );
        }
        self.drive_input();
    }

    /// Clicks the left button on a cell of the pane's screen: a press and a
    /// release, which the server reports to a program that asked for the
    /// mouse (others ignore them).
    pub fn click(&mut self, pane_id: usize, column: usize, row: usize) {
        if !self.is_ready() {
            return;
        }
        if self.input.in_paste(pane_id) {
            self.fail("mouse input cannot overtake unfinished input for its pane".to_owned());
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
            if self.dialect == Dialect::Current {
                if !self.enqueue_input(pane_id, OrderedEffect::Mouse(event)) {
                    return;
                }
                continue;
            }
            self.send(
                ident::SEND_MOUSE_EVENT,
                &SendMouseEvent { pane_id, event },
                Request::Input { pane_id },
            );
        }
        self.drive_input();
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
            desktop_list_after: 0,
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
                desktop_list_after: 0,
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
        self.send_resize(
            tab_id,
            pane_id,
            sizing.desktop,
            Request::RestoreSize { pane_id },
        );
        let restored = self.next_serial;
        if let Some(sizing) = self.sizing.get_mut(&pane_id) {
            sizing.phone_until = Some(restored);
        }
    }

    /// Whether a size [`Session::restore_size`] gave back is still
    /// unanswered, so the server may not have applied it. Closing the
    /// connection now can lose it on the way: a relay like `nc -U` drops
    /// what it has not forwarded when its channel closes.
    pub fn restoring(&self) -> bool {
        !self.failed
            && self.sizing.values().any(|sizing| {
                matches!(sizing.phone_until,
                    Some(serial) if serial != u64::MAX && self.requests.contains_key(&serial))
            })
    }

    /// Whether input sent to the pane ([`Session::write`], [`Session::paste`],
    /// [`Session::click`], [`Session::wheel`]) is still unanswered, so the
    /// server may not have written it to the pane. Once the connection has
    /// failed, true means input did not settle with a validated receipt. A
    /// malformed or unrelated reply never turns uncertainty into confirmation.
    pub fn unanswered_input(&self, pane_id: usize) -> bool {
        self.interrupted_input.contains(&pane_id)
            || self
                .ordered_input
                .iter()
                .any(|input| input.pane_id == pane_id)
            || self.input.in_paste(pane_id)
            || self
                .requests
                .values()
                .any(|request| matches!(request, Request::Input { pane_id: id } if *id == pane_id))
    }

    fn reserve_input_serials(&mut self, count: usize) -> Option<u64> {
        let Some(last) = u64::try_from(count)
            .ok()
            .filter(|count| *count != 0)
            .and_then(|count| self.next_input_serial.checked_add(count))
        else {
            self.fail("mux input serial space exhausted".to_owned());
            return None;
        };
        let first = self.next_input_serial + 1;
        self.next_input_serial = last;
        Some(first)
    }

    fn enqueue_input(&mut self, pane_id: usize, effect: OrderedEffect) -> bool {
        if self.failed {
            return false;
        }
        let Some(PaneAuthority::Available(registration)) = self.authorities.get(&pane_id).copied()
        else {
            self.interrupted_input.insert(pane_id);
            self.fail("the displayed mux pane has no current server input authority".to_owned());
            return false;
        };
        let charge = effect.charge();
        if self.ordered_input.len() >= MAX_QUEUED_INPUT_OPERATIONS
            || charge > MAX_QUEUED_INPUT_BYTES.saturating_sub(self.ordered_input_bytes)
        {
            self.interrupted_input.insert(pane_id);
            self.fail("mux input queue capacity exceeded".to_owned());
            return false;
        }
        let count = effect.serials();
        let Some(input_serial) = self.reserve_input_serials(count) else {
            self.interrupted_input.insert(pane_id);
            return false;
        };
        self.ordered_input_bytes += charge;
        self.ordered_input.push_back(OrderedInput {
            pane_id,
            registration,
            input_serial,
            last_serial: self.next_input_serial,
            effect,
            in_flight: false,
        });
        true
    }

    /// All current-dialect effects share this order. Paste and mouse retain
    /// their existing wire semantics (neither is a server controller lease),
    /// but cannot pass an unacknowledged key or the tail of a partial write.
    fn drive_input(&mut self) {
        if !self.is_ready() {
            return;
        }
        let Some(input) = self.ordered_input.front_mut() else {
            return;
        };
        if input.in_flight {
            return;
        }
        if !matches!(self.authorities.get(&input.pane_id),
            Some(PaneAuthority::Available(registration)) if *registration == input.registration)
        {
            self.fail("the original mux input registration is no longer available".to_owned());
            return;
        }
        let pane_id = input.pane_id;
        let input_serial = input.input_serial;
        let (ident, body) = match &input.effect {
            OrderedEffect::Bytes { data, offset } => (
                ident::RELIABLE_PANE_WRITE,
                wire::encode(&wire::ReliablePaneWrite {
                    pane_id,
                    pane_registration: Some(input.registration),
                    input_serial,
                    data: &data[*offset..],
                }),
            ),
            OrderedEffect::Key(event) => (
                ident::RELIABLE_KEY_EVENT,
                wire::encode(&wire::ReliableKeyEvent {
                    pane_id,
                    pane_registration: Some(input.registration),
                    event: event.clone(),
                    input_serial,
                    kind: wire::ReliableKeyEventKind::KeyDown,
                }),
            ),
            OrderedEffect::Paste(text) => (
                ident::SEND_PASTE,
                wire::encode(&SendPasteCurrent {
                    pane_id,
                    data: text,
                    input_serial,
                }),
            ),
            OrderedEffect::Mouse(event) => (
                ident::SEND_MOUSE_EVENT,
                wire::encode(&SendMouseEvent {
                    pane_id,
                    event: *event,
                }),
            ),
        };
        input.in_flight = true;
        self.send_encoded(
            ident,
            &body,
            Request::OrderedInput {
                pane_id,
                input_serial,
            },
        );
    }

    fn finish_input(&mut self) {
        if let Some(input) = self.ordered_input.pop_front() {
            self.ordered_input_bytes -= input.effect.charge();
        }
        self.drive_input();
    }

    fn settle_key(&mut self, response: wire::ReliableKeyResponse) -> Result<(), crate::Error> {
        let Some(input) = self.ordered_input.front() else {
            return Err(crate::Error::Schema);
        };
        if !input.in_flight
            || input.pane_id != response.pane_id
            || input.input_serial != response.input_serial
            || !matches!(input.effect, OrderedEffect::Key(_))
        {
            return Err(crate::Error::Frame(
                "reliable key receipt does not match its operation",
            ));
        }
        match response.outcome {
            wire::ReliableKeyOutcome::Applied | wire::ReliableKeyOutcome::DuplicateApplied => {
                self.finish_input()
            }
            // No-effect pressure can be retried by a future explicitly timed
            // driver. This client never ignores its delay, changes its target,
            // or interprets a rejection as permission for a legacy send.
            wire::ReliableKeyOutcome::Retry(_) | wire::ReliableKeyOutcome::Rejected(_) => {
                self.fail("the mux refused reliable key input; input was not replayed".to_owned());
            }
        }
        Ok(())
    }

    fn settle_write(&mut self, response: wire::ReliableWriteResponse) -> Result<(), crate::Error> {
        let Some(input) = self.ordered_input.front_mut() else {
            return Err(crate::Error::Schema);
        };
        if !input.in_flight
            || input.pane_id != response.pane_id
            || input.input_serial != response.input_serial
        {
            return Err(crate::Error::Frame(
                "reliable write receipt does not match its operation",
            ));
        }
        let OrderedEffect::Bytes { data, offset } = &mut input.effect else {
            return Err(crate::Error::Schema);
        };
        match response.outcome {
            wire::ReliableWriteOutcome::AppliedPrefix { bytes }
            | wire::ReliableWriteOutcome::DuplicateAppliedPrefix { bytes } => {
                let bytes = usize::try_from(bytes).map_err(|_| crate::Error::Schema)?;
                if bytes == 0 || bytes > data.len() - *offset {
                    return Err(crate::Error::Frame(
                        "reliable write receipt has an invalid applied prefix",
                    ));
                }
                *offset += bytes;
                if *offset == data.len() {
                    self.finish_input();
                } else {
                    if input.input_serial >= input.last_serial {
                        return Err(crate::Error::Frame(
                            "reliable write suffix serials exhausted",
                        ));
                    }
                    input.input_serial += 1;
                    input.in_flight = false;
                    self.drive_input();
                }
            }
            wire::ReliableWriteOutcome::Retry(_)
            | wire::ReliableWriteOutcome::Rejected(_)
            | wire::ReliableWriteOutcome::Indeterminate => {
                self.fail(
                    "the mux did not confirm reliable byte input; input was not replayed"
                        .to_owned(),
                );
            }
        }
        Ok(())
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
        self.send_encoded(pdu, &wire::encode(body), request);
    }

    fn send_encoded(&mut self, pdu: u64, body: &[u8], request: Request) {
        if self.failed {
            return;
        }
        let Some(serial) = self.next_serial.checked_add(1) else {
            self.fail("mux request serial space exhausted".to_owned());
            return;
        };
        self.next_serial = serial;
        self.outgoing
            .extend_from_slice(&frame::encode(pdu, serial, body));
        self.requests.insert(serial, request);
    }

    fn fail(&mut self, reason: String) {
        if !self.failed {
            self.failed = true;
            self.interrupted_input
                .extend(self.ordered_input.iter().map(|input| input.pane_id));
            self.interrupted_input.extend(self.input.pending_panes());
            self.ordered_input.clear();
            self.ordered_input_bytes = 0;
            self.outgoing.clear();
            self.input.clear();
            self.events.push_back(Event::Failed { reason });
        }
    }

    /// A new pane list, asked for with `serial`.
    fn listed(&mut self, list: &PaneList, serial: u64) {
        self.panes = pane_infos(list);
        if self
            .ordered_input
            .iter()
            .any(|input| !self.panes.iter().any(|pane| pane.pane_id == input.pane_id))
        {
            self.fail(
                "a pane closed while its input was pending; input was not redirected".to_owned(),
            );
            return;
        }
        // An inventory removal retires the observed generation just like an
        // explicit topology notice. A later same-number pane must start with
        // a new mirror, authority query and set of display requests.
        let missing: Vec<_> = self
            .mirrors
            .keys()
            .copied()
            .filter(|pane_id| !self.panes.iter().any(|pane| pane.pane_id == *pane_id))
            .collect();
        for pane_id in missing {
            self.pane_gone(pane_id);
        }
        self.authorities
            .retain(|pane_id, _| self.panes.iter().any(|pane| pane.pane_id == *pane_id));
        let marks = listed_marks(list);
        for pane_id in self.input.pending_panes() {
            if !self.panes.iter().any(|pane| pane.pane_id == pane_id) {
                self.interrupted_input.insert(pane_id);
            }
        }
        let pending_panes: Vec<_> = self.input.pending_panes().collect();
        let retained = self
            .input
            .retain(|pane_id| self.panes.iter().any(|pane| pane.pane_id == pane_id));
        if let Err(reason) = retained {
            self.interrupted_input.extend(pending_panes);
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
                desktop_list_after: 0,
            });
            if serial <= sizing.desktop_list_after {
                continue;
            }
            match sizing.phone_until {
                Some(until) if serial <= until || self.requests.contains_key(&until) => {}
                _ => {
                    sizing.desktop = pane.size;
                    sizing.phone_until = None;
                    sizing.desktop_list_after = serial;
                }
            }
        }
        self.events.push_back(Event::Panes(self.panes.clone()));
        // A new size is the program redrawing (perhaps for a size this phone
        // gave back), not output; the next list compares from there.
        for (pane_id, mark) in &marks {
            if !self.mirrors.contains_key(pane_id)
                && self.listed_marks.get(pane_id).is_some_and(|old| {
                    old.size == mark.size && (old.top != mark.top || old.cursor != mark.cursor)
                })
            {
                self.note_activity(*pane_id);
            }
        }
        self.listed_marks = marks;
        self.start_searches(serial);
    }

    fn note_activity(&mut self, pane_id: usize) {
        let queued = self.events.iter().any(
            |event| matches!(event, Event::Activity { pane_id: queued } if *queued == pane_id),
        );
        if !queued {
            self.events.push_back(Event::Activity { pane_id });
        }
    }

    fn handle(&mut self, frame: Frame) -> Result<(), crate::Error> {
        let request = if frame.serial == 0 {
            None
        } else {
            self.requests.remove(&frame.serial)
        };
        if frame.serial != 0 && request.is_none() {
            // Retired requests may finish after pane removal/reselection.
            // Their numeric pane IDs no longer identify the displayed visit.
            // An unmatched receipt also cannot settle any current operation.
            return Ok(());
        }
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
        if let Some(Request::OrderedInput {
            pane_id,
            input_serial,
        }) = &request
        {
            let Some(input) = self.ordered_input.front() else {
                return Err(crate::Error::Schema);
            };
            if input.pane_id != *pane_id || input.input_serial != *input_serial || !input.in_flight
            {
                return Err(crate::Error::Frame(
                    "an input reply does not match the outstanding request",
                ));
            }
            return match (&input.effect, frame.ident) {
                (OrderedEffect::Bytes { .. }, ident::RELIABLE_PANE_WRITE_RESPONSE) => {
                    self.settle_write(wire::decode_input_receipt(&frame.body)?)
                }
                (OrderedEffect::Key(_), ident::RELIABLE_KEY_EVENT_RESPONSE) => {
                    self.settle_key(wire::decode_input_receipt(&frame.body)?)
                }
                (OrderedEffect::Paste(_) | OrderedEffect::Mouse(_), ident::UNIT_RESPONSE)
                    if frame.body.is_empty() =>
                {
                    self.finish_input();
                    Ok(())
                }
                (_, ident::ERROR_RESPONSE) => {
                    self.fail("the mux rejected input without a matching effect receipt; input was not replayed".to_owned());
                    Ok(())
                }
                _ => Err(crate::Error::Frame(
                    "an input request received an unexpected reply",
                )),
            };
        }
        if let Some(Request::PaneIdentity { pane_id } | Request::PaneAuthority { pane_id, .. }) =
            &request
        {
            let pane_id = *pane_id;
            if !matches!(self.authorities.get(&pane_id), Some(PaneAuthority::Pending(serial)) if *serial == frame.serial)
            {
                // This observed pane was removed while its query was pending.
                // A later same-number selection owns its own query, not this one.
                return Ok(());
            }
            let registration = match (&request, frame.ident) {
                (Some(Request::PaneIdentity { .. }), ident::GET_PANE_IDENTITY_RESPONSE) => {
                    if frame.body.len() > wire::MAX_PANE_IDENTITY_RESPONSE_BYTES {
                        return Err(crate::Error::Frame(
                            "a pane identity reply exceeds its protocol bound",
                        ));
                    }
                    let response: wire::PaneIdentityResponse = wire::decode(&frame.body)?;
                    if response.pane_id != pane_id {
                        return Err(crate::Error::Schema);
                    }
                    match response.outcome {
                        wire::PaneIdentityOutcome::Available { identity } => {
                            if identity.session_incarnation == [0; 16]
                                || identity.durable_pane_id == [0; 16]
                                || identity.pane_registration == [0; 16]
                                || self
                                    .incarnation
                                    .is_some_and(|current| current != identity.session_incarnation)
                            {
                                return Err(crate::Error::Frame(
                                    "a pane identity does not match the current mux",
                                ));
                            }
                            Some(identity.pane_registration)
                        }
                        wire::PaneIdentityOutcome::PaneGone => {
                            self.pane_gone(pane_id);
                            return Ok(());
                        }
                        wire::PaneIdentityOutcome::Unavailable => {
                            // A remote/proxy pane may lack durable identity
                            // while still owning a live input registration.
                            // Probe before drawing; this grants no resume or
                            // durable artifact identity and applies no key.
                            self.probe_pane_authority(pane_id);
                            return Ok(());
                        }
                    }
                }
                (
                    Some(Request::PaneAuthority { input_serial, .. }),
                    ident::RELIABLE_KEY_EVENT_RESPONSE,
                ) => {
                    let response: wire::ReliableKeyResponse =
                        wire::decode_input_receipt(&frame.body)?;
                    if response.pane_id != pane_id || response.input_serial != *input_serial {
                        return Err(crate::Error::Schema);
                    }
                    match response.outcome {
                        wire::ReliableKeyOutcome::Retry(
                            wire::ReliableKeyRetry::PaneAuthorityRequired { pane_registration },
                        ) if pane_registration != [0; 16] => Some(pane_registration),
                        wire::ReliableKeyOutcome::Rejected(_) => None,
                        // An authority probe cannot apply a key or authorize an
                        // arbitrary retry. Treat either claim as a protocol fault.
                        _ => {
                            return Err(crate::Error::Frame(
                                "a no-effect pane probe returned invalid authority",
                            ));
                        }
                    }
                }
                (_, ident::ERROR_RESPONSE) => None,
                _ => {
                    return Err(crate::Error::Frame(
                        "a pane authority query received an unexpected reply",
                    ));
                }
            };
            self.authorities.insert(
                pane_id,
                registration.map_or(PaneAuthority::Unavailable, PaneAuthority::Available),
            );
            self.subscribe(pane_id);
            self.advance(pane_id);
            return Ok(());
        }
        if matches!(
            frame.ident,
            ident::RELIABLE_KEY_EVENT_RESPONSE
                | ident::RELIABLE_PANE_WRITE_RESPONSE
                | ident::GET_PANE_IDENTITY_RESPONSE
        ) {
            return Err(crate::Error::Frame(
                "an unsolicited input authority receipt was received",
            ));
        }
        if matches!(&request, Some(Request::RegisterClient { .. }))
            && !matches!(frame.ident, ident::UNIT_RESPONSE | ident::ERROR_RESPONSE)
        {
            return Err(crate::Error::Frame(
                "client registration received an unexpected reply",
            ));
        }
        if matches!(&request, Some(Request::RestoreSize { .. }))
            && !matches!(frame.ident, ident::UNIT_RESPONSE | ident::ERROR_RESPONSE)
        {
            return Err(crate::Error::Frame(
                "a restore received an unexpected reply",
            ));
        }
        match frame.ident {
            ident::GET_CODEC_VERSION_RESPONSE => {
                if !matches!(request, Some(Request::Version)) {
                    return Err(crate::Error::Schema);
                }
                let version: CodecVersion = wire::decode(&frame.body)?;
                let min_supported = wire::decode::<wire::CodecWindow>(&frame.body)
                    .ok()
                    .map(|window| window.min_supported);
                if let Some(dialect) = Dialect::of(version.codec_vers, min_supported) {
                    self.dialect = dialect;
                    self.codec_version = version.codec_vers.min(*wire::CURRENT_VERSIONS.end());
                    if dialect == Dialect::Current {
                        self.send(
                            ident::SET_CLIENT_ID,
                            &wire::SetClientId {
                                client_id: wire::ClientId {
                                    hostname: format!(
                                        "frankenterm-mobile-{}",
                                        uuid::Uuid::new_v4()
                                    ),
                                    username: "mobile".to_owned(),
                                    pid: 0,
                                    epoch: 0,
                                    id: 1,
                                    ssh_auth_sock: None,
                                },
                                is_proxy: false,
                            },
                            Request::RegisterClient {
                                server_version: version.version_string,
                            },
                        );
                    } else {
                        self.become_ready(version.version_string);
                    }
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
                self.list_again_if_asked();
            }
            ident::LIST_PANES_COHERENT_RESPONSE => {
                let response: ListPanesCoherentResponse = wire::decode(&frame.body)?;
                self.listing = false;
                if let CoherentOutcome::Snapshot(snapshot) = response.outcome {
                    if snapshot.session_incarnation == [0; 16]
                        || self
                            .incarnation
                            .is_some_and(|current| current != snapshot.session_incarnation)
                    {
                        return Err(crate::Error::Frame(
                            "the mux incarnation changed on a live connection",
                        ));
                    }
                    self.incarnation = Some(snapshot.session_incarnation);
                    self.events.push_back(Event::Incarnation {
                        id: snapshot.session_incarnation,
                    });
                    self.listed(&snapshot.panes, frame.serial);
                    self.list_again_if_asked();
                } else {
                    // No incarnation this time; the plain list still serves.
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
                    if let Some(mirror) = self.mirrors.get_mut(&pane_id)
                        && mirror.apply_changes(head, lines)?
                    {
                        self.note_activity(pane_id);
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
                if !frame.body.is_empty() {
                    return Err(crate::Error::Frame("a unit reply contains unexpected data"));
                }
                match request {
                    Some(Request::RegisterClient { server_version }) => {
                        self.become_ready(server_version)
                    }
                    Some(Request::Resize { pane_id }) => self.poll(pane_id),
                    Some(Request::RestoreSize { pane_id }) => {
                        if let Some(sizing) = self.sizing.get_mut(&pane_id)
                            && sizing.phone_until == Some(frame.serial)
                        {
                            // A list issued before this acknowledgement may
                            // still arrive with the old phone-sized snapshot.
                            // Keep request identity and snapshot freshness as
                            // separate fences; neither stands in for the other.
                            sizing.desktop_list_after = self.next_serial;
                            self.events.push_back(Event::PhoneSizeRestored { pane_id });
                        }
                    }
                    _ => {}
                }
            }
            ident::PANE_REMOVED => {
                let PaneRemoved { pane_id } = wire::decode(&frame.body)?;
                self.pane_gone(pane_id);
                self.refresh_panes();
            }
            // A tab or split appeared or changed shape on the desktop.
            ident::TAB_RESIZED | ident::TAB_ADDED_TO_WINDOW => self.refresh_panes(),
            // The same changes from a server that answered a coherent list.
            ident::TOPOLOGY_EVENT => match wire::decode::<TopologyEvent>(&frame.body) {
                Ok(TopologyEvent {
                    event: TopologyChange::PaneRemoved { pane_id },
                    ..
                }) => {
                    self.pane_gone(pane_id);
                    self.refresh_panes();
                }
                Ok(TopologyEvent {
                    event:
                        TopologyChange::FloatingPaneSpawned { .. }
                        | TopologyChange::WindowInvalidated { .. }
                        | TopologyChange::TabAddedToWindow { .. }
                        | TopologyChange::TabResized { .. },
                    ..
                }) => self.refresh_panes(),
                Ok(_) => {}
                // A change this client does not know; the list says what it did.
                Err(_) => self.refresh_panes(),
            },
            ident::ERROR_RESPONSE => {
                match request {
                    Some(Request::RegisterClient { .. }) => {
                        self.fail(
                            "the mux refused this connection's input-ledger registration"
                                .to_owned(),
                        );
                    }
                    Some(Request::Version) => {
                        self.fail("the mux server refused the protocol check".to_owned());
                    }
                    Some(Request::ListPanes) => {
                        self.fail("the mux server could not list its panes".to_owned());
                    }
                    Some(Request::CoherentPanes) => {
                        self.listing = false;
                        self.list_again = false;
                        self.refresh_panes();
                    }
                    Some(Request::Subscribe { pane_id } | Request::Poll { pane_id }) => {
                        self.pane_gone(pane_id);
                    }
                    Some(Request::Resize { pane_id }) => self.poll(pane_id),
                    Some(Request::RestoreSize { pane_id }) => {
                        if let Some(sizing) = self.sizing.get_mut(&pane_id)
                            && sizing.phone_until == Some(frame.serial)
                        {
                            // The server did not restore it. Preserve the
                            // original desktop dimensions and permit a later
                            // explicit restore; no recovery receipt is emitted.
                            sizing.phone_until = Some(u64::MAX);
                        }
                    }
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
                    Some(Request::Input { .. } | Request::Action) | None => {}
                    Some(
                        Request::PaneIdentity { .. }
                        | Request::PaneAuthority { .. }
                        | Request::OrderedInput { .. },
                    ) => {
                        return Err(crate::Error::Schema);
                    }
                }
            }
            // Unit replies, palettes, alerts and title notices need nothing.
            _ => {}
        }
        Ok(())
    }

    fn become_ready(&mut self, server_version: String) {
        self.ready = true;
        self.events.push_back(Event::Ready { server_version });
        if self.dialect == Dialect::Current {
            self.listing = true;
            self.send(
                ident::LIST_PANES_COHERENT,
                &ListPanesCoherent {
                    supported: wire::FENCED_SNAPSHOT,
                    required: wire::FENCED_SNAPSHOT,
                },
                Request::CoherentPanes,
            );
        } else {
            self.refresh_panes();
        }
    }

    fn poll(&mut self, pane_id: usize) {
        if matches!(
            self.authorities.get(&pane_id),
            Some(PaneAuthority::Pending(_))
        ) {
            // A resize can settle while the initial identity query is on its
            // way. The eventual subscription will fetch the current size.
            if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                mirror.size_settled();
            }
            return;
        }
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
        self.authorities.remove(&pane_id);
        self.requests.retain(|_, request| {
            !matches!(request,
                Request::PaneIdentity { pane_id: old }
                | Request::PaneAuthority { pane_id: old, .. }
                | Request::Subscribe { pane_id: old }
                | Request::Lines { pane_id: old, .. }
                | Request::Resize { pane_id: old }
                | Request::RestoreSize { pane_id: old }
                | Request::Poll { pane_id: old } if *old == pane_id
            )
        });
        if self.input.in_paste(pane_id) {
            self.interrupted_input.insert(pane_id);
        }
        if self
            .ordered_input
            .iter()
            .any(|input| input.pane_id == pane_id)
        {
            self.fail(
                "a pane closed while its input was pending; input was not redirected".to_owned(),
            );
        }
        let pending_panes: Vec<_> = self.input.pending_panes().collect();
        if let Err(reason) = self.input.remove(pane_id) {
            self.interrupted_input.extend(pending_panes);
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
        if matches!(
            self.authorities.get(&pane_id),
            Some(PaneAuthority::Pending(_))
        ) {
            return;
        }
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
        } else {
            for output in mirror.render() {
                self.events.push_back(Event::Output {
                    pane_id,
                    rows: output.rows,
                    cols: output.cols,
                    bytes: output.bytes,
                });
            }
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

/// What a pane list says about a pane's output: its screen's top row (which
/// moves as output scrolls), its cursor, and the size they are measured at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListedMark {
    top: StableRowIndex,
    cursor: (usize, StableRowIndex),
    size: (usize, usize),
}

fn listed_marks(list: &PaneList) -> HashMap<usize, ListedMark> {
    let mut entries = Vec::new();
    for tab in &list.tabs {
        tab.leaves(&mut entries);
    }
    entries
        .into_iter()
        .map(|entry| {
            let mark = ListedMark {
                top: entry.physical_top,
                cursor: (entry.cursor_pos.x, entry.cursor_pos.y),
                size: (entry.size.rows, entry.size.cols),
            };
            (entry.pane_id, mark)
        })
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
            codec_version: *wire::CURRENT_VERSIONS.end(),
            authorities: (0..=20)
                .map(|pane| (pane, PaneAuthority::Available([7; 16])))
                .collect(),
            ..Session::default()
        }
    }

    /// Decode the real outbound protocol bytes, not the splitter's pieces.
    /// The paste prefix is shared by both dialects; Current's serial follows it.
    fn writes(session: &mut Session) -> Vec<(u64, usize, Vec<u8>)> {
        let mut reader = FrameReader::default();
        reader.push(&session.take_outgoing());
        let mut result = Vec::new();
        loop {
            let Some(frame) = reader.next_frame().expect("valid encoded frame") else {
                let outgoing = session.take_outgoing();
                if outgoing.is_empty() {
                    break;
                }
                reader.push(&outgoing);
                continue;
            };
            match frame.ident {
                ident::SEND_PASTE => {
                    let body: PasteBody = wire::decode(&frame.body).expect("valid paste");
                    result.push((frame.ident, body.pane_id, body.data.into_bytes()));
                    if session.dialect == Dialect::Current {
                        session.receive(&frame::encode(ident::UNIT_RESPONSE, frame.serial, &[]));
                    }
                }
                ident::WRITE_TO_PANE => {
                    let body: BytesBody = wire::decode(&frame.body).expect("valid input");
                    result.push((frame.ident, body.pane_id, body.data));
                }
                ident::RELIABLE_PANE_WRITE => {
                    let body: codec::ReliablePaneWriteV1 =
                        wire::decode(&frame.body).expect("valid reliable input");
                    let response = codec::ReliablePaneWriteV1Response {
                        pane_id: body.pane_id,
                        input_serial: body.input_serial,
                        outcome: codec::ReliablePaneWriteOutcomeV1::AppliedPrefix {
                            bytes: body.data.len() as u32,
                        },
                    };
                    result.push((frame.ident, body.pane_id, body.data));
                    session.receive(&frame::encode(
                        ident::RELIABLE_PANE_WRITE_RESPONSE,
                        frame.serial,
                        &wire::encode(&response),
                    ));
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
                [(
                    if dialect == Dialect::Current {
                        ident::RELIABLE_PANE_WRITE
                    } else {
                        ident::WRITE_TO_PANE
                    },
                    20,
                    b"x".to_vec()
                )]
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
        // A removed pane's incomplete paste remains abandoned even if its
        // numeric ID is later reused. Typed, its Return would run it.
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
        assert!(session.unanswered_input(0));
        assert!(session.unanswered_input(16));
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

    #[test]
    fn byte_suffix_serials_are_reserved_before_any_part_can_escape() {
        let mut session = ready(Dialect::Current);
        session.next_input_serial = u64::MAX - 1;
        session.write(1, b"two bytes need two serials");
        assert!(!session.is_ready());
        assert!(session.unanswered_input(1));
        assert!(session.take_outgoing().is_empty());
        assert!(failed(&mut session));
    }
}
