//! Browser application contracts: assertions inspect the installed model, not
//! only an encoder/decoder round trip or queue admission.

use frankenterm_browser_ui::{ApplyOutcome, TerminalClient, TerminalModel};
use frankenterm_browser_ui::terminal_model::{
    MAX_MODEL_PANES, MAX_PENDING_SNAPSHOTS, MAX_RETIRED_SCOPES,
    RECOVERY_INTERVAL_MS, SNAPSHOT_TIMEOUT_MS,
};
use frankenterm_terminal_wire::*;

fn id(value: &str) -> Id { Id::new(value).unwrap() }
fn json(value: &impl serde::Serialize) -> String { serde_json::to_string(value).unwrap() }
fn dims(columns: u16, rows: u16) -> Dimensions {
    Dimensions { columns, rows, pixel_width: 0, pixel_height: 0 }
}
fn cursor() -> Cursor {
    Cursor { column: 0, row: 0, visible: true, blink: false, shape: CursorShape::Bar }
}
fn modes() -> TerminalModes {
    TerminalModes { alternate_screen: false, mouse_tracking: false, bracketed_paste: true }
}
fn style() -> CellStyle {
    CellStyle {
        foreground: Color::Rgb([12, 34, 56]), background: Color::Indexed(17),
        underline_color: Color::Default, underline: Underline::Curly,
        attributes: STYLE_BOLD | STYLE_ITALIC | STYLE_STRIKETHROUGH,
    }
}
fn scope(pane: &str, epoch: u64) -> Scope {
    Scope {
        identity: PaneIdentity { host: id("host"), pane: id(pane), incarnation: id("incarnation") },
        epoch: WireU64(epoch),
    }
}
fn line(row: u16, columns: u16, text: &str, link: Option<u64>) -> Line {
    let cells = (0..columns).map(|column| Cell {
        column, text: Text::new(if column == 0 { text } else { " " }).unwrap(),
        width: 1, style: style(), hyperlink: link.map(WireU64), image: None,
    }).collect::<Vec<_>>();
    Line { row, stable_row: WireI64(i64::from(row) - 1000), cells: BoundedVec::try_from(cells).unwrap() }
}
fn link(number: u64, tail: &str) -> Hyperlink {
    Hyperlink { id: WireU64(number), uri: Text::new(&format!("https://example.invalid/{tail}")).unwrap() }
}
fn frozen(scope: &Scope, dimensions: &Dimensions, snapshot_id: u64, sequence: u64) -> FrozenSnapshot {
    FrozenSnapshot {
        scope: scope.clone(), snapshot_id: WireU64(snapshot_id), sequence: WireU64(sequence),
        dimensions: dimensions.clone(), cursor: cursor(), modes: modes(), title: Text::new("terminal").unwrap(),
        cwd: None, lines: BoundedVec::try_from((0..dimensions.rows).map(|row|
            line(row, dimensions.columns, "a", None)).collect::<Vec<_>>()).unwrap(),
        hyperlinks: BoundedVec::new(), images: BoundedVec::new(),
    }
}
fn bytes(message: ServerMessage) -> Vec<u8> {
    encode_server(&ServerFrame::new(Some(id("session")), message).unwrap()).unwrap()
}

struct Fixture {
    server: ServerSession,
    client: TerminalClient,
    scope: Scope,
    dimensions: Dimensions,
}
impl Fixture {
    fn new(columns: u16, rows: u16) -> Self {
        let scope = scope("pane", 1);
        let dimensions = dims(columns, rows);
        let caps = Capabilities::try_from(vec![
            Capability::TerminalState, Capability::ResourceReferences, Capability::Control,
            Capability::Input, Capability::Resize, Capability::Events,
        ]).unwrap();
        let asset = Sha256::new(&"a".repeat(64)).unwrap();
        let hello = ClientFrame::new(None, ClientMessage::Hello(Hello {
            host: id("host"), hello_id: id("hello"), asset: asset.clone(),
            required: caps.clone(), optional: Capabilities::new(),
        })).unwrap();
        let mut client = TerminalClient::from_hello_frame(&encode_client(&hello).unwrap()).unwrap();
        let mut server = ServerSession::new(id("host"), id("session"), asset, caps).unwrap();
        let welcome = server.negotiate(&client.hello_frame().unwrap()).unwrap();
        client.accept_welcome(&welcome).unwrap();
        server.bind_scope(scope.clone(), dimensions.clone(), WireU64(0)).unwrap();
        client.bind_scope(&json(&scope), &json(&dimensions), "0").unwrap();
        Self { server, client, scope, dimensions }
    }
    fn publish(&self, snapshot_id: u64, sequence: u64) -> Vec<Vec<u8>> {
        self.server.encode_snapshot(frozen(&self.scope, &self.dimensions, snapshot_id, sequence)).unwrap().into_frames()
    }
    fn apply(&mut self, frames: &[Vec<u8>], now_ms: u64) {
        for frame in frames { self.client.ingest_server(frame, now_ms).unwrap(); }
    }
    fn text(&self) -> String {
        self.client.screen_text_page(&json(&self.scope), 0, self.dimensions.rows.min(64)).unwrap()
    }
}

