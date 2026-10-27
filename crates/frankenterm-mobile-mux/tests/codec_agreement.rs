//! Every frame the phone writes is byte for byte what the desktop codec
//! writes for a v46 server, and every reply the phone reads decodes the same
//! way in the desktop codec's v46 decoder.

mod support;

use codec::{
    CompressionMode, GetCodecVersion, GetLines, GetPaneRenderChanges, GetPaneRenderChangesResponse,
    InputSerial, ListPanes, MuxWireDecodedPayload, MuxWireDialect, Pdu, PduProducer, PduWireRole,
    Resize, SendPaste, WriteToPane,
};
use frankenterm_mobile_mux::{frame, wire};
use mux::renderable::{terminal_get_cursor_position, terminal_get_dimensions, terminal_get_lines};
use mux::tab::SerdeUrl;
use support::{
    V46PaneEntry, V46PaneNode, V46SplitDirection, V46SplitDirectionAndSize, looks, server_frame,
    size, terminal, v46_pane_list,
};

fn desktop_v46(pdu: Pdu, serial: u64) -> Vec<u8> {
    pdu.prepare_outbound_for_dialect(
        MuxWireDialect::LEGACY46,
        PduProducer::Client,
        PduWireRole::Request,
        None,
        CompressionMode::Never,
    )
    .expect("the desktop codec plans the v46 request")
    .encode_frame(serial)
    .expect("the desktop codec encodes the v46 request")
}

fn phone<T: serde::Serialize>(ident: u64, serial: u64, body: &T) -> Vec<u8> {
    frame::encode(ident, serial, &wire::encode(body))
}

fn desktop_current(pdu: Pdu, serial: u64) -> Vec<u8> {
    pdu.prepare_outbound_for_dialect(
        MuxWireDialect::current(codec::CODEC_VERSION).expect("the current dialect"),
        PduProducer::Client,
        PduWireRole::Request,
        None,
        CompressionMode::Never,
    )
    .expect("the desktop codec plans the current request")
    .encode_frame(serial)
    .expect("the desktop codec encodes the current request")
}

fn key_press(pane_id: usize) -> (wire::SendKeyDown, Pdu) {
    use termwiz::input::{KeyCode, KeyEvent, Modifiers};
    let event = KeyEvent {
        key: KeyCode::UpArrow,
        modifiers: Modifiers::CTRL | Modifiers::SHIFT,
    };
    (
        wire::SendKeyDown {
            pane_id,
            event: event.clone(),
            input_serial: 0,
        },
        Pdu::SendKeyDown(codec::SendKeyDown {
            pane_id,
            event,
            input_serial: InputSerial::empty(),
        }),
    )
}

#[test]
fn key_presses_match_the_desktop_encoding_in_both_dialects() {
    let (ours, theirs) = key_press(9);
    let phone = phone(wire::ident::SEND_KEY_DOWN, 8, &ours);
    assert_eq!(phone, desktop_v46(theirs, 8));
    assert_eq!(phone, desktop_current(key_press(9).1, 8));
}

#[test]
fn wheel_notches_match_the_desktop_encoding_in_both_dialects() {
    use frankenterm_term::input::{MouseButton, MouseEvent, MouseEventKind};
    let ours = wire::SendMouseEvent {
        pane_id: 5,
        event: wire::MouseEvent {
            kind: wire::MouseEventKind::Press,
            x: 33,
            y: 12,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            button: wire::MouseButton::WheelUp(1),
            modifiers: termwiz::input::Modifiers::NONE,
        },
    };
    let theirs = || {
        Pdu::SendMouseEvent(codec::SendMouseEvent {
            pane_id: 5,
            event: MouseEvent {
                kind: MouseEventKind::Press,
                x: 33,
                y: 12,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
                button: MouseButton::WheelUp(1),
                modifiers: termwiz::input::Modifiers::NONE,
            },
        })
    };
    let phone = phone(wire::ident::SEND_MOUSE_EVENT, 9, &ours);
    assert_eq!(phone, desktop_v46(theirs(), 9));
    assert_eq!(phone, desktop_current(theirs(), 9));
}

