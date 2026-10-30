//! Current-dialect input is verified through the desktop codec on both sides
//! of the phone's sans-I/O state machine. The fake writer controls effect
//! receipts; no assertion treats local transport acceptance as remote effect.

mod support;

use codec::{
    InputSerial, MuxWireDecodedPayload, MuxWireDialect, Pdu, ReliableKeyEventOutcomeV1,
    ReliableKeyEventV1Response, ReliablePaneWriteOutcomeV1, ReliablePaneWriteV1Response,
};
use frankenterm_mobile_mux::session::{INPUT_DEADLINE_NS, MAX_INPUT_RETRIES};
use frankenterm_mobile_mux::{Event, Session, wire};
use support::{Harness, server_frame};

fn current(version: usize) -> Harness {
    let mut harness = Harness::new(4, 30);
    harness.server.codec_vers = version;
    harness.server.min_supported = 61;
    harness.attach();
    assert!(harness.session.is_ready(), "{:?}", harness.events);
    harness
}

fn timed_current() -> Harness {
    let mut harness = current(71);
    assert!(harness.session.advance_time(0));
    harness
}

fn scheduler_pressure() -> codec::ReliableInputSchedulerPressureV1 {
    codec::ReliableInputSchedulerPressureV1 {
        queue_id: 1,
        scheduler_generation: 2,
        active_tasks: 3,
        task_capacity: 3,
        active_estimated_bytes: 10,
        estimated_byte_capacity: 20,
        requested_estimated_bytes: 7,
        retry_after_ns: 2_000_000,
    }
}

fn take_one(session: &mut Session) -> (u64, Pdu) {
    let bytes = session.take_outgoing();
    assert!(!bytes.is_empty(), "expected one outgoing request");
    let mut cursor = std::io::Cursor::new(bytes.as_slice());
    let decoded = Pdu::decode_for_dialect(
        &mut cursor,
        MuxWireDialect::current(codec::CODEC_VERSION).unwrap(),
    )
    .expect("phone request agrees with desktop codec");
    assert_eq!(
        cursor.position() as usize,
        bytes.len(),
        "only one effect may be in flight"
    );
    let (serial, payload) = decoded.into_parts();
    let MuxWireDecodedPayload::Pdu(pdu) = payload else {
        panic!("current input must not use a legacy frame")
    };
    (serial, pdu)
}

fn reply<T: serde::Serialize>(session: &mut Session, ident: u64, serial: u64, body: &T) {
    session.receive(&server_frame(
        ident,
        serial,
        &varbincode::serialize(body).unwrap(),
        false,
    ));
}

fn write_reply(
    session: &mut Session,
    rpc: u64,
    request: &codec::ReliablePaneWriteV1,
    outcome: ReliablePaneWriteOutcomeV1,
) {
    reply(
        session,
        101,
        rpc,
        &ReliablePaneWriteV1Response {
            pane_id: request.pane_id,
            input_serial: request.input_serial,
            outcome,
        },
    );
}

fn identity_reply(
    session: &mut Session,
    rpc: u64,
    pane: usize,
    incarnation: [u8; 16],
    registration: [u8; 16],
) {
    reply(
        session,
        118,
        rpc,
        &codec::GetPaneIdentityV1Response {
            pane_id: pane,
            outcome: codec::PaneIdentityOutcomeV1::Available {
                identity: codec::PaneIdentityV1 {
                    session_incarnation: mux::MuxSessionIncarnation::from_bytes(incarnation),
                    durable_pane_id: [0xd1; 16],
                    pane_registration: codec::ReliablePaneRegistrationIdentityV1::from_bytes(
                        registration,
                    ),
                },
                render_facts: None,
            },
        },
    );
}

fn assert_stopped(session: &mut Session, pane: usize) {
    assert!(!session.is_ready());
    assert!(
        session.unanswered_input(pane),
        "failure must retain unsettled input"
    );
    assert!(
        session.take_outgoing().is_empty(),
        "no suffix, refresh, downgrade or queued control may escape"
    );
    assert!(
        std::iter::from_fn(|| session.next_event())
            .any(|event| matches!(event, Event::Failed { .. }))
    );
}

#[test]
fn registration_is_acknowledged_before_ready_and_unique_per_connection() {
    let mut first = Session::new();
    let (rpc, pdu) = take_one(&mut first);
    assert!(matches!(pdu, Pdu::GetCodecVersion(_)));
    reply(
        &mut first,
        27,
        rpc,
        &codec::GetCodecVersionResponse {
            codec_vers: 71,
            version_string: "current".to_owned(),
            executable_path: "/mux".into(),
            config_file_path: None,
            min_supported: 61,
        },
    );
    assert!(!first.is_ready());
    assert!(first.next_event().is_none());
    let (register_rpc, pdu) = take_one(&mut first);
    let Pdu::SetClientId(registration) = pdu else {
        panic!("client registration must precede pane requests")
    };
    assert!(!registration.is_proxy);
    reply(&mut first, 10, register_rpc, &codec::UnitResponse {});
    assert!(first.is_ready());
    assert!(matches!(first.next_event(), Some(Event::Ready { .. })));
    assert!(matches!(take_one(&mut first).1, Pdu::ListPanesCoherent(_)));

    let second = current(71);
    assert_ne!(registration.client_id, second.server.client_ids[0]);
    assert_eq!(
        &second.server.requests[..5],
        [
            "GetCodecVersion",
            "SetClientId",
            "ListPanesCoherent",
            "GetPaneIdentityV1",
            "GetPaneRenderChanges",
        ]
    );
}

