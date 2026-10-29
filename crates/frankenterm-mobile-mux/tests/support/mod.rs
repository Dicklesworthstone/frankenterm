//! A v46 mux server over one real terminal, answering the way the v0.13
//! `frankenterm-mux-server` does (sessionhandler `compute_changes`, GetLines,
//! pushes with serial 0), and the phone's terminal it draws on.
//!
//! The server reads every phone request with the desktop codec's own
//! Legacy46 decoder, so a request it understands is one a v46 server
//! accepts; it writes replies from the desktop codec's structs, which v46
//! encodes identically for PDUs 23, 25, 27, 30 and 37, and PDU 4 from
//! copies of the v0.13 `mux::tab` types. Announcing a current codec, it
//! answers the coherent pane list (PDU 82) as a current server does and
//! then pushes topology changes as PDU 83.
#![allow(dead_code)]

use codec::{
    CoherentPaneSnapshot, GetCodecVersionResponse, GetLines, GetLinesResponse,
    GetPaneRenderChanges, GetPaneRenderChangesResponse, ListPanesCoherentOutcome,
    ListPanesCoherentResponse, ListPanesResponse, LivenessResponse, MuxWireDecodedPayload,
    MuxWireDialect, PaneRemoved, Pdu, Resize, SearchScrollbackRequest, SearchScrollbackResponse,
    SendKeyDown, TabAddedToWindow, TopologyCapabilities, TopologyEvent, TopologyEventKind,
    TopologyStreamId, UnitResponse, WriteToPane,
};
use frankenterm_mobile_mux::{Event, Session};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::config::TerminalFeatureProfile;
use frankenterm_term::{
    CellAttributes, Line, StableRowIndex, Terminal, TerminalConfiguration, TerminalSize,
};
use mux::pane::{Pattern, SearchResult};
use mux::renderable::{
    RenderableDimensions, StableCursorPosition, terminal_get_cursor_position,
    terminal_get_dimensions, terminal_get_dirty_lines, terminal_get_lines,
};
use mux::tab::SerdeUrl;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

/// A desktop pane's terminal, or with `NativeText` the phone's, which has
/// a work budget for each piece of output it takes.
#[derive(Debug)]
struct Config {
    profile: TerminalFeatureProfile,
    scrollback: usize,
}

impl Config {
    /// The phone's model keeps the history the app gives it (256 rows,
    /// `NativeTerminalModel`), and only those that fit beside both screens
    /// in `native::MAX_RESIDENT_CELLS` (`retained_scrollback_rows`).
    fn phone(rows: usize, cols: usize) -> Self {
        Self {
            profile: TerminalFeatureProfile::NativeText,
            scrollback: 256.min(65_536usize.saturating_sub(2 * rows * cols) / cols.max(1)),
        }
    }
}

impl TerminalConfiguration for Config {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn feature_profile(&self) -> TerminalFeatureProfile {
        self.profile
    }
}

/// The most the phone's terminal takes at once (`native::MAX_FEED_BYTES`).
const PHONE_FEED_BYTES: usize = 16 * 1024;

pub fn size(rows: usize, cols: usize) -> TerminalSize {
    TerminalSize {
        rows,
        cols,
        pixel_width: cols * 8,
        pixel_height: rows * 16,
        dpi: 96,
    }
}

pub fn terminal(rows: usize, cols: usize) -> Terminal {
    let config = Config {
        profile: TerminalFeatureProfile::Full,
        scrollback: 5000,
    };
    terminal_with(rows, cols, config)
}

/// The phone's terminal: the native profile and history its model runs.
pub fn phone_terminal(rows: usize, cols: usize) -> Terminal {
    terminal_with(rows, cols, Config::phone(rows, cols))
}

/// Resizes the phone's terminal as its model does, keeping the history
/// that still fits.
pub fn resize_phone(phone: &mut Terminal, rows: usize, cols: usize) {
    phone.resize(size(rows, cols));
    phone.set_config(Arc::new(Config::phone(rows, cols)));
    phone.trim_scrollback_to_config();
}

fn terminal_with(rows: usize, cols: usize, config: Config) -> Terminal {
    Terminal::new(
        size(rows, cols),
        Arc::new(config),
        "frankenterm-mobile-mux-test",
        "0",
        Box::new(std::io::sink()),
    )
}

/// The v0.13 `mux::tab` pane tree, copied field for field.
#[derive(Serialize)]
struct V46PaneList {
    tabs: Vec<V46PaneNode>,
    tab_titles: Vec<String>,
    window_titles: HashMap<usize, String>,
}

