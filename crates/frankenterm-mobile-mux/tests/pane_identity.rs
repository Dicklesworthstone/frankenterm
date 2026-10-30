//! Reconnect witnesses cross the real desktop codec, but never carry input
//! effects or permission to reuse a replaced live registration.

mod support;

use codec::{MuxWireDecodedPayload, MuxWireDialect, Pdu};
use frankenterm_mobile_mux::session::{IDENTITY_QUERY_DEADLINE_NS, MAX_IDENTITY_QUERIES};
use frankenterm_mobile_mux::{Event, wire};
use support::{Harness, V46PaneEntry, V46PaneNode, server_frame, v46_pane_list};

fn connected(version: usize) -> Harness {
    let mut harness = Harness::new(4, 30);
    harness.server.codec_vers = version;
    harness.server.min_supported = if version >= 61 { 61 } else { 46 };
    harness.pump();
    assert!(harness.session.is_ready());
    harness.events.clear();
    harness
}

fn requests(bytes: &[u8]) -> Vec<(u64, Pdu)> {
    let mut cursor = std::io::Cursor::new(bytes);
    let mut result = Vec::new();
    while (cursor.position() as usize) < bytes.len() {
        let decoded =
            Pdu::decode_for_dialect(&mut cursor, MuxWireDialect::current(71).unwrap()).unwrap();
        let (serial, payload) = decoded.into_parts();
        let MuxWireDecodedPayload::Pdu(pdu) = payload else {
            panic!("not a current PDU")
        };
        result.push((serial, pdu));
    }
    result
}

fn inventory(harness: &mut Harness, panes: &[usize]) {
    harness.session.refresh_panes();
    let sent = requests(&harness.session.take_outgoing());
    assert_eq!(sent.len(), 1);
    assert!(matches!(sent[0].1, Pdu::ListPanes(_)));
    let tabs = panes
        .iter()
        .map(|pane| {
            V46PaneNode::Leaf(V46PaneEntry {
                window_id: 0,
                tab_id: *pane,
                pane_id: *pane,
                title: format!("pane {pane}"),
                size: harness.server.term.get_size(),
                working_dir: None,
                alt_screen_active: false,
                is_active_pane: false,
                is_zoomed_pane: false,
                workspace: "default".into(),
                cursor_pos: Default::default(),
                physical_top: 0,
                top_row: 0,
                left_col: 0,
                tty_name: None,
            })
        })
        .collect();
    harness.session.receive(&server_frame(
        4,
        sent[0].0,
        &v46_pane_list(tabs, vec![]),
        false,
    ));
    while let Some(event) = harness.session.next_event() {
        harness.events.push(event);
    }
}

fn identities(events: &[Event]) -> Vec<(usize, Option<wire::PaneIdentity>)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::PaneIdentity { pane_id, identity } => Some((*pane_id, *identity)),
            _ => None,
        })
        .collect()
}

fn metadata_reply(
    harness: &Harness,
    serial: u64,
    facts: codec::PaneRenderMetadataV1,
    registration: [u8; 16],
) -> Vec<u8> {
    let response = codec::GetPaneIdentityV1Response {
        pane_id: harness.server.pane_id,
        outcome: codec::PaneIdentityOutcomeV1::Available {
            identity: codec::PaneIdentityV1 {
                session_incarnation: mux::MuxSessionIncarnation::from_bytes(
                    harness.server.incarnation,
                ),
                durable_pane_id: [0xd1; 16],
                pane_registration: codec::ReliablePaneRegistrationIdentityV1::from_bytes(
                    registration,
                ),
            },
            render_facts: Some(Box::new(facts)),
        },
    };
    server_frame(
        118,
        serial,
        &varbincode::serialize(&response).unwrap(),
        false,
    )
}

/// Stop immediately before the second metadata query is answered. The first
/// uses the desktop codec and real terminal; no reset, palette or cells may
/// escape until the independent final publication agrees.
fn awaiting_coherent_confirmation() -> (Harness, u64) {
    awaiting_coherent_confirmation_with_mouse(false)
}