#[test]
fn an_unobserved_pane_cannot_lazily_capture_new_input_authority() {
    let mut harness = current(71);
    let pane = harness.server.pane_id + 1;
    harness.session.write(pane, b"never redirect this");
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn every_partial_suffix_precedes_later_keys_paste_and_mouse() {
    let mut harness = current(71);
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"abc\x1b[A");
    harness.session.paste(pane, "later paste");
    harness.session.click(pane, 2, 1);
    harness.session.wheel(pane, -1, 2, 1);
    let mut last_input_serial = 0;
    for expected in [b"abc".as_slice(), b"bc", b"c"] {
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliablePaneWriteV1(request) = pdu else {
            panic!("the original byte suffix must be first")
        };
        assert_eq!(request.data, expected);
        assert_eq!(request.pane_id, pane);
        assert_eq!(
            request.pane_registration.unwrap().as_bytes(),
            harness.server.registration
        );
        assert!(request.input_serial.get() > last_input_serial);
        last_input_serial = request.input_serial.get();
        write_reply(
            &mut harness.session,
            rpc,
            &request,
            ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes: 1 },
        );
        assert!(harness.session.unanswered_input(pane));
    }
    let (rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliableKeyEventV1(key) = pdu else {
        panic!("key must follow all byte suffixes")
    };
    assert_eq!(key.event.key, termwiz::input::KeyCode::UpArrow);
    assert_eq!(
        key.pane_registration.unwrap().as_bytes(),
        harness.server.registration
    );
    assert!(key.input_serial.get() > last_input_serial);
    reply(
        &mut harness.session,
        97,
        rpc,
        &ReliableKeyEventV1Response {
            pane_id: pane,
            input_serial: key.input_serial,
            outcome: ReliableKeyEventOutcomeV1::Applied,
        },
    );
    let (rpc, pdu) = take_one(&mut harness.session);
    let Pdu::SendPaste(paste) = pdu else {
        panic!("paste must follow reliable key acknowledgement")
    };
    assert_eq!(paste.data, "later paste");
    assert!(paste.input_serial > key.input_serial);
    reply(&mut harness.session, 10, rpc, &codec::UnitResponse {});
    for _ in 0..3 {
        let (rpc, pdu) = take_one(&mut harness.session);
        assert!(matches!(pdu, Pdu::SendMouseEvent(_)));
        reply(&mut harness.session, 10, rpc, &codec::UnitResponse {});
    }
    assert!(harness.session.take_outgoing().is_empty());
    assert!(!harness.session.unanswered_input(pane));
}

#[test]
fn full_partial_writer_integration_keeps_content_and_effect_order() {
    let mut harness = current(71);
    harness.server.write_prefix = Some(1);
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"abc\x1b[A");
    harness.session.paste(pane, "paste");
    harness.session.click(pane, 0, 0);
    harness.pump();
    assert_eq!(harness.server.written, b"abc");
    assert_eq!(harness.server.keys.len(), 1);
    assert_eq!(harness.server.pastes, ["paste"]);
    assert_eq!(harness.server.mouse.len(), 2);
    let effects: Vec<_> = harness
        .server
        .requests
        .iter()
        .copied()
        .filter(|name| {
            [
                "ReliablePaneWriteV1",
                "ReliableKeyEventV1",
                "SendPaste",
                "SendMouseEvent",
            ]
            .contains(name)
        })
        .collect();
    assert_eq!(
        effects,
        [
            "ReliablePaneWriteV1",
            "ReliablePaneWriteV1",
            "ReliablePaneWriteV1",
            "ReliableKeyEventV1",
            "SendPaste",
            "SendMouseEvent",
            "SendMouseEvent",
        ]
    );
    assert!(!harness.session.unanswered_input(pane));
}

#[test]
fn stale_registration_is_never_refreshed_or_downgraded() {
    let mut harness = current(71);
    let pane = harness.server.pane_id;
    harness.server.registration = [0x55; 16];
    harness.session.write(pane, b"old pane only");
    harness.session.paste(pane, "must stay queued");
    harness.pump();
    assert!(!harness.session.is_ready());
    assert!(harness.session.unanswered_input(pane));
    assert!(harness.server.written.is_empty());
    assert!(harness.server.pastes.is_empty());
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|name| **name == "GetPaneIdentityV1")
            .count(),
        1
    );
    assert!(!harness.server.requests.contains(&"WriteToPane"));
    assert!(!harness.server.requests.contains(&"SendKeyDown"));
}