#[derive(Serialize)]
pub enum V46PaneNode {
    Empty,
    Split {
        left: Box<V46PaneNode>,
        right: Box<V46PaneNode>,
        node: V46SplitDirectionAndSize,
    },
    Leaf(V46PaneEntry),
}

#[derive(Serialize)]
pub enum V46SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Serialize)]
pub struct V46SplitDirectionAndSize {
    pub direction: V46SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

#[derive(Serialize)]
pub struct V46PaneEntry {
    pub window_id: usize,
    pub tab_id: usize,
    pub pane_id: usize,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    pub alt_screen_active: bool,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    pub workspace: String,
    pub cursor_pos: StableCursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub tty_name: Option<String>,
}

pub fn v46_pane_list(tabs: Vec<V46PaneNode>, tab_titles: Vec<String>) -> Vec<u8> {
    let list = V46PaneList {
        tabs,
        tab_titles,
        window_titles: HashMap::from([(0, "window".to_owned())]),
    };
    varbincode::serialize(&list).expect("pane list serializes")
}

/// Frames a reply the way the server does: bodies over 32 bytes are zstd
/// compressed when `compress` is set (codec `CompressionMode::Auto`).
pub fn server_frame(ident: u64, serial: u64, body: &[u8], compress: bool) -> Vec<u8> {
    let (body, flag) = if compress && body.len() > 32 {
        (zstd::bulk::compress(body, 3).expect("compress"), 1u64 << 63)
    } else {
        (body.to_vec(), 0)
    };
    let mut ids = Vec::new();
    leb128::write::unsigned(&mut ids, serial).expect("leb");
    leb128::write::unsigned(&mut ids, ident).expect("leb");
    let mut frame = Vec::new();
    leb128::write::unsigned(&mut frame, (ids.len() + body.len()) as u64 | flag).expect("leb");
    frame.extend_from_slice(&ids);
    frame.extend_from_slice(&body);
    frame
}

#[derive(Debug, Clone, PartialEq)]
struct Pushed {
    dims: RenderableDimensions,
    cursor: StableCursorPosition,
    alt: bool,
    title: String,
}

pub struct Server {
    pub term: Terminal,
    pub pane_id: usize,
    pub tab_id: usize,
    pub codec_vers: usize,
    /// The oldest version it accepts, which its PDU 27 reply ends with.
    pub min_supported: usize,
    pub compress: bool,
    pub title: String,
    pub written: Vec<u8>,
    pub pastes: Vec<String>,
    pub keys: Vec<termwiz::input::KeyEvent>,
    pub mouse: Vec<frankenterm_term::input::MouseEvent>,
    pub resizes: Vec<TerminalSize>,
    /// Search replies expand past the phone's frame limit.
    pub oversize_search: bool,
    /// The pane's program has asked for the mouse.
    pub mouse_grabbed: bool,
    pub requests: Vec<&'static str>,
    /// GetLines requests still to answer with an ErrorResponse.
    pub refuse_lines: usize,
    /// GetPaneRenderChanges requests still to refuse, in order: a current
    /// server's structured rejection, or (`None`) a v46 server's text.
    pub refuse_render: std::collections::VecDeque<Option<codec::ErrorResponse>>,
    /// The row ranges GetLines asked for, in order.
    pub line_ranges: Vec<std::ops::Range<StableRowIndex>>,
    /// The row ranges searches asked for, in order.
    pub search_ranges: Vec<std::ops::Range<StableRowIndex>>,
    /// No push follows a resize (the v0.13 handler pushes nothing itself;
    /// the program's redraw usually does).
    pub quiet_resize: bool,
    /// A current server's incarnation, in its coherent pane list.
    pub incarnation: [u8; 16],
    /// Exact live pane authority; changing this simulates reuse of its number.
    pub registration: [u8; 16],
    pub durable_identity: bool,
    pub client_ids: Vec<mux::client::ClientId>,
    /// Limit a reliable writer to this many bytes per acknowledged prefix.
    pub write_prefix: Option<usize>,
    /// A current server answers a coherent list without one.
    pub unfenced: bool,
    /// After a coherent list: topology changes go as PDU 83, numbered.
    fenced: bool,
    revision: u64,
    subscribed: bool,
    pushed: Option<Pushed>,
    seqno: termwiz::surface::SequenceNo,
    inbound: Vec<u8>,
    pub to_phone: Vec<u8>,
}

impl Server {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            term: terminal(rows, cols),
            pane_id: 7,
            tab_id: 3,
            codec_vers: 46,
            min_supported: 46,
            compress: false,
            title: "agent".to_owned(),
            written: Vec::new(),
            pastes: Vec::new(),
            keys: Vec::new(),
            mouse: Vec::new(),
            resizes: Vec::new(),
            oversize_search: false,
            mouse_grabbed: false,
            requests: Vec::new(),
            refuse_lines: 0,
            refuse_render: std::collections::VecDeque::new(),
            line_ranges: Vec::new(),
            search_ranges: Vec::new(),
            quiet_resize: false,
            incarnation: [0x1c; 16],
            registration: [0x27; 16],
            durable_identity: true,
            client_ids: Vec::new(),
            write_prefix: None,
            unfenced: false,
            fenced: false,
            revision: 0,
            subscribed: false,
            pushed: None,
            seqno: 0,
            inbound: Vec::new(),
            to_phone: Vec::new(),
        }
    }

    fn reply<T: Serialize>(&mut self, ident: u64, serial: u64, body: &T) {
        let body = varbincode::serialize(body).expect("reply serializes");
        let frame = server_frame(ident, serial, &body, self.compress);
        self.to_phone.extend_from_slice(&frame);
    }

    /// The layout this server reads: v46, or the current one from codec 61.
    /// A server newer than this codec reads the newest layout this codec
    /// has, as a newer server keeps its PDUs within its accepted window.
    /// (A version outside both reads the protocol check, which is the same
    /// in every dialect, and nothing after it.)
    fn dialect(&self) -> MuxWireDialect {
        MuxWireDialect::current(self.codec_vers.min(codec::CODEC_VERSION))
            .unwrap_or(MuxWireDialect::LEGACY46)
    }

    /// Phone bytes, split into frames by the desktop codec's decoder for the
    /// dialect this server announced.
    pub fn receive(&mut self, bytes: &[u8]) {
        self.inbound.extend_from_slice(bytes);
        while !self.inbound.is_empty() {
            let mut cursor = std::io::Cursor::new(self.inbound.as_slice());
            let decoded = Pdu::decode_for_dialect(&mut cursor, self.dialect())
                .expect("the phone's request is a frame of the server's dialect");
            let used = usize::try_from(cursor.position()).expect("position");
            self.inbound.drain(..used);
            let (serial, payload) = decoded.into_parts();
            self.handle(serial, payload);
        }
    }

    fn handle(&mut self, serial: u64, payload: MuxWireDecodedPayload) {
        match payload {
            MuxWireDecodedPayload::Pdu(Pdu::GetCodecVersion(_)) => {
                self.requests.push("GetCodecVersion");
                let reply = GetCodecVersionResponse {
                    codec_vers: self.codec_vers,
                    version_string: "20260727-v0.13.0".to_owned(),
                    executable_path: "/home/u/.local/bin/frankenterm-mux-server".into(),
                    config_file_path: None,
                    min_supported: self.min_supported,
                };
                self.reply(27, serial, &reply);
            }
            MuxWireDecodedPayload::Pdu(Pdu::ListPanes(_)) => {
                self.requests.push("ListPanes");
                let body = v46_pane_list(
                    vec![V46PaneNode::Leaf(self.entry())],
                    vec!["tab one".to_owned()],
                );
                let frame = server_frame(4, serial, &body, self.compress);
                self.to_phone.extend_from_slice(&frame);
            }
            MuxWireDecodedPayload::Pdu(Pdu::SetClientId(request)) => {
                self.requests.push("SetClientId");
                assert!(!request.is_proxy);
                self.client_ids.push(request.client_id);
                self.reply(10, serial, &UnitResponse {});
            }
            MuxWireDecodedPayload::Pdu(Pdu::GetPaneIdentityV1(request)) => {
                self.requests.push("GetPaneIdentityV1");
                let outcome = if request.pane_id != self.pane_id {
                    codec::PaneIdentityOutcomeV1::PaneGone
                } else if !self.durable_identity {
                    codec::PaneIdentityOutcomeV1::Unavailable
                } else {
                    codec::PaneIdentityOutcomeV1::Available {
                        identity: codec::PaneIdentityV1 {
                            session_incarnation: mux::MuxSessionIncarnation::from_bytes(
                                self.incarnation,
                            ),
                            durable_pane_id: [0xd1; 16],
                            pane_registration:
                                codec::ReliablePaneRegistrationIdentityV1::from_bytes(
                                    self.registration,
                                ),
                        },
                        render_facts: None,
                    }
                };
                self.reply(
                    118,
                    serial,
                    &codec::GetPaneIdentityV1Response {
                        pane_id: request.pane_id,
                        outcome,
                    },
                );
            }
            MuxWireDecodedPayload::Pdu(Pdu::ReliableKeyEventV1(request)) => {
                self.requests.push("ReliableKeyEventV1");
                use codec::{
                    ReliableKeyEventOutcomeV1 as Outcome, ReliableKeyEventRejectionV1 as Rejected,
                };
                let registration =
                    codec::ReliablePaneRegistrationIdentityV1::from_bytes(self.registration);
                let outcome = if request.pane_id != self.pane_id {
                    Outcome::Rejected(Rejected::PaneUnavailable)
                } else if request.pane_registration.is_none() {
                    Outcome::Retry(codec::ReliableKeyEventRetryV1::PaneAuthorityRequired {
                        pane_registration: registration,
                    })
                } else if request.pane_registration != Some(registration) {
                    Outcome::Rejected(Rejected::PaneRegistrationMismatch)
                } else if self.client_ids.is_empty() {
                    Outcome::Rejected(Rejected::ClientLedgerUnavailable)
                } else {
                    assert!(!request.input_serial.is_empty());
                    self.keys.push(request.event);
                    Outcome::Applied
                };
                self.reply(
                    97,
                    serial,
                    &codec::ReliableKeyEventV1Response {
                        pane_id: request.pane_id,
                        input_serial: request.input_serial,
                        outcome,
                    },
                );
            }
            MuxWireDecodedPayload::Pdu(Pdu::ReliablePaneWriteV1(request)) => {
                self.requests.push("ReliablePaneWriteV1");
                use codec::{
                    ReliableKeyEventRejectionV1 as InputRejected,
                    ReliablePaneWriteOutcomeV1 as Outcome,
                    ReliablePaneWriteRejectionV1 as Rejected,
                };
                let expected =
                    codec::ReliablePaneRegistrationIdentityV1::from_bytes(self.registration);
                let outcome = if request.pane_id != self.pane_id {
                    Outcome::Rejected(Rejected::Input(InputRejected::PaneUnavailable))
                } else if request.pane_registration != Some(expected) {
                    Outcome::Rejected(Rejected::Input(InputRejected::PaneRegistrationMismatch))
                } else if self.client_ids.is_empty() {
                    Outcome::Rejected(Rejected::Input(InputRejected::ClientLedgerUnavailable))
                } else {
                    assert!(!request.input_serial.is_empty());
                    assert!(!request.data.is_empty());
                    assert!(request.data.len() <= codec::MAX_RELIABLE_PANE_WRITE_DATA_BYTES);
                    let count = self
                        .write_prefix
                        .unwrap_or(request.data.len())
                        .min(request.data.len());
                    self.written.extend_from_slice(&request.data[..count]);
                    Outcome::AppliedPrefix {
                        bytes: count as u32,
                    }
                };
                self.reply(
                    101,
                    serial,
                    &codec::ReliablePaneWriteV1Response {
                        pane_id: request.pane_id,
                        input_serial: request.input_serial,
                        outcome,
                    },
                );
            }
            MuxWireDecodedPayload::Pdu(Pdu::ListPanesCoherent(request)) => {
                self.requests.push("ListPanesCoherent");
                let fenced = TopologyCapabilities::FENCED_SNAPSHOT_V1;
                let outcome = if self.unfenced || !request.supported.contains(fenced) {
                    ListPanesCoherentOutcome::Unsupported {
                        supported: TopologyCapabilities::NONE,
                    }
                } else {
                    self.fenced = true;
                    ListPanesCoherentOutcome::Snapshot(CoherentPaneSnapshot {
                        session_incarnation: mux::MuxSessionIncarnation::from_bytes(
                            self.incarnation,
                        ),
                        snapshot_revision: mux::TopologyRevision::new(self.revision),
                        panes: ListPanesResponse {
                            tabs: vec![mux::tab::PaneNode::Leaf(self.current_entry())],
                            tab_titles: vec!["tab one".to_owned()],
                            window_titles: HashMap::from([(0, "window".to_owned())]),
                            floating_panes: Vec::new(),
                        },
                    })
                };
                let reply = ListPanesCoherentResponse {
                    negotiated: if self.fenced {
                        fenced
                    } else {
                        TopologyCapabilities::NONE
                    },
                    stream_id: TopologyStreamId::from_bytes([0x5a; 16]),
                    outcome,
                };
                self.reply(82, serial, &reply);
            }
            MuxWireDecodedPayload::Pdu(Pdu::GetPaneRenderChanges(GetPaneRenderChanges {
                pane_id,
            })) => {
                self.requests.push("GetPaneRenderChanges");
                if let Some(refusal) = self.refuse_render.pop_front() {
                    match refusal {
                        Some(error) => self.reply(0, serial, &error),
                        None => self.reply(0, serial, &"Error: no such pane".to_owned()),
                    }
                    return;
                }
                let is_alive = pane_id == self.pane_id;
                if is_alive {
                    self.subscribed = true;
                    self.push();
                }
                self.reply(30, serial, &LivenessResponse { pane_id, is_alive });
            }
            MuxWireDecodedPayload::Pdu(Pdu::GetLines(GetLines { pane_id, lines })) => {
                self.requests.push("GetLines");
                self.line_ranges.extend(lines.iter().cloned());
                assert_eq!(pane_id, self.pane_id);
                if self.refuse_lines > 0 {
                    self.refuse_lines -= 1;
                    // v46 PDU 0 is the error text alone.
                    self.reply(0, serial, &"Error: no such pane".to_owned());
                    return;
                }
                let mut entries = Vec::new();
                for range in lines {
                    let (first, found) = terminal_get_lines(&mut self.term, range);
                    for (index, mut line) in found.into_iter().enumerate() {
                        line.compress_for_scrollback();
                        entries.push((first + index as StableRowIndex, line));
                    }
                }
                let reply = GetLinesResponse {
                    pane_id,
                    lines: entries.into(),
                };
                self.reply(23, serial, &reply);
            }
            MuxWireDecodedPayload::Pdu(Pdu::WriteToPane(WriteToPane { data, .. })) => {
                self.requests.push("WriteToPane");
                self.written.extend_from_slice(&data);
                self.push();
                self.reply(10, serial, &UnitResponse {});
            }
            MuxWireDecodedPayload::Legacy46SendPaste(paste) => {
                self.requests.push("SendPaste");
                self.pastes.push(paste.data().to_owned());
                self.push();
                self.reply(10, serial, &UnitResponse {});
            }
            MuxWireDecodedPayload::Pdu(Pdu::SendPaste(paste)) => {
                self.requests.push("SendPaste");
                self.pastes.push(paste.data);
                self.push();
                self.reply(10, serial, &UnitResponse {});
            }
            MuxWireDecodedPayload::Pdu(Pdu::SendMouseEvent(codec::SendMouseEvent {
                event,
                ..
            })) => {
                self.requests.push("SendMouseEvent");
                self.mouse.push(event);
                self.push();
                self.reply(10, serial, &UnitResponse {});
            }
            MuxWireDecodedPayload::Pdu(Pdu::SendKeyDown(SendKeyDown { event, .. })) => {
                self.requests.push("SendKeyDown");
                self.keys.push(event);
                self.push();
                self.reply(10, serial, &UnitResponse {});
            }
            MuxWireDecodedPayload::Pdu(Pdu::Resize(Resize { size, .. })) => {
                self.requests.push("Resize");
                self.resizes.push(size);
                self.term.resize(size);
                self.reply(10, serial, &UnitResponse {});
                if !self.quiet_resize {
                    self.push();
                }
            }
            MuxWireDecodedPayload::Pdu(Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pattern,
                range,
                limit,
                ..
            })) => {
                self.requests.push("SearchScrollback");
                self.search_ranges.push(range.clone());
                if self.oversize_search {
                    // A reply that expands past 16 MiB (a pattern matching
                    // nearly every cell), compressed as the server sends it.
                    let body = vec![0u8; 16 * 1024 * 1024 + 1];
                    let frame = server_frame(32, serial, &body, true);
                    self.to_phone.extend_from_slice(&frame);
                    return;
                }
                // The v0.13 LocalPane::search, for plain text: rows in range
                // oldest first, stopping at the limit. A regex here is matched
                // as its literal text, and "(" is the invalid one.
                let (needle, folded) = match pattern {
                    Pattern::CaseSensitiveString(text) => (text, false),
                    Pattern::CaseInSensitiveString(text) => (text.to_lowercase(), true),
                    Pattern::Regex(text) if text == "(" => {
                        self.reply(
                            0,
                            serial,
                            &"Error: regex parse error: unclosed group".to_owned(),
                        );
                        return;
                    }
                    Pattern::Regex(text) => (text, false),
                };
                let (first, lines) = terminal_get_lines(&mut self.term, range);
                let mut results = Vec::new();
                'rows: for (index, line) in lines.iter().enumerate() {
                    let text = line.as_str().to_string();
                    let haystack = if folded {
                        text.to_lowercase()
                    } else {
                        text.clone()
                    };
                    for (at, _) in haystack.match_indices(&needle) {
                        if limit.is_some_and(|limit| results.len() == limit as usize) {
                            break 'rows;
                        }
                        let column = haystack[..at].chars().count();
                        let row = first + index as StableRowIndex;
                        results.push(SearchResult {
                            start_y: row,
                            start_x: column,
                            end_y: row,
                            end_x: column + needle.chars().count(),
                            match_id: 0,
                        });
                    }
                }
                self.reply(32, serial, &SearchScrollbackResponse { results });
            }
            other => panic!("the v0.13 server test does not expect {other:?}"),
        }
    }

    fn entry(&mut self) -> V46PaneEntry {
        let dims = terminal_get_dimensions(&mut self.term);
        V46PaneEntry {
            window_id: 0,
            tab_id: self.tab_id,
            pane_id: self.pane_id,
            title: self.title.clone(),
            size: self.term.get_size(),
            working_dir: Some(
                SerdeUrl::try_from("file:///home/u/project".to_owned()).expect("url"),
            ),
            alt_screen_active: self.term.is_alt_screen_active(),
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: "default".to_owned(),
            cursor_pos: terminal_get_cursor_position(&mut self.term),
            physical_top: dims.physical_top,
            top_row: 0,
            left_col: 0,
            tty_name: Some("/dev/pts/4".to_owned()),
        }
    }

    /// The same pane as the current `mux::tab` type.
    fn current_entry(&mut self) -> mux::tab::PaneEntry {
        let V46PaneEntry {
            window_id,
            tab_id,
            pane_id,
            title,
            size,
            working_dir,
            alt_screen_active,
            is_active_pane,
            is_zoomed_pane,
            workspace,
            cursor_pos,
            physical_top,
            top_row,
            left_col,
            tty_name,
        } = self.entry();
        mux::tab::PaneEntry {
            window_id,
            tab_id,
            pane_id,
            title,
            size,
            working_dir,
            alt_screen_active,
            is_active_pane,
            is_zoomed_pane,
            workspace,
            cursor_pos,
            physical_top,
            top_row,
            left_col,
            tty_name,
        }
    }

    /// A topology change as a current server pushes it after a coherent list.
    pub fn topology(&mut self, event: TopologyEventKind) {
        self.revision += 1;
        let event = TopologyEvent {
            stream_id: TopologyStreamId::from_bytes([0x5a; 16]),
            revision: mux::TopologyRevision::new(self.revision),
            event,
        };
        self.reply(83, 0, &event);
    }

    /// The v0.13 `maybe_push_pane_changes`: dirty viewport rows travel as
    /// bonus lines, plus the cursor's row.
    pub fn push(&mut self) {
        if !self.subscribed {
            return;
        }
        let dims = terminal_get_dimensions(&mut self.term);
        let cursor = terminal_get_cursor_position(&mut self.term);
        let alt = self.term.is_alt_screen_active();
        let state = Pushed {
            dims,
            cursor,
            alt,
            title: self.title.clone(),
        };
        let viewport = dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex;
        let mut dirty = terminal_get_dirty_lines(&mut self.term, viewport.clone(), self.seqno);
        self.seqno = self.term.current_seqno();
        if dirty.iter().next().is_none() && self.pushed.as_ref() == Some(&state) {
            return;
        }
        let (first, lines) = terminal_get_lines(&mut self.term, viewport);
        let mut bonus = Vec::new();
        for (index, mut line) in lines.into_iter().enumerate() {
            let row = first + index as StableRowIndex;
            if dirty.contains(row) {
                dirty.remove(row);
                line.compress_for_scrollback();
                bonus.push((row, line));
            }
        }
        let (cursor_row, cursor_lines) = terminal_get_lines(&mut self.term, cursor.y..cursor.y + 1);
        if let Some(mut line) = cursor_lines.into_iter().next() {
            line.compress_for_scrollback();
            bonus.push((cursor_row, line));
        }
        let reply = GetPaneRenderChangesResponse {
            pane_id: self.pane_id,
            mouse_grabbed: self.mouse_grabbed,
            alt_screen_active: alt,
            cursor_position: cursor,
            dimensions: dims,
            tiered_scrollback_status: None,
            dirty_lines: dirty.iter().cloned().collect(),
            title: self.title.clone(),
            working_dir: None,
            bonus_lines: bonus.into(),
            input_serial: None,
            seqno: self.seqno,
        };
        self.pushed = Some(state);
        self.reply(25, 0, &reply);
    }

    pub fn remove_pane(&mut self) {
        let pane_id = self.pane_id;
        if self.fenced {
            self.topology(TopologyEventKind::PaneRemoved { pane_id });
        } else {
            self.reply(37, 0, &PaneRemoved { pane_id });
        }
    }

    /// The notice every client gets when the desktop opens a tab.
    pub fn notify_tab_added(&mut self) {
        let (tab_id, window_id) = (self.tab_id + 1, 0);
        if self.fenced {
            self.topology(TopologyEventKind::TabAddedToWindow { tab_id, window_id });
        } else {
            self.reply(55, 0, &TabAddedToWindow { tab_id, window_id });
        }
    }
}

