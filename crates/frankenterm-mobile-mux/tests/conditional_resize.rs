//! Geometry targets cross the actual codec with exact pane authority. An
//! enqueue receipt, a live replacement and a late reply are distinct facts.

mod support;

use codec::{MuxWireDecodedPayload, MuxWireDialect, Pdu};
use frankenterm_mobile_mux::session::{
    CONDITIONAL_RESIZE_DEADLINE_NS, MAX_CONDITIONAL_RESIZE_PANES,
};
use frankenterm_mobile_mux::{Event, wire};
use support::{Harness, V46PaneEntry, V46PaneNode, server_frame, size, v46_pane_list};

fn connected(version: usize, identity: bool) -> Harness {
    let mut harness = Harness::new(6, 20);
    harness.server.codec_vers = version;
    harness.server.min_supported = if version >= 61 { 61 } else { 46 };
    harness.server.durable_identity = identity;
    assert!(harness.session.advance_time(100));
    harness.pump();
    assert!(harness.session.is_ready());
    if version >= 71 {
        assert!(
            harness
                .session
                .inspect_pane_identity(harness.server.pane_id)
        );
        harness.pump();
    }
    harness.events.clear();
    harness
}

fn restored(harness: &Harness) -> usize {
    harness
        .events
        .iter()
        .filter(|event| matches!(event, Event::PhoneSizeRestored { .. }))
        .count()
}

fn conditional_request(harness: &mut Harness) -> (u64, codec::ResizePaneConditionalV1) {
    let bytes = harness.session.take_outgoing();
    let mut cursor = std::io::Cursor::new(bytes.as_slice());
    let decoded =
        Pdu::decode_for_dialect(&mut cursor, MuxWireDialect::current(72).unwrap()).unwrap();
    assert_eq!(
        cursor.position() as usize,
        bytes.len(),
        "one bounded geometry request"
    );
    let (serial, payload) = decoded.into_parts();
    let MuxWireDecodedPayload::Pdu(Pdu::ResizePaneConditionalV1(request)) = payload else {
        panic!("expected conditional resize, never numeric fallback");
    };
    (serial, request)
}

#[test]
fn current_resize_and_restore_carry_the_preflight_identity_while_old_peers_keep_their_dialect() {
    for version in [46, 71, 72] {
        let mut harness = connected(version, true);
        let pane = harness.server.pane_id;
        harness.session.resize(pane, 9, 37);
        harness.pump();
        harness.session.restore_size(pane);
        harness.pump();
        assert!(
            harness.session.is_ready(),
            "v{version}: {:?}",
            harness.events
        );
        assert_eq!(harness.server.resizes, [size(9, 37), size(6, 20)]);
        assert_eq!(restored(&harness), 1);
        assert!(!harness.session.restoring());
        if version == 72 {
            assert_eq!(
                harness
                    .server
                    .requests
                    .iter()
                    .filter(|name| **name == "ResizePaneConditionalV1")
                    .count(),
                2
            );
            assert!(!harness.server.requests.contains(&"Resize"));
        } else {
            assert_eq!(
                harness
                    .server
                    .requests
                    .iter()
                    .filter(|name| **name == "Resize")
                    .count(),
                2
            );
            assert!(!harness.server.requests.contains(&"ResizePaneConditionalV1"));
        }
    }
}

#[test]
fn replacement_after_preflight_is_never_resized_or_retried_through_legacy_pdu() {
    for restoring in [false, true] {
        let mut harness = connected(72, true);
        let pane = harness.server.pane_id;
        if restoring {
            harness.server.term.resize(size(9, 37));
            harness.session.refresh_panes();
            harness.pump();
            assert!(harness.session.adopt_phone_size(
                pane,
                wire::TerminalSize {
                    rows: 6,
                    cols: 20,
                    pixel_width: 160,
                    pixel_height: 96,
                    dpi: 96,
                },
                9,
                37
            ));
        }
        harness.server.registration = [0x99; 16];
        if restoring {
            harness.session.restore_size(pane);
        } else {
            harness.session.resize(pane, 9, 37);
        }
        harness.pump();
        assert!(!harness.session.is_ready());
        assert!(
            harness.server.resizes.is_empty(),
            "the server must reject the old registration before resize"
        );
        assert_eq!(restored(&harness), 0);
        assert!(!harness.server.requests.contains(&"Resize"));
        assert!(harness.session.take_outgoing().is_empty());
    }
}

#[test]
fn a_v72_backend_without_exact_identity_cannot_silently_take_geometry() {
    let mut harness = connected(72, false);
    let pane = harness.server.pane_id;
    assert!(!harness.session.adopt_phone_size(
        pane,
        wire::TerminalSize {
            rows: 12,
            cols: 40,
            pixel_width: 320,
            pixel_height: 192,
            dpi: 96,
        },
        6,
        20
    ));
    harness.session.resize(pane, 9, 37);
    harness.pump();
    assert!(!harness.session.is_ready());
    assert!(harness.server.resizes.is_empty());
    assert!(!harness.server.requests.contains(&"Resize"));
    assert!(!harness.server.requests.contains(&"ResizePaneConditionalV1"));
    assert!(
        !harness
            .events
            .iter()
            .any(|event| matches!(event, Event::PhoneSize { .. }))
    );
}