#[test]
fn missing_or_ambiguous_receipts_cannot_replay_or_release_later_input() {
    use codec::{ReliableKeyEventRetryV1, ReliablePaneWriteRejectionV1, ReliablePaneWriteRetryV1};
    for outcome in [
        ReliablePaneWriteOutcomeV1::Indeterminate,
        ReliablePaneWriteOutcomeV1::Rejected(ReliablePaneWriteRejectionV1::DefinitelyNotApplied),
        ReliablePaneWriteOutcomeV1::Retry(ReliablePaneWriteRetryV1::DefinitelyNotApplied {
            retry_after_ns: 1_000_000,
        }),
        ReliablePaneWriteOutcomeV1::Retry(ReliablePaneWriteRetryV1::Input(
            ReliableKeyEventRetryV1::DuplicatePending {
                retry_after_ns: 1_000_000,
            },
        )),
    ] {
        let mut harness = current(71);
        let pane = harness.server.pane_id;
        harness.session.write(pane, b"abc");
        harness.session.paste(pane, "later");
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliablePaneWriteV1(request) = pdu else {
            panic!("reliable bytes")
        };
        write_reply(&mut harness.session, rpc, &request, outcome);
        assert_stopped(&mut harness.session, pane);
    }
    let mut harness = current(71);
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"possibly applied");
    harness.session.click(pane, 0, 0);
    take_one(&mut harness.session);
    harness.session.closed();
    assert_stopped(&mut harness.session, pane);
    harness.session.write(pane, b"cannot resume this session");
    assert!(harness.session.take_outgoing().is_empty());
}

#[test]
fn malformed_unrelated_or_invalid_prefix_receipts_leave_input_unsettled() {
    for fault in 0..7 {
        let mut harness = current(71);
        let pane = harness.server.pane_id;
        harness.session.write(pane, b"abc");
        harness.session.click(pane, 1, 1);
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliablePaneWriteV1(request) = pdu else {
            panic!("reliable bytes")
        };
        let response = ReliablePaneWriteV1Response {
            pane_id: if fault == 0 { pane + 1 } else { pane },
            input_serial: if fault == 1 {
                InputSerial::from_millis_since_epoch(request.input_serial.get() + 1)
            } else {
                request.input_serial
            },
            outcome: ReliablePaneWriteOutcomeV1::AppliedPrefix {
                bytes: match fault {
                    2 => 0,
                    3 => 4,
                    _ => 3,
                },
            },
        };
        let mut body = varbincode::serialize(&response).unwrap();
        if fault == 4 {
            body.push(0);
        }
        if fault == 5 {
            body.pop();
        }
        let ident = if fault == 6 { 10 } else { 101 };
        harness
            .session
            .receive(&server_frame(ident, rpc, &body, false));
        assert_stopped(&mut harness.session, pane);
    }
}

#[test]
fn older_current_peers_probe_without_effect_before_rendering() {
    let mut harness = current(64);
    assert_eq!(
        &harness.server.requests[..5],
        [
            "GetCodecVersion",
            "SetClientId",
            "ListPanesCoherent",
            "ReliableKeyEventV1",
            "GetPaneRenderChanges",
        ]
    );
    assert!(
        harness.server.keys.is_empty(),
        "authority probing must never type its sentinel"
    );
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"text\x1b[A");
    harness.pump();
    assert_eq!(harness.server.written, b"text");
    assert_eq!(harness.server.keys.len(), 1);
    assert!(!harness.server.requests.contains(&"WriteToPane"));

    let mut oldest = current(61);
    let pane = oldest.server.pane_id;
    oldest
        .session
        .write(pane, b"requires reliable write capability");
    assert_stopped(&mut oldest.session, pane);
}

#[test]
fn current_remote_panes_without_durable_identity_keep_live_input_authority() {
    let mut harness = Harness::new(4, 30);
    harness.server.codec_vers = 71;
    harness.server.min_supported = 61;
    harness.server.durable_identity = false;
    harness.attach();
    assert_eq!(
        &harness.server.requests[..6],
        [
            "GetCodecVersion",
            "SetClientId",
            "ListPanesCoherent",
            "GetPaneIdentityV1",
            "ReliableKeyEventV1",
            "GetPaneRenderChanges",
        ]
    );
    assert!(harness.server.keys.is_empty());
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"remote pane");
    harness.pump();
    assert!(harness.session.is_ready());
    assert_eq!(harness.server.written, b"remote pane");
    assert!(!harness.session.unanswered_input(pane));
}

#[test]
fn removing_a_pane_cancels_the_queue_without_sending_to_a_reused_number() {
    let mut harness = current(71);
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"pending");
    take_one(&mut harness.session);
    harness.session.paste(pane, "queued");
    reply(
        &mut harness.session,
        37,
        0,
        &codec::PaneRemoved { pane_id: pane },
    );
    assert_stopped(&mut harness.session, pane);
    harness.session.show(pane);
    assert!(harness.session.take_outgoing().is_empty());
}

