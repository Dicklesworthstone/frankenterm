use std::collections::{BTreeMap, BTreeSet};

use frankenterm_terminal_wire::*;

const SESSION: &str = "snapshot-session";
const BASELINE: u64 = 7;
const NEXT: u64 = 11;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn scope() -> Scope {
    Scope {
        identity: PaneIdentity {
            host: id("host-snapshot"),
            pane: id("20000000-0000-4000-8000-000000000001"),
            incarnation: id("30000000-0000-4000-8000-000000000002"),
        },
        epoch: WireU64(9_007_199_254_740_993),
    }
}

fn dimensions(columns: u16, rows: u16) -> Dimensions {
    Dimensions {
        columns,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn capabilities() -> Capabilities {
    vec![Capability::TerminalState, Capability::ResourceReferences]
        .try_into()
        .unwrap()
}

fn unopened_server() -> ServerSession {
    ServerSession::new(
        scope().identity.host,
        id(SESSION),
        Sha256::new(&"a".repeat(64)).unwrap(),
        capabilities(),
    )
    .unwrap()
}

fn server(dimensions: Dimensions) -> ServerSession {
    let mut server = unopened_server();
    let hello = ClientFrame::new(
        None,
        ClientMessage::Hello(Hello {
            host: scope().identity.host,
            hello_id: id("snapshot-hello"),
            asset: Sha256::new(&"a".repeat(64)).unwrap(),
            required: capabilities(),
            optional: Capabilities::new(),
        }),
    )
    .unwrap();
    server.negotiate(&encode_client(&hello).unwrap()).unwrap();
    server
        .bind_scope(scope(), dimensions, WireU64(BASELINE))
        .unwrap();
    server
}

fn style() -> CellStyle {
    CellStyle {
        foreground: Color::Default,
        background: Color::Default,
        underline_color: Color::Default,
        underline: Underline::None,
        attributes: 0,
    }
}

fn plain_snapshot(columns: u16, rows: u16, text: &str) -> FrozenSnapshot {
    let text = Text::<256>::new(text).unwrap();
    let lines = (0..rows)
        .map(|row| Line {
            row,
            stable_row: WireI64(i64::MIN + i64::from(row)),
            cells: (0..columns)
                .map(|column| Cell {
                    column,
                    text: text.clone(),
                    width: 1,
                    style: style(),
                    hyperlink: None,
                    image: None,
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        })
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    FrozenSnapshot {
        scope: scope(),
        snapshot_id: WireU64(9_007_199_254_740_995),
        sequence: WireU64(NEXT),
        dimensions: dimensions(columns, rows),
        cursor: Cursor {
            column: 0,
            row: 0,
            visible: true,
            blink: true,
            shape: CursorShape::Bar,
        },
        modes: TerminalModes {
            alternate_screen: true,
            mouse_tracking: true,
            bracketed_paste: true,
        },
        title: Text::new("synthetic: e\u{301} \"quoted\" 🚀").unwrap(),
        cwd: Some(Text::new("file:///synthetic/snapshot").unwrap()),
        lines,
        hyperlinks: BoundedVec::new(),
        images: BoundedVec::new(),
    }
}

fn change_lines(snapshot: &mut FrozenSnapshot, change: impl FnOnce(&mut Vec<Line>)) {
    let mut lines = snapshot.lines.as_slice().to_vec();
    change(&mut lines);
    snapshot.lines = lines.try_into().unwrap();
}

fn image_reference(resource_id: u64) -> ImageReference {
    ImageReference {
        id: WireU64(resource_id),
        digest: Sha256::new(&"b".repeat(64)).unwrap(),
        pixel_width: 16,
        pixel_height: 8,
        media_type: Text::new("image/png").unwrap(),
    }
}

fn linked_snapshot(columns: u16, rows: u16) -> FrozenSnapshot {
    let mut snapshot = plain_snapshot(columns, rows, "x");
    let mut lines = snapshot.lines.as_slice().to_vec();
    let mut links = Vec::new();
    for line in &mut lines {
        let mut cells = line.cells.as_slice().to_vec();
        for cell in &mut cells {
            let resource_id = u64::try_from(links.len() + 1).unwrap();
            cell.hyperlink = Some(WireU64(resource_id));
            links.push(Hyperlink {
                id: WireU64(resource_id),
                uri: Text::new(&format!("urn:ft:snapshot:{resource_id}")).unwrap(),
            });
        }
        line.cells = cells.try_into().unwrap();
    }
    snapshot.lines = lines.try_into().unwrap();
    snapshot.hyperlinks = links.try_into().unwrap();
    snapshot
}

fn chunks(publication: &SnapshotPublication) -> Vec<SnapshotChunk> {
    publication
        .frames()
        .iter()
        .filter_map(|bytes| match decode_server(bytes).unwrap().message {
            ServerMessage::SnapshotChunk(chunk) => Some(chunk),
            _ => None,
        })
        .collect()
}

fn assert_frame_bounds(publication: &SnapshotPublication) {
    let observed: usize = publication.frames().iter().map(Vec::len).sum();
    assert_eq!(publication.wire_bytes(), observed);
    assert!(observed <= MAX_SNAPSHOT_BYTES as usize);
    for bytes in publication.frames() {
        assert!(!bytes.is_empty());
        assert!(bytes.len() <= MAX_FRAME_BYTES);
        let frame = decode_server(bytes).unwrap();
        assert_eq!(frame.protocol.as_str(), PROTOCOL);
        assert_eq!(frame.session.as_ref().unwrap().as_str(), SESSION);
    }
}

fn error_from(server: &ServerSession, snapshot: FrozenSnapshot) -> WireError {
    match server.encode_snapshot(snapshot) {
        Ok(_) => panic!("invalid snapshot unexpectedly exposed a publication"),
        Err(error) => error,
    }
}

fn ack(sequence: u64) -> Vec<u8> {
    encode_client(
        &ClientFrame::new(
            Some(id(SESSION)),
            ClientMessage::AppliedAck(AppliedAck {
                scope: scope(),
                sequence: WireU64(sequence),
            }),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn publication_preserves_the_frozen_image_and_exact_totals_without_acknowledging_it() {
    let mut snapshot = plain_snapshot(4, 2, "x");
    let link_id = WireU64(9_007_199_254_740_993);
    let image_id = WireU64(u64::MAX);
    change_lines(&mut snapshot, |lines| {
        let mut cells = lines[0].cells.as_slice().to_vec();
        cells[0].text = Text::new("e\u{301}").unwrap();
        cells[0].image = Some(image_id);
        cells[1].text = Text::new("👩‍💻").unwrap();
        cells[1].width = 2;
        cells[1].hyperlink = Some(link_id);
        cells[1].style = CellStyle {
            foreground: Color::Rgb([12, 34, 56]),
            background: Color::Indexed(128),
            underline_color: Color::Rgb([1, 2, 3]),
            underline: Underline::Curly,
            attributes: STYLE_BOLD | STYLE_ITALIC | STYLE_REVERSE,
        };
        cells[2].text = Text::new("").unwrap();
        cells[2].width = 0;
        cells[2].hyperlink = Some(link_id);
        cells[2].style = cells[1].style.clone();
        lines[0].cells = cells.try_into().unwrap();
    });
    snapshot.hyperlinks = vec![Hyperlink {
        id: link_id,
        uri: Text::new("https://example.invalid/é?q=\"quoted\"").unwrap(),
    }]
    .try_into()
    .unwrap();
    snapshot.images = vec![image_reference(image_id.0)].try_into().unwrap();
    let expected_lines = snapshot.lines.as_slice().to_vec();
    let expected_links = snapshot.hyperlinks.as_slice().to_vec();
    let expected_images = snapshot.images.as_slice().to_vec();
    let expected_cursor = snapshot.cursor.clone();
    let expected_modes = snapshot.modes.clone();
    let expected_title = snapshot.title.clone();
    let expected_cwd = snapshot.cwd.clone();
    let snapshot_id = snapshot.snapshot_id;
    let mut server = server(snapshot.dimensions.clone());

    let publication = server.encode_snapshot(snapshot).unwrap();
    assert_frame_bounds(&publication);
    assert_eq!(publication.scope(), &scope());
    assert_eq!(publication.snapshot_id(), snapshot_id);
    assert_eq!(publication.sequence(), WireU64(NEXT));
    let ServerMessage::SnapshotStart(start) =
        decode_server(&publication.frames()[0]).unwrap().message
    else {
        panic!("first frame must start the complete snapshot");
    };
    let ServerMessage::SnapshotEnd(end) =
        decode_server(publication.frames().last().unwrap()).unwrap().message
    else {
        panic!("last frame must finish the complete snapshot");
    };
    assert_eq!(start.cursor, expected_cursor);
    assert_eq!(start.modes, expected_modes);
    assert_eq!(start.title, expected_title);
    assert_eq!(start.cwd, expected_cwd);
    assert_eq!(start.total_cells, 8);
    assert_eq!(start.scope, scope());
    assert_eq!(end.scope, scope());
    assert_eq!(start.snapshot_id, snapshot_id);
    assert_eq!(end.snapshot_id, snapshot_id);
    assert_eq!(start.sequence, WireU64(NEXT));
    assert_eq!(end.sequence, WireU64(NEXT));

    let chunks = chunks(&publication);
    let mut observed_lines = Vec::new();
    let mut observed_links = BTreeMap::new();
    let mut observed_images = BTreeMap::new();
    let mut observed_payload_bytes = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(usize::from(chunk.index), index);
        assert_eq!(usize::from(chunk.chunks), chunks.len());
        assert_eq!(chunk.scope, scope());
        assert_eq!(chunk.snapshot_id, snapshot_id);
        assert_eq!(chunk.sequence, WireU64(NEXT));
        assert_eq!(chunk.patch.dimensions, dimensions(4, 2));
        assert_eq!(chunk.patch.cursor, expected_cursor);
        // Independent materialized serializer oracle: do not derive expected
        // totals by adding the publisher's counting helper to itself.
        let payload_bytes = serde_json::to_vec(chunk).unwrap().len();
        assert_eq!(snapshot_chunk_bytes(chunk).unwrap(), payload_bytes);
        observed_payload_bytes += payload_bytes;
        observed_lines.extend_from_slice(chunk.patch.lines.as_slice());
        for link in chunk.patch.hyperlinks.as_slice() {
            if let Some(previous) = observed_links.insert(link.id, link.clone()) {
                assert_eq!(previous, *link);
            }
        }
        for image in chunk.patch.images.as_slice() {
            if let Some(previous) = observed_images.insert(image.id, image.clone()) {
                assert_eq!(previous, *image);
            }
        }
    }
    assert_eq!(observed_lines, expected_lines);
    assert_eq!(observed_links.into_values().collect::<Vec<_>>(), expected_links);
    assert_eq!(observed_images.into_values().collect::<Vec<_>>(), expected_images);
    assert_eq!(usize::from(start.chunks), chunks.len());
    assert_eq!(start.chunks, end.chunks);
    assert_eq!(start.total_cells, end.total_cells);
    assert_eq!(start.total_bytes, end.total_bytes);
    assert_eq!(start.total_bytes, publication.canonical_bytes());
    assert_eq!(start.total_bytes as usize, observed_payload_bytes);
    assert_eq!(publication.frames().len(), chunks.len() + 2);

    assert_eq!(server.decode_request(&ack(NEXT)), Err(WireError::StaleSequence));
    server
        .record_delivery(&scope(), WireU64(BASELINE), WireU64(NEXT))
        .unwrap();
    assert!(server.decode_request(&ack(NEXT)).is_ok());
    let wire_bytes = publication.wire_bytes();
    assert_eq!(publication.into_frames().iter().map(Vec::len).sum::<usize>(), wire_bytes);
}

#[test]
fn oversized_row_groups_split_without_cutting_graphemes_or_wide_rows() {
    let grapheme = format!("a{}", "\u{0301}".repeat(127));
    assert_eq!(grapheme.len(), 255);
    let snapshot = plain_snapshot(MAX_COLUMNS, 3, &grapheme);
    let expected_lines = snapshot.lines.as_slice().to_vec();
    let server = server(snapshot.dimensions.clone());
    let publication = server.encode_snapshot(snapshot).unwrap();
    assert_frame_bounds(&publication);
    let chunks = chunks(&publication);
    assert!(chunks.len() > 1, "the large rows cannot share one frame");
    let observed: Vec<Line> = chunks
        .iter()
        .flat_map(|chunk| chunk.patch.lines.as_slice().iter().cloned())
        .collect();
    assert_eq!(observed, expected_lines);
    let rows: BTreeSet<_> = observed.iter().map(|line| line.row).collect();
    assert_eq!(rows, BTreeSet::from([0, 1, 2]));
    for chunk in chunks {
        assert!(!chunk.patch.lines.is_empty());
        assert!(chunk.patch.cell_count() <= MAX_CHUNK_CELLS);
        assert!(chunk.patch.lines.len() <= MAX_CHUNK_LINES);
    }
}

#[test]
fn per_chunk_resource_limits_split_a_valid_larger_snapshot() {
    let snapshot = linked_snapshot(4, 40);
    assert!(snapshot.hyperlinks.len() > MAX_RESOURCES);
    let expected_lines = snapshot.lines.as_slice().to_vec();
    let expected_links: BTreeMap<_, _> = snapshot
        .hyperlinks
        .as_slice()
        .iter()
        .map(|link| (link.id, link.clone()))
        .collect();
    let server = server(snapshot.dimensions.clone());
    let publication = server.encode_snapshot(snapshot).unwrap();
    assert_frame_bounds(&publication);
    let chunks = chunks(&publication);
    assert!(chunks.len() >= expected_links.len().div_ceil(MAX_RESOURCES));
    let mut observed_lines = Vec::new();
    let mut observed_links = BTreeMap::new();
    for chunk in chunks {
        assert!(chunk.patch.hyperlinks.len() <= MAX_RESOURCES);
        observed_lines.extend_from_slice(chunk.patch.lines.as_slice());
        for link in chunk.patch.hyperlinks.as_slice() {
            assert_eq!(expected_links.get(&link.id), Some(link));
            let _ = observed_links.insert(link.id, link.clone());
        }
    }
    assert_eq!(observed_lines, expected_lines);
    assert_eq!(observed_links, expected_links);
}

#[test]
fn invalid_rows_and_resources_never_return_partial_publications() {
    type Mutation = fn(&mut FrozenSnapshot);
    let cases: &[(&str, Mutation, WireError)] = &[
        ("missing row", |snapshot| {
            change_lines(snapshot, |lines| { let _ = lines.pop(); });
        }, WireError::SnapshotIncomplete),
        ("duplicate row", |snapshot| {
            change_lines(snapshot, |lines| { lines[1].row = 0; });
        }, WireError::InvalidBounds),
        ("row outside viewport", |snapshot| {
            change_lines(snapshot, |lines| { lines[1].row = 2; });
        }, WireError::InvalidBounds),
        ("duplicate stable row", |snapshot| {
            change_lines(snapshot, |lines| { lines[1].stable_row = lines[0].stable_row; });
        }, WireError::InvalidBounds),
        ("incomplete cell row", |snapshot| {
            change_lines(snapshot, |lines| {
                let mut cells = lines[0].cells.as_slice().to_vec();
                let _ = cells.pop();
                lines[0].cells = cells.try_into().unwrap();
            });
        }, WireError::InvalidBounds),
        ("wide lead at right edge", |snapshot| {
            change_lines(snapshot, |lines| {
                let mut cells = lines[0].cells.as_slice().to_vec();
                cells[1].width = 2;
                lines[0].cells = cells.try_into().unwrap();
            });
        }, WireError::InvalidBounds),
        ("missing referenced hyperlink", |snapshot| {
            change_lines(snapshot, |lines| {
                let mut cells = lines[0].cells.as_slice().to_vec();
                cells[0].hyperlink = Some(WireU64(9));
                lines[0].cells = cells.try_into().unwrap();
            });
        }, WireError::InvalidState),
        ("duplicate hyperlink identity", |snapshot| {
            snapshot.hyperlinks = vec![
                Hyperlink { id: WireU64(1), uri: Text::new("urn:ft:first").unwrap() },
                Hyperlink { id: WireU64(1), uri: Text::new("urn:ft:second").unwrap() },
            ].try_into().unwrap();
        }, WireError::InvalidState),
        ("duplicate image identity", |snapshot| {
            snapshot.images = vec![image_reference(1), image_reference(1)].try_into().unwrap();
        }, WireError::InvalidState),
    ];
    let mut server = server(dimensions(2, 2));
    for &(label, mutate, expected) in cases {
        let mut snapshot = plain_snapshot(2, 2, "x");
        mutate(&mut snapshot);
        assert_eq!(error_from(&server, snapshot), expected, "{label}");
        assert_eq!(server.decode_request(&ack(NEXT)), Err(WireError::StaleSequence));
    }
    let valid = server.encode_snapshot(plain_snapshot(2, 2, "x")).unwrap();
    assert_frame_bounds(&valid);
}

#[test]
fn publisher_obeys_negotiation_identity_geometry_and_sequence_fences() {
    let unopened = unopened_server();
    assert_eq!(
        error_from(&unopened, plain_snapshot(2, 2, "x")),
        WireError::NotNegotiated,
    );
    let mut server = server(dimensions(2, 2));
    let mut wrong_host = plain_snapshot(2, 2, "x");
    wrong_host.scope.identity.host = id("other-host");
    assert_eq!(error_from(&server, wrong_host), WireError::IdentityMismatch);
    let mut wrong_epoch = plain_snapshot(2, 2, "x");
    wrong_epoch.scope.epoch = WireU64(1);
    assert_eq!(error_from(&server, wrong_epoch), WireError::StaleScope);
    let mut old = plain_snapshot(2, 2, "x");
    old.sequence = WireU64(BASELINE - 1);
    assert_eq!(error_from(&server, old), WireError::StaleSequence);
    assert_eq!(
        error_from(&server, plain_snapshot(3, 2, "x")),
        WireError::StaleSequence,
    );
    assert!(server.encode_snapshot(plain_snapshot(2, 2, "x")).is_ok());
    assert_eq!(server.decode_request(&ack(NEXT)), Err(WireError::StaleSequence));
}

fn one_row_chunk(snapshot: &FrozenSnapshot, line: Line, index: u16, count: u16) -> SnapshotChunk {
    SnapshotChunk {
        scope: snapshot.scope.clone(),
        snapshot_id: snapshot.snapshot_id,
        sequence: snapshot.sequence,
        index,
        chunks: count,
        patch: GridPatch {
            dimensions: snapshot.dimensions.clone(),
            cursor: snapshot.cursor.clone(),
            lines: vec![line].try_into().unwrap(),
            hyperlinks: snapshot.hyperlinks.as_slice().to_vec().try_into().unwrap(),
            images: snapshot.images.as_slice().to_vec().try_into().unwrap(),
        },
    }
}

#[test]
fn a_fitting_payload_does_not_hide_an_oversized_envelope() {
    let mut snapshot = linked_snapshot(64, 1);
    let mut chunk = one_row_chunk(&snapshot, snapshot.lines.as_slice()[0].clone(), 0, 1);
    let target = MAX_FRAME_BYTES - 16;
    let initial = serde_json::to_vec(&chunk).unwrap().len();
    let mut remaining = target.checked_sub(initial).unwrap();
    let mut links = chunk.patch.hyperlinks.as_slice().to_vec();
    for link in &mut links {
        let added = remaining.min(4096 - link.uri.len());
        let mut uri = link.uri.as_str().to_owned();
        uri.push_str(&"a".repeat(added));
        link.uri = Text::new(&uri).unwrap();
        remaining -= added;
    }
    assert_eq!(remaining, 0, "fixture must fill its intended payload budget");
    chunk.patch.hyperlinks = links.try_into().unwrap();
    assert_eq!(serde_json::to_vec(&chunk).unwrap().len(), target);
    assert_eq!(snapshot_chunk_bytes(&chunk).unwrap(), target);
    let frame = ServerFrame::new(Some(id(SESSION)), ServerMessage::SnapshotChunk(chunk.clone())).unwrap();
    assert_eq!(frame.validate(), Ok(()));
    assert_eq!(encode_server(&frame), Err(WireError::FrameTooLarge));
    snapshot.hyperlinks = chunk.patch.hyperlinks.into_vec().try_into().unwrap();
    let server = server(snapshot.dimensions.clone());
    assert_eq!(error_from(&server, snapshot), WireError::FrameTooLarge);
    assert!(server.encode_snapshot(plain_snapshot(64, 1, "x")).is_ok());
}

fn boundary_metadata(snapshot: &FrozenSnapshot, count: u16, total_bytes: u32) -> (Vec<u8>, Vec<u8>) {
    let start = ServerMessage::SnapshotStart(SnapshotStart {
        scope: snapshot.scope.clone(),
        snapshot_id: snapshot.snapshot_id,
        sequence: snapshot.sequence,
        dimensions: snapshot.dimensions.clone(),
        cursor: snapshot.cursor.clone(),
        modes: snapshot.modes.clone(),
        title: snapshot.title.clone(),
        cwd: snapshot.cwd.clone(),
        chunks: count,
        total_cells: snapshot.dimensions.cell_count(),
        total_bytes,
    });
    let end = ServerMessage::SnapshotEnd(SnapshotEnd {
        scope: snapshot.scope.clone(),
        snapshot_id: snapshot.snapshot_id,
        sequence: snapshot.sequence,
        chunks: count,
        total_cells: snapshot.dimensions.cell_count(),
        total_bytes,
    });
    (
        encode_server(&ServerFrame::new(Some(id(SESSION)), start).unwrap()).unwrap(),
        encode_server(&ServerFrame::new(Some(id(SESSION)), end).unwrap()).unwrap(),
    )
}

#[test]
fn aggregate_admission_counts_start_end_and_envelopes_not_only_chunk_payloads() {
    // 63 rows force one whole row per frame at this size. The independent
    // construction leaves all chunk frames below the aggregate cap, but the
    // complete Start + chunks + End transfer exceeds it. This is a causal
    // control for accidentally checking payload totals or chunk frames alone.
    let count = 63_u16;
    let mut snapshot = plain_snapshot(MAX_COLUMNS, count, "a");
    let probe = one_row_chunk(&snapshot, snapshot.lines.as_slice()[0].clone(), 0, count);
    let payload_len = serde_json::to_vec(&probe).unwrap().len();
    let probe_frame = encode_server(
        &ServerFrame::new(Some(id(SESSION)), ServerMessage::SnapshotChunk(probe)).unwrap(),
    ).unwrap();
    let envelope_bytes = probe_frame.len() - payload_len;
    let (start_probe, end_probe) = boundary_metadata(&snapshot, count, MAX_SNAPSHOT_BYTES);
    let metadata_bytes = start_probe.len() + end_probe.len();
    assert!(metadata_bytes / 2 > 2 * usize::from(count));
    let payload_target = (MAX_SNAPSHOT_BYTES as usize
        - envelope_bytes * usize::from(count)
        - metadata_bytes / 2)
        / usize::from(count);
    let mut lines = snapshot.lines.as_slice().to_vec();
    for (index, line) in lines.iter_mut().enumerate() {
        let base = one_row_chunk(&snapshot, line.clone(), u16::try_from(index).unwrap(), count);
        let mut remaining = payload_target - serde_json::to_vec(&base).unwrap().len();
        let mut cells = line.cells.as_slice().to_vec();
        for cell in &mut cells {
            let accents = (remaining / 2).min(127);
            cell.text = Text::new(&format!("a{}", "\u{0301}".repeat(accents))).unwrap();
            remaining -= accents * 2;
        }
        assert!(remaining < 2, "fixture's bounded cells must absorb the budget");
        line.cells = cells.try_into().unwrap();
    }
    snapshot.lines = lines.try_into().unwrap();
    let mut canonical_bytes = 0usize;
    let mut chunk_wire_bytes = 0usize;
    let mut previous_line_bytes = None;
    for (index, line) in snapshot.lines.as_slice().iter().enumerate() {
        let line_bytes = serde_json::to_vec(line).unwrap().len();
        if let Some(previous) = previous_line_bytes {
            assert!(previous + line_bytes > MAX_FRAME_BYTES, "two rows cannot share a frame");
        }
        previous_line_bytes = Some(line_bytes);
        let chunk = one_row_chunk(&snapshot, line.clone(), u16::try_from(index).unwrap(), count);
        canonical_bytes += serde_json::to_vec(&chunk).unwrap().len();
        let frame = ServerFrame::new(Some(id(SESSION)), ServerMessage::SnapshotChunk(chunk)).unwrap();
        let bytes = encode_server(&frame).unwrap();
        assert!(bytes.len() <= MAX_FRAME_BYTES);
        chunk_wire_bytes += bytes.len();
    }
    let (start, end) = boundary_metadata(&snapshot, count, u32::try_from(canonical_bytes).unwrap());
    assert!(canonical_bytes <= MAX_SNAPSHOT_BYTES as usize);
    assert!(chunk_wire_bytes <= MAX_SNAPSHOT_BYTES as usize);
    assert!(start.len() + chunk_wire_bytes + end.len() > MAX_SNAPSHOT_BYTES as usize);
    let mut server = server(snapshot.dimensions.clone());
    assert_eq!(error_from(&server, snapshot), WireError::LimitExceeded);
    assert_eq!(server.decode_request(&ack(NEXT)), Err(WireError::StaleSequence));
    server
        .record_delivery(&scope(), WireU64(BASELINE), WireU64(NEXT))
        .unwrap();
}
