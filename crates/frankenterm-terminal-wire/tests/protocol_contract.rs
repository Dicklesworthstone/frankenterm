use frankenterm_terminal_wire::*;

fn scope() -> Scope {
    Scope {
        identity: PaneIdentity {
            host: Id::new("host-a").unwrap(),
            pane: Id::new("pane-uuid").unwrap(),
            incarnation: Id::new("incarnation-uuid").unwrap(),
        },
        epoch: WireU64(1),
    }
}

fn effect() -> Mutation {
    Mutation { scope: scope(), request_id: Id::new("request-a").unwrap(), lease_fence: WireU64(1) }
}

fn dimensions(columns: u16, rows: u16) -> Dimensions {
    Dimensions { columns, rows, pixel_width: 0, pixel_height: 0 }
}

fn cursor() -> Cursor {
    Cursor { column: 0, row: 0, visible: true, blink: true, shape: CursorShape::Block }
}

fn modes() -> TerminalModes {
    TerminalModes { alternate_screen: false, mouse_tracking: false, bracketed_paste: true }
}

fn frame(message: ServerMessage) -> ServerFrame {
    ServerFrame::new(Some(Id::new("session-a").unwrap()), message).unwrap()
}

fn wide_patch() -> GridPatch {
    let style = CellStyle {
        foreground: Color::Rgb([12, 34, 56]), background: Color::Indexed(128),
        underline_color: Color::Default, underline: Underline::Curly, attributes: STYLE_MASK,
    };
    let link = WireU64(9_007_199_254_740_993);
    GridPatch {
        dimensions: dimensions(2, 1),
        cursor: cursor(),
        lines: vec![Line {
            row: 0, stable_row: WireI64(i64::MIN),
            cells: vec![
                Cell { column: 0, text: Text::new("👩‍💻").unwrap(), width: 2,
                    style: style.clone(), hyperlink: Some(link), image: None },
                Cell { column: 1, text: Text::new("").unwrap(), width: 0,
                    style, hyperlink: Some(link), image: None },
            ].try_into().unwrap(),
        }].try_into().unwrap(),
        hyperlinks: vec![Hyperlink { id: link, uri: Text::new("https://example.invalid/").unwrap() }].try_into().unwrap(),
        images: BoundedVec::new(),
    }
}

#[test]
fn composed_text_and_key_release_are_lossless_and_distinct_from_paste() {
    for text in ["e\u{301}", "👩‍💻", "日本語入力"] {
        for action in [KeyAction::Press, KeyAction::Release, KeyAction::Repeat] {
            let message = ClientMessage::Key(KeyRequest {
                effect: effect(), key: Key::Text(Text::new(text).unwrap()), action,
                modifiers: Modifiers { shift: false, alt: false, control: false, super_key: false },
            });
            let frame = ClientFrame::new(Some(Id::new("session-a").unwrap()), message).unwrap();
            let bytes = encode_client(&frame).unwrap();
            assert_eq!(decode_client(&bytes).unwrap(), frame);
        }
    }
}

#[test]
fn complete_graphemes_styles_wide_continuations_and_reference_ids_round_trip() {
    let message = ServerMessage::Delta(Delta {
        scope: scope(), base_sequence: WireU64(10), sequence: WireU64(11),
        modes: modes(), patch: wide_patch(),
    });
    let frame = frame(message);
    let bytes = encode_server(&frame).unwrap();
    assert_eq!(decode_server(&bytes).unwrap(), frame);
    assert!(String::from_utf8(bytes).unwrap().contains("\"9007199254740993\""));

    let patch = wide_patch();
    let mut cells = patch.lines.as_slice()[0].cells.as_slice().to_vec();
    cells[1].width = 1;
    cells[1].text = Text::new("x").unwrap();
    let mut invalid = patch.clone();
    invalid.lines = vec![Line { row: 0, stable_row: WireI64(0), cells: cells.try_into().unwrap() }].try_into().unwrap();
    assert_eq!(invalid.validate(), Err(WireError::InvalidBounds));

    let mut invalid = patch.clone();
    invalid.hyperlinks = BoundedVec::new();
    assert_eq!(invalid.validate(), Err(WireError::InvalidState));
    let mut invalid = patch;
    invalid.cursor.column = 2;
    assert_eq!(invalid.validate(), Err(WireError::InvalidBounds));
}