#[test]
fn the_current_dialect_is_the_desktop_codecs_window_and_layout() {
    assert_eq!(
        wire::CURRENT_VERSIONS,
        codec::CODEC_VERSION_MIN_SUPPORTED..=codec::CODEC_VERSION,
        "a new codec window needs this client's current layouts checked again"
    );
    // The requests the phone sends unchanged in both dialects.
    let cases = [
        (
            phone(wire::ident::GET_CODEC_VERSION, 1, &wire::GetCodecVersion {}),
            desktop_current(Pdu::GetCodecVersion(GetCodecVersion {}), 1),
        ),
        (
            phone(wire::ident::LIST_PANES, 2, &wire::ListPanes {}),
            desktop_current(Pdu::ListPanes(ListPanes {}), 2),
        ),
        (
            phone(
                wire::ident::GET_PANE_RENDER_CHANGES,
                3,
                &wire::GetPaneRenderChanges { pane_id: 77 },
            ),
            desktop_current(
                Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id: 77 }),
                3,
            ),
        ),
        (
            phone(
                wire::ident::GET_LINES,
                4,
                &wire::GetLines {
                    pane_id: 3,
                    lines: vec![-5..2, 4000..4100],
                },
            ),
            desktop_current(
                Pdu::GetLines(GetLines {
                    pane_id: 3,
                    lines: vec![-5..2, 4000..4100],
                }),
                4,
            ),
        ),
        (
            phone(
                wire::ident::WRITE_TO_PANE,
                5,
                &wire::WriteToPane {
                    pane_id: 9,
                    data: b"ls\r".to_vec(),
                },
            ),
            desktop_current(
                Pdu::WriteToPane(WriteToPane {
                    pane_id: 9,
                    data: b"ls\r".to_vec(),
                }),
                5,
            ),
        ),
        (
            phone(
                wire::ident::SEARCH_SCROLLBACK,
                6,
                &wire::SearchScrollback {
                    pane_id: 4,
                    pattern: wire::Pattern::Regex("e+".into()),
                    range: -10..5000,
                    limit: Some(200),
                },
            ),
            desktop_current(
                Pdu::SearchScrollbackRequest(codec::SearchScrollbackRequest {
                    pane_id: 4,
                    pattern: mux::pane::Pattern::Regex("e+".into()),
                    range: -10..5000,
                    limit: Some(200),
                }),
                6,
            ),
        ),
        // Current servers only: the pane list with the server's incarnation.
        (
            phone(
                wire::ident::LIST_PANES_COHERENT,
                8,
                &wire::ListPanesCoherent {
                    supported: wire::FENCED_SNAPSHOT,
                    required: wire::FENCED_SNAPSHOT,
                },
            ),
            desktop_current(
                Pdu::ListPanesCoherent(codec::ListPanesCoherent {
                    supported: codec::TopologyCapabilities::FENCED_SNAPSHOT_V1,
                    required: codec::TopologyCapabilities::FENCED_SNAPSHOT_V1,
                }),
                8,
            ),
        ),
        // The one the dialects write differently.
        (
            phone(
                wire::ident::SEND_PASTE,
                7,
                &wire::SendPasteCurrent {
                    pane_id: 9,
                    data: "héllo\nwörld 🦀",
                    input_serial: 0,
                },
            ),
            desktop_current(
                Pdu::SendPaste(SendPaste {
                    pane_id: 9,
                    data: "héllo\nwörld 🦀".to_owned(),
                    input_serial: InputSerial::empty(),
                }),
                7,
            ),
        ),
    ];
    for (index, (phone, desktop)) in cases.iter().enumerate() {
        assert_eq!(phone, desktop, "request {index}");
    }
}

