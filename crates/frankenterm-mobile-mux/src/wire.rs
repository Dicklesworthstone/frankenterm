//! The mux protocol exactly as codec v46 (FrankenTerm v0.13) frames it, the
//! dialect the fleet's mux servers speak, and as the current codec window
//! does where it differs: the requests the phone sends and the prefixes of
//! the replies it reads. Varbincode is positional, so field order and types
//! match `codec` and `mux` byte for byte; the tests compare them with the
//! desktop codec's own encoding and decoding in both dialects.
//!
//! A reply struct may stop before the reply's last field: decoding reads a
//! prefix and ignores the rest (image references, input serials and
//! sequence numbers, which the phone does not use).

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
/// CODEC_VERSION`. Every PDU the phone uses has kept one schema across it.
/// Version 71 is additive; it does not change the layouts used by this client.
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
    pub fn of(codec_version: usize) -> Option<Self> {
        if LEGACY_VERSIONS.contains(&codec_version) {
            Some(Self::Legacy)
        } else if CURRENT_VERSIONS.contains(&codec_version) {
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