#[test]
fn writes_are_bounded_and_chunked_before_the_wire() {
    let mut harness = current(71);
    let pane = harness.server.pane_id;
    let bytes = vec![b'x'; 3 * wire::MAX_RELIABLE_WRITE_BYTES + 1];
    harness.session.write(pane, &bytes);
    harness.pump();
    assert_eq!(harness.server.written, bytes);
    assert_eq!(
        harness
            .server
            .requests
            .iter()
            .filter(|name| **name == "ReliablePaneWriteV1")
            .count(),
        4
    );

    harness.session.write(pane, b"one outstanding request");
    take_one(&mut harness.session);
    for _ in 0..4 {
        harness.session.wheel(pane, 64, 0, 0);
    }
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn inventory_removal_retires_the_mirror_and_delayed_display_replies() {
    let mut harness = current(71);
    let pane = harness.server.pane_id;
    harness.session.resize(pane, 5, 30);
    let (resize_rpc, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::Resize(_)));
    reply(
        &mut harness.session,
        10,
        resize_rpc,
        &codec::UnitResponse {},
    );
    let (retired_poll, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::GetPaneRenderChanges(_)));

    harness.session.refresh_panes();
    let (list_rpc, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::ListPanes(_)));
    reply(
        &mut harness.session,
        4,
        list_rpc,
        &codec::ListPanesResponse {
            tabs: Vec::new(),
            tab_titles: Vec::new(),
            window_titles: Default::default(),
            floating_panes: Vec::new(),
        },
    );
    assert!(harness.session.panes().is_empty());

    harness.server.registration = [0x55; 16];
    harness.session.refresh_panes();
    harness.pump();
    harness.session.show(pane);
    let (identity_rpc, pdu) = take_one(&mut harness.session);
    assert!(
        matches!(pdu, Pdu::GetPaneIdentityV1(_)),
        "same-number reappearance must capture fresh authority"
    );
    identity_reply(
        &mut harness.session,
        identity_rpc,
        pane,
        harness.server.incarnation,
        harness.server.registration,
    );
    assert!(matches!(
        take_one(&mut harness.session).1,
        Pdu::GetPaneRenderChanges(_)
    ));

    // Neither the old request's refusal nor a late liveness answer can close
    // the newly observed pane or revoke its frozen registration.
    harness
        .session
        .receive(&server_frame(0, retired_poll, &[], false));
    reply(
        &mut harness.session,
        30,
        retired_poll,
        &codec::LivenessResponse {
            pane_id: pane,
            is_alive: false,
        },
    );
    assert!(harness.session.is_ready());
    harness.session.write(pane, b"new pane");
    let (_, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(write) = pdu else {
        panic!("new input must still have exact authority")
    };
    assert_eq!(write.pane_registration.unwrap().as_bytes(), [0x55; 16]);
}

#[test]
fn rendering_and_resize_polling_wait_for_the_observed_panes_authority() {
    use mux::renderable::{
        terminal_get_cursor_position, terminal_get_dimensions, terminal_get_lines,
    };
    let mut harness = Harness::new(4, 30);
    harness.server.codec_vers = 71;
    harness.server.min_supported = 61;
    harness.pump();
    let pane = harness.server.pane_id;
    harness.session.show(pane);
    let (identity_rpc, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::GetPaneIdentityV1(_)));
    harness.session.resize(pane, 5, 30);
    let (resize_rpc, _) = take_one(&mut harness.session);
    reply(
        &mut harness.session,
        10,
        resize_rpc,
        &codec::UnitResponse {},
    );
    harness.server.term.advance_bytes(b"before authority");
    let dims = terminal_get_dimensions(&mut harness.server.term);
    let (first, lines) = terminal_get_lines(
        &mut harness.server.term,
        dims.physical_top..dims.physical_top + dims.viewport_rows as isize,
    );
    let bonus: Vec<_> = lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| (first + index as isize, line))
        .collect();
    reply(
        &mut harness.session,
        25,
        0,
        &codec::GetPaneRenderChangesResponse {
            pane_id: pane,
            mouse_grabbed: false,
            alt_screen_active: false,
            cursor_position: terminal_get_cursor_position(&mut harness.server.term),
            dimensions: dims,
            tiered_scrollback_status: None,
            dirty_lines: Vec::new(),
            title: "pending".to_owned(),
            working_dir: None,
            bonus_lines: bonus.into(),
            input_serial: None,
            seqno: 1,
        },
    );
    assert!(
        harness.session.take_outgoing().is_empty(),
        "no render fetch or resize poll before authority"
    );
    assert!(
        !std::iter::from_fn(|| harness.session.next_event())
            .any(|event| matches!(event, Event::Output { .. }))
    );
    identity_reply(
        &mut harness.session,
        identity_rpc,
        pane,
        harness.server.incarnation,
        harness.server.registration,
    );
    harness.pump();
    assert!(harness.session.is_ready());
    assert!(
        harness.outputs > 0,
        "drawing resumes after authority settles"
    );
}