#[test]
fn native_multichunk_snapshot_installs_rich_model_before_ack() {
    let mut fixture = Fixture::new(4, 65);
    let target = 9_007_199_254_740_993;
    let mut snapshot = frozen(&fixture.scope, &fixture.dimensions, 7, target);
    let mut rows = snapshot.lines.into_vec();
    let mut rich = line(0, 4, "界", Some(target));
    let mut cells = rich.cells.into_vec();
    cells[0].width = 2;
    cells[0].image = Some(WireU64(target + 2));
    cells[1].text = Text::new("").unwrap();
    cells[1].width = 0;
    cells[1].image = Some(WireU64(target + 2));
    cells[2].text = Text::new("e\u{301}").unwrap();
    cells[3].text = Text::new("z").unwrap();
    rich.cells = BoundedVec::try_from(cells).unwrap();
    rows[0] = rich.clone();
    rows[64] = line(64, 4, "z", Some(target));
    snapshot.lines = BoundedVec::try_from(rows).unwrap();
    snapshot.hyperlinks = BoundedVec::try_from(vec![link(target, "a")]).unwrap();
    snapshot.images = BoundedVec::try_from(vec![ImageReference {
        id: WireU64(target + 2), digest: Sha256::new(&"b".repeat(64)).unwrap(),
        pixel_width: 8, pixel_height: 12, media_type: Text::new("image/png").unwrap(),
    }]).unwrap();
    let frames = fixture.server.encode_snapshot(snapshot).unwrap().into_frames();
    assert!(frames.len() >= 4, "actual producer must emit multiple chunks");
    let scope_json = json(&fixture.scope);
    let fabricated = ClientMessage::AppliedAck(AppliedAck { scope: fixture.scope.clone(), sequence: WireU64(0) });
    assert_eq!(fixture.client.encode_request(&json(&fabricated)), Err(WireError::StaleSequence));
    assert_eq!(fixture.client.record_delivery(&scope_json, "0", "0"), Err(WireError::StaleSequence));
    for frame in &frames[..frames.len() - 1] {
        fixture.client.ingest_server(frame, 100).unwrap();
        assert!(fixture.client.model().screen(&fixture.scope).unwrap().is_none());
        assert_eq!(fixture.client.take_applied_ack(&scope_json).unwrap(), None);
    }
    assert!(matches!(fixture.client.ingest_server(frames.last().unwrap(), 101).unwrap(), ApplyOutcome::Applied { .. }));
    let screen = fixture.client.model().screen(&fixture.scope).unwrap().unwrap();
    assert_eq!(screen.sequence(), WireU64(target));
    assert_eq!(screen.lines().len(), 65);
    assert_eq!(screen.lines()[0], rich);
    assert_eq!(screen.cursor(), &cursor());
    assert_eq!(screen.modes(), &modes());
    assert_eq!(screen.hyperlinks().get(&target), Some(&link(target, "a")));
    assert_eq!(screen.images()[&(target + 2)].pixel_height, 12);
    assert_eq!(fixture.client.screen_text_page(&scope_json, 0, 1).unwrap(), "界e\u{301}z");
    let row_json = fixture.client.screen_row_json(&scope_json, 0).unwrap();
    assert!(row_json.contains("9007199254740993"));
    assert!(row_json.contains("curly"));
    let ack = fixture.client.take_applied_ack(&scope_json).unwrap().unwrap();
    assert_eq!(fixture.server.decode_request(&ack), Err(WireError::StaleSequence));
    fixture.server.record_delivery(&fixture.scope, WireU64(0), WireU64(target)).unwrap();
    assert_eq!(fixture.server.decode_request(&ack).unwrap().message,
        ClientMessage::AppliedAck(AppliedAck { scope: fixture.scope.clone(), sequence: WireU64(target) }));
    assert!(fixture.client.take_applied_ack(&scope_json).unwrap().is_none());
    assert_eq!(fixture.client.record_delivery(&scope_json, "0", &(target + 1).to_string()), Err(WireError::StaleSequence));
    fixture.client.record_delivery(&scope_json, "0", &target.to_string()).unwrap();
}

