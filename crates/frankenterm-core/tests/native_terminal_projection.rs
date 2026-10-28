#![cfg(all(feature = "web", feature = "vendored", unix))]

//! Parser-ground native projection tests. These exercise real terminal cells
//! and the production browser publisher, not a live browser or authorization.

use std::collections::HashMap;
use std::sync::Arc;

use codec::{PaneIdentityV1, PaneRenderMetadataV1, ReliablePaneRegistrationIdentityV1};
use frankenterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
use frankenterm_cell::{CellAttributes, VerticalAlign};
use frankenterm_core::web::terminal_projection::{
    TerminalProjectionError, project_terminal_snapshot, scope_for_identity,
};
use frankenterm_term::color::{ColorPalette, SrgbaTuple};
use frankenterm_term::{Line, Terminal, TerminalConfiguration, TerminalSize};
use frankenterm_terminal_wire as wire;
use mux::renderable::{terminal_get_cursor_position, terminal_get_dimensions, terminal_get_lines};

#[derive(Debug)]
struct ProjectionConfig;

impl TerminalConfiguration for ProjectionConfig {
    fn color_palette(&self) -> ColorPalette {
        let mut palette = ColorPalette::default();
        palette.foreground = SrgbaTuple::from((220_u8, 230_u8, 240_u8));
        palette.background = SrgbaTuple::from((10_u8, 20_u8, 30_u8));
        palette.colors.0[17] = SrgbaTuple::from((40_u8, 50_u8, 60_u8));
        palette
    }

    fn scrollback_size(&self) -> usize {
        100
    }
}

fn terminal(rows: usize, cols: usize) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        },
        Arc::new(ProjectionConfig),
        "FrankenTerm",
        "native-projection-test",
        Box::new(std::io::sink()),
    )
}

fn identity() -> PaneIdentityV1 {
    PaneIdentityV1 {
        session_incarnation: mux::MuxSessionIncarnation::from_bytes([0x11; 16]),
        durable_pane_id: [0x22; 16],
        pane_registration: ReliablePaneRegistrationIdentityV1::from_bytes([0x33; 16]),
    }
}

fn scope() -> wire::Scope {
    scope_for_identity(
        wire::Id::new("enrolled-host").unwrap(),
        &identity(),
        wire::WireU64(7),
    )
    .unwrap()
}

fn parts(term: &mut Terminal) -> (PaneRenderMetadataV1, Vec<(isize, Line)>) {
    let dimensions = terminal_get_dimensions(term);
    let cursor = terminal_get_cursor_position(term);
    let metadata = PaneRenderMetadataV1 {
        seqno: term.current_seqno(),
        dimensions,
        cursor,
        mouse_grabbed: term.is_mouse_grabbed(),
        alt_screen_active: term.is_alt_screen_active(),
        bracketed_paste: term.bracketed_paste_enabled(),
        focus_tracking: term.focus_tracking_enabled(),
        palette_generation: 9,
        palette: term.palette(),
    };
    let (first, lines) = terminal_get_lines(
        term,
        dimensions.physical_top..dimensions.physical_top + dimensions.viewport_rows as isize,
    );
    let rows = lines
        .into_iter()
        .enumerate()
        .map(|(offset, line)| (first + offset as isize, line))
        .collect();
    (metadata, rows)
}

fn project(term: &mut Terminal) -> wire::FrozenSnapshot {
    let (metadata, rows) = parts(term);
    project_terminal_snapshot(
        scope(),
        wire::WireU64(9_007_199_254_740_993),
        &metadata,
        term.get_title().to_string(),
        term.get_current_dir().map(ToString::to_string),
        rows,
    )
    .unwrap()
}

fn publication(snapshot: wire::FrozenSnapshot) -> wire::SnapshotPublication {
    let capabilities: wire::Capabilities = vec![
        wire::Capability::TerminalState,
        wire::Capability::ResourceReferences,
    ]
    .try_into()
    .unwrap();
    let asset = wire::Sha256::new(&"a".repeat(64)).unwrap();
    let mut server = wire::ServerSession::new(
        snapshot.scope.identity.host.clone(),
        wire::Id::new("projection-session").unwrap(),
        asset.clone(),
        capabilities.clone(),
    )
    .unwrap();
    let hello = wire::ClientFrame::new(
        None,
        wire::ClientMessage::Hello(wire::Hello {
            host: snapshot.scope.identity.host.clone(),
            hello_id: wire::Id::new("projection-hello").unwrap(),
            asset,
            required: capabilities,
            optional: wire::Capabilities::new(),
        }),
    )
    .unwrap();
    server
        .negotiate(&wire::encode_client(&hello).unwrap())
        .unwrap();
    server
        .bind_scope(
            snapshot.scope.clone(),
            snapshot.dimensions.clone(),
            wire::WireU64(0),
        )
        .unwrap();
    server.encode_snapshot(snapshot).unwrap()
}

