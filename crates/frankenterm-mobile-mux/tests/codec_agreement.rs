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
