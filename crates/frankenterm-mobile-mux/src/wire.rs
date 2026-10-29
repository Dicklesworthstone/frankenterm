//! The mux protocol exactly as codec v46 (FrankenTerm v0.13) frames it, the
//! dialect the fleet's mux servers speak, and as the current codec window
//! does where it differs: the requests the phone sends and the prefixes of
//! the replies it reads. Varbincode is positional, so field order and types
//! match `codec` and `mux` byte for byte; the tests compare them with the
//! desktop codec's own encoding and decoding in both dialects.
//!
//! A reply struct may stop before the reply's last field: decoding reads a
//! prefix and ignores the rest (such as image references). Effect receipts
//! instead consume their complete bounded schema: an incomplete or extended
//! reply cannot settle a different input operation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use termwiz::hyperlink::Hyperlink;
use termwiz::input::KeyEvent;
use termwiz::surface::{CursorShape, CursorVisibility, Line};

/// The fleet's v0.13 servers: v46, and v0.13.0's v47, which only adds two
/// PDUs the phone never sends.
pub const LEGACY_VERSIONS: RangeInclusive<usize> = 46..=47;
/// A current FrankenTerm's own mux (a Mac's GUI, reached through its relay):
/// the rolling-upgrade window `codec::CODEC_VERSION_MIN_SUPPORTED..=
/// CODEC_VERSION`. Additive reliable writes (v64) and pane identity (v71)
/// are gated by the negotiated version; neither silently falls back to a
/// numeric-pane legacy write on a current connection.
pub const CURRENT_VERSIONS: RangeInclusive<usize> = 61..=71;

/// Which of the two layouts a server speaks. They differ, for this client,
/// in SendPaste (an input serial follows the text) and ErrorResponse (a
/// structured rejection rather than the error's text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Legacy,
    Current,
}

impl Dialect {
    /// A server newer than this app still speaks the current layout while
    /// the oldest version it accepts (`min_supported`) reaches this app's
    /// newest: the codec raises that floor with any breaking change, so this
    /// is the overlap the desktop client's `check_compat` agrees on.
    pub fn of(codec_version: usize, min_supported: Option<usize>) -> Option<Self> {
        let newest = *CURRENT_VERSIONS.end();
        if LEGACY_VERSIONS.contains(&codec_version) {
            Some(Self::Legacy)
        } else if CURRENT_VERSIONS.contains(&codec_version)
            || (codec_version > newest && min_supported.is_some_and(|min| min <= newest))
        {
            Some(Self::Current)
        } else {
            None
        }
    }
}

/// PDU identifiers (codec's `pdu!` table).
pub mod ident {
    pub const ERROR_RESPONSE: u64 = 0;
    pub const LIST_PANES: u64 = 3;
    pub const LIST_PANES_RESPONSE: u64 = 4;
    pub const WRITE_TO_PANE: u64 = 9;
    pub const UNIT_RESPONSE: u64 = 10;
    pub const SEND_KEY_DOWN: u64 = 11;
    pub const SEND_MOUSE_EVENT: u64 = 12;
    pub const SEND_PASTE: u64 = 13;
    pub const RESIZE: u64 = 14;
    pub const GET_LINES: u64 = 22;
    pub const GET_LINES_RESPONSE: u64 = 23;
    pub const GET_PANE_RENDER_CHANGES: u64 = 24;
    pub const GET_PANE_RENDER_CHANGES_RESPONSE: u64 = 25;
    pub const GET_CODEC_VERSION: u64 = 26;
    pub const GET_CODEC_VERSION_RESPONSE: u64 = 27;
    pub const LIVENESS_RESPONSE: u64 = 30;
    pub const SEARCH_SCROLLBACK: u64 = 31;
    pub const SEARCH_SCROLLBACK_RESPONSE: u64 = 32;
    pub const PANE_REMOVED: u64 = 37;
    pub const SET_CLIENT_ID: u64 = 40;
    /// Pushed when a tab's split layout changes or a tab is added: the
    /// pane list is out of date.
    pub const TAB_RESIZED: u64 = 54;
    pub const TAB_ADDED_TO_WINDOW: u64 = 55;
    /// Current servers only: the pane list with the server's incarnation.
    pub const LIST_PANES_COHERENT: u64 = 81;
    pub const LIST_PANES_COHERENT_RESPONSE: u64 = 82;
    /// Pushed in place of PANE_REMOVED, TAB_RESIZED and TAB_ADDED_TO_WINDOW
    /// once the server answered LIST_PANES_COHERENT with a snapshot.
    pub const TOPOLOGY_EVENT: u64 = 83;
    pub const RELIABLE_KEY_EVENT: u64 = 96;
    pub const RELIABLE_KEY_EVENT_RESPONSE: u64 = 97;
    pub const RELIABLE_PANE_WRITE: u64 = 100;
    pub const RELIABLE_PANE_WRITE_RESPONSE: u64 = 101;
    pub const GET_PANE_IDENTITY: u64 = 117;
    pub const GET_PANE_IDENTITY_RESPONSE: u64 = 118;
}