#[test]
fn sequence_zero_is_a_real_baseline_only_after_a_snapshot() {
    let mut fixture = Fixture::new(2, 1);
    let scope_json = json(&fixture.scope);
    assert!(fixture.client.take_applied_ack(&scope_json).unwrap().is_none());
    let frames = fixture.publish(1, 0);
    fixture.apply(&frames, 0);
    assert_eq!(fixture.client.model().applied_sequence(&fixture.scope).unwrap(), Some(WireU64(0)));
    let ack = fixture.client.take_applied_ack(&scope_json).unwrap().unwrap();
    fixture.server.record_delivery(&fixture.scope, WireU64(0), WireU64(0)).unwrap();
    assert_eq!(fixture.server.decode_request(&ack).unwrap().message,
        ClientMessage::AppliedAck(AppliedAck { scope: fixture.scope.clone(), sequence: WireU64(0) }));
}

#[test]
fn out_of_order_chunks_and_exact_duplicates_install_once() {
    let mut fixture = Fixture::new(4, 65);
    let frames = fixture.publish(1, 10);
    assert_eq!(frames.len(), 4);
    fixture.client.ingest_server(&frames[0], 1).unwrap();
    fixture.client.ingest_server(&frames[2], 2).unwrap();
    assert!(matches!(fixture.client.ingest_server(&frames[2], 3).unwrap(), ApplyOutcome::Duplicate { .. }));
    fixture.client.ingest_server(&frames[1], 4).unwrap();
    fixture.client.ingest_server(&frames[3], 5).unwrap();
    let before = fixture.text();
    assert!(matches!(fixture.client.ingest_server(&frames[3], 6).unwrap(), ApplyOutcome::Duplicate { .. }));
    assert!(matches!(fixture.client.ingest_server(&frames[1], 7).unwrap(), ApplyOutcome::Duplicate { .. }));
    assert_eq!(fixture.text(), before);
}

#[test]
fn incomplete_or_mismatched_end_never_advances_model_or_codec() {
    for corrupt_totals in [false, true] {
        let mut fixture = Fixture::new(4, 65);
        let frames = fixture.publish(1, 10);
        fixture.client.ingest_server(&frames[0], 1).unwrap();
        fixture.client.ingest_server(&frames[1], 2).unwrap();
        if corrupt_totals { fixture.client.ingest_server(&frames[2], 3).unwrap(); }
        let mut end = decode_server(frames.last().unwrap()).unwrap();
        if corrupt_totals {
            let ServerMessage::SnapshotEnd(value) = &mut end.message else { panic!("end"); };
            value.total_bytes += 1;
        }
        assert_eq!(fixture.client.ingest_server(&encode_server(&end).unwrap(), 4), Err(WireError::SnapshotIncomplete));
        assert!(fixture.client.model().screen(&fixture.scope).unwrap().is_none());
        assert!(fixture.client.take_applied_ack(&json(&fixture.scope)).unwrap().is_none());
        let delta = Delta {
            scope: fixture.scope.clone(), base_sequence: WireU64(0), sequence: WireU64(1), modes: modes(),
            patch: GridPatch { dimensions: fixture.dimensions.clone(), cursor: cursor(),
                lines: BoundedVec::try_from(vec![line(0, 4, "x", None)]).unwrap(),
                hyperlinks: BoundedVec::new(), images: BoundedVec::new() },
        };
        assert!(fixture.client.decode_server(&bytes(ServerMessage::Delta(delta))).is_ok(), "failed End must leave the codec cursor at zero");
        let replacement = fixture.publish(2, 11);
        fixture.apply(&replacement, 1000);
        assert_eq!(fixture.client.model().applied_sequence(&fixture.scope).unwrap(), Some(WireU64(11)));
    }
}