#[test]
fn requests_match_the_desktop_v46_encoding() {
    let cases = [
        (
            phone(wire::ident::GET_CODEC_VERSION, 1, &wire::GetCodecVersion {}),
            desktop_v46(Pdu::GetCodecVersion(GetCodecVersion {}), 1),
        ),
        (
            phone(wire::ident::LIST_PANES, 2, &wire::ListPanes {}),
            desktop_v46(Pdu::ListPanes(ListPanes {}), 2),
        ),
        (
            phone(
                wire::ident::GET_PANE_RENDER_CHANGES,
                300,
                &wire::GetPaneRenderChanges { pane_id: 77 },
            ),
            desktop_v46(
                Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id: 77 }),
                300,
            ),
        ),
        (
            phone(
                wire::ident::GET_LINES,
                70_000,
                &wire::GetLines {
                    pane_id: 3,
                    lines: vec![-5..2, 4000..4100],
                },
            ),
            desktop_v46(
                Pdu::GetLines(GetLines {
                    pane_id: 3,
                    lines: vec![-5..2, 4000..4100],
                }),
                70_000,
            ),
        ),
        (
            phone(
                wire::ident::WRITE_TO_PANE,
                5,
                &wire::WriteToPane {
                    pane_id: 9,
                    data: b"ls -la\r\x1b[A\x03".to_vec(),
                },
            ),
            desktop_v46(
                Pdu::WriteToPane(WriteToPane {
                    pane_id: 9,
                    data: b"ls -la\r\x1b[A\x03".to_vec(),
                }),
                5,
            ),
        ),
        (
            phone(
                wire::ident::SEND_PASTE,
                6,
                &wire::SendPaste {
                    pane_id: 9,
                    data: "héllo\nwörld 🦀",
                },
            ),
            desktop_v46(
                Pdu::SendPaste(SendPaste {
                    pane_id: 9,
                    data: "héllo\nwörld 🦀".to_owned(),
                    input_serial: InputSerial::from_millis_since_epoch(1),
                }),
                6,
            ),
        ),
        (
            phone(
                wire::ident::RESIZE,
                7,
                &wire::Resize {
                    containing_tab_id: 2,
                    pane_id: 9,
                    size: wire::TerminalSize {
                        rows: 41,
                        cols: 50,
                        pixel_width: 400,
                        pixel_height: 656,
                        dpi: 144,
                    },
                },
            ),
            desktop_v46(
                Pdu::Resize(Resize {
                    containing_tab_id: 2,
                    pane_id: 9,
                    size: frankenterm_term::TerminalSize {
                        rows: 41,
                        cols: 50,
                        pixel_width: 400,
                        pixel_height: 656,
                        dpi: 144,
                    },
                }),
                7,
            ),
        ),
    ];
    for (index, (phone, desktop)) in cases.iter().enumerate() {
        assert_eq!(phone, desktop, "request {index}");
    }
}

#[test]
fn searches_match_the_desktop_v46_encoding_and_decoding() {
    use mux::pane::{Pattern, SearchResult};
    let patterns = [
        (
            wire::Pattern::CaseSensitiveString("Needle".into()),
            Pattern::CaseSensitiveString("Needle".into()),
        ),
        (
            wire::Pattern::CaseInSensitiveString("needle".into()),
            Pattern::CaseInSensitiveString("needle".into()),
        ),
        (
            wire::Pattern::Regex(r"err(or)?\s+\d+".into()),
            Pattern::Regex(r"err(or)?\s+\d+".into()),
        ),
    ];
    for (serial, (ours, theirs)) in (40u64..).zip(patterns) {
        let phone = phone(
            wire::ident::SEARCH_SCROLLBACK,
            serial,
            &wire::SearchScrollback {
                pane_id: 4,
                pattern: ours,
                range: -10..5000,
                limit: Some(200),
            },
        );
        let desktop = desktop_v46(
            Pdu::SearchScrollbackRequest(codec::SearchScrollbackRequest {
                pane_id: 4,
                pattern: theirs,
                range: -10..5000,
                limit: Some(200),
            }),
            serial,
        );
        assert_eq!(phone, desktop, "serial {serial}");
    }

    let results = vec![
        SearchResult {
            start_y: -3,
            start_x: 7,
            end_y: -2,
            end_x: 1,
            match_id: 0,
        },
        SearchResult {
            start_y: 4000,
            start_x: 0,
            end_y: 4000,
            end_x: 12,
            match_id: 5,
        },
    ];
    let body = varbincode::serialize(&codec::SearchScrollbackResponse {
        results: results.clone(),
    })
    .expect("serialize");
    let bytes = server_frame(32, 9, &body, true);
    let decoded = Pdu::decode_for_dialect(bytes.as_slice(), MuxWireDialect::LEGACY46)
        .expect("desktop decodes");
    let MuxWireDecodedPayload::Pdu(Pdu::SearchScrollbackResponse(desktop)) = decoded.payload()
    else {
        panic!("desktop decoded {:?}", decoded.payload());
    };
    assert_eq!(desktop.results, results);
    let mut reader = frame::FrameReader::default();
    reader.push(&bytes);
    let frame = reader.next_frame().expect("frame").expect("complete");
    let ours: wire::SearchResults = wire::decode(&frame.body).expect("the phone decodes it");
    let ours: Vec<_> = ours
        .results
        .iter()
        .map(|r| (r.start_y, r.start_x, r.end_y, r.end_x, r.match_id))
        .collect();
    let theirs: Vec<_> = results
        .iter()
        .map(|r| (r.start_y, r.start_x, r.end_y, r.end_x, r.match_id))
        .collect();
    assert_eq!(ours, theirs);
}