#[test]
fn snapshot_declarations_reject_impossible_totals_without_allocating_a_screen() {
    let snapshot = SnapshotStart {
        scope: scope(), snapshot_id: WireU64(1), sequence: WireU64(10),
        dimensions: dimensions(120, 24), cursor: cursor(), modes: modes(),
        title: Text::new("synthetic").unwrap(), cwd: None,
        chunks: 2, total_cells: 2880, total_bytes: 20_000,
    };
    assert!(ServerMessage::SnapshotStart(snapshot.clone()).validate().is_ok());
    let mut impossible = snapshot.clone();
    impossible.chunks = 1;
    assert_eq!(ServerMessage::SnapshotStart(impossible).validate(), Err(WireError::InvalidBounds));
    let mut impossible = snapshot.clone();
    impossible.chunks = 25;
    assert_eq!(ServerMessage::SnapshotStart(impossible).validate(), Err(WireError::InvalidBounds));
    let mut impossible = snapshot;
    impossible.total_bytes = u32::try_from(2 * MAX_FRAME_BYTES + 1).unwrap();
    assert_eq!(ServerMessage::SnapshotStart(impossible).validate(), Err(WireError::InvalidBounds));
    let chunk = SnapshotChunk {
        scope: scope(), snapshot_id: WireU64(1), sequence: WireU64(10),
        index: 1, chunks: 1, patch: wide_patch(),
    };
    assert_eq!(ServerMessage::SnapshotChunk(chunk).validate(), Err(WireError::InvalidBounds));
}

#[test]
fn retired_leases_and_indeterminate_resize_outcomes_keep_their_correlation() {
    for disposition in [LeaseDisposition::Released, LeaseDisposition::Revoked] {
        let message = ServerMessage::LeaseState(LeaseState {
            scope: scope(), request_id: Id::new("lease-request").unwrap(), disposition,
            fence: Some(WireU64(u64::MAX)), ttl_ms: 0,
        });
        let frame = frame(message);
        assert_eq!(decode_server(&encode_server(&frame).unwrap()).unwrap(), frame);
    }
    let invalid = ServerMessage::LeaseState(LeaseState {
        scope: scope(), request_id: Id::new("lease-request").unwrap(),
        disposition: LeaseDisposition::Revoked, fence: None, ttl_ms: 0,
    });
    assert_eq!(invalid.validate(), Err(WireError::InvalidState));

    let indeterminate = frame(ServerMessage::ResizeReceipt(ResizeReceipt {
        effect: effect(), disposition: ReceiptDisposition::Indeterminate,
        dimensions: None, reason: Some(WireError::Indeterminate),
    }));
    assert_eq!(decode_server(&encode_server(&indeterminate).unwrap()).unwrap(), indeterminate);
    let invalid = ServerMessage::ResizeReceipt(ResizeReceipt {
        effect: effect(), disposition: ReceiptDisposition::Accepted,
        dimensions: None, reason: None,
    });
    assert_eq!(invalid.validate(), Err(WireError::InvalidState));
}

#[test]
fn snapshot_lifecycle_detection_and_typed_errors_use_the_same_server_codec() {
    let messages = vec![
        ServerMessage::Subscribed(Subscribed { scope: scope(), request_id: Id::new("subscribe").unwrap(), sequence: WireU64(0), dimensions: dimensions(2, 1) }),
        ServerMessage::Unsubscribed(Unsubscribed { scope: scope(), request_id: Id::new("unsubscribe").unwrap() }),
        ServerMessage::SnapshotStart(SnapshotStart { scope: scope(), snapshot_id: WireU64(1), sequence: WireU64(1), dimensions: dimensions(2, 1), cursor: cursor(), modes: modes(), title: Text::new("title").unwrap(), cwd: None, chunks: 1, total_cells: 2, total_bytes: 4096 }),
        ServerMessage::SnapshotChunk(SnapshotChunk { scope: scope(), snapshot_id: WireU64(1), sequence: WireU64(1), index: 0, chunks: 1, patch: wide_patch() }),
        ServerMessage::SnapshotEnd(SnapshotEnd { scope: scope(), snapshot_id: WireU64(1), sequence: WireU64(1), chunks: 1, total_cells: 2, total_bytes: 4096 }),
        ServerMessage::ResyncRequired(Resync { scope: scope(), last_applied: WireU64(1), reason: ResyncReason::Gap }),
        ServerMessage::Detection(Detection { scope: scope(), event_id: WireU64(u64::MAX), rule_id: Text::new("synthetic.rule").unwrap(), severity: Severity::Warning, detected_at_ms: WireU64(1) }),
        ServerMessage::PaneEvent(PaneEvent { scope: scope(), event: PaneEventKind::Resized, title: None, dimensions: Some(dimensions(80, 24)) }),
        ServerMessage::Error(ProtocolError { request_id: Some(Id::new("request-a").unwrap()), scope: Some(scope()), code: WireError::Denied }),
    ];
    for message in messages {
        let frame = frame(message);
        assert_eq!(decode_server(&encode_server(&frame).unwrap()).unwrap(), frame);
    }
}