#[test]
fn stale_and_foreign_frames_cannot_destroy_a_newer_snapshot() {
    let mut fixture = Fixture::new(4, 65);
    let frames = fixture.publish(2, 4);
    fixture.client.ingest_server(&frames[0], 1).unwrap();
    let mut old_start = decode_server(&frames[0]).unwrap();
    let ServerMessage::SnapshotStart(start) = &mut old_start.message else { panic!("start"); };
    start.snapshot_id = WireU64(1);
    start.sequence = WireU64(3);
    assert!(matches!(fixture.client.ingest_server(&encode_server(&old_start).unwrap(), 2).unwrap(), ApplyOutcome::Duplicate { .. }));
    let mut old = decode_server(&frames[1]).unwrap();
    let ServerMessage::SnapshotChunk(chunk) = &mut old.message else { panic!("chunk"); };
    chunk.snapshot_id = WireU64(1);
    chunk.sequence = WireU64(3);
    assert!(matches!(fixture.client.ingest_server(&encode_server(&old).unwrap(), 3).unwrap(), ApplyOutcome::Duplicate { .. }));
    let mut old_end = decode_server(frames.last().unwrap()).unwrap();
    let ServerMessage::SnapshotEnd(end) = &mut old_end.message else { panic!("end"); };
    end.snapshot_id = WireU64(1);
    end.sequence = WireU64(3);
    assert!(matches!(fixture.client.ingest_server(&encode_server(&old_end).unwrap(), 4).unwrap(), ApplyOutcome::Duplicate { .. }));
    let mut foreign = decode_server(&frames[1]).unwrap();
    let ServerMessage::SnapshotChunk(chunk) = &mut foreign.message else { panic!("chunk"); };
    chunk.scope.epoch = WireU64(99);
    assert_eq!(fixture.client.ingest_server(&encode_server(&foreign).unwrap(), 5), Err(WireError::StaleScope));
    assert_eq!(fixture.client.ingest_server(b"not JSON", 6), Err(WireError::Malformed));
    let delta = Delta {
        scope: fixture.scope.clone(), base_sequence: WireU64(99), sequence: WireU64(100), modes: modes(),
        patch: GridPatch { dimensions: fixture.dimensions.clone(), cursor: cursor(),
            lines: BoundedVec::try_from(vec![line(0, 4, "x", None)]).unwrap(),
            hyperlinks: BoundedVec::new(), images: BoundedVec::new() },
    };
    let foreign_session = ServerFrame::new(Some(id("other-session")), ServerMessage::Delta(delta.clone())).unwrap();
    assert_eq!(fixture.client.ingest_server(&encode_server(&foreign_session).unwrap(), 7), Err(WireError::IdentityMismatch));
    let mut foreign_scope = delta;
    foreign_scope.scope.identity.pane = id("other-pane");
    assert_eq!(fixture.client.ingest_server(&bytes(ServerMessage::Delta(foreign_scope)), 8), Err(WireError::StaleScope));
    for frame in &frames[1..] { fixture.client.ingest_server(frame, 9).unwrap(); }
    assert_eq!(fixture.client.model().applied_sequence(&fixture.scope).unwrap(), Some(WireU64(4)));
}

