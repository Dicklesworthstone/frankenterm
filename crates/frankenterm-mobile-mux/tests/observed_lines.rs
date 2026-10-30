//! Exercise the live mobile session through the desktop codec, with row
//! replies deliberately held across output, layout changes and view visits.
mod support;

use codec::{
    GetLinesAtLayout, GetLinesAtLayoutResponse, GetPaneRenderChangesResponse,
    MuxWireDecodedPayload, MuxWireDialect, Pdu,
};
use frankenterm_mobile_mux::{Session, wire};
use mux::renderable::{terminal_get_cursor_position, terminal_get_dimensions, terminal_get_lines};
use std::ops::Range;
use support::{Harness, server_frame};

fn connected(version: usize) -> Harness {
    let mut harness = Harness::new(4, 30);
    harness.server.codec_vers = version;
    harness.server.min_supported = 61;
    harness.server.term.advance_bytes(b"ORIGINAL\r\nSECOND");
    harness.attach();
    assert!(harness.session.is_ready(), "{:?}", harness.events);
    assert!(harness.session.advance_time(100));
    harness.outputs = 0;
    harness.events.clear();
    harness
}

fn take_one(session: &mut Session) -> (u64, Pdu) {
    let bytes = session.take_outgoing();
    assert!(!bytes.is_empty(), "a bounded follow-up must exist");
    let mut cursor = std::io::Cursor::new(bytes.as_slice());
    let decoded = Pdu::decode_for_dialect(
        &mut cursor,
        MuxWireDialect::current(codec::CODEC_VERSION).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cursor.position() as usize,
        bytes.len(),
        "one read at a time"
    );
    let (serial, payload) = decoded.into_parts();
    let MuxWireDecodedPayload::Pdu(pdu) = payload else {
        panic!("current PDU")
    };
    (serial, pdu)
}

fn deliver<T: serde::Serialize>(session: &mut Session, ident: u64, serial: u64, value: &T) {
    session.receive(&server_frame(
        ident,
        serial,
        &varbincode::serialize(value).unwrap(),
        false,
    ));
}

fn acknowledge_size(session: &mut Session, serial: u64, request: Pdu) {
    match request {
        Pdu::Resize(_) => deliver(session, 10, serial, &codec::UnitResponse {}),
        Pdu::ResizePaneConditionalV1(request) => deliver(
            session,
            120,
            serial,
            &codec::ResizePaneConditionalV1Response {
                pane_id: request.pane_id,
                identity: request.identity,
                outcome: codec::ResizePaneConditionalOutcomeV1::Enqueued,
            },
        ),
        _ => panic!("expected a geometry effect, never a read"),
    }
}

fn observation(
    harness: &mut Harness,
    dirty_lines: Vec<Range<isize>>,
    include_rows: bool,
) -> GetPaneRenderChangesResponse {
    let dimensions = terminal_get_dimensions(&mut harness.server.term);
    let cursor_position = terminal_get_cursor_position(&mut harness.server.term);
    let rows = if include_rows {
        let (first, rows) = terminal_get_lines(
            &mut harness.server.term,
            dimensions.physical_top..dimensions.physical_top + dimensions.viewport_rows as isize,
        );
        rows.into_iter()
            .enumerate()
            .map(|(index, line)| (first + index as isize, line))
            .collect()
    } else {
        Vec::new()
    };
    GetPaneRenderChangesResponse {
        pane_id: harness.server.pane_id,
        mouse_grabbed: false,
        alt_screen_active: harness.server.term.is_alt_screen_active(),
        cursor_position,
        dimensions,
        tiered_scrollback_status: None,
        dirty_lines,
        title: "observed".into(),
        working_dir: None,
        bonus_lines: rows.into(),
        input_serial: None,
        seqno: harness.server.term.current_seqno(),
    }
}

fn line_reply(harness: &mut Harness, request: &GetLinesAtLayout) -> GetLinesAtLayoutResponse {
    let mut entries = Vec::new();
    for range in &request.lines {
        let (first, rows) = terminal_get_lines(&mut harness.server.term, range.clone());
        entries.extend(
            rows.into_iter()
                .enumerate()
                .map(|(index, line)| (first + index as isize, line)),
        );
    }
    GetLinesAtLayoutResponse {
        pane_id: request.pane_id,
        layout: request.layout,
        lines: entries.into(),
    }
}