#[test]
fn an_authority_probe_cannot_claim_a_key_effect_or_reserved_registration() {
    for outcome in [
        ReliableKeyEventOutcomeV1::Applied,
        ReliableKeyEventOutcomeV1::DuplicateApplied,
        ReliableKeyEventOutcomeV1::Retry(codec::ReliableKeyEventRetryV1::PaneAuthorityRequired {
            pane_registration: codec::ReliablePaneRegistrationIdentityV1::from_bytes([0; 16]),
        }),
    ] {
        let mut harness = Harness::new(4, 30);
        harness.server.codec_vers = 64;
        harness.server.min_supported = 61;
        harness.pump();
        let pane = harness.server.pane_id;
        harness.session.show(pane);
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliableKeyEventV1(probe) = pdu else {
            panic!("no-effect probe")
        };
        assert!(probe.pane_registration.is_none());
        reply(
            &mut harness.session,
            97,
            rpc,
            &ReliableKeyEventV1Response {
                pane_id: pane,
                input_serial: probe.input_serial,
                outcome,
            },
        );
        assert!(!harness.session.is_ready());
        assert!(harness.session.take_outgoing().is_empty());
    }
}

#[test]
fn identity_replies_cannot_exceed_their_canonical_body_bound() {
    let mut harness = Harness::new(4, 30);
    harness.server.codec_vers = 71;
    harness.server.min_supported = 61;
    harness.pump();
    let pane = harness.server.pane_id;
    harness.session.show(pane);
    let (rpc, _) = take_one(&mut harness.session);
    let mut body = varbincode::serialize(&codec::GetPaneIdentityV1Response {
        pane_id: pane,
        outcome: codec::PaneIdentityOutcomeV1::Unavailable,
    })
    .unwrap();
    body.resize(wire::MAX_PANE_IDENTITY_RESPONSE_BYTES + 1, 0);
    harness
        .session
        .receive(&server_frame(118, rpc, &body, true));
    assert!(!harness.session.is_ready());
    assert!(
        harness.session.take_outgoing().is_empty(),
        "oversize identity cannot authorize a fallback probe"
    );
}

#[test]
fn a_no_effect_write_retries_on_time_with_identical_identity_and_fifo_order() {
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"abc");
    let (rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(original) = pdu else {
        panic!("reliable write")
    };
    write_reply(
        &mut harness.session,
        rpc,
        &original,
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied {
            retry_after_ns: 2_000_000,
        }),
    );
    harness.session.write(pane, b"\x1b[A");
    harness.session.paste(pane, "later");
    harness.session.click(pane, 0, 0);
    assert_eq!(harness.session.next_input_wake_ns(), Some(2_000_000));
    assert!(harness.session.advance_time(1_999_999));
    assert!(harness.session.take_outgoing().is_empty());
    assert!(harness.session.advance_time(2_000_000));
    let (retry_rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(retry) = pdu else {
        panic!("same write, before any later effect")
    };
    assert_ne!(rpc, retry_rpc);
    assert_eq!(retry, original, "only the transport RPC serial changes");
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(INPUT_DEADLINE_NS)
    );
    assert!(harness.session.advance_time(2_000_000));
    assert!(
        harness.session.take_outgoing().is_empty(),
        "an equal tick cannot duplicate the retry"
    );
    write_reply(
        &mut harness.session,
        retry_rpc,
        &retry,
        ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes: 3 },
    );
    let (key_rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliableKeyEventV1(key) = pdu else {
        panic!("later key")
    };
    assert!(key.input_serial > retry.input_serial);
    reply(
        &mut harness.session,
        97,
        key_rpc,
        &ReliableKeyEventV1Response {
            pane_id: pane,
            input_serial: key.input_serial,
            outcome: ReliableKeyEventOutcomeV1::Applied,
        },
    );
    let (paste_rpc, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::SendPaste(_)));
    reply(&mut harness.session, 10, paste_rpc, &codec::UnitResponse {});
    for _ in 0..2 {
        let (mouse_rpc, pdu) = take_one(&mut harness.session);
        assert!(matches!(pdu, Pdu::SendMouseEvent(_)));
        reply(&mut harness.session, 10, mouse_rpc, &codec::UnitResponse {});
    }
    assert!(!harness.session.unanswered_input(pane));
    assert_eq!(harness.session.next_input_wake_ns(), None);
}