/// The server, a session, and the phone terminal the session draws on.
pub struct Harness {
    pub server: Server,
    pub session: Session,
    pub phone: Terminal,
    pub events: Vec<Event>,
    /// Phone-bound bytes are delivered in pieces of this size.
    pub chunk: usize,
    pub outputs: usize,
    /// The server's replies wait (in order) until this is cleared: they are
    /// on their way while the phone does something else.
    pub hold: bool,
    /// The phone's requests wait likewise, so the server keeps pushing
    /// while they are on their way.
    pub hold_requests: bool,
    held_requests: Vec<u8>,
}

impl Harness {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            server: Server::new(rows, cols),
            session: Session::new(),
            phone: phone_terminal(rows, cols),
            events: Vec::new(),
            chunk: usize::MAX,
            outputs: 0,
            hold: false,
            hold_requests: false,
            held_requests: Vec::new(),
        }
    }

    pub fn pump(&mut self) {
        loop {
            self.held_requests
                .extend_from_slice(&self.session.take_outgoing());
            let outgoing = if self.hold_requests {
                Vec::new()
            } else {
                std::mem::take(&mut self.held_requests)
            };
            let mut progressed = !outgoing.is_empty();
            if progressed {
                self.server.receive(&outgoing);
            }
            let incoming = if self.hold {
                Vec::new()
            } else {
                std::mem::take(&mut self.server.to_phone)
            };
            if !incoming.is_empty() {
                progressed = true;
                for piece in incoming.chunks(self.chunk.max(1)) {
                    self.session.receive(piece);
                }
            }
            while let Some(event) = self.session.next_event() {
                if let Event::Output {
                    rows, cols, bytes, ..
                } = &event
                {
                    if (self.phone.get_size().rows, self.phone.get_size().cols) != (*rows, *cols) {
                        resize_phone(&mut self.phone, *rows, *cols);
                    }
                    for piece in bytes.chunks(PHONE_FEED_BYTES) {
                        self.phone.advance_bytes(piece);
                        // The phone's model ends the session on this.
                        assert!(
                            !self.phone.native_execution_limit_exceeded(),
                            "a piece of {} bytes for a {rows}x{cols} screen overran the phone's work budget",
                            piece.len()
                        );
                    }
                    self.outputs += 1;
                }
                self.events.push(event);
            }
            if !progressed {
                return;
            }
        }
    }

    /// Connects and shows the server's pane.
    pub fn attach(&mut self) {
        self.pump();
        assert!(self.session.is_ready(), "events: {:?}", self.events);
        let pane_id = self.server.pane_id;
        self.session.show(pane_id);
        self.pump();
    }

    /// Program output on the remote pane, pushed as the server would.
    pub fn output(&mut self, bytes: &[u8]) {
        self.server.term.advance_bytes(bytes);
        self.server.push();
        self.pump();
    }

    /// Program output the server has not pushed yet.
    pub fn quiet_output(&mut self, bytes: &[u8]) {
        self.server.term.advance_bytes(bytes);
    }

    pub fn push(&mut self) {
        self.server.push();
        self.pump();
    }
}