#[test]
fn native_parser_cells_modes_and_metadata_survive_wire_publication() {
    let mut term = terminal(4, 12);
    term.advance_bytes(
        b"\x1b]2;projection title\x07\x1b]7;file:///tmp/projection\x07\x1b[?2004h\x1b[?1000h",
    );
    term.advance_bytes(
        b"\x1b]8;id=fixture;https://example.test/link\x1b\\\x1b[1;3;4:3;38;2;18;52;86;48;5;17m",
    );
    term.advance_bytes("Ae\u{301}界".as_bytes());
    term.advance_bytes(b"\x1b]8;;\x1b\\\x1b[0m\x1b[2;5H\x1b[5 q\x1b[?25l");
    let snapshot = project(&mut term);
    assert_eq!(snapshot.title.as_str(), "projection title");
    assert_eq!(
        snapshot.cwd.as_ref().unwrap().as_str(),
        "file:///tmp/projection"
    );
    assert!(snapshot.modes.bracketed_paste);
    assert!(snapshot.modes.mouse_tracking);
    assert!(!snapshot.modes.alternate_screen);
    assert_eq!((snapshot.cursor.row, snapshot.cursor.column), (1, 4));
    assert_eq!(snapshot.cursor.shape, wire::CursorShape::Bar);
    assert!(snapshot.cursor.blink);
    assert!(!snapshot.cursor.visible);
    let cells = snapshot.lines.as_slice()[0].cells.as_slice();
    assert_eq!(cells[0].text.as_str(), "A");
    assert_eq!(cells[1].text.as_str(), "e\u{301}");
    assert_eq!((cells[2].text.as_str(), cells[2].width), ("界", 2));
    assert_eq!((cells[3].text.as_str(), cells[3].width), ("", 0));
    assert_eq!(cells[0].style.foreground, wire::Color::Rgb([18, 52, 86]));
    assert_eq!(cells[0].style.background, wire::Color::Rgb([40, 50, 60]));
    assert_eq!(
        cells[0].style.attributes,
        wire::STYLE_BOLD | wire::STYLE_ITALIC
    );
    assert_eq!(cells[0].style.underline, wire::Underline::Curly);
    assert_eq!(cells[2].hyperlink, cells[3].hyperlink);
    assert_eq!(snapshot.hyperlinks.len(), 1);
    assert_eq!(
        snapshot.hyperlinks.as_slice()[0].uri.as_str(),
        "https://example.test/link"
    );
    let expected_lines = snapshot.lines.as_slice().to_vec();
    let expected_sequence = snapshot.sequence;
    let published = publication(snapshot);
    let mut actual = Vec::new();
    for frame in published.frames() {
        assert!(frame.len() <= wire::MAX_FRAME_BYTES);
        match wire::decode_server(frame).unwrap().message {
            wire::ServerMessage::SnapshotChunk(chunk) => {
                actual.extend_from_slice(chunk.patch.lines.as_slice());
            }
            wire::ServerMessage::SnapshotEnd(end) => assert_eq!(end.sequence, expected_sequence),
            wire::ServerMessage::SnapshotStart(_) => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert_eq!(actual, expected_lines);
}

#[test]
fn native_scroll_coordinates_and_blank_padding_preserve_visible_screen() {
    let mut term = terminal(3, 8);
    for row in 0..8 {
        term.advance_bytes(format!("row{row}\r\n").as_bytes());
    }
    let expected_top = terminal_get_dimensions(&mut term).physical_top;
    assert!(expected_top > 0);
    let snapshot = project(&mut term);
    for (offset, line) in snapshot.lines.as_slice().iter().enumerate() {
        assert_eq!(line.row, offset as u16);
        assert_eq!(line.stable_row.0, expected_top as i64 + offset as i64);
        assert_eq!(line.cells.len(), 8);
        assert!(
            line.cells.as_slice()[4..]
                .iter()
                .all(|cell| cell.text.as_str() == " ")
        );
    }
    assert_eq!(snapshot.cursor.row, 2);
}

#[test]
fn native_alt_screen_reverse_video_and_palette_changes_are_projected() {
    let mut term = terminal(2, 8);
    term.advance_bytes(b"PRIMARY\x1b[?1049h\x1b[?5hA\x1b[7mB\x1b[0m");
    let snapshot = project(&mut term);
    assert!(snapshot.modes.alternate_screen);
    let cells = snapshot.lines.as_slice()[0].cells.as_slice();
    assert_eq!(cells[0].text.as_str(), "A");
    assert_eq!(
        cells[0].style.attributes & wire::STYLE_REVERSE,
        wire::STYLE_REVERSE
    );
    assert_eq!(cells[1].style.attributes & wire::STYLE_REVERSE, 0);
    term.advance_bytes(b"\x1b]10;#123456\x07\x1b[?1049l");
    let restored = project(&mut term);
    assert!(!restored.modes.alternate_screen);
    assert_eq!(
        restored.lines.as_slice()[0].cells.as_slice()[0]
            .text
            .as_str(),
        "P"
    );
    assert_eq!(
        restored.lines.as_slice()[0].cells.as_slice()[0]
            .style
            .foreground,
        wire::Color::Rgb([18, 52, 86])
    );
}

#[test]
fn native_identity_scope_separates_hosts_restarts_and_reused_registrations() {
    let original = scope();
    let mut replacement = identity();
    replacement.pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x44; 16]);
    let changed =
        scope_for_identity(original.identity.host.clone(), &replacement, original.epoch).unwrap();
    assert_eq!(original.identity.pane, changed.identity.pane);
    assert_ne!(original.identity.incarnation, changed.identity.incarnation);
    replacement = identity();
    replacement.session_incarnation = mux::MuxSessionIncarnation::from_bytes([0x55; 16]);
    assert_ne!(
        original.identity.incarnation,
        scope_for_identity(original.identity.host.clone(), &replacement, original.epoch)
            .unwrap()
            .identity
            .incarnation
    );
    assert_ne!(
        original,
        scope_for_identity(
            wire::Id::new("other-enrolled-host").unwrap(),
            &identity(),
            original.epoch
        )
        .unwrap()
    );
    replacement.durable_pane_id = [0; 16];
    assert!(scope_for_identity(original.identity.host, &replacement, original.epoch).is_err());
}