#[test]
fn conflicting_chunk_resources_abort_without_publishing_partial_state() {
    let mut fixture = Fixture::new(4, 65);
    let mut snapshot = frozen(&fixture.scope, &fixture.dimensions, 1, 10);
    let mut rows = snapshot.lines.into_vec();
    rows[0] = line(0, 4, "a", Some(9));
    rows[64] = line(64, 4, "z", Some(9));
    snapshot.lines = BoundedVec::try_from(rows).unwrap();
    snapshot.hyperlinks = BoundedVec::try_from(vec![link(9, "a")]).unwrap();
    let frames = fixture.server.encode_snapshot(snapshot).unwrap().into_frames();
    fixture.client.ingest_server(&frames[0], 1).unwrap();
    fixture.client.ingest_server(&frames[1], 2).unwrap();
    let mut conflicting = decode_server(&frames[2]).unwrap();
    let ServerMessage::SnapshotChunk(chunk) = &mut conflicting.message else { panic!("chunk"); };
    chunk.patch.hyperlinks = BoundedVec::try_from(vec![link(9, "b")]).unwrap();
    assert_eq!(fixture.client.ingest_server(&encode_server(&conflicting).unwrap(), 3), Err(WireError::InvalidState));
    assert!(fixture.client.model().screen(&fixture.scope).unwrap().is_none());
    assert!(fixture.client.take_applied_ack(&json(&fixture.scope)).unwrap().is_none());
}

#[test]
fn delta_preserves_untouched_rows_and_reclaims_only_unused_resources() {
    let mut fixture = Fixture::new(2, 2);
    let mut snapshot = frozen(&fixture.scope, &fixture.dimensions, 1, 10);
    snapshot.lines = BoundedVec::try_from(vec![line(0, 2, "a", Some(1)), line(1, 2, "b", Some(2))]).unwrap();
    snapshot.hyperlinks = BoundedVec::try_from(vec![link(1, "one"), link(2, "two")]).unwrap();
    let frames = fixture.server.encode_snapshot(snapshot).unwrap().into_frames();
    fixture.apply(&frames, 1);
    fixture.server.record_delivery(&fixture.scope, WireU64(0), WireU64(10)).unwrap();
    let delta = Delta {
        scope: fixture.scope.clone(), base_sequence: WireU64(10), sequence: WireU64(11), modes: modes(),
        patch: GridPatch { dimensions: fixture.dimensions.clone(), cursor: cursor(),
            lines: BoundedVec::try_from(vec![line(0, 2, "x", Some(3))]).unwrap(),
            hyperlinks: BoundedVec::try_from(vec![link(3, "three")]).unwrap(), images: BoundedVec::new() },
    };
    let encoded = fixture.server.encode_response(ServerMessage::Delta(delta)).unwrap();
    fixture.client.ingest_server(&encoded, 2).unwrap();
    let screen = fixture.client.model().screen(&fixture.scope).unwrap().unwrap();
    assert_eq!(screen.lines()[1], line(1, 2, "b", Some(2)));
    assert!(!screen.hyperlinks().contains_key(&1));
    assert_eq!(screen.hyperlinks().get(&2), Some(&link(2, "two")));
    assert_eq!(screen.hyperlinks().get(&3), Some(&link(3, "three")));
    let before = fixture.text();
    let mut bad_line = line(0, 2, "!", None);
    bad_line.stable_row = WireI64(-999);
    let bad_delta = Delta {
        scope: fixture.scope.clone(), base_sequence: WireU64(11), sequence: WireU64(12), modes: modes(),
        patch: GridPatch { dimensions: fixture.dimensions.clone(), cursor: cursor(),
            lines: BoundedVec::try_from(vec![bad_line]).unwrap(), hyperlinks: BoundedVec::new(), images: BoundedVec::new() },
    };
    let bad_bytes = bytes(ServerMessage::Delta(bad_delta));
    assert_eq!(fixture.client.ingest_server(&bad_bytes, 3), Err(WireError::InvalidState));
    assert_eq!(fixture.text(), before);
    assert_eq!(fixture.client.model().applied_sequence(&fixture.scope).unwrap(), Some(WireU64(11)));
    assert_eq!(fixture.client.model().recovery_reason(&fixture.scope).unwrap(), Some(ResyncReason::Gap));
    assert!(fixture.client.decode_server(&bad_bytes).is_ok(), "failed application cannot advance codec baseline");
    assert!(fixture.client.take_applied_ack(&json(&fixture.scope)).unwrap().is_none());
}