/// A cell as it looks: its column, text and the attributes SGR can carry.
/// Blank cells that look like erased ones are left out, so a phone row the
/// mirror erased compares equal to a remote row that never had those cells.
pub type CellLook = (usize, String, String);

pub fn looks(line: &Line) -> Vec<CellLook> {
    line.visible_cells()
        .filter_map(|cell| {
            let attrs: &CellAttributes = cell.attrs();
            let blank = cell.str() == " "
                && attrs.background() == frankenterm_term::color::ColorAttribute::Default
                && !attrs.reverse()
                && attrs.underline() == frankenterm_term::Underline::None
                && !attrs.strikethrough()
                && !attrs.overline();
            (!blank).then(|| {
                let style = format!(
                    "{:?} {:?} {:?} {:?} {} {} {} {} {} {:?}",
                    attrs.foreground(),
                    attrs.background(),
                    attrs.intensity(),
                    attrs.underline(),
                    attrs.italic(),
                    attrs.reverse(),
                    attrs.strikethrough(),
                    attrs.invisible(),
                    attrs.overline(),
                    attrs.blink(),
                );
                (cell.cell_index(), cell.str().to_owned(), style)
            })
        })
        .collect()
}

/// Every row on screen, as it looks, and whether it wraps onto the next.
pub fn screen(term: &mut Terminal) -> Vec<(Vec<CellLook>, bool)> {
    let dims = terminal_get_dimensions(term);
    let top = dims.physical_top;
    let (_, lines) = terminal_get_lines(term, top..top + dims.viewport_rows as StableRowIndex);
    lines
        .iter()
        .map(|line| (looks(line), line.last_cell_was_wrapped()))
        .collect()
}

