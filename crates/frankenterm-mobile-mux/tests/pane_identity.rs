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