#[test]
fn mode_switch_and_forward_gap_require_full_recovery() {
    for alternate in [false, true] {
        let mut fixture = Fixture::new(2, 2);
        let frames = fixture.publish(1, 10);
        fixture.apply(&frames, 1);
        let before = fixture.text();
        let mut changed_modes = modes();
        changed_modes.alternate_screen = alternate;
        let delta = Delta {
            scope: fixture.scope.clone(), base_sequence: WireU64(if alternate { 10 } else { 11 }),
            sequence: WireU64(if alternate { 11 } else { 12 }), modes: changed_modes,
            patch: GridPatch { dimensions: fixture.dimensions.clone(), cursor: cursor(),
                lines: BoundedVec::try_from(vec![line(0, 2, "x", None)]).unwrap(),
                hyperlinks: BoundedVec::new(), images: BoundedVec::new() },
        };
        assert_eq!(fixture.client.ingest_server(&bytes(ServerMessage::Delta(delta)), 2),
            Err(if alternate { WireError::StaleScope } else { WireError::StaleSequence }));
        assert_eq!(fixture.text(), before);
        assert!(!fixture.client.model().screen(&fixture.scope).unwrap().unwrap().modes().alternate_screen);
        let request = fixture.client.take_recovery_request(&json(&fixture.scope), "recover", 3).unwrap().unwrap();
        let ClientMessage::Resync(resync) = decode_client(&request).unwrap().message else { panic!("resync"); };
        assert_eq!(resync.last_applied, WireU64(10));
        assert!(fixture.client.take_applied_ack(&json(&fixture.scope)).unwrap().is_none());
    }
}

#[test]
fn duplicates_cannot_extend_timeout_or_exceed_raw_transfer_budget() {
    let mut fixture = Fixture::new(4, 65);
    let frames = fixture.publish(1, 10);
    fixture.client.ingest_server(&frames[0], 100).unwrap();
    fixture.client.ingest_server(&frames[1], 14_000).unwrap();
    fixture.client.ingest_server(&frames[0], 15_099).unwrap();
    assert_eq!(fixture.client.expire_snapshots(1), 0, "backwards clock keeps the high-water mark");
    assert_eq!(fixture.client.expire_snapshots(100 + SNAPSHOT_TIMEOUT_MS), 1);
    assert_eq!(fixture.client.ingest_server(frames.last().unwrap(), 15_101), Err(WireError::SnapshotIncomplete));
    assert!(fixture.client.model().screen(&fixture.scope).unwrap().is_none());

    let mut fixture = Fixture::new(4, 65);
    let frames = fixture.publish(1, 10);
    fixture.client.ingest_server(&frames[0], 0).unwrap();
    let mut padded = frames[1].clone();
    padded.resize(MAX_FRAME_BYTES, b' ');
    let mut limited = false;
    for tick in 1..=40 {
        match fixture.client.ingest_server(&padded, tick) {
            Ok(_) => {}
            Err(WireError::LimitExceeded) => { limited = true; break; }
            Err(error) => panic!("unexpected raw-byte outcome: {error:?}"),
        }
    }
    assert!(limited, "duplicate raw bytes must consume the 8MiB transfer bound");
    assert!(fixture.client.take_applied_ack(&json(&fixture.scope)).unwrap().is_none());
}

#[test]
fn resize_and_retirement_discard_old_baseline_and_staging() {
    let mut fixture = Fixture::new(2, 2);
    let frames = fixture.publish(1, 10);
    fixture.apply(&frames, 0);
    let next = fixture.publish(2, 11);
    fixture.client.ingest_server(&next[0], 1).unwrap();
    fixture.client.record_lease(&json(&fixture.scope), "9", true).unwrap();
    fixture.client.update_dimensions(&json(&fixture.scope), &json(&dims(3, 2))).unwrap();
    assert!(fixture.client.model().screen(&fixture.scope).unwrap().is_none());
    assert_eq!(fixture.client.model().watermark(&fixture.scope).unwrap(), WireU64(10));
    assert!(fixture.client.take_applied_ack(&json(&fixture.scope)).unwrap().is_none());
    assert_eq!(fixture.client.record_lease(&json(&fixture.scope), "8", true), Err(WireError::StaleLease));
    fixture.client.unbind_scope(&json(&fixture.scope)).unwrap();
    assert_eq!(fixture.client.bind_scope(&json(&fixture.scope), &json(&dims(2, 2)), "0"), Err(WireError::StaleScope));
    let mut replacement = fixture.scope.clone();
    replacement.epoch = WireU64(2);
    fixture.client.bind_scope(&json(&replacement), &json(&dims(2, 2)), "0").unwrap();
    assert_eq!(fixture.client.ingest_server(&frames[1], 2), Err(WireError::StaleScope));
    assert!(fixture.client.model().screen(&replacement).unwrap().is_none());
}