/// Whether each row the terminal holds wraps onto the next, history first.
pub fn wraps(term: &mut Terminal) -> Vec<bool> {
    let dims = terminal_get_dimensions(term);
    let (_, lines) = terminal_get_lines(
        term,
        dims.scrollback_top..dims.physical_top + dims.viewport_rows as StableRowIndex,
    );
    lines.iter().map(Line::last_cell_was_wrapped).collect()
}

/// The text of each row that wraps, trailing blanks kept: what runs on into
/// the row below when the two are read as one line.
pub fn wrapped_text(term: &mut Terminal) -> Vec<String> {
    let dims = terminal_get_dimensions(term);
    let (_, lines) = terminal_get_lines(
        term,
        dims.scrollback_top..dims.physical_top + dims.viewport_rows as StableRowIndex,
    );
    lines
        .iter()
        .filter(|line| line.last_cell_was_wrapped())
        .map(|line| line.as_str().into_owned())
        .collect()
}

/// The text of rows `range`, trailing blanks dropped.
pub fn text(term: &mut Terminal, range: std::ops::Range<StableRowIndex>) -> Vec<String> {
    let (_, lines) = terminal_get_lines(term, range);
    lines
        .iter()
        .map(|line| line.as_str().trim_end().to_owned())
        .collect()
}