#[test]
fn geometry_coalesces_only_the_unsent_successor_and_restore_can_follow_an_unanswered_resize() {
    let mut harness = connected(72, true);
    let pane = harness.server.pane_id;
    harness.hold = true;
    harness.session.resize(pane, 8, 30);
    harness.pump();
    assert_eq!(harness.server.resizes, [size(8, 30)]);
    assert!(harness.session.advance_time(200));
    harness.session.resize(pane, 9, 31);
    harness.session.resize(pane, 10, 32);
    assert!(harness.session.take_outgoing().is_empty());
    harness.hold = false;
    harness.pump();
    assert_eq!(harness.server.resizes, [size(8, 30), size(10, 32)]);
    assert_eq!(harness.session.next_input_wake_ns(), None);

    harness.hold = true;
    harness.session.resize(pane, 11, 33);
    harness.pump();
    harness.session.restore_size(pane);
    assert!(
        harness.session.restoring(),
        "the unsent restore is owned while its predecessor settles"
    );
    assert!(harness.session.take_outgoing().is_empty());
    harness.hold = false;
    harness.pump();
    assert_eq!(
        harness.server.resizes,
        [size(8, 30), size(10, 32), size(11, 33), size(6, 20)]
    );
    assert_eq!(restored(&harness), 1);
    assert!(!harness.session.restoring());
}

#[test]
fn enqueue_ack_does_not_clear_restoration_until_fresh_geometry_arrives() {
    let mut harness = connected(72, true);
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 9, 37);
    harness.pump();
    harness.server.defer_conditional_resize = true;
    harness.session.restore_size(pane);
    harness.pump();
    assert_eq!(harness.server.resizes, [size(9, 37), size(6, 20)]);
    assert_eq!(
        harness.server.term.get_size(),
        size(9, 37),
        "enqueue did not apply the worker's reflow"
    );
    assert_eq!(restored(&harness), 0);
    assert!(harness.session.restoring());

    // Simulate the separately scheduled worker applying its accepted target.
    // The timer requests only observation, never repeats the geometry effect.
    harness.server.term.resize(size(6, 20));
    assert!(harness.session.advance_time(100_000_100));
    harness.pump();
    assert_eq!(restored(&harness), 1);
    assert!(!harness.session.restoring());
    assert_eq!(harness.server.resizes.len(), 2);
    assert_eq!(harness.session.next_input_wake_ns(), None);
}

#[test]
fn an_unobserved_restore_keeps_the_original_desktop_target_when_its_read_deadline_ends() {
    let mut harness = connected(72, true);
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 9, 37);
    harness.pump();
    harness.server.defer_conditional_resize = true;
    harness.session.restore_size(pane);
    harness.pump();
    assert!(
        harness
            .session
            .advance_time(100 + CONDITIONAL_RESIZE_DEADLINE_NS)
    );
    assert!(!harness.session.restoring());
    assert_eq!(restored(&harness), 0);
    assert!(harness.session.is_ready());
    harness.session.refresh_panes();
    harness.pump();
    harness.events.clear();
    harness.session.resize(pane, 10, 38);
    harness.pump();
    let desktop = harness
        .events
        .iter()
        .find_map(|event| match event {
            Event::PhoneSize {
                desktop: Some(desktop),
                ..
            } => Some(*desktop),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (desktop.rows, desktop.cols),
        (6, 20),
        "an unconfirmed phone-sized list must never redefine the desktop target"
    );
}

#[test]
fn lost_resize_reply_keeps_its_original_deadline_and_never_replays_an_effect_or_successor() {
    let mut harness = connected(72, true);
    let pane = harness.server.pane_id;
    harness.hold = true;
    harness.session.resize(pane, 8, 30);
    harness.pump();
    assert_eq!(harness.server.resizes, [size(8, 30)]);
    let expiry = 100 + CONDITIONAL_RESIZE_DEADLINE_NS;
    assert!(harness.session.advance_time(expiry - 1));
    harness.session.resize(pane, 9, 31);
    assert_eq!(harness.session.next_input_wake_ns(), Some(expiry));
    assert!(!harness.session.advance_time(expiry));
    assert!(harness.session.take_outgoing().is_empty());
    harness.hold = false;
    harness.pump();
    assert!(!harness.session.is_ready());
    assert_eq!(harness.server.resizes, [size(8, 30)]);
    assert_eq!(restored(&harness), 0);
    assert_eq!(harness.session.next_input_wake_ns(), None);
    assert!(!harness.server.requests.contains(&"Resize"));
}