fn pending_row(harness: &mut Harness) -> (u64, GetLinesAtLayoutResponse) {
    harness
        .server
        .term
        .advance_bytes(b"\x1b[H\x1b[2KOLDER REPLY");
    let change = observation(harness, std::iter::once(0..1).collect(), false);
    deliver(&mut harness.session, 25, 0, &change);
    let (serial, pdu) = take_one(&mut harness.session);
    let Pdu::GetLinesAtLayout(request) = pdu else {
        panic!("rows must carry their observed layout")
    };
    assert_eq!(request.layout.seqno, change.seqno);
    assert_eq!(request.layout.dimensions, change.dimensions);
    (serial, line_reply(harness, &request))
}

fn screen(harness: &mut Harness) -> String {
    let dimensions = terminal_get_dimensions(&mut harness.phone);
    let (_, rows) = terminal_get_lines(
        &mut harness.phone,
        dimensions.physical_top..dimensions.physical_top + dimensions.viewport_rows as isize,
    );
    let mut text = String::new();
    for row in rows {
        for cell in row.visible_cells() {
            text.push_str(cell.str());
        }
        text.push('\n');
    }
    text
}

#[test]
fn negotiated_observations_and_row_fences_use_the_desktop_wire_contract() {
    for version in [68, 69, 71] {
        let mut harness = Harness::new(4, 30);
        harness.server.codec_vers = version;
        harness.server.min_supported = 61;
        for row in 0..20 {
            harness
                .server
                .term
                .advance_bytes(format!("line {row}\r\n").as_bytes());
        }
        harness.attach();
        assert!(harness.session.is_ready(), "{:?}", harness.events);
        assert_eq!(
            harness.server.requests.contains(&"GetPaneRenderStateV1"),
            version >= 69
        );
        assert_eq!(
            harness.server.requests.contains(&"GetLinesAtLayout"),
            version >= 69
        );
        if version >= 69 {
            assert!(!harness.server.requests.contains(&"GetPaneRenderChanges"));
            assert!(!harness.server.requests.contains(&"GetLines"));
        }
        assert!(screen(&mut harness).contains("line 19"));
        assert!(harness.server.resizes.is_empty());
    }
}

#[test]
fn delayed_rows_never_replace_a_newer_push_at_the_same_coordinates() {
    let mut harness = connected(71);
    let (serial, old) = pending_row(&mut harness);
    harness
        .server
        .term
        .advance_bytes(b"\x1b[H\x1b[2KNEWER PUSH");
    let newer = observation(&mut harness, std::iter::once(0..1).collect(), true);
    deliver(&mut harness.session, 25, 0, &newer);
    assert!(
        harness.session.take_outgoing().is_empty(),
        "the old read still owns its slot"
    );
    deliver(&mut harness.session, 103, serial, &old);
    harness.pump();
    let text = screen(&mut harness);
    assert!(text.contains("NEWER PUSH"), "{text}");
    assert!(!text.contains("OLDER REPLY"), "{text}");
}

#[test]
fn unrelated_output_does_not_starve_a_read_of_an_unchanged_row() {
    let mut harness = connected(71);
    let (serial, old) = pending_row(&mut harness);
    harness
        .server
        .term
        .advance_bytes(b"\x1b[2;1H\x1b[2KNEW SECOND ROW");
    let mut newer = observation(&mut harness, std::iter::once(1..2).collect(), false);
    let (first, rows) = terminal_get_lines(&mut harness.server.term, 1..2);
    newer.bonus_lines = rows
        .into_iter()
        .enumerate()
        .map(|(index, line)| (first + index as isize, line))
        .collect::<Vec<_>>()
        .into();
    deliver(&mut harness.session, 25, 0, &newer);
    deliver(&mut harness.session, 103, serial, &old);
    harness.pump();
    let text = screen(&mut harness);
    assert!(
        text.contains("OLDER REPLY") && text.contains("NEW SECOND ROW"),
        "{text}"
    );
    assert!(harness.session.take_outgoing().is_empty());
}

