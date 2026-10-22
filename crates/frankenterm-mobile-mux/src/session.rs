//! One connection to one machine's mux server, as a state machine: the app
//! passes in what the channel delivered, sends what [`Session::take_outgoing`]
//! returns, and reacts to [`Event`]s.

use crate::frame::{self, Frame, FrameReader};
use crate::mirror::PaneMirror;
use crate::wire::{
    self, CodecVersion, GetCodecVersion, GetLines, GetPaneRenderChanges, LinesResponse, ListPanes,
    LivenessResponse, PaneList, PaneRemoved, RenderChanges, RenderHead, Resize, SendPaste,
    StableRowIndex, TerminalSize, WriteToPane, ident,
};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The server speaks this client's protocol; the pane list follows.
    Ready {
        server_version: String,
    },
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
    Action,
}

/// The pane id that opens PDUs 25, 30 and 37.
#[derive(Deserialize)]
struct PanePrefix {
    pane_id: usize,
}

#[derive(Debug, Default)]
pub struct Session {
    next_serial: u64,
    reader: FrameReader,
    outgoing: Vec<u8>,
    ready: bool,
    failed: bool,
    requests: HashMap<u64, Request>,
    panes: Vec<PaneInfo>,
    mirrors: HashMap<usize, PaneMirror>,
    original_sizes: HashMap<usize, TerminalSize>,
    events: VecDeque<Event>,
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

    /// Asks for the pane list again; an [`Event::Panes`] answers.
    pub fn refresh_panes(&mut self) {
        if self.is_ready() {
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

    /// Input for the pane, exactly as its program reads it.
    pub fn write(&mut self, pane_id: usize, data: &[u8]) {
        if self.is_ready() && !data.is_empty() {
            let request = WriteToPane {
                pane_id,
                data: data.to_vec(),
            };
            self.send(ident::WRITE_TO_PANE, &request, Request::Action);
        }
    }

    /// Pasted text; the server brackets it when the pane's program asked.
    pub fn paste(&mut self, pane_id: usize, text: &str) {
        if self.is_ready() && !text.is_empty() {
            let request = SendPaste {
                pane_id,
                data: text,
            };
            self.send(ident::SEND_PASTE, &request, Request::Action);
        }
    }

    /// Resizes the remote pane, remembering its size before the first resize
    /// so [`Session::restore_size`] can put it back.
    pub fn resize(&mut self, pane_id: usize, rows: usize, cols: usize) {
        let Some(pane) = self.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        let tab_id = pane.tab_id;
        let original = *self.original_sizes.entry(pane_id).or_insert(pane.size);
        if !self.is_ready() || rows == 0 || cols == 0 {
            return;
        }
        let size = TerminalSize {
            rows,
            cols,
            pixel_width: original.pixel_width / original.cols.max(1) * cols,
            pixel_height: original.pixel_height / original.rows.max(1) * rows,
            dpi: original.dpi,
        };
        let request = if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.expect_size(rows, cols);
            Request::Resize { pane_id }
        } else {
            Request::Action
        };
        self.send_resize(tab_id, pane_id, size, request);
    }

    /// Returns the pane to the size it had before [`Session::resize`].
    pub fn restore_size(&mut self, pane_id: usize) {
        let Some(size) = self.original_sizes.remove(&pane_id) else {
            return;
        };
        let Some(tab_id) = self
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .map(|pane| pane.tab_id)
        else {
            return;
        };
        if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
            mirror.size_settled();
        }
        if self.is_ready() {
            self.send_resize(tab_id, pane_id, size, Request::Action);
        }
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
            self.events.push_back(Event::Failed { reason });
        }
    }

    fn handle(&mut self, frame: Frame) -> Result<(), crate::Error> {
        let request = if frame.serial == 0 {
            None
        } else {
            self.requests.remove(&frame.serial)
        };
        match frame.ident {
            ident::GET_CODEC_VERSION_RESPONSE => {
                let version: CodecVersion = wire::decode(&frame.body)?;
                if version.codec_vers == wire::CODEC_VERSION {
                    self.ready = true;
                    self.events.push_back(Event::Ready {
                        server_version: version.version_string,
                    });
                    self.refresh_panes();
                } else {
                    self.fail(format!(
                        "the mux server speaks protocol {}, and this app speaks {}",
                        version.codec_vers,
                        wire::CODEC_VERSION
                    ));
                }
            }
            ident::LIST_PANES_RESPONSE => {
                let list: PaneList = wire::decode(&frame.body)?;
                self.panes = pane_infos(&list);
                self.events.push_back(Event::Panes(self.panes.clone()));
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
                    if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                        mirror.apply_changes(head, lines);
                    }
                    self.advance(pane_id);
                }
            }
            ident::GET_LINES_RESPONSE => {
                if let Some(Request::Lines { pane_id, ranges }) = request {
                    let lines = wire::decode::<LinesResponse>(&frame.body)
                        .map(|reply| reply.lines)
                        .unwrap_or_default();
                    if let Some(mirror) = self.mirrors.get_mut(&pane_id) {
                        mirror.apply_lines(lines, &ranges);
                    }
                    self.advance(pane_id);
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
            ident::ERROR_RESPONSE => match request {
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
                Some(Request::Action) | None => {}
            },
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
        self.mirrors.remove(&pane_id);
        self.original_sizes.remove(&pane_id);
        self.panes.retain(|pane| pane.pane_id != pane_id);
        self.events.push_back(Event::PaneGone { pane_id });
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
        }));
    }
    infos
}