fn awaiting_coherent_confirmation_with_mouse(grabbed: bool) -> (Harness, u64) {
    let mut harness = connected(71);
    harness.server.coherent_render = true;
    harness.server.mouse_grabbed = grabbed;
    harness
        .server
        .term
        .advance_bytes(b"\x1b[31mremote screen\x1b[0m");
    assert!(harness.session.advance_time(0));
    harness.session.show(harness.server.pane_id);
    for expected in [
        "GetPaneIdentityV1",
        "GetPaneRenderStateV1",
        "GetPaneIdentityV1",
    ] {
        let bytes = harness.session.take_outgoing();
        assert_eq!(requests(&bytes).len(), 1, "expected {expected}");
        harness.server.receive(&bytes);
        assert_eq!(harness.server.requests.last(), Some(&expected));
        harness
            .session
            .receive(&std::mem::take(&mut harness.server.to_phone));
    }
    while let Some(event) = harness.session.next_event() {
        assert!(!matches!(
            event,
            Event::Output { .. }
                | Event::PreparingOutput { .. }
                | Event::Palette { .. }
                | Event::MouseGrab { .. }
        ));
        harness.events.push(event);
    }
    let pending = requests(&harness.session.take_outgoing());
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending[0].1, Pdu::GetPaneIdentityV1(_)));
    (harness, pending[0].0)
}

#[test]
fn coherent_publication_projects_palette_and_reverse_video_after_reset_before_readiness() {
    let mut harness = connected(71);
    harness.server.coherent_render = true;
    harness.server.mouse_grabbed = true;
    let color = frankenterm_term::color::SrgbaTuple(1.0, 0.0, 0.5, 0.25);
    harness.server.term.palette_mut().cursor_fg = color;
    harness.server.term.palette_mut().cursor_border =
        frankenterm_term::color::SrgbaTuple(0.0, 1.0, 0.0, 1.0);
    harness.server.term.palette_mut().selection_bg = color;
    harness
        .server
        .term
        .advance_bytes(b"\x1b[?5h\x1b[?2004h\x1b[?1004h\x1b[31mremote");
    harness.attach();
    assert!(harness.session.is_ready(), "{:?}", harness.events);
    support::assert_mirrored(&mut harness);
    let prepare = harness
        .events
        .iter()
        .position(|event| matches!(event, Event::PreparingOutput { .. }))
        .unwrap();
    let palette = harness
        .events
        .iter()
        .position(|event| matches!(event, Event::Palette { .. }))
        .unwrap();
    let complete = harness
        .events
        .iter()
        .position(|event| matches!(event, Event::Output { .. }))
        .unwrap();
    assert!(prepare < palette && palette < complete);
    let grabbed = harness
        .events
        .iter()
        .position(|event| matches!(event, Event::MouseGrab { grabbed: true, .. }))
        .unwrap();
    assert!(complete < grabbed, "pointer policy follows complete cells");
    let Event::Palette { rgba, .. } = &harness.events[palette] else {
        unreachable!()
    };
    assert_eq!(&rgba[258 * 4..259 * 4], &[255, 0, 128, 64]);
    assert_eq!(&rgba[260 * 4..261 * 4], &[0, 255, 0, 255]);
    assert_eq!(&rgba[262 * 4..263 * 4], &[255, 0, 128, 64]);
    assert!(mux::renderable::terminal_get_dimensions(&mut harness.phone).reverse_video);
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|request| **request == "GetPaneIdentityV1")
            .count(),
        3
    );
    assert!(harness.server.written.is_empty() && harness.server.keys.is_empty());
}

#[test]
fn unconfirmed_mouse_grab_never_changes_the_published_pointer_policy() {
    let (mut harness, serial) = awaiting_coherent_confirmation_with_mouse(true);
    // The input mode changed after the candidate's rows. Neither candidate
    // can be shown until the new state is independently read and confirmed.
    harness.server.mouse_grabbed = false;
    let facts = harness.server.render_metadata();
    let reply = metadata_reply(&harness, serial, facts, harness.server.registration);
    harness.session.receive(&reply);
    harness.pump();
    assert_eq!(harness.outputs, 0);
    assert!(
        !harness
            .events
            .iter()
            .any(|event| matches!(event, Event::MouseGrab { .. }))
    );
    assert!(
        harness
            .session
            .advance_time(frankenterm_mobile_mux::session::MIN_RENDER_RETRY_DELAY_NS)
    );
    harness.pump();
    support::assert_mirrored(&mut harness);
    let complete = harness
        .events
        .iter()
        .position(|event| matches!(event, Event::Output { .. }))
        .unwrap();
    let grabbed = harness
        .events
        .iter()
        .position(|event| matches!(event, Event::MouseGrab { grabbed: false, .. }))
        .unwrap();
    assert!(complete < grabbed);
    assert!(
        !harness
            .events
            .iter()
            .any(|event| matches!(event, Event::MouseGrab { grabbed: true, .. }))
    );
    assert!(harness.server.written.is_empty() && harness.server.keys.is_empty());
}