#[test]
fn native_projection_refuses_missing_duplicate_reordered_or_out_of_bounds_rows() {
    let mut term = terminal(3, 8);
    term.advance_bytes(b"one\r\ntwo");
    let (metadata, rows) = parts(&mut term);
    for case in 0..4 {
        let mut invalid = rows.clone();
        match case {
            0 => {
                invalid.pop();
            }
            1 => invalid[1].0 = invalid[0].0,
            2 => invalid.swap(0, 1),
            3 => invalid[2].0 = isize::MAX,
            _ => unreachable!(),
        }
        assert!(matches!(
            project_terminal_snapshot(
                scope(),
                wire::WireU64(1),
                &metadata,
                String::new(),
                None,
                invalid
            ),
            Err(TerminalProjectionError::InvalidSource)
        ));
    }
}

#[test]
fn unsupported_native_geometry_images_and_alignment_are_explicit() {
    let mut term = terminal(1, 8);
    let (metadata, _) = parts(&mut term);
    for case in 0..3 {
        let mut attrs = CellAttributes::default();
        if case == 1 {
            attrs.set_vertical_align(VerticalAlign::SuperScript);
        } else if case == 2 {
            attrs.set_image(Box::new(ImageCell::new(
                TextureCoordinate::new_f32(0.0, 0.0),
                TextureCoordinate::new_f32(1.0, 1.0),
                Arc::new(ImageData::with_data(ImageDataType::placeholder())),
            )));
        }
        let mut line = Line::from_text("X", &attrs, 1, None);
        if case == 0 {
            line.set_double_width(1);
        }
        let error = project_terminal_snapshot(
            scope(),
            wire::WireU64(1),
            &metadata,
            String::new(),
            None,
            vec![(metadata.dimensions.physical_top, line)],
        )
        .unwrap_err();
        assert!(matches!(error, TerminalProjectionError::Unsupported(_)));
    }
}