#[test]
fn each_canonical_no_effect_key_retry_is_paced_and_keeps_its_registration() {
    use codec::ReliableKeyEventRetryV1 as Retry;
    for disposition in [
        Retry::SchedulerFull(scheduler_pressure()),
        Retry::SchedulerRetired(scheduler_pressure()),
        Retry::SchedulerUnavailable {
            retry_after_ns: 2_000_000,
        },
        Retry::ClientRegistrationTransition {
            retry_after_ns: 2_000_000,
        },
    ] {
        let mut harness = timed_current();
        let pane = harness.server.pane_id;
        harness.session.write(pane, b"\x1b[A");
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliableKeyEventV1(original) = pdu else {
            panic!("reliable key")
        };
        reply(
            &mut harness.session,
            97,
            rpc,
            &ReliableKeyEventV1Response {
                pane_id: pane,
                input_serial: original.input_serial,
                outcome: ReliableKeyEventOutcomeV1::Retry(disposition),
            },
        );
        assert_eq!(harness.session.next_input_wake_ns(), Some(2_000_000));
        assert!(harness.session.advance_time(1_999_999));
        assert!(harness.session.take_outgoing().is_empty());
        assert!(harness.session.advance_time(2_000_000));
        let (retry_rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliableKeyEventV1(retry) = pdu else {
            panic!("same key")
        };
        assert_eq!(retry, original);
        reply(
            &mut harness.session,
            97,
            retry_rpc,
            &ReliableKeyEventV1Response {
                pane_id: pane,
                input_serial: retry.input_serial,
                outcome: ReliableKeyEventOutcomeV1::Applied,
            },
        );
        assert!(harness.session.is_ready());
        assert!(!harness.session.unanswered_input(pane));
    }
}

#[test]
fn retry_attempt_budget_and_admission_deadline_survive_a_duplicate_applied_prefix() {
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"ab");
    let (mut rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(original) = pdu else {
        panic!("reliable write")
    };
    let mut last_wake = 0;
    for _ in 0..MAX_INPUT_RETRIES {
        write_reply(
            &mut harness.session,
            rpc,
            &original,
            ReliablePaneWriteOutcomeV1::Retry(
                codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied { retry_after_ns: 1 },
            ),
        );
        let wake = harness.session.next_input_wake_ns().unwrap();
        assert!(
            wake >= last_wake + 1_000_000,
            "tiny peer prompts must not busy-spin"
        );
        assert!(harness.session.advance_time(wake));
        let (next_rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliablePaneWriteV1(retry) = pdu else {
            panic!("same write")
        };
        assert_eq!(retry, original);
        assert_eq!(
            harness.session.next_input_wake_ns(),
            Some(INPUT_DEADLINE_NS)
        );
        rpc = next_rpc;
        last_wake = wake;
    }
    write_reply(
        &mut harness.session,
        rpc,
        &original,
        ReliablePaneWriteOutcomeV1::DuplicateAppliedPrefix { bytes: 1 },
    );
    let (suffix_rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(suffix) = pdu else {
        panic!("reserved suffix")
    };
    assert_eq!(suffix.data, b"b");
    assert!(suffix.input_serial > original.input_serial);
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(INPUT_DEADLINE_NS)
    );
    write_reply(
        &mut harness.session,
        suffix_rpc,
        &suffix,
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied {
            retry_after_ns: 1,
        }),
    );
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn peer_delay_is_not_shortened_to_fit_the_original_deadline_after_a_partial_write() {
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"ab");
    let (rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(original) = pdu else {
        panic!("reliable write")
    };
    assert!(harness.session.advance_time(4_000_000_000));
    write_reply(
        &mut harness.session,
        rpc,
        &original,
        ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes: 1 },
    );
    let (suffix_rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(suffix) = pdu else {
        panic!("suffix")
    };
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(INPUT_DEADLINE_NS)
    );
    write_reply(
        &mut harness.session,
        suffix_rpc,
        &suffix,
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied {
            retry_after_ns: 1_000_000_000,
        }),
    );
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn missing_or_ambiguous_receipts_never_gain_retry_authority_from_a_clock() {
    for outcome in [
        ReliablePaneWriteOutcomeV1::Indeterminate,
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::Input(
            codec::ReliableKeyEventRetryV1::DuplicatePending {
                retry_after_ns: 1_000_000,
            },
        )),
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::Input(
            codec::ReliableKeyEventRetryV1::PaneAuthorityRequired {
                pane_registration: codec::ReliablePaneRegistrationIdentityV1::from_bytes(
                    [0x55; 16],
                ),
            },
        )),
    ] {
        let mut harness = timed_current();
        let pane = harness.server.pane_id;
        harness.session.write(pane, b"possibly applied");
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliablePaneWriteV1(original) = pdu else {
            panic!("write")
        };
        write_reply(&mut harness.session, rpc, &original, outcome);
        assert_stopped(&mut harness.session, pane);
    }
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"lost ACK");
    take_one(&mut harness.session);
    assert!(harness.session.advance_time(INPUT_DEADLINE_NS - 1));
    assert!(harness.session.take_outgoing().is_empty());
    assert!(!harness.session.advance_time(INPUT_DEADLINE_NS));
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn a_lone_paste_expires_without_replay_or_later_input() {
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.paste(pane, "possibly applied paste");
    let (rpc, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::SendPaste(_)));
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(INPUT_DEADLINE_NS)
    );
    assert!(harness.session.advance_time(INPUT_DEADLINE_NS - 1));
    assert!(harness.session.take_outgoing().is_empty());
    assert!(!harness.session.advance_time(INPUT_DEADLINE_NS));
    assert_stopped(&mut harness.session, pane);

    // A late ACK cannot revive the failed connection or clear its uncertainty.
    reply(&mut harness.session, 10, rpc, &codec::UnitResponse {});
    assert!(!harness.session.is_ready());
    assert!(harness.session.unanswered_input(pane));
    assert!(harness.session.take_outgoing().is_empty());
}