fn styled_terminal() -> frankenterm_term::Terminal {
    let mut term = terminal(5, 30);
    term.advance_bytes(
        "plain\r\n\x1b[1;31mbold red\x1b[0m \x1b[3;4:3;38;2;10;200;30mcurly\x1b[0m\r\n\
         \x1b[48;5;236m界 wide \x1b[0m\x1b[9mgone\x1b[0m\r\n$ "
            .as_bytes(),
    );
    term
}

fn render_changes(term: &mut frankenterm_term::Terminal) -> GetPaneRenderChangesResponse {
    let dims = terminal_get_dimensions(term);
    let top = dims.physical_top;
    let (first, lines) = terminal_get_lines(term, top..top + 5);
    let bonus: Vec<_> = lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            line.compress_for_scrollback();
            (first + index as isize, line)
        })
        .collect();
    GetPaneRenderChangesResponse {
        pane_id: 12,
        mouse_grabbed: true,
        alt_screen_active: false,
        cursor_position: terminal_get_cursor_position(term),
        dimensions: dims,
        tiered_scrollback_status: Some(mux::renderable::PaneTieredScrollbackStatus {
            tiering_enabled: true,
            configured_scrollback_rows: 3500,
            warm_spill_bytes_total: 1 << 40,
            ..Default::default()
        }),
        dirty_lines: vec![2..3, 9..12],
        title: "nvim — ~/src".to_owned(),
        working_dir: Some(SerdeUrl::try_from("file:///home/u/src".to_owned()).expect("url")),
        bonus_lines: bonus.into(),
        input_serial: None,
        seqno: 99,
    }
}

#[test]
fn render_changes_decode_like_the_desktop_v46_decoder() {
    for compress in [false, true] {
        let mut term = styled_terminal();
        let response = render_changes(&mut term);
        let body = varbincode::serialize(&response).expect("serialize");
        let bytes = server_frame(25, 0, &body, compress);

        let decoded = Pdu::decode_for_dialect(bytes.as_slice(), MuxWireDialect::LEGACY46)
            .expect("the desktop decodes it as v46");
        let MuxWireDecodedPayload::Pdu(Pdu::GetPaneRenderChangesResponse(desktop)) =
            decoded.payload()
        else {
            panic!("desktop decoded {:?}", decoded.payload());
        };

        let mut reader = frame::FrameReader::default();
        reader.push(&bytes);
        let frame = reader.next_frame().expect("frame").expect("complete");
        assert_eq!((frame.serial, frame.ident), (0, 25));
        let ours: wire::RenderChanges = wire::decode(&frame.body).expect("the phone decodes it");
        let head = &ours.head;

        assert_eq!(head.pane_id, desktop.pane_id);
        assert_eq!(head.mouse_grabbed, desktop.mouse_grabbed);
        assert_eq!(head.alt_screen_active, desktop.alt_screen_active);
        let cursor = desktop.cursor_position;
        assert_eq!(
            (head.cursor_position.x, head.cursor_position.y),
            (cursor.x, cursor.y)
        );
        assert_eq!(head.cursor_position.shape, cursor.shape);
        assert_eq!(head.cursor_position.visibility, cursor.visibility);
        let dims = desktop.dimensions;
        assert_eq!(
            (
                head.dimensions.cols,
                head.dimensions.viewport_rows,
                head.dimensions.scrollback_rows,
                head.dimensions.physical_top,
                head.dimensions.scrollback_top,
                head.dimensions.dpi,
                head.dimensions.pixel_width,
                head.dimensions.pixel_height,
                head.dimensions.reverse_video,
            ),
            (
                dims.cols,
                dims.viewport_rows,
                dims.scrollback_rows,
                dims.physical_top,
                dims.scrollback_top,
                dims.dpi,
                dims.pixel_width,
                dims.pixel_height,
                dims.reverse_video,
            )
        );
        assert_eq!(head.dirty_lines, desktop.dirty_lines);
        assert_eq!(head.title, desktop.title);
        assert_eq!(head.working_dir.as_deref(), Some("file:///home/u/src"));

        let desktop_lines: Vec<_> = desktop.bonus_lines.lines().collect();
        assert_eq!(ours.bonus_lines.lines.len(), desktop_lines.len());
        for ((row, line), desktop) in ours.bonus_lines.lines.iter().zip(desktop_lines) {
            assert_eq!(*row, desktop.0);
            assert_eq!(looks(line), looks(&desktop.1), "row {row}");
        }
        assert!(
            ours.bonus_lines
                .lines
                .iter()
                .any(|(_, line)| line.as_str().contains("curly"))
        );
    }
}