#[test]
fn resize_and_alternate_screen_retire_old_row_coordinates() {
    for alternate in [false, true] {
        let mut harness = connected(71);
        let (serial, old) = pending_row(&mut harness);
        if alternate {
            harness.server.term.advance_bytes(b"\x1b[?1049hNEW SCREEN");
        } else {
            harness.server.term.resize(support::size(4, 22));
            harness
                .server
                .term
                .advance_bytes(b"\x1b[H\x1b[2KNEW GEOMETRY");
        }
        let newer = observation(&mut harness, std::iter::once(0..4).collect(), true);
        deliver(&mut harness.session, 25, 0, &newer);
        deliver(&mut harness.session, 103, serial, &old);
        harness.pump();
        let text = screen(&mut harness);
        assert!(
            text.contains(if alternate {
                "NEW SCREEN"
            } else {
                "NEW GEOMETRY"
            }),
            "{text}"
        );
        assert!(!text.contains("OLDER REPLY"), "{text}");
        assert_eq!(
            harness.phone.get_size().cols,
            if alternate { 30 } else { 22 }
        );
    }
}

#[test]
fn refused_fenced_rows_keep_the_last_display_until_a_fresh_observation() {
    let mut harness = connected(71);
    let before = screen(&mut harness);
    let (serial, _) = pending_row(&mut harness);
    deliver(
        &mut harness.session,
        0,
        serial,
        &codec::ErrorResponse::backend_failure(102),
    );
    harness.pump();
    assert_eq!(
        harness.outputs, 0,
        "a refused read cannot manufacture blank rows"
    );
    assert_eq!(screen(&mut harness), before);
    let due = harness
        .session
        .next_input_wake_ns()
        .expect("bounded render retry");
    assert!(harness.session.advance_time(due - 1));
    assert!(harness.session.take_outgoing().is_empty());
    assert!(harness.session.advance_time(due));
    let (poll, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::GetPaneRenderStateV1(_)));
    let refreshed = observation(&mut harness, std::iter::once(0..1).collect(), true);
    deliver(&mut harness.session, 25, poll, &refreshed);
    harness.pump();
    assert!(screen(&mut harness).contains("OLDER REPLY"));
    assert_eq!(harness.session.next_input_wake_ns(), None);
    assert!(harness.server.resizes.is_empty() && harness.server.written.is_empty());
}

#[test]
fn an_unchanged_correlated_observation_settles_waiting_without_repainting() {
    let mut harness = connected(71);
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 4, 30);
    let (resize, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::Resize(_)));
    // No geometry actually changes; the observation still has its own reply.
    deliver(&mut harness.session, 10, resize, &codec::UnitResponse {});
    let (mut poll, _) = take_one(&mut harness.session);
    for _ in 0..frankenterm_mobile_mux::session::WAITING_AFTER_REFUSALS {
        deliver(
            &mut harness.session,
            0,
            poll,
            &codec::ErrorResponse::resource_busy(114),
        );
        harness.pump();
        let due = harness.session.next_input_wake_ns().unwrap();
        assert!(harness.session.advance_time(due));
        let (next, pdu) = take_one(&mut harness.session);
        assert!(matches!(pdu, Pdu::GetPaneRenderStateV1(_)));
        poll = next;
    }
    let mut unchanged = observation(&mut harness, vec![], false);
    unchanged.title = harness.server.title.clone();
    deliver(&mut harness.session, 25, poll, &unchanged);
    harness.pump();
    let waiting: Vec<_> = harness
        .events
        .iter()
        .filter_map(|event| match event {
            frankenterm_mobile_mux::Event::PaneWaiting { waiting, .. } => Some(*waiting),
            _ => None,
        })
        .collect();
    assert_eq!(waiting, [true, false]);
    assert_eq!(
        harness.outputs, 0,
        "the already displayed target is unchanged"
    );
    assert_eq!(harness.session.next_input_wake_ns(), None);
}

#[test]
fn a_new_size_retires_an_earlier_post_ack_render_observation() {
    for version in [69, 71, 72] {
        let mut harness = connected(version);
        let pane = harness.server.pane_id;
        harness.session.resize(pane, 4, 22);
        let (first_size, request) = take_one(&mut harness.session);
        harness.server.term.resize(support::size(4, 22));
        acknowledge_size(&mut harness.session, first_size, request);
        let (old_poll, pdu) = take_one(&mut harness.session);
        assert!(matches!(pdu, Pdu::GetPaneRenderStateV1(_)));
        let old = observation(&mut harness, std::iter::once(0..4).collect(), true);

        harness.session.resize(pane, 4, 26);
        let (latest_size, request) = take_one(&mut harness.session);
        deliver(&mut harness.session, 25, old_poll, &old);
        deliver(&mut harness.session, 25, 0, &old);
        harness.pump();
        assert_eq!(
            harness.outputs, 0,
            "v{version}: the newer size still owns presentation"
        );
        assert_eq!(harness.phone.get_size().cols, 30);

        acknowledge_size(&mut harness.session, latest_size, request);
        let (latest_poll, pdu) = take_one(&mut harness.session);
        assert!(matches!(pdu, Pdu::GetPaneRenderStateV1(_)));
        assert_ne!(old_poll, latest_poll);
        harness.server.term.resize(support::size(4, 26));
        harness
            .server
            .term
            .advance_bytes(b"\x1b[H\x1b[2KLATEST GRID");
        let latest = observation(&mut harness, std::iter::once(0..4).collect(), true);
        deliver(&mut harness.session, 25, latest_poll, &latest);
        harness.pump();
        assert!(harness.session.is_ready());
        assert_eq!(harness.phone.get_size().cols, 26);
        assert!(screen(&mut harness).contains("LATEST GRID"));
    }
}