/// Every row the terminal holds, history first.
pub fn all_text(term: &mut Terminal) -> Vec<String> {
    let dims = terminal_get_dimensions(term);
    text(
        term,
        dims.scrollback_top..dims.physical_top + dims.viewport_rows as StableRowIndex,
    )
}

/// The phone holds the remote's newest rows, as many as it keeps beside
/// its screens: the same text, wrapping in the same places.
pub fn assert_kept(harness: &mut Harness) {
    let phone = all_text(&mut harness.phone);
    let remote = all_text(&mut harness.server.term);
    assert!(phone.len() <= remote.len(), "{phone:?}");
    let older = remote.len() - phone.len();
    assert_eq!(phone, remote[older..], "rows differ");
    let wrapped = wraps(&mut harness.server.term);
    assert_eq!(wraps(&mut harness.phone), wrapped[older..], "wraps differ");
}

/// The cursor's column and its row on screen.
pub fn cursor(term: &mut Terminal) -> (usize, StableRowIndex) {
    let dims = terminal_get_dimensions(term);
    let position = terminal_get_cursor_position(term);
    (position.x, position.y - dims.physical_top)
}

/// Asserts the phone shows exactly what the remote pane shows.
pub fn assert_mirrored(harness: &mut Harness) {
    let remote = screen(&mut harness.server.term);
    let phone = screen(&mut harness.phone);
    assert_eq!(phone, remote, "screen differs");
    assert_eq!(
        cursor(&mut harness.phone),
        cursor(&mut harness.server.term),
        "cursor differs"
    );
}