/// `codec::TopologyCapabilities::FENCED_SNAPSHOT_V1`, the one capability a
/// coherent pane list needs.
pub const FENCED_SNAPSHOT: u64 = 1;

pub type StableRowIndex = isize;

#[derive(Serialize)]
pub struct GetCodecVersion {}

/// Prefix of PDU 27.
#[derive(Deserialize, Debug)]
pub struct CodecVersion {
    pub codec_vers: usize,
    pub version_string: String,
}

/// All of PDU 27 from a current server; a v0.13 server's ends before
/// `min_supported`.
#[derive(Deserialize, Debug)]
pub struct CodecWindow {
    pub codec_vers: usize,
    pub version_string: String,
    pub executable_path: String,
    pub config_file_path: Option<String>,
    pub min_supported: usize,
}

#[derive(Serialize)]
pub struct ListPanes {}

#[derive(Serialize)]
pub struct GetPaneRenderChanges {
    pub pane_id: usize,
}

#[derive(Serialize)]
pub struct GetLines {
    pub pane_id: usize,
    pub lines: Vec<Range<StableRowIndex>>,
}

#[derive(Serialize)]
pub struct WriteToPane {
    pub pane_id: usize,
    pub data: Vec<u8>,
}

/// The canonical `mux::client::ClientId` positional schema. The mobile client
/// uses a fresh opaque hostname per connection, never the device's hostname,
/// account, or SSH agent socket. It does not reuse a previous input ledger.
#[derive(Serialize)]
pub struct ClientId {
    pub hostname: String,
    pub username: String,
    pub pid: u32,
    pub epoch: u64,
    pub id: usize,
    pub ssh_auth_sock: Option<String>,
}

#[derive(Serialize)]
pub struct SetClientId {
    pub client_id: ClientId,
    pub is_proxy: bool,
}

pub const RELIABLE_WRITE_MIN_VERSION: usize = 64;
pub const PANE_IDENTITY_MIN_VERSION: usize = 71;
pub const MAX_PANE_IDENTITY_RESPONSE_BYTES: usize = 8 * 1024;
pub const MAX_RELIABLE_WRITE_BYTES: usize = 16 * 1024;
pub const MAX_RELIABLE_RETRY_DELAY_NS: u64 = 1_000_000_000;