/// Each cell's link address, in column order.
fn link_addresses(line: &termwiz::surface::Line) -> Vec<Option<String>> {
    line.visible_cells()
        .map(|cell| cell.attrs().hyperlink().map(|link| link.uri().to_owned()))
        .collect()
}

#[test]
fn links_come_back_on_the_cells_the_desktop_puts_them_on() {
    let mut term = terminal(5, 30);
    term.advance_bytes(
        "see \x1b]8;id=pr;https://example.com/pr/1\x1b\\PR #1\x1b]8;;\x1b\\ and \
         \x1b]8;;https://example.com/ci\x1b\\CI\x1b]8;;\x1b\\\r\n\
         \x1b]8;;https://example.com/pr/1\x1b\\again\x1b]8;;\x1b\\\r\n$ "
            .as_bytes(),
    );
    let response = render_changes(&mut term);
    let body = varbincode::serialize(&response).expect("serialize");
    let bytes = server_frame(25, 0, &body, false);
    let dialects = [
        MuxWireDialect::LEGACY46,
        MuxWireDialect::current(codec::CODEC_VERSION).expect("the current dialect"),
    ];
    for dialect in dialects {
        let decoded =
            Pdu::decode_for_dialect(bytes.as_slice(), dialect).expect("the desktop decodes it");
        let MuxWireDecodedPayload::Pdu(Pdu::GetPaneRenderChangesResponse(desktop)) =
            decoded.payload()
        else {
            panic!("desktop decoded {:?}", decoded.payload());
        };
        let (desktop_lines, _) = desktop.bonus_lines.clone().extract_data();

        let mut reader = frame::FrameReader::default();
        reader.push(&bytes);
        let frame = reader.next_frame().expect("frame").expect("complete");
        let ours: wire::RenderChanges = wire::decode(&frame.body).expect("the phone decodes it");
        let ours = ours.bonus_lines.linked_lines();

        assert_eq!(ours.len(), desktop_lines.len());
        for ((row, line), (desktop_row, desktop_line)) in ours.iter().zip(&desktop_lines) {
            assert_eq!(row, desktop_row);
            assert_eq!(
                link_addresses(line),
                link_addresses(desktop_line),
                "row {row}"
            );
        }
        let first = link_addresses(&ours[0].1);
        let pr = Some("https://example.com/pr/1".to_owned());
        let ci = Some("https://example.com/ci".to_owned());
        assert_eq!(first[..4], [None, None, None, None]);
        assert_eq!(
            first[4..9],
            [pr.clone(), pr.clone(), pr.clone(), pr.clone(), pr.clone()]
        );
        assert_eq!(first[14..16], [ci.clone(), ci]);
        assert_eq!(
            link_addresses(&ours[1].1)[..5],
            [pr.clone(), pr.clone(), pr.clone(), pr.clone(), pr]
        );
    }
}