#[test]
fn disabled_bidi_still_refuses_unrepresented_rtl_character_path() {
    let mut term = terminal(1, 8);
    // A character path applies to rows cleared or created after it, as the
    // terminal's other rows do: erasing this one gives it right-to-left.
    term.advance_bytes(b"\x1b[2 k\x1b[2KABC");
    let (metadata, rows) = parts(&mut term);
    let (bidi_enabled, direction) = rows[0].1.bidi_info();
    assert!(
        !bidi_enabled && direction != Default::default(),
        "fixture must exercise direction without bidi reordering"
    );
    assert!(matches!(
        project_terminal_snapshot(
            scope(),
            wire::WireU64(1),
            &metadata,
            String::new(),
            None,
            rows
        ),
        Err(TerminalProjectionError::Unsupported(_))
    ));
}

#[test]
fn native_projection_bounds_geometry_graphemes_and_metadata_before_publication() {
    let mut term = terminal(1, 8);
    let (metadata, rows) = parts(&mut term);
    let mut oversized = metadata.clone();
    oversized.dimensions.cols = usize::from(wire::MAX_COLUMNS) + 1;
    assert!(
        project_terminal_snapshot(
            scope(),
            wire::WireU64(1),
            &oversized,
            String::new(),
            None,
            rows.clone()
        )
        .is_err()
    );
    assert!(
        project_terminal_snapshot(
            scope(),
            wire::WireU64(1),
            &metadata,
            "t".repeat(4097),
            None,
            rows
        )
        .is_err()
    );
    let text = format!("e{}", "\u{301}".repeat(128));
    let line = Line::from_text(&text, &CellAttributes::default(), 1, None);
    assert!(line.visible_cells().next().unwrap().str().len() > 256);
    assert!(
        project_terminal_snapshot(
            scope(),
            wire::WireU64(1),
            &metadata,
            String::new(),
            None,
            vec![(metadata.dimensions.physical_top, line)]
        )
        .is_err()
    );
}

#[test]
fn hyperlink_projection_owns_only_bounded_wire_meaning() {
    let mut term = terminal(2, 4);
    let (metadata, _) = parts(&mut term);
    let mut rows = Vec::new();
    let mut weak = Vec::new();
    for row in 0..2 {
        let params = HashMap::from([("ignored-param".to_string(), "x".repeat(32 * 1024 + row))]);
        let link = Arc::new(termwiz::hyperlink::Hyperlink::new_with_params(
            "https://example.test/same",
            params,
        ));
        weak.push(Arc::downgrade(&link));
        let mut attrs = CellAttributes::default();
        attrs.set_hyperlink(Some(link));
        rows.push((
            metadata.dimensions.physical_top + row as isize,
            Line::from_text("L", &attrs, 1, None),
        ));
    }
    let snapshot = project_terminal_snapshot(
        scope(),
        wire::WireU64(1),
        &metadata,
        String::new(),
        None,
        rows,
    )
    .unwrap();
    assert_eq!(
        snapshot.hyperlinks.len(),
        1,
        "wire URI dictionary must not retain native parameter identity"
    );
    assert_eq!(
        snapshot.lines.as_slice()[0].cells.as_slice()[0].hyperlink,
        snapshot.lines.as_slice()[1].cells.as_slice()[0].hyperlink
    );
    assert!(weak.iter().all(|link| link.upgrade().is_none()));
    publication(snapshot);
}

#[test]
fn a_real_multi_chunk_viewport_is_published_whole() {
    let mut term = terminal(65, 8);
    for row in 0..65 {
        term.advance_bytes(format!("\x1b[{};1H{row:02}", row + 1).as_bytes());
    }
    let snapshot = project(&mut term);
    let publication = publication(snapshot);
    assert!(publication.frames().len() >= 4);
    let mut rows = Vec::new();
    for frame in publication.frames() {
        if let wire::ServerMessage::SnapshotChunk(chunk) =
            wire::decode_server(frame).unwrap().message
        {
            rows.extend(chunk.patch.lines.into_vec());
        }
    }
    assert_eq!(rows.len(), 65);
    for (index, row) in rows.iter().enumerate() {
        let text: String = row
            .cells
            .as_slice()
            .iter()
            .take(2)
            .map(|cell| cell.text.as_str())
            .collect();
        assert_eq!(text, format!("{index:02}"));
    }
}