#[test]
fn malformed_or_mismatched_resize_receipts_cannot_settle_another_pane_or_release_a_successor() {
    for case in 0..6 {
        let mut harness = connected(72, true);
        let pane = harness.server.pane_id;
        harness.session.resize(pane, 8, 30);
        let (serial, request) = conditional_request(&mut harness);
        harness.session.restore_size(pane);
        let mut reply = codec::ResizePaneConditionalV1Response {
            pane_id: request.pane_id,
            identity: request.identity,
            outcome: codec::ResizePaneConditionalOutcomeV1::Enqueued,
        };
        match case {
            0 => reply.pane_id += 1,
            1 => {
                reply.identity.pane_registration =
                    codec::ReliablePaneRegistrationIdentityV1::from_bytes([0xee; 16])
            }
            2 => reply.outcome = codec::ResizePaneConditionalOutcomeV1::Indeterminate,
            _ => {}
        }
        let mut body = varbincode::serialize(&reply).unwrap();
        if case == 3 {
            body.push(0);
        }
        if case == 4 {
            body.pop();
        }
        let ident = if case == 5 {
            body.clear();
            10
        } else {
            120
        };
        harness
            .session
            .receive(&server_frame(ident, serial, &body, true));
        harness.pump();
        assert!(!harness.session.is_ready(), "case {case}");
        assert!(harness.server.resizes.is_empty());
        assert_eq!(restored(&harness), 0);
        assert!(harness.session.take_outgoing().is_empty());
    }
}

#[test]
fn the_wire_request_includes_every_identity_component_and_the_exact_desktop_restore() {
    let mut harness = connected(72, true);
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 9, 37);
    let (serial, request) = conditional_request(&mut harness);
    assert_eq!(
        request.identity.session_incarnation.as_bytes(),
        harness.server.incarnation
    );
    assert_eq!(request.identity.durable_pane_id, [0xd1; 16]);
    assert_eq!(
        request.identity.pane_registration.as_bytes(),
        harness.server.registration
    );
    assert_eq!(request.size, size(9, 37));
    assert_eq!(request.containing_tab_id, harness.server.tab_id);
    harness.session.receive(&server_frame(
        120,
        serial,
        &codec::ResizePaneConditionalV1Response {
            pane_id: pane,
            identity: request.identity,
            outcome: codec::ResizePaneConditionalOutcomeV1::Enqueued,
        },
        false,
    ));
    harness.session.restore_size(pane);
    let (_, restore) = conditional_request(&mut harness);
    assert_eq!(restore.identity, request.identity);
    assert_eq!(restore.size, size(6, 20));
}

#[test]
fn unanswered_geometry_cannot_grow_beyond_the_per_connection_pane_budget() {
    let mut harness = connected(72, true);
    harness.session.refresh_panes();
    let list = Pdu::decode(harness.session.take_outgoing().as_slice()).unwrap();
    assert!(matches!(list.pdu, Pdu::ListPanes(_)));
    let tabs = (0..=MAX_CONDITIONAL_RESIZE_PANES)
        .map(|index| {
            let pane = 1_000 + index;
            V46PaneNode::Leaf(V46PaneEntry {
                window_id: 0,
                tab_id: pane,
                pane_id: pane,
                title: String::new(),
                size: size(6, 20),
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
        list.serial,
        &v46_pane_list(tabs, vec![]),
        false,
    ));
    for index in 0..=MAX_CONDITIONAL_RESIZE_PANES {
        let pane = 1_000 + index;
        assert!(harness.session.inspect_pane_identity(pane));
        let query = Pdu::decode(harness.session.take_outgoing().as_slice()).unwrap();
        assert!(matches!(query.pdu, Pdu::GetPaneIdentityV1(_)));
        harness.session.receive(&server_frame(
            118,
            query.serial,
            &codec::GetPaneIdentityV1Response {
                pane_id: pane,
                outcome: codec::PaneIdentityOutcomeV1::Available {
                    identity: codec::PaneIdentityV1 {
                        session_incarnation: mux::MuxSessionIncarnation::from_bytes(
                            harness.server.incarnation,
                        ),
                        durable_pane_id: [u8::try_from(index + 1).unwrap(); 16],
                        pane_registration: codec::ReliablePaneRegistrationIdentityV1::from_bytes(
                            [u8::try_from(index + 65).unwrap(); 16],
                        ),
                    },
                    render_facts: None,
                },
            },
            false,
        ));
        harness.session.resize(pane, 8, 30);
        if index < MAX_CONDITIONAL_RESIZE_PANES {
            assert!(harness.session.is_ready());
            let (_, request) = conditional_request(&mut harness);
            assert_eq!(request.pane_id, pane);
        } else {
            assert!(
                !harness.session.is_ready(),
                "the next distinct pane cannot enlarge the pending lane"
            );
            assert!(harness.session.take_outgoing().is_empty());
            assert_eq!(harness.session.next_input_wake_ns(), None);
        }
    }
}