#[test]
fn palette_generation_change_at_the_same_sequence_refuses_mixed_output_then_recovers() {
    let (mut harness, serial) = awaiting_coherent_confirmation();
    harness.server.palette_generation += 1;
    let facts = harness.server.render_metadata();
    let reply = metadata_reply(&harness, serial, facts, harness.server.registration);
    harness.session.receive(&reply);
    harness.pump();
    assert_eq!(harness.outputs, 0);
    assert!(harness.session.is_ready());
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(frankenterm_mobile_mux::session::MIN_RENDER_RETRY_DELAY_NS)
    );
    assert!(
        harness
            .session
            .advance_time(frankenterm_mobile_mux::session::MIN_RENDER_RETRY_DELAY_NS)
    );
    harness.pump();
    support::assert_mirrored(&mut harness);
    assert!(
        harness
            .events
            .iter()
            .any(|event| matches!(event, Event::Output { .. }))
    );
    assert!(harness.server.written.is_empty() && harness.server.keys.is_empty());
}

#[test]
fn all_palette_values_participate_even_when_the_peer_reuses_its_generation() {
    let (mut harness, serial) = awaiting_coherent_confirmation();
    let mut facts = harness.server.render_metadata();
    facts.palette.split.0 = if facts.palette.split.0 == 0.0 {
        1.0
    } else {
        0.0
    };
    let reply = metadata_reply(&harness, serial, facts, harness.server.registration);
    harness.session.receive(&reply);
    harness.pump();
    assert_eq!(
        harness.outputs, 0,
        "even a non-rendered source palette slot fences the publication"
    );
    assert!(harness.session.is_ready());
}

#[test]
fn reused_numeric_pane_cannot_complete_an_old_coherent_candidate() {
    let (mut harness, serial) = awaiting_coherent_confirmation();
    let facts = harness.server.render_metadata();
    let reply = metadata_reply(&harness, serial, facts, [0x88; 16]);
    harness.session.receive(&reply);
    harness.pump();
    assert!(!harness.session.is_ready());
    assert_eq!(harness.outputs, 0);
    assert!(harness.server.written.is_empty() && harness.server.keys.is_empty());
}

#[test]
fn later_output_invalidates_the_in_flight_publication_without_replaying_input() {
    let (mut harness, serial) = awaiting_coherent_confirmation();
    let facts = harness.server.render_metadata();
    let old = metadata_reply(&harness, serial, facts, harness.server.registration);
    harness.server.term.advance_bytes(b"\r\nnew generation");
    harness.server.push();
    harness
        .session
        .receive(&std::mem::take(&mut harness.server.to_phone));
    harness.session.receive(&old);
    harness.pump();
    assert_eq!(harness.outputs, 0);
    assert!(
        harness
            .session
            .advance_time(frankenterm_mobile_mux::session::MIN_RENDER_RETRY_DELAY_NS)
    );
    harness.pump();
    support::assert_mirrored(&mut harness);
    assert!(harness.server.written.is_empty() && harness.server.keys.is_empty());
}

#[test]
fn palette_notice_retires_old_confirmation_without_resetting_a_newer_read() {
    let (mut harness, serial) = awaiting_coherent_confirmation();
    let facts = harness.server.render_metadata();
    let old = metadata_reply(&harness, serial, facts, harness.server.registration);
    harness.server.palette_changed();
    harness
        .session
        .receive(&std::mem::take(&mut harness.server.to_phone));
    assert!(
        harness
            .session
            .advance_time(frankenterm_mobile_mux::session::MIN_RENDER_RETRY_DELAY_NS)
    );
    let poll = harness.session.take_outgoing();
    harness.server.receive(&poll);
    harness
        .session
        .receive(&std::mem::take(&mut harness.server.to_phone));
    let next = harness.session.take_outgoing();
    assert_eq!(requests(&next).len(), 1);
    harness.session.receive(&old);
    assert!(
        harness.session.take_outgoing().is_empty(),
        "retired reply cannot restart the successor"
    );
    harness.server.receive(&next);
    harness.pump();
    support::assert_mirrored(&mut harness);
    assert!(
        harness
            .events
            .iter()
            .any(|event| matches!(event, Event::Output { .. }))
    );
}

#[test]
fn unanswered_coherent_read_expires_without_publishing_a_partial_frame() {
    let (mut harness, _) = awaiting_coherent_confirmation();
    assert!(
        !harness
            .session
            .advance_time(frankenterm_mobile_mux::session::RENDER_METADATA_DEADLINE_NS)
    );
    harness.pump();
    assert_eq!(harness.outputs, 0);
    assert!(harness.server.written.is_empty() && harness.server.keys.is_empty());
}

