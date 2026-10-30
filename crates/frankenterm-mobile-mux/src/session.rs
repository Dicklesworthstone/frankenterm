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
    /// Current servers say before the first pane list when a coherent snapshot
    /// is available, otherwise before the first verified pane identity.
    Incarnation {
        id: [u8; 16],
    },
    /// A passive identity query, or the identity captured before showing a
    /// pane. An absent witness grants no automatic reconnect or size-recovery
    /// authority. It does not prevent a deliberate fresh pane selection.
    PaneIdentity {
        pane_id: usize,
        identity: Option<wire::PaneIdentity>,
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
    /// A current server has refused, as busy, to send the pane's screen
    /// [`WAITING_AFTER_REFUSALS`] times in a row, and the session keeps
    /// asking; `waiting` turns false once a read is answered.
    PaneWaiting {
        pane_id: usize,
        waiting: bool,
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
    InspectPaneIdentity {
        pane_id: usize,
        deadline_ns: u64,
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
        layout: Option<wire::LineReadLayout>,
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
    /// One conditional geometry operation per pane may be on the wire. Its
    /// serial, full source witness and original deadline survive newer intents.
    ConditionalResize {
        intent: ResizeIntent,
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
/// Reliable input retains one admission budget across retries and suffixes.
pub const INPUT_DEADLINE_NS: u64 = 5_000_000_000;
/// Passive reconnect checks have their own bounded lane and never consume
/// an input serial or subscribe to a pane's output.
pub const MAX_IDENTITY_QUERIES: usize = 8;
pub const IDENTITY_QUERY_DEADLINE_NS: u64 = 5_000_000_000;
pub const CONDITIONAL_RESIZE_DEADLINE_NS: u64 = 5_000_000_000;
pub const MAX_CONDITIONAL_RESIZE_PANES: usize = 32;
const RESTORE_OBSERVATION_INTERVAL_NS: u64 = 100_000_000;
/// At most this many proven-no-effect retransmissions per admitted operation.
pub const MAX_INPUT_RETRIES: u8 = 8;
const MIN_INPUT_RETRY_DELAY_NS: u64 = 1_000_000;
const MAX_INPUT_RETRY_BACKOFF_NS: u64 = 100_000_000;
/// A render read a current server refused as safe to repeat goes again after
/// this delay, doubled per refusal in a row up to the maximum.
pub const MIN_RENDER_RETRY_DELAY_NS: u64 = 100_000_000;
pub const MAX_RENDER_RETRY_DELAY_NS: u64 = 5_000_000_000;
/// From this many refused render reads in a row (about 0.7 s of them) a pane
/// is reported waiting, so a brief busy spell does not flash a notice.
pub const WAITING_AFTER_REFUSALS: u32 = 3;
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
    /// None means the owner has not supplied a clock. Such an operation may
    /// complete normally, but cannot gain timed retry authority retroactively.
    deadline_ns: Option<u64>,
    retry_at_ns: Option<u64>,
    retries: u8,
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
    /// A v72 enqueue receipt cannot prove that asynchronous reflow finished.
    /// Keep the original desktop target until a newer list actually shows it.
    restore_observation: Option<RestoreObservation>,
}

#[derive(Debug, Clone, Copy)]
struct RestoreObservation {
    deadline_ns: u64,
    next_read_ns: u64,
}

#[derive(Debug, Clone, Copy)]
struct ResizeIntent {
    request: wire::ResizePaneConditional,
    restore: bool,
    deadline_ns: u64,
}

/// A pane whose render read a current server refused as safe to repeat.
#[derive(Debug, Clone, Copy, Default)]
struct RenderRetry {
    /// When to ask again; `None` while the repeat is on its way.
    due_ns: Option<u64>,
    /// Which reads to repeat: the first subscription, a poll, or both.
    subscribe: bool,
    poll: bool,
    /// Refusals in a row, which set the next delay.
    refusals: u32,
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
    /// At most one unsent successor per in-flight conditional resize. A
    /// newer desired grid replaces only this unsent intent, never the receipt
    /// authority or deadline of the operation already on the wire.
    resize_successors: HashMap<usize, ResizeIntent>,
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
    /// Exact witnesses learned on this connection. A later show must still
    /// name the same live registration; durable UUID equality alone is not
    /// permission to inherit a replaced pane.
    identities: HashMap<usize, wire::PaneIdentity>,
    render_retries: HashMap<usize, RenderRetry>,
    next_input_serial: u64,
    monotonic_ns: Option<u64>,
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

    /// Advance the owner's monotonic clock, in nanoseconds from any fixed
    /// origin chosen for this connection. Call before admitting/receiving input
    /// and at [`Session::next_input_wake_ns`]. Equal ticks are harmless; a
    /// backward clock fails closed. There is no wall clock, timer or I/O here.
    ///
    /// False means the session ended; the owner must stop egress and drain its
    /// failure event. Expiry never retries an unanswered/possibly applied RPC;
    /// it repeats only a render read the server answered as safe to repeat.
    pub fn advance_time(&mut self, now_ns: u64) -> bool {
        if self.failed {
            return false;
        }
        if self.monotonic_ns.is_some_and(|previous| now_ns < previous) {
            self.fail("the mux input clock moved backwards; input was not replayed".to_owned());
            return false;
        }
        self.monotonic_ns = Some(now_ns);
        let expired: Vec<_> = self
            .requests
            .iter()
            .filter_map(|(serial, request)| match request {
                Request::InspectPaneIdentity {
                    pane_id,
                    deadline_ns,
                } if now_ns >= *deadline_ns => Some((*serial, *pane_id)),
                _ => None,
            })
            .collect();
        for (serial, pane_id) in expired {
            self.requests.remove(&serial);
            self.events.push_back(Event::PaneIdentity {
                pane_id,
                identity: None,
            });
        }
        if self.requests.values().any(|request| {
            matches!(request, Request::ConditionalResize { intent } if now_ns >= intent.deadline_ns)
        }) || self
            .resize_successors
            .values()
            .any(|intent| now_ns >= intent.deadline_ns)
        {
            self.fail("the mux size request was not confirmed before its deadline; it was not replayed".to_owned());
            return false;
        }
        let mut read_restoration = false;
        for sizing in self.sizing.values_mut() {
            let Some(observation) = &mut sizing.restore_observation else {
                continue;
            };
            if now_ns >= observation.deadline_ns {
                // No proof of reflow: retain the durable recovery record and
                // stop polling. A later deliberate restore is still possible.
                sizing.restore_observation = None;
                sizing.phone_until = Some(u64::MAX);
            } else if now_ns >= observation.next_read_ns {
                observation.next_read_ns = now_ns.saturating_add(RESTORE_OBSERVATION_INTERVAL_NS);
                read_restoration = true;
            }
        }
        if read_restoration {
            self.refresh_panes();
        }
        if self
            .ordered_input
            .iter()
            .any(|input| input.deadline_ns.is_some_and(|deadline| now_ns >= deadline))
        {
            self.fail(
                "the mux input deadline expired; unconfirmed input was not replayed".to_owned(),
            );
            return false;
        }
        self.drive_input();
        self.repeat_due_renders(now_ns);
        !self.failed
    }

    /// Earliest absolute wake in the same clock domain supplied to
    /// [`Session::advance_time`]. Includes every admitted operation's expiry,
    /// including a lone paste or mouse RPC, a passive identity query's expiry,
    /// and a refused render read's repeat.
    /// A preceding RPC cannot hide a queued operation's original deadline.
    /// Only one owner timer is needed; the bounded FIFO remains authoritative.
    pub fn next_input_wake_ns(&self) -> Option<u64> {
        if self.failed {
            return None;
        }
        self.ordered_input
            .iter()
            .filter_map(|input| input.deadline_ns)
            .chain(
                self.ordered_input
                    .front()
                    .and_then(|input| input.retry_at_ns),
            )
            .chain(
                self.render_retries
                    .values()
                    .filter_map(|retry| retry.due_ns),
            )
            .chain(self.requests.values().filter_map(|request| match request {
                Request::InspectPaneIdentity { deadline_ns, .. } => Some(*deadline_ns),
                Request::ConditionalResize { intent } => Some(intent.deadline_ns),
                _ => None,
            }))
            .chain(
                self.resize_successors
                    .values()
                    .map(|intent| intent.deadline_ns),
            )
            .chain(
                self.sizing
                    .values()
                    .filter_map(|sizing| sizing.restore_observation)
                    .map(|observation| observation.deadline_ns.min(observation.next_read_ns)),
            )
            .min()
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

    /// Read a pane's exact v71 witness without showing, resizing or sending
    /// input. At most eight distinct queries may be outstanding; duplicate
    /// calls coalesce. Older peers answer locally with no witness, without
    /// sending an unsupported PDU or an input-authority probe. A v71 query
    /// requires an owner clock sample from `advance_time`; without one it
    /// returns false rather than admitting work with an unbounded lifetime.
    pub fn inspect_pane_identity(&mut self, pane_id: usize) -> bool {
        if !self.is_ready() {
            return false;
        }
        if self.requests.values().any(|request| {
            matches!(request, Request::InspectPaneIdentity { pane_id: pending, .. } if *pending == pane_id)
        }) {
            return true;
        }
        if self.dialect != Dialect::Current
            || self.codec_version < wire::PANE_IDENTITY_MIN_VERSION
            || !self.panes.iter().any(|pane| pane.pane_id == pane_id)
        {
            self.events.push_back(Event::PaneIdentity {
                pane_id,
                identity: None,
            });
            return true;
        }
        if self
            .requests
            .values()
            .filter(|request| matches!(request, Request::InspectPaneIdentity { .. }))
            .count()
            >= MAX_IDENTITY_QUERIES
        {
            return false;
        }
        let Some(deadline_ns) = self
            .monotonic_ns
            .and_then(|now| now.checked_add(IDENTITY_QUERY_DEADLINE_NS))
        else {
            return false;
        };
        self.send(
            ident::GET_PANE_IDENTITY,
            &wire::GetPaneIdentity { pane_id },
            Request::InspectPaneIdentity {
                pane_id,
                deadline_ns,
            },
        );
        !self.failed
    }

    fn retain_pane_identity(
        &mut self,
        pane_id: usize,
        identity: wire::PaneIdentity,
    ) -> Result<(), crate::Error> {
        if identity.session_incarnation == [0; 16]
            || identity.durable_pane_id == [0; 16]
            || identity.pane_registration == [0; 16]
            || self
                .incarnation
                .is_some_and(|current| current != identity.session_incarnation)
            || self
                .identities
                .get(&pane_id)
                .is_some_and(|current| *current != identity)
            || self.identities.iter().any(|(other, current)| {
                *other != pane_id
                    && (current.durable_pane_id == identity.durable_pane_id
                        || current.pane_registration == identity.pane_registration)
            })
        {
            return Err(crate::Error::Frame(
                "a pane identity changed or does not match the current mux",
            ));
        }
        if self.incarnation.is_none() {
            self.incarnation = Some(identity.session_incarnation);
            self.events.push_back(Event::Incarnation {
                id: identity.session_incarnation,
            });
        }
        self.identities.insert(pane_id, identity);
        self.events.push_back(Event::PaneIdentity {
            pane_id,
            identity: Some(identity),
        });
        Ok(())
    }

    /// Mirrors the pane onto its terminal, drawing it in full first.
    pub fn show(&mut self, pane_id: usize) {
        if !self.is_ready() {
            return;
        }
        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.show();
            if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
                self.poll(pane_id);
            } else if !matches!(
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
        let observed = self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION;
        if observed && let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.require_observation();
        }
        if observed && self.size_effect_pending(pane_id) {
            // The final size receipt starts the observation. A retry of an
            // earlier subscribe must not open that newer presentation gate.
            return;
        }
        self.send(
            if observed {
                ident::GET_PANE_RENDER_STATE
            } else {
                ident::GET_PANE_RENDER_CHANGES
            },
            &GetPaneRenderChanges { pane_id },
            Request::Subscribe { pane_id },
        );
        if observed
            && !self.failed
            && let Some(mirror) = self.mirrors.get_mut(&pane_id)
        {
            mirror.expect_observation(self.next_serial);
        }
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
            restore_observation: None,
        });
        sizing.phone_until = Some(u64::MAX);
        sizing.restore_observation = None;
        let desktop = sizing.desktop;
        let Some(pixel_width) = (desktop.pixel_width / desktop.cols.max(1)).checked_mul(cols)
        else {
            self.fail("the requested terminal width overflowed its geometry".to_owned());
            return;
        };
        let Some(pixel_height) = (desktop.pixel_height / desktop.rows.max(1)).checked_mul(rows)
        else {
            self.fail("the requested terminal height overflowed its geometry".to_owned());
            return;
        };
        let size = TerminalSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
            dpi: desktop.dpi,
        };
        let request = if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.expect_size(rows, cols);
            if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
                mirror.require_observation();
            }
            Request::Resize { pane_id }
        } else {
            Request::Action
        };
        if !self.send_resize(tab_id, pane_id, size, request) {
            return;
        }
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
    /// resized since is left alone. On v72, an exact identity must also have
    /// been read on this connection, and every eventual restore carries it
    /// to the server; a numeric ID and matching dimensions are insufficient.
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
        if !self.is_ready()
            || !still_phone_sized
            || (desktop.rows, desktop.cols) == (rows, cols)
            || (self.conditional_resize_enabled()
                && (!self.identities.contains_key(&pane_id)
                    || !wire::conditional_resize_size_is_valid(desktop)))
        {
            return false;
        }
        self.sizing.insert(
            pane_id,
            PaneSizing {
                desktop,
                phone_until: Some(u64::MAX),
                desktop_list_after: 0,
                restore_observation: None,
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
            if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
                mirror.require_observation();
            }
        }
        self.send_resize(
            tab_id,
            pane_id,
            sizing.desktop,
            Request::RestoreSize { pane_id },
        );
    }

    /// Whether a size [`Session::restore_size`] gave back is still
    /// unanswered, so the server may not have applied it. Closing the
    /// connection now can lose it on the way: a relay like `nc -U` drops
    /// what it has not forwarded when its channel closes.
    pub fn restoring(&self) -> bool {
        !self.failed
            && (self.sizing.values().any(|sizing| {
                matches!(sizing.phone_until,
                    Some(serial) if serial != u64::MAX && self.requests.contains_key(&serial))
                    || sizing.restore_observation.is_some()
            }) || self.resize_successors.values().any(|intent| intent.restore))
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
        // A successful transport keepalive does not settle input. Paste and
        // mouse RPCs need the same admission deadline as reliable writes even
        // though their ambiguous outcomes never permit an automatic retry.
        let deadline_ns = match self.monotonic_ns {
            Some(now) => match now.checked_add(INPUT_DEADLINE_NS) {
                Some(deadline) => Some(deadline),
                None => {
                    self.interrupted_input.insert(pane_id);
                    self.fail("mux input deadline overflow; input was not sent".to_owned());
                    return false;
                }
            },
            None => None,
        };
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
            deadline_ns,
            retry_at_ns: None,
            retries: 0,
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
        if let Some(due) = input.retry_at_ns {
            let Some(now) = self.monotonic_ns else { return };
            if now < due {
                return;
            }
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
        input.retry_at_ns = None;
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

    fn schedule_input_retry(&mut self, peer_delay_ns: u64) -> Result<(), crate::Error> {
        let Some(now) = self.monotonic_ns else {
            self.fail(
                "timed mux input retry requires an owner clock; input was not replayed".to_owned(),
            );
            return Ok(());
        };
        let Some(input) = self.ordered_input.front_mut() else {
            return Err(crate::Error::Schema);
        };
        let Some(deadline) = input.deadline_ns else {
            self.fail("mux input had no clock at admission; input was not replayed".to_owned());
            return Ok(());
        };
        if input.retries >= MAX_INPUT_RETRIES {
            self.fail(
                "mux input retry budget exhausted; unconfirmed input was not replayed".to_owned(),
            );
            return Ok(());
        }
        // Amplify short prompts to avoid a busy loop; never shorten a valid
        // authoritative delay. Retry count and deadline survive partial writes.
        let base = peer_delay_ns.max(MIN_INPUT_RETRY_DELAY_NS);
        let delay = base
            .saturating_mul(1u64 << input.retries)
            .min(MAX_INPUT_RETRY_BACKOFF_NS.max(base));
        let Some(due) = now.checked_add(delay).filter(|due| *due < deadline) else {
            self.fail(
                "mux input retry would exceed its original deadline; input was not replayed"
                    .to_owned(),
            );
            return Ok(());
        };
        input.retries += 1;
        input.retry_at_ns = Some(due);
        input.in_flight = false;
        Ok(())
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
                self.finish_input();
            }
            wire::ReliableKeyOutcome::Retry(retry) => {
                if let Some(delay) = retry.no_effect_delay_ns()? {
                    return self.schedule_input_retry(delay);
                }
                self.fail(
                    "the mux did not authorize a no-effect key retry; input was not replayed"
                        .to_owned(),
                );
            }
            wire::ReliableKeyOutcome::Rejected(_) => {
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
            wire::ReliableWriteOutcome::Retry(retry) => {
                if let Some(delay) = retry.no_effect_delay_ns()? {
                    return self.schedule_input_retry(delay);
                }
                self.fail(
                    "the mux did not authorize a no-effect byte retry; input was not replayed"
                        .to_owned(),
                );
            }
            wire::ReliableWriteOutcome::Rejected(_) | wire::ReliableWriteOutcome::Indeterminate => {
                self.fail(
                    "the mux did not confirm reliable byte input; input was not replayed"
                        .to_owned(),
                );
            }
        }
        Ok(())
    }

    fn conditional_resize_enabled(&self) -> bool {
        self.dialect == Dialect::Current
            && self.codec_version >= wire::CONDITIONAL_RESIZE_MIN_VERSION
    }

    fn send_resize(
        &mut self,
        tab_id: usize,
        pane_id: usize,
        size: TerminalSize,
        request: Request,
    ) -> bool {
        let restore = matches!(&request, Request::RestoreSize { .. });
        if self.conditional_resize_enabled() {
            let Some(identity) = self.identities.get(&pane_id).copied() else {
                self.fail(
                    "the mux pane's exact identity is unavailable; its shared size was not changed"
                        .to_owned(),
                );
                return false;
            };
            if !wire::conditional_resize_size_is_valid(size) {
                self.fail(
                    "the requested terminal size exceeds the bounded mux geometry profile"
                        .to_owned(),
                );
                return false;
            }
            let Some(deadline_ns) = self
                .monotonic_ns
                .and_then(|now| now.checked_add(CONDITIONAL_RESIZE_DEADLINE_NS))
            else {
                self.fail("the mux size request has no bounded owner clock".to_owned());
                return false;
            };
            let intent = ResizeIntent {
                request: wire::ResizePaneConditional {
                    containing_tab_id: tab_id,
                    pane_id,
                    identity,
                    size,
                },
                restore,
                deadline_ns,
            };
            if self.requests.values().any(|request| {
                matches!(request, Request::ConditionalResize { intent } if intent.request.pane_id == pane_id)
            }) {
                self.resize_successors.insert(pane_id, intent);
                return true;
            }
            return self.dispatch_conditional_resize(intent);
        }
        let resize = Resize {
            containing_tab_id: tab_id,
            pane_id,
            size,
        };
        self.send(ident::RESIZE, &resize, request);
        if restore && !self.failed {
            if let Some(sizing) = self.sizing.get_mut(&pane_id) {
                sizing.phone_until = Some(self.next_serial);
            }
        }
        !self.failed
    }

    fn dispatch_conditional_resize(&mut self, intent: ResizeIntent) -> bool {
        if self
            .requests
            .values()
            .filter(|request| matches!(request, Request::ConditionalResize { .. }))
            .count()
            >= MAX_CONDITIONAL_RESIZE_PANES
        {
            self.fail("too many mux panes have unconfirmed size requests".to_owned());
            return false;
        }
        self.send(
            ident::RESIZE_PANE_CONDITIONAL,
            &intent.request,
            Request::ConditionalResize { intent },
        );
        if intent.restore && !self.failed {
            if let Some(sizing) = self.sizing.get_mut(&intent.request.pane_id) {
                sizing.phone_until = Some(self.next_serial);
            }
        }
        !self.failed
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
            self.resize_successors.clear();
            self.outgoing.clear();
            self.input.clear();
            self.events.push_back(Event::Failed { reason });
        }
    }

    /// A new pane list, asked for with `serial`.
    fn listed(&mut self, list: &PaneList, serial: u64) {
        let previously_listed: Vec<_> = self.panes.iter().map(|pane| pane.pane_id).collect();
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
        let missing: HashSet<_> = self
            .mirrors
            .keys()
            .copied()
            .chain(previously_listed)
            .filter(|pane_id| !self.panes.iter().any(|pane| pane.pane_id == *pane_id))
            .collect();
        for pane_id in missing {
            self.pane_gone(pane_id);
        }
        self.authorities
            .retain(|pane_id, _| self.panes.iter().any(|pane| pane.pane_id == *pane_id));
        self.identities
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
                restore_observation: None,
            });
            if serial <= sizing.desktop_list_after {
                continue;
            }
            if sizing.restore_observation.is_some() {
                // The receipt acknowledged enqueue only. A fresh list with
                // the actual target grid is the restoration evidence. An old
                // phone-sized snapshot must not overwrite the recovery target.
                if pane.size == sizing.desktop {
                    sizing.restore_observation = None;
                    sizing.phone_until = None;
                    sizing.desktop_list_after = serial;
                    self.events.push_back(Event::PhoneSizeRestored {
                        pane_id: pane.pane_id,
                    });
                }
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
        if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
            match &request {
                Some(Request::Subscribe { pane_id } | Request::Poll { pane_id }) => {
                    if !self
                        .mirrors
                        .get(pane_id)
                        .is_some_and(|mirror| mirror.expects_observation(frame.serial))
                    {
                        // A hidden/reopened view or a newer poll owns a different
                        // request. This reply cannot settle that view's gate.
                        return Ok(());
                    }
                    if !matches!(
                        frame.ident,
                        ident::GET_PANE_RENDER_CHANGES_RESPONSE
                            | ident::LIVENESS_RESPONSE
                            | ident::ERROR_RESPONSE
                    ) {
                        return Err(crate::Error::Schema);
                    }
                    if frame.ident == ident::ERROR_RESPONSE {
                        let error: wire::ErrorResponse = wire::decode_input_receipt(&frame.body)?;
                        if error.schema_version != 1
                            || error.request_ident != ident::GET_PANE_RENDER_STATE
                        {
                            return Err(crate::Error::Schema);
                        }
                    }
                }
                Some(Request::Lines {
                    layout: Some(_), ..
                }) if !matches!(
                    frame.ident,
                    ident::GET_LINES_AT_LAYOUT_RESPONSE | ident::ERROR_RESPONSE
                ) =>
                {
                    return Err(crate::Error::Schema);
                }
                _ => {}
            }
        }
        if let Some(Request::InspectPaneIdentity { pane_id, .. }) = &request {
            let pane_id = *pane_id;
            // Removal retires a pending query; a late reply must not restore
            // the pane or publish a witness for a same-number replacement.
            if !self.panes.iter().any(|pane| pane.pane_id == pane_id) {
                return Ok(());
            }
            let outcome = match frame.ident {
                ident::GET_PANE_IDENTITY_RESPONSE => {
                    if frame.body.len() > wire::MAX_PANE_IDENTITY_RESPONSE_BYTES {
                        return Err(crate::Error::Frame(
                            "a pane identity reply exceeds its protocol bound",
                        ));
                    }
                    let response: wire::PaneIdentityResponse = wire::decode(&frame.body)?;
                    if response.pane_id != pane_id {
                        return Err(crate::Error::Schema);
                    }
                    response.outcome
                }
                ident::ERROR_RESPONSE => wire::PaneIdentityOutcome::Unavailable,
                _ => {
                    return Err(crate::Error::Frame(
                        "a pane identity query received an unexpected reply",
                    ));
                }
            };
            match outcome {
                wire::PaneIdentityOutcome::Available { identity } => {
                    self.retain_pane_identity(pane_id, identity)?;
                }
                wire::PaneIdentityOutcome::PaneGone => self.pane_gone(pane_id),
                wire::PaneIdentityOutcome::Unavailable => {
                    if self.identities.contains_key(&pane_id) {
                        return Err(crate::Error::Frame(
                            "a verified pane lost its exact identity",
                        ));
                    }
                    self.events.push_back(Event::PaneIdentity {
                        pane_id,
                        identity: None,
                    });
                }
            }
            return Ok(());
        }
        if let Some(Request::ConditionalResize { intent }) = &request {
            let intent = *intent;
            if frame.ident == ident::ERROR_RESPONSE {
                self.fail("the mux refused a conditional size request; its outcome was not replayed through a numeric resize".to_owned());
                return Ok(());
            }
            if frame.ident != ident::RESIZE_PANE_CONDITIONAL_RESPONSE {
                return Err(crate::Error::Frame(
                    "a conditional size request received an unexpected reply",
                ));
            }
            let response = wire::decode_conditional_resize_receipt(&frame.body)?;
            let pane_id = intent.request.pane_id;
            if response.pane_id != pane_id
                || response.identity != intent.request.identity
                || self.identities.get(&pane_id) != Some(&intent.request.identity)
            {
                return Err(crate::Error::Frame(
                    "a size receipt does not match its exact pane request",
                ));
            }
            match response.outcome {
                wire::ConditionalResizeOutcome::Enqueued => {
                    if intent.restore && !self.resize_successors.contains_key(&pane_id) {
                        if let Some(sizing) = self.sizing.get_mut(&pane_id)
                            && sizing.phone_until == Some(frame.serial)
                        {
                            sizing.desktop_list_after = self.next_serial;
                            sizing.restore_observation = Some(RestoreObservation {
                                deadline_ns: intent.deadline_ns,
                                next_read_ns: self
                                    .monotonic_ns
                                    .unwrap_or(0)
                                    .saturating_add(RESTORE_OBSERVATION_INTERVAL_NS),
                            });
                            self.refresh_panes();
                        }
                    }
                    if !self.resize_successors.contains_key(&pane_id) {
                        // Even a restore can supersede an outstanding phone
                        // resize. Its fresh render read releases that view's
                        // gate; the independent pane list still proves when
                        // the requested desktop geometry was actually applied.
                        self.poll(pane_id);
                    }
                    if let Some(successor) = self.resize_successors.remove(&pane_id) {
                        self.dispatch_conditional_resize(successor);
                    }
                }
                wire::ConditionalResizeOutcome::PaneGone => self.pane_gone(pane_id),
                wire::ConditionalResizeOutcome::IdentityMismatch => {
                    self.fail("the mux pane was replaced before its size request; the replacement was not resized".to_owned());
                }
                wire::ConditionalResizeOutcome::IdentityUnavailable => {
                    self.fail("the mux could not verify this pane for its size request; its shared size was not changed".to_owned());
                }
                wire::ConditionalResizeOutcome::Retired => {
                    self.fail(
                        "the mux retired this pane's size authority before admitting the request"
                            .to_owned(),
                    );
                }
                wire::ConditionalResizeOutcome::Indeterminate => {
                    self.fail("the mux could not confirm whether a size request was enqueued; it was not replayed".to_owned());
                }
            }
            return Ok(());
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
                            self.retain_pane_identity(pane_id, identity)?;
                            Some(identity.pane_registration)
                        }
                        wire::PaneIdentityOutcome::PaneGone => {
                            self.pane_gone(pane_id);
                            return Ok(());
                        }
                        wire::PaneIdentityOutcome::Unavailable => {
                            if self.identities.contains_key(&pane_id) {
                                return Err(crate::Error::Frame(
                                    "a verified pane lost its exact identity",
                                ));
                            }
                            self.events.push_back(Event::PaneIdentity {
                                pane_id,
                                identity: None,
                            });
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
                (_, ident::ERROR_RESPONSE) => {
                    if self.identities.contains_key(&pane_id) {
                        return Err(crate::Error::Frame(
                            "a verified pane lost its exact identity",
                        ));
                    }
                    None
                }
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
                let correlated = match request {
                    Some(
                        Request::Subscribe { pane_id: expected }
                        | Request::Poll { pane_id: expected },
                    ) if expected == pane_id => true,
                    None => false,
                    _ => return Err(crate::Error::Schema),
                };
                if self.mirrors.contains_key(&pane_id) {
                    let (head, lines) = if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
                        let changes: wire::RenderObservation =
                            wire::decode_observation(&frame.body)?;
                        if changes.seqno == usize::MAX {
                            return Err(crate::Error::Schema);
                        }
                        let layout = wire::LineReadLayout {
                            seqno: changes.seqno,
                            dimensions: changes.head.dimensions,
                        };
                        let Some(mirror) = self.mirrors.get_mut(&pane_id) else {
                            return Ok(());
                        };
                        let accepted = mirror.observe(layout, correlated);
                        if matches!(request, Some(Request::Poll { .. })) {
                            mirror.size_settled();
                        }
                        if !accepted {
                            self.advance(pane_id);
                            return Ok(());
                        }
                        (changes.head, Some(changes.bonus_lines.entries))
                    } else {
                        // Legacy image cells can lack a decodable line prefix.
                        // Current observed reads never downgrade this way.
                        match wire::decode::<RenderChanges>(&frame.body) {
                            Ok(changes) => (changes.head, Some(changes.bonus_lines)),
                            Err(_) => (wire::decode::<RenderHead>(&frame.body)?, None),
                        }
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
                let reply = wire::decode::<LinesResponse>(&frame.body);
                if let Some(Request::Lines {
                    pane_id, layout, ..
                }) = &request
                {
                    if layout.is_some()
                        || reply.as_ref().is_ok_and(|reply| reply.pane_id != *pane_id)
                    {
                        return Err(crate::Error::Schema);
                    }
                }
                let lines = reply.map(|reply| reply.lines).unwrap_or_default();
                match request {
                    Some(Request::Lines {
                        pane_id, ranges, ..
                    }) => {
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
            ident::GET_LINES_AT_LAYOUT_RESPONSE => {
                let Some(Request::Lines {
                    pane_id,
                    ranges,
                    layout: Some(expected),
                }) = request
                else {
                    return Err(crate::Error::Schema);
                };
                let reply: wire::LinesAtLayoutResponse = wire::decode_observation(&frame.body)?;
                if reply.pane_id != pane_id || reply.layout != expected {
                    return Err(crate::Error::Frame(
                        "a line reply does not match its observed layout",
                    ));
                }
                if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                    mirror.apply_lines(reply.lines.entries, &ranges);
                }
                self.advance(pane_id);
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
                if let Some(Request::Subscribe { pane_id } | Request::Poll { pane_id }) = request {
                    if pane_id != reply.pane_id
                        || (reply.is_alive
                            && self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION)
                    {
                        return Err(crate::Error::Schema);
                    }
                }
                if !reply.is_alive {
                    self.pane_gone(reply.pane_id);
                } else {
                    if let Some(Request::Subscribe { pane_id } | Request::Poll { pane_id }) =
                        request
                        && self
                            .render_retries
                            .get(&pane_id)
                            .is_some_and(|retry| retry.due_ns.is_none())
                    {
                        // The repeat was answered; a later refusal starts over.
                        if self
                            .render_retries
                            .remove(&pane_id)
                            .is_some_and(|retry| retry.refusals >= WAITING_AFTER_REFUSALS)
                        {
                            self.events.push_back(Event::PaneWaiting {
                                pane_id,
                                waiting: false,
                            });
                        }
                    }
                    if let Some(Request::Poll { pane_id }) = request {
                        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                            mirror.size_settled();
                        }
                        self.advance(pane_id);
                    }
                }
            }
            ident::UNIT_RESPONSE => {
                if !frame.body.is_empty() {
                    return Err(crate::Error::Frame("a unit reply contains unexpected data"));
                }
                match request {
                    Some(Request::RegisterClient { server_version }) => {
                        self.become_ready(server_version);
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
                        if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
                            self.poll(pane_id);
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
                    Some(Request::Subscribe { pane_id }) => {
                        self.render_refused(pane_id, true, &frame.body);
                    }
                    Some(Request::Poll { pane_id }) => {
                        self.render_refused(pane_id, false, &frame.body);
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
                        if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
                            self.poll(pane_id);
                        }
                    }
                    Some(Request::Lines {
                        pane_id,
                        ranges,
                        layout,
                    }) => {
                        if layout.is_some() {
                            let error: wire::ErrorResponse =
                                wire::decode_input_receipt(&frame.body)?;
                            if error.schema_version != 1
                                || error.request_ident != ident::GET_LINES_AT_LAYOUT
                            {
                                return Err(crate::Error::Schema);
                            }
                            if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                                mirror.reject_lines();
                            }
                            self.render_refused(pane_id, false, &frame.body);
                            return Ok(());
                        }
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
                        | Request::InspectPaneIdentity { .. }
                        | Request::PaneAuthority { .. }
                        | Request::ConditionalResize { .. }
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

    fn size_effect_pending(&self, pane_id: usize) -> bool {
        self.resize_successors.contains_key(&pane_id)
            || self.requests.values().any(|request| match request {
                Request::Resize { pane_id: pending }
                | Request::RestoreSize { pane_id: pending } => *pending == pane_id,
                Request::ConditionalResize { intent } => intent.request.pane_id == pane_id,
                _ => false,
            })
    }

    fn poll(&mut self, pane_id: usize) {
        if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION
            && self.size_effect_pending(pane_id)
        {
            if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                mirror.require_observation();
            }
            return;
        }
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
            let observed = self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION;
            if observed && let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                mirror.require_observation();
            }
            self.send(
                if observed {
                    ident::GET_PANE_RENDER_STATE
                } else {
                    ident::GET_PANE_RENDER_CHANGES
                },
                &GetPaneRenderChanges { pane_id },
                Request::Poll { pane_id },
            );
            if observed
                && !self.failed
                && let Some(mirror) = self.mirrors.get_mut(&pane_id)
            {
                mirror.expect_observation(self.next_serial);
            }
        }
    }

    /// The server refused a render read. A current server that applied
    /// nothing and allows a repeat (busy render metadata, a full scheduler)
    /// is asked again after a growing delay; the pane stays. A gone pane is
    /// answered by liveness, not here; any other refusal cannot be drawn.
    fn render_refused(&mut self, pane_id: usize, subscribe: bool, body: &[u8]) {
        let repeatable = self.dialect == Dialect::Current
            && wire::decode::<wire::ErrorResponse>(body)
                .is_ok_and(|error| error.retry_after_backoff());
        if !repeatable || !self.mirrors.contains_key(&pane_id) {
            self.pane_gone(pane_id);
            return;
        }
        let now = self.monotonic_ns.unwrap_or(0);
        let retry = self.render_retries.entry(pane_id).or_default();
        let delay = MIN_RENDER_RETRY_DELAY_NS
            .saturating_mul(1 << retry.refusals.min(6))
            .min(MAX_RENDER_RETRY_DELAY_NS);
        retry.refusals = retry.refusals.saturating_add(1);
        retry.subscribe |= subscribe;
        retry.poll |= !subscribe;
        retry.due_ns = Some(now.saturating_add(delay));
        if retry.refusals == WAITING_AFTER_REFUSALS {
            self.events.push_back(Event::PaneWaiting {
                pane_id,
                waiting: true,
            });
        }
    }

    fn repeat_due_renders(&mut self, now_ns: u64) {
        let mut due = Vec::new();
        for (pane_id, retry) in &mut self.render_retries {
            if retry.due_ns.is_some_and(|at| at <= now_ns) {
                due.push((*pane_id, retry.subscribe, retry.poll));
                *retry = RenderRetry {
                    refusals: retry.refusals,
                    ..RenderRetry::default()
                };
            }
        }
        due.sort_unstable();
        for (pane_id, subscribe, poll) in due {
            if subscribe {
                self.subscribe(pane_id);
            }
            if poll {
                self.poll(pane_id);
            }
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
        self.identities.remove(&pane_id);
        self.resize_successors.remove(&pane_id);
        self.render_retries.remove(&pane_id);
        self.requests.retain(|_, request| {
            !matches!(request,
                Request::PaneIdentity { pane_id: old }
                | Request::InspectPaneIdentity { pane_id: old, .. }
                | Request::PaneAuthority { pane_id: old, .. }
                | Request::Subscribe { pane_id: old }
                | Request::Lines { pane_id: old, .. }
                | Request::Resize { pane_id: old }
                | Request::RestoreSize { pane_id: old }
                | Request::Poll { pane_id: old } if *old == pane_id
            ) && !matches!(request, Request::ConditionalResize { intent } if intent.request.pane_id == pane_id)
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
            let layout = if self.codec_version >= wire::OBSERVED_LINES_MIN_VERSION {
                let Some(layout) = mirror.observation() else {
                    return;
                };
                Some(layout)
            } else {
                None
            };
            mirror.set_fetching(&wanted);
            if let Some(layout) = layout {
                self.send(
                    ident::GET_LINES_AT_LAYOUT,
                    &wire::GetLinesAtLayout {
                        pane_id,
                        layout,
                        lines: wanted.clone(),
                    },
                    Request::Lines {
                        pane_id,
                        ranges: wanted,
                        layout: Some(layout),
                    },
                );
            } else {
                self.send(
                    ident::GET_LINES,
                    &GetLines {
                        pane_id,
                        lines: wanted.clone(),
                    },
                    Request::Lines {
                        pane_id,
                        ranges: wanted,
                        layout: None,
                    },
                );
            }
        } else {
            let outputs = mirror.render();
            if mirror.is_drawable()
                && self
                    .render_retries
                    .remove(&pane_id)
                    .is_some_and(|retry| retry.refusals >= WAITING_AFTER_REFUSALS)
            {
                self.events.push_back(Event::PaneWaiting {
                    pane_id,
                    waiting: false,
                });
            }
            for output in outputs {
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