#[test]
fn a_pane_tree_decodes_like_the_desktop_v46_decoder() {
    let cursor = terminal_get_cursor_position(&mut terminal(3, 10));
    let entry = |pane_id: usize, title: &str, working_dir: Option<&str>| V46PaneEntry {
        window_id: 1,
        tab_id: 4,
        pane_id,
        title: title.to_owned(),
        size: size(24, 80),
        working_dir: working_dir.map(|dir| SerdeUrl::try_from(dir.to_owned()).expect("url")),
        alt_screen_active: pane_id == 11,
        is_active_pane: pane_id == 10,
        is_zoomed_pane: false,
        workspace: "default".to_owned(),
        cursor_pos: cursor,
        physical_top: 1200,
        top_row: 0,
        left_col: if pane_id == 10 { 0 } else { 81 },
        tty_name: Some(format!("/dev/pts/{pane_id}")),
    };
    let tabs = vec![
        V46PaneNode::Split {
            left: Box::new(V46PaneNode::Leaf(entry(
                10,
                "claude",
                Some("file:///home/u/a"),
            ))),
            right: Box::new(V46PaneNode::Leaf(entry(11, "codex — vim", None))),
            node: V46SplitDirectionAndSize {
                direction: V46SplitDirection::Horizontal,
                first: size(24, 80),
                second: size(24, 80),
            },
        },
        V46PaneNode::Leaf(entry(12, "zsh", Some("file:///home/u/b"))),
        V46PaneNode::Empty,
    ];
    let titles = ["agents", "shell", "empty"].map(str::to_owned).to_vec();
    let body = v46_pane_list(tabs, titles);
    let bytes = server_frame(4, 2, &body, true);

    let decoded = Pdu::decode_for_dialect(bytes.as_slice(), MuxWireDialect::LEGACY46)
        .expect("the desktop decodes it as v46");
    let MuxWireDecodedPayload::Legacy46ListPanesResponse(desktop) = decoded.payload() else {
        panic!("desktop decoded {:?}", decoded.payload());
    };
    let mut desktop_panes = Vec::new();
    for tab in desktop.tabs() {
        let mut stack = vec![tab];
        while let Some(node) = stack.pop() {
            match node {
                mux::tab::PaneNode::Leaf(entry) => desktop_panes.push(entry.clone()),
                mux::tab::PaneNode::Split { left, right, .. } => {
                    stack.push(right);
                    stack.push(left);
                }
                mux::tab::PaneNode::Empty => {}
            }
        }
    }

    let mut reader = frame::FrameReader::default();
    reader.push(&bytes);
    let frame = reader.next_frame().expect("frame").expect("complete");
    let ours: wire::PaneList = wire::decode(&frame.body).expect("the phone decodes it");
    assert_eq!(ours.tab_titles, desktop.tab_titles());
    assert_eq!(
        ours.window_titles.get(&0).map(String::as_str),
        Some("window")
    );
    let mut our_panes = Vec::new();
    for tab in &ours.tabs {
        tab.leaves(&mut our_panes);
    }
    assert_eq!(our_panes.len(), 3);
    assert_eq!(our_panes.len(), desktop_panes.len());
    for (ours, desktop) in our_panes.iter().zip(&desktop_panes) {
        assert_eq!(ours.pane_id, desktop.pane_id);
        assert_eq!(
            (ours.window_id, ours.tab_id),
            (desktop.window_id, desktop.tab_id)
        );
        assert_eq!(ours.title, desktop.title);
        assert_eq!(
            (ours.size.rows, ours.size.cols),
            (desktop.size.rows, desktop.size.cols)
        );
        assert_eq!(
            ours.working_dir.as_deref(),
            desktop.working_dir.as_ref().map(SerdeUrl::as_str)
        );
        assert_eq!(ours.alt_screen_active, desktop.alt_screen_active);
        assert_eq!(ours.is_active_pane, desktop.is_active_pane);
        assert_eq!(ours.workspace, desktop.workspace);
        assert_eq!(ours.physical_top, desktop.physical_top);
        assert_eq!(ours.left_col, desktop.left_col);
        assert_eq!(ours.tty_name, desktop.tty_name);
    }
}