#[test]
fn either_unanswered_half_of_a_click_keeps_the_original_deadline() {
    for acknowledge_press in [false, true] {
        let mut harness = timed_current();
        let pane = harness.server.pane_id;
        harness.session.click(pane, 2, 3);
        let (press_rpc, pdu) = take_one(&mut harness.session);
        assert!(matches!(pdu, Pdu::SendMouseEvent(_)));
        assert!(harness.session.advance_time(INPUT_DEADLINE_NS - 1));
        assert!(harness.session.take_outgoing().is_empty());
        if acknowledge_press {
            reply(&mut harness.session, 10, press_rpc, &codec::UnitResponse {});
            let (_, release) = take_one(&mut harness.session);
            assert!(matches!(release, Pdu::SendMouseEvent(_)));
        }
        assert_eq!(
            harness.session.next_input_wake_ns(),
            Some(INPUT_DEADLINE_NS),
            "receiving the press ACK cannot grant the queued release more time"
        );
        assert!(!harness.session.advance_time(INPUT_DEADLINE_NS));
        assert_stopped(&mut harness.session, pane);
    }
}

#[test]
fn settling_paste_and_mouse_receipts_retires_only_their_own_deadlines() {
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.paste(pane, "first");
    let (paste_rpc, paste) = take_one(&mut harness.session);
    assert!(matches!(paste, Pdu::SendPaste(_)));
    assert!(harness.session.advance_time(1_000));
    harness.session.click(pane, 2, 3);
    assert!(harness.session.take_outgoing().is_empty());
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(INPUT_DEADLINE_NS)
    );
    reply(&mut harness.session, 10, paste_rpc, &codec::UnitResponse {});
    for _ in 0..2 {
        assert_eq!(
            harness.session.next_input_wake_ns(),
            Some(INPUT_DEADLINE_NS + 1_000)
        );
        let (rpc, mouse) = take_one(&mut harness.session);
        assert!(matches!(mouse, Pdu::SendMouseEvent(_)));
        reply(&mut harness.session, 10, rpc, &codec::UnitResponse {});
    }
    assert!(!harness.session.unanswered_input(pane));
    assert_eq!(harness.session.next_input_wake_ns(), None);
    assert!(harness.session.advance_time(INPUT_DEADLINE_NS + 1_000));
    assert!(harness.session.is_ready());
    assert!(harness.session.take_outgoing().is_empty());
}

#[test]
fn a_lone_mouse_wheel_operation_expires_without_replay() {
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.wheel(pane, 1, 2, 3);
    let (_, pdu) = take_one(&mut harness.session);
    assert!(matches!(pdu, Pdu::SendMouseEvent(_)));
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(INPUT_DEADLINE_NS)
    );
    assert!(!harness.session.advance_time(INPUT_DEADLINE_NS));
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn invalid_retry_pressure_or_delay_fails_before_scheduling() {
    use codec::ReliableKeyEventRetryV1 as Retry;
    let mut invalid = Vec::new();
    for field in 0..9 {
        let mut pressure = scheduler_pressure();
        match field {
            0 => pressure.queue_id = 0,
            1 => pressure.scheduler_generation = 0,
            2 => pressure.task_capacity = 0,
            3 => pressure.estimated_byte_capacity = 0,
            4 => pressure.requested_estimated_bytes = 0,
            5 => pressure.active_tasks = pressure.task_capacity + 1,
            6 => pressure.active_estimated_bytes = pressure.estimated_byte_capacity + 1,
            7 => pressure.retry_after_ns = 0,
            _ => pressure.retry_after_ns = wire::MAX_RELIABLE_RETRY_DELAY_NS + 1,
        }
        invalid.push(Retry::SchedulerFull(pressure));
    }
    invalid.push(Retry::SchedulerUnavailable { retry_after_ns: 0 });
    invalid.push(Retry::ClientRegistrationTransition {
        retry_after_ns: u64::MAX,
    });
    for disposition in invalid {
        let mut harness = timed_current();
        let pane = harness.server.pane_id;
        harness.session.write(pane, b"\x1b[A");
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliableKeyEventV1(key) = pdu else {
            panic!("key")
        };
        reply(
            &mut harness.session,
            97,
            rpc,
            &ReliableKeyEventV1Response {
                pane_id: pane,
                input_serial: key.input_serial,
                outcome: ReliableKeyEventOutcomeV1::Retry(disposition),
            },
        );
        assert_stopped(&mut harness.session, pane);
        assert_eq!(harness.session.next_input_wake_ns(), None);
    }
}