#[derive(Serialize)]
pub struct ReliableKeyEvent {
    pub pane_id: usize,
    pub pane_registration: Option<[u8; 16]>,
    pub event: KeyEvent,
    pub input_serial: u64,
    pub kind: ReliableKeyEventKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReliableKeyEventKind {
    KeyDown,
    KeyUp,
}

#[derive(Serialize)]
pub struct ReliablePaneWrite<'a> {
    pub pane_id: usize,
    pub pane_registration: Option<[u8; 16]>,
    pub input_serial: u64,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct ReliableInputSchedulerPressure {
    pub queue_id: u64,
    pub scheduler_generation: u64,
    pub active_tasks: u64,
    pub task_capacity: u64,
    pub active_estimated_bytes: u64,
    pub estimated_byte_capacity: u64,
    pub requested_estimated_bytes: u64,
    pub retry_after_ns: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum ReliableKeyRetry {
    SchedulerFull(ReliableInputSchedulerPressure),
    SchedulerRetired(ReliableInputSchedulerPressure),
    SchedulerUnavailable { retry_after_ns: u64 },
    DuplicatePending { retry_after_ns: u64 },
    ClientRegistrationTransition { retry_after_ns: u64 },
    PaneAuthorityRequired { pane_registration: [u8; 16] },
}

impl ReliableKeyRetry {
    /// Validate the canonical retry schema before granting any scheduling
    /// authority. DuplicatePending may already have reached the pane; a new
    /// registration cannot replace the one frozen at admission. Neither is
    /// permission for this client's no-effect retry path.
    pub fn no_effect_delay_ns(self) -> Result<Option<u64>, crate::Error> {
        let delay = match self {
            Self::SchedulerFull(pressure) | Self::SchedulerRetired(pressure) => {
                if pressure.queue_id == 0
                    || pressure.scheduler_generation == 0
                    || pressure.task_capacity == 0
                    || pressure.estimated_byte_capacity == 0
                    || pressure.requested_estimated_bytes == 0
                    || pressure.active_tasks > pressure.task_capacity
                    || pressure.active_estimated_bytes > pressure.estimated_byte_capacity
                {
                    return Err(crate::Error::Frame(
                        "invalid reliable input scheduler pressure",
                    ));
                }
                pressure.retry_after_ns
            }
            Self::SchedulerUnavailable { retry_after_ns }
            | Self::ClientRegistrationTransition { retry_after_ns } => retry_after_ns,
            Self::DuplicatePending { retry_after_ns } => {
                validate_retry_delay(retry_after_ns)?;
                return Ok(None);
            }
            Self::PaneAuthorityRequired { pane_registration } => {
                if pane_registration == [0; 16] {
                    return Err(crate::Error::Frame("reserved reliable input registration"));
                }
                return Ok(None);
            }
        };
        validate_retry_delay(delay)?;
        Ok(Some(delay))
    }
}

fn validate_retry_delay(delay_ns: u64) -> Result<(), crate::Error> {
    if !(1..=MAX_RELIABLE_RETRY_DELAY_NS).contains(&delay_ns) {
        return Err(crate::Error::Frame("invalid reliable input retry delay"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum ReliableKeyRejection {
    ClientLedgerUnavailable,
    IdentityAuthorityExhausted,
    PaneUnavailable,
    PaneRegistrationMismatch,
    StaleSerial,
    IdentityConflict,
    OutcomeUnknown,
    InvalidSchedulerConfiguration,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum ReliableKeyOutcome {
    Applied,
    DuplicateApplied,
    Retry(ReliableKeyRetry),
    Rejected(ReliableKeyRejection),
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct ReliableKeyResponse {
    pub pane_id: usize,
    pub input_serial: u64,
    pub outcome: ReliableKeyOutcome,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum ReliableWriteRetry {
    Input(ReliableKeyRetry),
    DefinitelyNotApplied { retry_after_ns: u64 },
}

impl ReliableWriteRetry {
    pub fn no_effect_delay_ns(self) -> Result<Option<u64>, crate::Error> {
        match self {
            Self::Input(retry) => retry.no_effect_delay_ns(),
            Self::DefinitelyNotApplied { retry_after_ns } => {
                validate_retry_delay(retry_after_ns)?;
                Ok(Some(retry_after_ns))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum ReliableWriteRejection {
    Input(ReliableKeyRejection),
    DefinitelyNotApplied,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum ReliableWriteOutcome {
    AppliedPrefix { bytes: u32 },
    DuplicateAppliedPrefix { bytes: u32 },
    Retry(ReliableWriteRetry),
    Rejected(ReliableWriteRejection),
    Indeterminate,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct ReliableWriteResponse {
    pub pane_id: usize,
    pub input_serial: u64,
    pub outcome: ReliableWriteOutcome,
}

#[derive(Serialize)]
pub struct GetPaneIdentity {
    pub pane_id: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct PaneIdentity {
    pub session_incarnation: [u8; 16],
    pub durable_pane_id: [u8; 16],
    pub pane_registration: [u8; 16],
}

/// Only the identity prefix is consumed. Optional coherent render facts are
/// not interpreted as a complete mobile snapshot or a controller lease.
#[derive(Debug, Deserialize)]
pub enum PaneIdentityOutcome {
    Available { identity: PaneIdentity },
    PaneGone,
    Unavailable,
}

#[derive(Debug, Deserialize)]
pub struct PaneIdentityResponse {
    pub pane_id: usize,
    pub outcome: PaneIdentityOutcome,
}

/// The v46 PDU 13 schema (no input serial).
#[derive(Serialize)]
pub struct SendPaste<'a> {
    pub pane_id: usize,
    pub data: &'a str,
}

/// The current PDU 13 schema: `input_serial` is codec's `InputSerial(u64)`.
#[derive(Serialize)]
pub struct SendPasteCurrent<'a> {
    pub pane_id: usize,
    pub data: &'a str,
    pub input_serial: u64,
}

/// `frankenterm_term::input::MouseButton`.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp(usize),
    WheelDown(usize),
    WheelLeft(usize),
    WheelRight(usize),
    None,
}

/// `frankenterm_term::input::MouseEventKind`.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
}

/// `frankenterm_term::input::MouseEvent`: `x` is a column, `y` a row of
/// the screen (not a stable row).
#[derive(Serialize, Debug, Clone, Copy)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub x: usize,
    pub y: i64,
    pub x_pixel_offset: isize,
    pub y_pixel_offset: isize,
    pub button: MouseButton,
    pub modifiers: termwiz::input::Modifiers,
}

/// PDU 12, the same in both dialects.
#[derive(Serialize)]
pub struct SendMouseEvent {
    pub pane_id: usize,
    pub event: MouseEvent,
}

/// PDU 11: a key press the server encodes for the pane's program, in the
/// modes that program set (application cursor keys, keypad, kitty).
/// `input_serial` is codec's `InputSerial(u64)` newtype.
#[derive(Serialize)]
pub struct SendKeyDown {
    pub pane_id: usize,
    pub event: KeyEvent,
    pub input_serial: u64,
}

#[derive(Serialize)]
pub struct Resize {
    pub containing_tab_id: usize,
    pub pane_id: usize,
    pub size: TerminalSize,
}

/// `mux::pane::Pattern`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    CaseSensitiveString(String),
    CaseInSensitiveString(String),
    Regex(String),
}

/// PDU 31: matches in a pane's rows `range`, oldest first, at most `limit`.
#[derive(Serialize)]
pub struct SearchScrollback {
    pub pane_id: usize,
    pub pattern: Pattern,
    pub range: Range<StableRowIndex>,
    pub limit: Option<u32>,
}

/// `mux::pane::SearchResult`: from `start_x` of row `start_y` to `end_x` of
/// row `end_y`, in cells.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchResult {
    pub start_y: StableRowIndex,
    pub start_x: usize,
    pub end_y: StableRowIndex,
    pub end_x: usize,
    pub match_id: usize,
}

/// PDU 32.
#[derive(Deserialize, Debug)]
pub struct SearchResults {
    pub results: Vec<SearchResult>,
}

/// `frankenterm_term::TerminalSize`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

/// `mux::renderable::StableCursorPosition`.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub x: usize,
    pub y: StableRowIndex,
    pub shape: CursorShape,
    pub visibility: CursorVisibility,
}

/// `mux::renderable::RenderableDimensions`.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub cols: usize,
    pub viewport_rows: usize,
    pub scrollback_rows: usize,
    pub physical_top: StableRowIndex,
    pub scrollback_top: StableRowIndex,
    pub dpi: u32,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub reverse_video: bool,
}

/// `mux::renderable::PaneTieredScrollbackStatus`: read only to stay aligned.
#[allow(dead_code, clippy::struct_excessive_bools)]
#[derive(Deserialize, Debug)]
pub struct TieredScrollbackStatus {
    tiering_enabled: bool,
    configured_scrollback_rows: usize,
    configured_hot_lines: usize,
    configured_warm_max_bytes: usize,
    visible_rows: usize,
    in_memory_scrollback_rows: usize,
    warm_resident_lines: usize,
    warm_resident_bytes: usize,
    warm_spill_lines_total: u64,
    warm_spill_bytes_total: u64,
    cold_spill_lines_total: u64,
    cold_spill_bytes_total: u64,
    cold_sink_retained_lines: usize,
    cold_sink_retained_bytes: usize,
    cold_worker_peak_backlog_depth: usize,
    cold_worker_completion_throughput_lines_per_sec: u64,
    cold_worker_completed_lines_total: u64,
    cold_worker_completed_batches_total: u64,
    cold_worker_cancellation_count: u64,
}

/// The `lines` and `hyperlinks` fields that open codec's `SerializedLines`.
/// The server takes each link out of the cells it covers and sends it once,
/// with those cells; `linked_lines` puts it back.
#[derive(Deserialize, Debug, Default)]
pub struct LineEntries {
    pub lines: Vec<(StableRowIndex, Line)>,
    hyperlinks: Vec<LineHyperlink>,
}

#[derive(Deserialize, Debug)]
struct LineHyperlink {
    link: Hyperlink,
    coords: Vec<CellCoordinates>,
}

/// Cells `cols` of the `line_idx`th entry in `lines`.
#[derive(Deserialize, Debug)]
struct CellCoordinates {
    line_idx: usize,
    cols: Range<usize>,
}

impl LineEntries {
    /// The lines with their links back on their cells, as the desktop's
    /// `SerializedLines::extract_data` does: a span past its line is skipped.
    pub fn linked_lines(self) -> Vec<(StableRowIndex, Line)> {
        let mut lines = self.lines;
        for hyperlink in self.hyperlinks {
            let shared = Arc::new(hyperlink.link);
            for coord in hyperlink.coords {
                let Some((_, line)) = lines.get_mut(coord.line_idx) else {
                    continue;
                };
                if let Some(cells) = line.cells_mut_for_attr_changes_only().get_mut(coord.cols) {
                    for cell in cells {
                        cell.attrs_mut().set_hyperlink(Some(Arc::clone(&shared)));
                    }
                }
            }
        }
        lines
    }
}

/// PDU 25 up to its lines, which still decodes when the lines do not.
#[derive(Deserialize, Debug)]
pub struct RenderHead {
    pub pane_id: usize,
    pub mouse_grabbed: bool,
    pub alt_screen_active: bool,
    pub cursor_position: CursorPosition,
    pub dimensions: Dimensions,
    pub tiered_scrollback_status: Option<TieredScrollbackStatus>,
    pub dirty_lines: Vec<Range<StableRowIndex>>,
    pub title: String,
    pub working_dir: Option<String>,
}

/// Prefix of PDU 25, a reply to GetPaneRenderChanges or a pushed update.
/// Varbincode writes a struct as its fields alone, so the nested head is
/// byte for byte the flat v46 layout.
#[derive(Deserialize, Debug)]
pub struct RenderChanges {
    pub head: RenderHead,
    pub bonus_lines: LineEntries,
}

/// Prefix of PDU 23.
#[derive(Deserialize, Debug)]
pub struct LinesResponse {
    pub pane_id: usize,
    pub lines: LineEntries,
}

#[derive(Deserialize, Debug)]
pub struct LivenessResponse {
    pub pane_id: usize,
    pub is_alive: bool,
}

/// PDU 0 from a current server: codec's structured `ErrorResponse`, each
/// newtype code its byte. (A v46 server sends the error's text instead.)
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorResponse {
    pub schema_version: u8,
    pub request_ident: u64,
    pub code: u16,
    pub object: Option<ErrorObject>,
    pub effect: u8,
    pub retry: u8,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorObject {
    pub kind: u8,
    pub id: u64,
}

impl ErrorResponse {
    /// codec's `MuxErrorEffect::NOT_APPLIED`.
    pub const NOT_APPLIED: u8 = 1;
    /// codec's `MuxErrorRetry::SAFE_AFTER_BACKOFF`.
    pub const SAFE_AFTER_BACKOFF: u8 = 2;

    /// The server applied nothing and the same request may go again after
    /// a delay (busy render metadata, a full scheduler, a backend failure).
    #[must_use]
    pub fn retry_after_backoff(&self) -> bool {
        self.effect == Self::NOT_APPLIED && self.retry == Self::SAFE_AFTER_BACKOFF
    }
}

#[derive(Deserialize, Debug)]
pub struct PaneRemoved {
    pub pane_id: usize,
}

/// PDU 81. `supported` and `required` are codec's `TopologyCapabilities`
/// bits.
#[derive(Serialize)]
pub struct ListPanesCoherent {
    pub supported: u64,
    pub required: u64,
}

/// PDU 82.
#[derive(Deserialize, Debug)]
pub struct ListPanesCoherentResponse {
    pub negotiated: u64,
    pub stream_id: [u8; 16],
    pub outcome: CoherentOutcome,
}

#[derive(Deserialize, Debug)]
pub enum CoherentOutcome {
    Snapshot(CoherentSnapshot),
    /// The panes kept changing while the server listed them.
    Contended {
        attempts: u8,
        first_revision: u64,
        last_revision: u64,
    },
    RevisionExhausted,
    Unsupported {
        supported: u64,
    },
}

/// `codec::CoherentPaneSnapshot`: `session_incarnation` is the same for as
/// long as the server runs, and new after it restarts, even when the
/// restarted server numbers its panes as before.
#[derive(Deserialize, Debug)]
pub struct CoherentSnapshot {
    pub session_incarnation: [u8; 16],
    pub snapshot_revision: u64,
    pub panes: PaneList,
}

/// PDU 83.
#[derive(Deserialize, Debug)]
pub struct TopologyEvent {
    pub stream_id: [u8; 16],
    pub revision: u64,
    pub event: TopologyChange,
}

/// `codec::TopologyEventKind`, in its order.
#[derive(Deserialize, Debug)]
pub enum TopologyChange {
    PaneAdded {
        pane_id: usize,
    },
    FloatingPaneSpawned {
        pane_id: usize,
        tab_id: usize,
        window_id: usize,
    },
    PaneRemoved {
        pane_id: usize,
    },
    WindowCreated {
        window_id: usize,
    },
    WindowRemoved {
        window_id: usize,
    },
    WindowInvalidated {
        window_id: usize,
    },
    WindowWorkspaceChanged {
        window_id: usize,
        workspace: Option<String>,
    },
    Empty,
    TabAddedToWindow {
        tab_id: usize,
        window_id: usize,
    },
    PaneFocused {
        pane_id: usize,
    },
    TabResized {
        tab_id: usize,
    },
    TabTitleChanged {
        tab_id: usize,
        title: String,
    },
    WindowTitleChanged {
        window_id: usize,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

/// The exact v46 PDU 4 schema.
#[derive(Deserialize, Debug)]
pub struct PaneList {
    pub tabs: Vec<PaneNode>,
    pub tab_titles: Vec<String>,
    pub window_titles: HashMap<usize, String>,
}

/// v46 `mux::tab::PaneNode`: a split's geometry follows both children.
#[derive(Deserialize, Debug)]
pub enum PaneNode {
    Empty,
    Split {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
}

#[derive(Deserialize, Debug, Clone, Copy)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Deserialize, Debug, Clone, Copy)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

/// The exact v46 `mux::tab::PaneEntry` field order.
#[allow(clippy::struct_excessive_bools)]
#[derive(Deserialize, Debug, Clone)]
pub struct PaneEntry {
    pub window_id: usize,
    pub tab_id: usize,
    pub pane_id: usize,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<String>,
    pub alt_screen_active: bool,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    pub workspace: String,
    pub cursor_pos: CursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub tty_name: Option<String>,
}

impl PaneNode {
    /// Every pane in this tab, left to right / top to bottom.
    pub fn leaves<'a>(&'a self, out: &mut Vec<&'a PaneEntry>) {
        match self {
            Self::Empty => {}
            Self::Split { left, right, .. } => {
                left.leaves(out);
                right.leaves(out);
            }
            Self::Leaf(entry) => out.push(entry),
        }
    }
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    // Serializing these plain structs into memory cannot fail.
    varbincode::serialize(value).unwrap_or_default()
}

/// Decodes a reply (or its prefix) from the server, never trusting its
/// lengths beyond the bytes it actually sent.
pub fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, crate::Error> {
    crate::varbin::deserialize(body).map_err(|_| crate::Error::Schema)
}

/// Reliable input receipts have a fixed small schema and must consume the
/// complete body before they can settle an operation.
pub fn decode_input_receipt<T: serde::de::DeserializeOwned>(
    body: &[u8],
) -> Result<T, crate::Error> {
    if body.len() > 4 * 1024 {
        return Err(crate::Error::Frame("an input receipt is too large"));
    }
    crate::varbin::deserialize_exact(body).map_err(|_| crate::Error::Schema)
}