#[test]
fn a_retry_or_new_visit_cannot_poll_a_size_effect_before_its_receipt() {
    let mut harness = connected(72);
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 4, 22);
    let (serial, request) = take_one(&mut harness.session);
    acknowledge_size(&mut harness.session, serial, request);
    let (poll, _) = take_one(&mut harness.session);
    deliver(
        &mut harness.session,
        0,
        poll,
        &codec::ErrorResponse::resource_busy(114),
    );
    let due = harness.session.next_input_wake_ns().unwrap();

    harness.session.resize(pane, 4, 26);
    let (latest_size, request) = take_one(&mut harness.session);
    assert!(harness.session.advance_time(due));
    assert!(
        harness.session.take_outgoing().is_empty(),
        "a retry waits for the size receipt"
    );
    harness.session.hide(pane);
    harness.session.show(pane);
    assert!(
        harness.session.take_outgoing().is_empty(),
        "a new visit also waits"
    );
    harness.pump();
    assert_eq!(harness.outputs, 0);

    acknowledge_size(&mut harness.session, latest_size, request);
    let (poll, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::GetPaneRenderStateV1(_)));
    harness.server.term.resize(support::size(4, 26));
    let latest = observation(&mut harness, std::iter::once(0..4).collect(), true);
    deliver(&mut harness.session, 25, poll, &latest);
    harness.pump();
    assert_eq!(harness.phone.get_size().cols, 26);
    assert!(screen(&mut harness).contains("ORIGINAL"));
    assert_eq!(harness.session.next_input_wake_ns(), None);
}

#[test]
fn a_queued_restore_releases_the_view_with_its_own_fresh_observation() {
    let mut harness = connected(72);
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 4, 22);
    let (resize, request) = take_one(&mut harness.session);
    harness.session.restore_size(pane);
    assert!(harness.session.take_outgoing().is_empty());
    harness.server.term.resize(support::size(4, 22));
    acknowledge_size(&mut harness.session, resize, request);
    let (restore, request) = take_one(&mut harness.session);
    let Pdu::ResizePaneConditionalV1(ref request_body) = request else {
        panic!("queued restore")
    };
    assert_eq!(request_body.size, support::size(4, 30));

    let prior_reads = harness
        .server
        .requests
        .iter()
        .filter(|name| **name == "GetPaneRenderStateV1")
        .count();
    harness.server.term.resize(support::size(4, 30));
    acknowledge_size(&mut harness.session, restore, request);
    harness.pump();
    assert!(harness.session.is_ready());
    assert!(!harness.session.restoring());
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|name| **name == "GetPaneRenderStateV1")
            .count(),
        prior_reads + 1
    );
    assert!(harness.events.iter().any(|event| matches!(event,
        frankenterm_mobile_mux::Event::PhoneSizeRestored { pane_id } if *pane_id == pane)));
    harness.output(b"\x1b[H\x1b[2KRESTORED VIEW");
    assert!(
        screen(&mut harness).contains("RESTORED VIEW"),
        "the restore cannot leave presentation gated"
    );
}

#[test]
fn hidden_visit_and_overtaken_correlated_reply_cannot_rewind_the_view() {
    let mut harness = connected(71);
    let (read, stale) = pending_row(&mut harness);
    let pane = harness.server.pane_id;
    harness.session.hide(pane);
    harness.session.show(pane);
    let (poll, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::GetPaneRenderStateV1(_)));
    let older = observation(&mut harness, vec![], true);
    harness.server.term.advance_bytes(b"\x1b[H\x1b[2KNEW VISIT");
    let newer = observation(&mut harness, std::iter::once(0..1).collect(), true);
    deliver(&mut harness.session, 25, 0, &newer);
    deliver(&mut harness.session, 25, poll, &older);
    deliver(&mut harness.session, 103, read, &stale);
    harness.pump();
    let text = screen(&mut harness);
    assert!(
        text.contains("NEW VISIT") && !text.contains("OLDER REPLY"),
        "{text}"
    );
}