#[test]
fn hidden_visit_retires_its_confirmation_and_cannot_complete_a_new_visit() {
    let (mut harness, serial) = awaiting_coherent_confirmation();
    let facts = harness.server.render_metadata();
    let old = metadata_reply(&harness, serial, facts, harness.server.registration);
    let pane = harness.server.pane_id;
    harness.session.hide(pane);
    assert_eq!(harness.session.next_input_wake_ns(), None);
    harness.session.show(pane);
    harness.session.receive(&old);
    while let Some(event) = harness.session.next_event() {
        assert!(!matches!(
            event,
            Event::Output { .. } | Event::PreparingOutput { .. } | Event::Palette { .. }
        ));
    }
    harness.pump();
    support::assert_mirrored(&mut harness);
    assert!(harness.session.is_ready());
}

#[test]
fn coherent_metadata_decoder_rejects_trailing_truncated_nonfinite_and_oversized_bodies() {
    let mut harness = connected(71);
    let mut facts = harness.server.render_metadata();
    let make = |facts| codec::GetPaneIdentityV1Response {
        pane_id: 7,
        outcome: codec::PaneIdentityOutcomeV1::Available {
            identity: codec::PaneIdentityV1 {
                session_incarnation: mux::MuxSessionIncarnation::from_bytes([1; 16]),
                durable_pane_id: [2; 16],
                pane_registration: codec::ReliablePaneRegistrationIdentityV1::from_bytes([3; 16]),
            },
            render_facts: Some(Box::new(facts)),
        },
    };
    let valid = varbincode::serialize(&make(facts.clone())).unwrap();
    assert!(wire::decode_pane_identity(&valid).is_ok());
    assert!(wire::decode_pane_identity(&valid[..valid.len() - 1]).is_err());
    let mut trailing = valid;
    trailing.push(0);
    assert!(wire::decode_pane_identity(&trailing).is_err());
    facts.palette.cursor_fg.0 = f32::NAN;
    assert!(wire::decode_pane_identity(&varbincode::serialize(&make(facts)).unwrap()).is_err());
    assert!(
        wire::decode_pane_identity(&vec![0; wire::MAX_PANE_IDENTITY_RESPONSE_BYTES + 1]).is_err()
    );
}

#[test]
fn passive_preflight_retains_the_complete_witness_without_output_or_effects() {
    let mut harness = connected(71);
    let pane = harness.server.pane_id;
    assert!(
        !harness.session.inspect_pane_identity(pane),
        "an unsampled clock cannot admit unbounded work"
    );
    assert!(harness.session.take_outgoing().is_empty());
    assert!(harness.session.advance_time(100));
    assert!(harness.session.inspect_pane_identity(pane));
    assert!(
        harness.session.inspect_pane_identity(pane),
        "duplicate preflight coalesces"
    );
    harness.pump();
    assert_eq!(
        identities(&harness.events),
        [(
            pane,
            Some(wire::PaneIdentity {
                session_incarnation: harness.server.incarnation,
                durable_pane_id: [0xd1; 16],
                pane_registration: harness.server.registration,
            })
        )]
    );
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|name| **name == "GetPaneIdentityV1")
            .count(),
        1
    );
    assert_eq!(harness.outputs, 0);
    assert!(
        harness.server.written.is_empty()
            && harness.server.keys.is_empty()
            && harness.server.pastes.is_empty()
            && harness.server.mouse.is_empty()
            && harness.server.resizes.is_empty()
    );
    assert_eq!(harness.session.next_input_wake_ns(), None);
}

#[test]
fn older_peers_answer_without_an_unsupported_pdu_or_a_key_probe() {
    for version in [46, 64, 70] {
        let mut harness = connected(version);
        let pane = harness.server.pane_id;
        let before = harness.server.requests.len();
        assert!(harness.session.inspect_pane_identity(pane));
        harness.pump();
        assert_eq!(identities(&harness.events), [(pane, None)]);
        assert_eq!(harness.server.requests.len(), before);
        assert_eq!(harness.outputs, 0);
    }
    let mut harness = connected(71);
    harness.server.durable_identity = false;
    assert!(harness.session.advance_time(0));
    assert!(
        harness
            .session
            .inspect_pane_identity(harness.server.pane_id)
    );
    harness.pump();
    assert_eq!(
        identities(&harness.events),
        [(harness.server.pane_id, None)]
    );
    assert!(!harness.server.requests.contains(&"ReliableKeyEventV1"));
    assert_eq!(harness.outputs, 0);
}