#[test]
fn monotonic_regression_overflow_or_close_cannot_release_a_waiting_retry() {
    for failure in 0..3 {
        let mut harness = timed_current();
        let pane = harness.server.pane_id;
        assert!(harness.session.advance_time(10));
        harness.session.write(pane, b"pending");
        let (rpc, pdu) = take_one(&mut harness.session);
        let Pdu::ReliablePaneWriteV1(original) = pdu else {
            panic!("write")
        };
        write_reply(
            &mut harness.session,
            rpc,
            &original,
            ReliablePaneWriteOutcomeV1::Retry(
                codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied {
                    retry_after_ns: 1_000_000,
                },
            ),
        );
        match failure {
            0 => assert!(!harness.session.advance_time(9)),
            1 => harness.session.closed(),
            _ => reply(
                &mut harness.session,
                37,
                0,
                &codec::PaneRemoved { pane_id: pane },
            ),
        }
        assert!(!harness.session.advance_time(1_000_010));
        assert_stopped(&mut harness.session, pane);
    }
    for effect in 0..4 {
        let mut harness = timed_current();
        let pane = harness.server.pane_id;
        assert!(
            harness
                .session
                .advance_time(u64::MAX - INPUT_DEADLINE_NS + 1)
        );
        match effect {
            0 => harness
                .session
                .write(pane, b"cannot allocate a wrapping deadline"),
            1 => harness
                .session
                .paste(pane, "cannot allocate a wrapping deadline"),
            2 => harness.session.click(pane, 2, 3),
            _ => harness.session.wheel(pane, 1, 2, 3),
        }
        assert_stopped(&mut harness.session, pane);
    }
}

#[test]
fn a_new_clock_cannot_extend_an_operation_that_lacked_one_at_admission() {
    let mut harness = current(71);
    let pane = harness.server.pane_id;
    harness
        .session
        .write(pane, b"admitted without a timer owner");
    let (rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(original) = pdu else {
        panic!("write")
    };
    assert!(harness.session.advance_time(0));
    write_reply(
        &mut harness.session,
        rpc,
        &original,
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied {
            retry_after_ns: 1,
        }),
    );
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn only_a_matching_current_receipt_can_schedule_or_settle_a_retry() {
    let mut harness = timed_current();
    let pane = harness.server.pane_id;
    harness.session.write(pane, b"original");
    let (rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(original) = pdu else {
        panic!("write")
    };
    let disposition =
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied {
            retry_after_ns: 1_000_000,
        });
    write_reply(&mut harness.session, rpc + 100, &original, disposition);
    assert_eq!(
        harness.session.next_input_wake_ns(),
        Some(INPUT_DEADLINE_NS),
        "unmatched RPC grants no retry"
    );
    write_reply(&mut harness.session, rpc, &original, disposition);
    // The previous RPC has already settled with a no-effect result. A late
    // contradictory success cannot release this or a following operation.
    write_reply(
        &mut harness.session,
        rpc,
        &original,
        ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes: 8 },
    );
    assert_eq!(harness.session.next_input_wake_ns(), Some(1_000_000));
    assert!(harness.session.advance_time(1_000_000));
    let (retry_rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(retry) = pdu else {
        panic!("retry")
    };
    assert_eq!(retry, original);
    reply(
        &mut harness.session,
        101,
        retry_rpc,
        &ReliablePaneWriteV1Response {
            pane_id: pane,
            input_serial: InputSerial::from_millis_since_epoch(retry.input_serial.get() + 1),
            outcome: disposition,
        },
    );
    assert_stopped(&mut harness.session, pane);
}

#[test]
fn selecting_another_pane_during_pressure_cannot_retarget_or_overtake_input() {
    let mut harness = timed_current();
    let first = harness.server.pane_id;
    harness.session.write(first, b"first");
    let (rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(original) = pdu else {
        panic!("write")
    };
    write_reply(
        &mut harness.session,
        rpc,
        &original,
        ReliablePaneWriteOutcomeV1::Retry(codec::ReliablePaneWriteRetryV1::DefinitelyNotApplied {
            retry_after_ns: 1_000_000,
        }),
    );
    harness.session.hide(first);
    let second = first + 1;
    harness.session.show(second);
    let (identity_rpc, _) = take_one(&mut harness.session);
    identity_reply(
        &mut harness.session,
        identity_rpc,
        second,
        harness.server.incarnation,
        [0x55; 16],
    );
    assert!(matches!(
        take_one(&mut harness.session).1,
        Pdu::GetPaneRenderChanges(_)
    ));
    harness.session.write(second, b"second");
    assert!(harness.session.take_outgoing().is_empty());
    assert!(harness.session.advance_time(1_000_000));
    let (retry_rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(retry) = pdu else {
        panic!("first pane retains FIFO ownership")
    };
    assert_eq!(retry, original);
    write_reply(
        &mut harness.session,
        retry_rpc,
        &retry,
        ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes: 5 },
    );
    let (_, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(later) = pdu else {
        panic!("second pane follows")
    };
    assert_eq!(later.pane_id, second);
    assert_eq!(later.pane_registration.unwrap().as_bytes(), [0x55; 16]);
    assert_eq!(later.data, b"second");
}