#[test]
fn a_previous_visits_poll_cannot_open_a_new_visits_display_gate() {
    let mut harness = connected(71);
    let pane = harness.server.pane_id;
    harness.session.hide(pane);
    harness.session.show(pane);
    let (old_poll, _) = take_one(&mut harness.session);
    let old = observation(&mut harness, vec![], true);
    harness.session.hide(pane);
    harness.session.show(pane);
    let (new_poll, _) = take_one(&mut harness.session);
    deliver(&mut harness.session, 25, old_poll, &old);
    harness.pump();
    assert_eq!(
        harness.outputs, 0,
        "only the new visit's correlated reply can open presentation"
    );
    harness
        .server
        .term
        .advance_bytes(b"\x1b[H\x1b[2KEXACT VISIT");
    let current = observation(&mut harness, std::iter::once(0..1).collect(), true);
    deliver(&mut harness.session, 25, new_poll, &current);
    harness.pump();
    assert!(screen(&mut harness).contains("EXACT VISIT"));
}

#[test]
fn malformed_or_downgraded_fenced_replies_cannot_publish_rows() {
    for fault in 0..7 {
        let mut harness = connected(71);
        let before = screen(&mut harness);
        let (serial, mut reply) = pending_row(&mut harness);
        match fault {
            0 => reply.pane_id += 1,
            1 => reply.layout.seqno += 1,
            2 => reply.layout.dimensions.cols += 1,
            _ => {}
        }
        let mut body = if fault == 5 {
            varbincode::serialize(&codec::GetLinesResponse {
                pane_id: reply.pane_id,
                lines: reply.lines,
            })
            .unwrap()
        } else {
            varbincode::serialize(&reply).unwrap()
        };
        if fault == 3 {
            body.push(0);
        }
        if fault == 4 {
            body.pop();
        }
        if fault == 6 {
            body.clear();
        }
        let ident = match fault {
            5 => 23,
            6 => 10,
            _ => 103,
        };
        harness
            .session
            .receive(&server_frame(ident, serial, &body, false));
        harness.pump();
        assert!(!harness.session.is_ready(), "fault {fault}");
        assert_eq!(screen(&mut harness), before, "fault {fault}");
    }
}

#[test]
fn full_observation_decodes_sequence_after_image_references_not_from_their_prefix() {
    let mut harness = connected(71);
    let mut change = observation(&mut harness, vec![], true);
    change.seqno = 12345;
    let tail = varbincode::serialize(&(change.input_serial, change.seqno)).unwrap();
    let mut body = varbincode::serialize(&change).unwrap();
    let empty_images = body.len() - tail.len() - 1;
    assert_eq!(body[empty_images], 0, "empty image vector tag");
    body.truncate(empty_images);
    body.extend(
        varbincode::serialize(&vec![codec::SerializedImageCell {
            line_idx: 0,
            cell_idx: 0,
            top_left: termwiz::image::TextureCoordinate::new_f32(0.0, 0.0),
            bottom_right: termwiz::image::TextureCoordinate::new_f32(1.0, 1.0),
            data_hash: [0x7b; 32],
            z_index: -2,
            padding_left: 1,
            padding_top: 2,
            padding_right: 3,
            padding_bottom: 4,
            image_id: Some(9),
            placement_id: Some(10),
        }])
        .unwrap(),
    );
    body.extend(tail);
    let frame = server_frame(25, 0, &body, false);
    let decoded =
        Pdu::decode_for_dialect(frame.as_slice(), MuxWireDialect::current(71).unwrap()).unwrap();
    let MuxWireDecodedPayload::Pdu(Pdu::GetPaneRenderChangesResponse(reference)) =
        decoded.into_parts().1
    else {
        panic!("reference codec must accept this observation")
    };
    assert_eq!(reference.bonus_lines.extract_data().1.len(), 1);
    let observed: wire::RenderObservation = wire::decode_observation(&body).unwrap();
    assert_eq!(observed.seqno, reference.seqno);
    assert_eq!(observed.seqno, 12345);
    body.push(0);
    assert!(wire::decode_observation::<wire::RenderObservation>(&body).is_err());
}