#[test]
fn lost_reply_expires_once_without_retry_and_late_identity_cannot_revive_it() {
    let mut harness = connected(71);
    let pane = harness.server.pane_id;
    assert!(harness.session.advance_time(100));
    assert!(harness.session.inspect_pane_identity(pane));
    harness.hold = true;
    harness.pump();
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(100 + IDENTITY_QUERY_DEADLINE_NS)
    );
    assert!(
        harness
            .session
            .advance_time(99 + IDENTITY_QUERY_DEADLINE_NS)
    );
    harness.pump();
    assert!(identities(&harness.events).is_empty());
    assert!(
        harness
            .session
            .advance_time(100 + IDENTITY_QUERY_DEADLINE_NS)
    );
    harness.pump();
    assert_eq!(identities(&harness.events), [(pane, None)]);
    assert_eq!(harness.session.next_input_wake_ns(), None);
    assert!(harness.session.take_outgoing().is_empty());
    harness.hold = false;
    harness.pump();
    assert_eq!(identities(&harness.events), [(pane, None)]);
    assert!(harness.session.is_ready());
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|name| **name == "GetPaneIdentityV1")
            .count(),
        1
    );
}

#[test]
fn the_query_lane_is_bounded_and_retired_requests_release_capacity() {
    let mut harness = connected(71);
    let panes: Vec<_> = (0..=MAX_IDENTITY_QUERIES).collect();
    inventory(&mut harness, &panes);
    assert!(harness.session.advance_time(0));
    for pane in &panes[..MAX_IDENTITY_QUERIES] {
        assert!(harness.session.inspect_pane_identity(*pane));
    }
    assert!(
        !harness
            .session
            .inspect_pane_identity(panes[MAX_IDENTITY_QUERIES])
    );
    assert!(harness.session.inspect_pane_identity(panes[0]));
    let sent = requests(&harness.session.take_outgoing());
    assert_eq!(sent.len(), MAX_IDENTITY_QUERIES);
    assert!(
        sent.iter()
            .all(|(_, pdu)| matches!(pdu, Pdu::GetPaneIdentityV1(_)))
    );
    assert!(harness.session.advance_time(IDENTITY_QUERY_DEADLINE_NS));
    assert!(
        harness
            .session
            .inspect_pane_identity(panes[MAX_IDENTITY_QUERIES])
    );
    assert_eq!(requests(&harness.session.take_outgoing()).len(), 1);
}

#[test]
fn a_show_rechecks_preflight_identity_instead_of_inheriting_a_replaced_registration() {
    let mut harness = connected(71);
    let pane = harness.server.pane_id;
    assert!(harness.session.advance_time(0));
    assert!(harness.session.inspect_pane_identity(pane));
    harness.pump();
    harness.server.registration = [0x99; 16];
    harness.session.show(pane);
    harness.pump();
    assert!(!harness.session.is_ready());
    assert_eq!(harness.outputs, 0);
    assert!(!harness.server.requests.contains(&"GetPaneRenderChanges"));
    assert!(!harness.server.requests.contains(&"GetPaneRenderStateV1"));
    assert!(
        harness
            .events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. }))
    );
    assert!(harness.server.written.is_empty() && harness.server.keys.is_empty());
}

#[test]
fn removed_inventory_retires_preflight_even_if_its_number_returns_before_the_reply() {
    let mut harness = connected(71);
    let pane = harness.server.pane_id;
    assert!(harness.session.advance_time(0));
    assert!(harness.session.inspect_pane_identity(pane));
    harness.hold = true;
    harness.pump();
    let old_reply = std::mem::take(&mut harness.server.to_phone);
    assert!(!old_reply.is_empty());
    inventory(&mut harness, &[]);
    inventory(&mut harness, &[pane]);
    assert!(harness.session.inspect_pane_identity(pane));
    let new_bytes = harness.session.take_outgoing();
    let new_query = requests(&new_bytes);
    assert_eq!(new_query.len(), 1);
    harness.session.receive(&old_reply);
    while let Some(event) = harness.session.next_event() {
        harness.events.push(event);
    }
    assert!(
        identities(&harness.events).is_empty(),
        "a late old registration is not a new query's answer"
    );
    assert!(harness.session.is_ready());
    harness.server.registration = [0x99; 16];
    harness.server.receive(&new_bytes);
    harness.hold = false;
    harness.pump();
    assert_eq!(identities(&harness.events).len(), 1);
    assert_eq!(
        identities(&harness.events)[0].1.unwrap().pane_registration,
        [0x99; 16]
    );
}