fn declared_start(scope: &Scope, dimensions: &Dimensions, chunks: u16, total_bytes: u32) -> SnapshotStart {
    SnapshotStart {
        scope: scope.clone(), snapshot_id: WireU64(1), sequence: WireU64(1), dimensions: dimensions.clone(),
        cursor: cursor(), modes: modes(), title: Text::new("").unwrap(), cwd: None,
        chunks, total_cells: dimensions.cell_count(), total_bytes,
    }
}

#[test]
fn pane_pending_and_aggregate_reservations_are_bounded_before_allocation() {
    let mut model = TerminalModel::new();
    for pane in 0..MAX_MODEL_PANES {
        model.bind_scope(scope(&format!("pane-{pane}"), 1), dims(4, 64), WireU64(0)).unwrap();
    }
    assert_eq!(model.bind_scope(scope("one-too-many", 1), dims(1, 1), WireU64(0)), Err(WireError::LimitExceeded));
    for pane in 0..MAX_PENDING_SNAPSHOTS {
        model.ingest(ServerMessage::SnapshotStart(declared_start(&scope(&format!("pane-{pane}"), 1), &dims(4, 64), 1, 1000)), 200, 0).unwrap();
    }
    let next_scope = scope("pane-4", 1);
    assert_eq!(model.ingest(ServerMessage::SnapshotStart(declared_start(&next_scope, &dims(4, 64), 1, 1000)), 200, 0), Err(WireError::LimitExceeded));
    model.abort_snapshot(&scope("pane-0", 1)).unwrap();
    model.ingest(ServerMessage::SnapshotStart(declared_start(&next_scope, &dims(4, 64), 1, 1000)), 200, 0).unwrap();

    let mut model = TerminalModel::new();
    for pane in 0..3 {
        let scope = scope(&format!("large-{pane}"), 1);
        model.bind_scope(scope.clone(), dims(4, 64), WireU64(0)).unwrap();
        let result = model.ingest(ServerMessage::SnapshotStart(declared_start(&scope, &dims(4, 64), 24, 6 * 1024 * 1024)), 200, 0);
        if pane < 2 { assert!(result.is_ok()); } else { assert_eq!(result, Err(WireError::LimitExceeded)); }
    }
}

#[test]
fn recovery_retries_clock_exhaustion_and_tombstones_fail_closed() {
    let mut fixture = Fixture::new(1, 1);
    let scope_json = json(&fixture.scope);
    for attempt in 0..3 {
        let request = fixture.client.take_recovery_request(&scope_json, "request", attempt * RECOVERY_INTERVAL_MS).unwrap().unwrap();
        assert!(matches!(decode_client(&request).unwrap().message, ClientMessage::SnapshotRequest(_)));
        assert!(fixture.client.take_recovery_request(&scope_json, "request", attempt * RECOVERY_INTERVAL_MS).unwrap().is_none());
    }
    assert!(fixture.client.take_recovery_request(&scope_json, "request", 10_000).unwrap().is_none());
    let mut model = TerminalModel::new();
    let target = scope("clock", 1);
    model.bind_scope(target.clone(), dims(1, 1), WireU64(0)).unwrap();
    assert_eq!(model.ingest(ServerMessage::SnapshotStart(declared_start(&target, &dims(1, 1), 1, 100)), 100, u64::MAX), Err(WireError::InvalidBounds));
    assert!(model.recovery_request(&target, id("request")).unwrap().is_none());

    let mut model = TerminalModel::new();
    for epoch in 1..=MAX_RETIRED_SCOPES + 1 {
        let target = scope("retired", epoch as u64);
        model.bind_scope(target.clone(), dims(1, 1), WireU64(0)).unwrap();
        model.unbind_scope(&target).unwrap();
        assert!(model.screen(&target).is_err(), "history capacity cannot prevent retirement");
    }
    assert_eq!(model.bind_scope(scope("new-pane", 1), dims(1, 1), WireU64(0)), Err(WireError::LimitExceeded));
}
