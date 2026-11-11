//! Explicit persistent-shell creation through the real desktop wire codec.
//! A creation receipt never selects a pane or grants input/resize authority.

mod support;

use codec::{
    CoherentPaneSnapshot, GetCodecVersionResponse, ListPanesCoherentOutcome,
    ListPanesCoherentResponse, ListPanesResponse, MuxErrorCode, MuxErrorEffect, MuxErrorRetry,
    MuxWireDecodedPayload, MuxWireDialect, Pdu, SpawnResponse, TopologyCapabilities,
    TopologyStreamId,
};
use frankenterm_mobile_mux::session::{SHELL_CREATION_DEADLINE_NS, ShellCreationError};
use frankenterm_mobile_mux::{Event, Session, wire};
use support::{Harness, server_frame};

const INCARNATION: [u8; 16] = [0x1c; 16];

fn take_one(session: &mut Session) -> (u64, Pdu) {
    let bytes = session.take_outgoing();
    assert!(!bytes.is_empty(), "expected one request");
    let mut cursor = std::io::Cursor::new(bytes.as_slice());
    let decoded = Pdu::decode_for_dialect(
        &mut cursor,
        MuxWireDialect::current(codec::CODEC_VERSION).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cursor.position() as usize,
        bytes.len(),
        "exactly one request"
    );
    let (serial, payload) = decoded.into_parts();
    let MuxWireDecodedPayload::Pdu(pdu) = payload else {
        panic!("current PDU")
    };
    (serial, pdu)
}

fn reply<T: serde::Serialize>(session: &mut Session, ident: u64, serial: u64, value: &T) {
    session.receive(&server_frame(
        ident,
        serial,
        &varbincode::serialize(value).unwrap(),
        false,
    ));
}

fn drain(session: &mut Session) -> Vec<Event> {
    std::iter::from_fn(|| session.next_event()).collect()
}

fn empty_panes() -> ListPanesResponse {
    ListPanesResponse {
        tabs: Vec::new(),
        tab_titles: Vec::new(),
        window_titles: Default::default(),
        floating_panes: Vec::new(),
    }
}

fn registered(version: usize) -> Session {
    let mut session = Session::new();
    let (rpc, pdu) = take_one(&mut session);
    assert!(matches!(pdu, Pdu::GetCodecVersion(_)));
    reply(
        &mut session,
        27,
        rpc,
        &GetCodecVersionResponse {
            codec_vers: version,
            version_string: "creation-test".to_owned(),
            executable_path: "/mux".into(),
            config_file_path: None,
            min_supported: 61,
        },
    );
    let (rpc, pdu) = take_one(&mut session);
    assert!(matches!(pdu, Pdu::SetClientId(_)));
    reply(&mut session, 10, rpc, &codec::UnitResponse {});
    session
}

fn empty_session(version: usize, clock: bool) -> Session {
    let mut session = registered(version);
    let (rpc, pdu) = take_one(&mut session);
    assert!(matches!(pdu, Pdu::ListPanesCoherent(_)));
    reply(
        &mut session,
        82,
        rpc,
        &ListPanesCoherentResponse {
            negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            stream_id: TopologyStreamId::from_bytes([0x5a; 16]),
            outcome: ListPanesCoherentOutcome::Snapshot(CoherentPaneSnapshot {
                session_incarnation: mux::MuxSessionIncarnation::from_bytes(INCARNATION),
                snapshot_revision: mux::TopologyRevision::new(0),
                panes: empty_panes(),
            }),
        },
    );
    assert!(session.is_ready());
    assert!(session.panes().is_empty());
    assert!(session.take_outgoing().is_empty());
    drain(&mut session);
    if clock {
        assert!(session.advance_time(0));
    }
    session
}

fn receipt() -> SpawnResponse {
    SpawnResponse {
        tab_id: 9,
        pane_id: 17,
        window_id: 23,
        size: frankenterm_term::TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        },
    }
}

fn assert_unknown_before_failure(session: &mut Session, request: u64) {
    assert!(!session.is_ready());
    assert!(session.take_outgoing().is_empty());
    let events = drain(session);
    let creation = events
        .iter()
        .position(|event| {
            matches!(event,
                Event::ShellCreationFailed { request: got, uncertain: true, reason }
                if *got == request && !reason.is_empty() && reason.len() <= 512
            )
        })
        .expect("creation must retain its uncertain result");
    let failed = events
        .iter()
        .position(|event| matches!(event, Event::Failed { .. }))
        .expect("failed session");
    assert!(
        creation < failed,
        "teardown receives the operation result first"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ShellCreated { .. } | Event::ShellCreationFailed { .. }
            ))
            .count(),
        1
    );
}

#[test]
fn an_empty_current_mux_can_create_one_default_shell_without_selecting_it() {
    for version in [61, 71, 74] {
        let mut session = empty_session(version, true);
        let request = session
            .create_shell("réparation", 24, 80, INCARNATION)
            .unwrap();
        assert_eq!(request, 4);
        let (rpc, pdu) = take_one(&mut session);
        assert_eq!(rpc, request);
        let Pdu::SpawnV2(spawn) = pdu else {
            panic!("existing SpawnV2")
        };
        assert_eq!(
            spawn.domain,
            config::keyassignment::SpawnTabDomain::DefaultDomain
        );
        assert_eq!(spawn.window_id, None);
        assert!(spawn.command.is_none());
        assert!(spawn.command_dir.is_none());
        assert_eq!(spawn.workspace, "réparation");
        assert_eq!(spawn.size, receipt().size);
        assert_eq!(
            session.next_input_wake_ns(),
            Some(SHELL_CREATION_DEADLINE_NS)
        );
        assert!(drain(&mut session).is_empty());
        reply(&mut session, 8, rpc, &receipt());
        assert_eq!(
            drain(&mut session),
            [Event::ShellCreated {
                request,
                pane_id: 17,
                tab_id: 9,
                window_id: 23,
            }]
        );
        assert!(session.is_ready());
        assert!(
            session.panes().is_empty(),
            "only a fresh inventory can add the pane"
        );
        assert!(
            session.take_outgoing().is_empty(),
            "creation must not show, inspect, refresh, resize, or send input automatically"
        );
        assert_eq!(session.next_input_wake_ns(), None);
    }
}

#[test]
fn creation_does_not_retarget_or_settle_existing_pane_input() {
    let mut harness = Harness::new(4, 30);
    harness.server.codec_vers = 74;
    harness.server.min_supported = 61;
    harness.attach();
    let pane = harness.server.pane_id;
    let original_panes = harness.session.panes().to_vec();
    assert!(harness.session.advance_time(0));
    harness.session.write(pane, b"old");
    let (write_rpc, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(write) = pdu else {
        panic!("original write")
    };
    let request = harness
        .session
        .create_shell("default", 24, 80, INCARNATION)
        .unwrap();
    assert!(matches!(take_one(&mut harness.session).1, Pdu::SpawnV2(_)));
    reply(&mut harness.session, 8, request, &receipt());
    assert_eq!(harness.session.panes(), original_panes.as_slice());
    assert!(harness.session.unanswered_input(pane));
    assert!(harness.session.take_outgoing().is_empty());
    assert_eq!(
        drain(&mut harness.session),
        [Event::ShellCreated {
            request,
            pane_id: 17,
            tab_id: 9,
            window_id: 23,
        }]
    );
    harness.session.write(pane, b"later");
    assert!(harness.session.take_outgoing().is_empty());
    reply(
        &mut harness.session,
        101,
        write_rpc,
        &codec::ReliablePaneWriteV1Response {
            pane_id: pane,
            input_serial: write.input_serial,
            outcome: codec::ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes: 3 },
        },
    );
    let (_, pdu) = take_one(&mut harness.session);
    let Pdu::ReliablePaneWriteV1(later) = pdu else {
        panic!("queued original-pane write")
    };
    assert_eq!(later.pane_id, pane);
    assert_eq!(later.pane_registration, write.pane_registration);
    assert_eq!(later.data, b"later");
}

#[test]
fn invalid_or_stale_admission_consumes_no_serial_or_wire_bytes() {
    let invalid = [
        ("".to_owned(), 24, 80),
        ("bad\nworkspace".to_owned(), 24, 80),
        ("bad\u{0085}workspace".to_owned(), 24, 80),
        ("x".repeat(257), 24, 80),
        (format!("{}é", "x".repeat(255)), 24, 80),
        ("default".to_owned(), 0, 80),
        ("default".to_owned(), 24, 0),
        ("default".to_owned(), 513, 1),
        ("default".to_owned(), 256, 129),
        ("default".to_owned(), usize::MAX, usize::MAX),
    ];
    let mut session = empty_session(74, true);
    for (workspace, rows, cols) in invalid {
        assert_eq!(
            session.create_shell(&workspace, rows, cols, INCARNATION),
            Err(ShellCreationError::Invalid)
        );
    }
    for stale in [[0; 16], [0x55; 16]] {
        assert_eq!(
            session.create_shell("default", 24, 80, stale),
            Err(ShellCreationError::Stale)
        );
    }
    assert!(session.take_outgoing().is_empty());
    assert!(drain(&mut session).is_empty());
    assert_eq!(session.next_input_wake_ns(), None);
    assert_eq!(
        session.create_shell(&"x".repeat(256), 64, 512, INCARNATION),
        Ok(4)
    );
    let (_, pdu) = take_one(&mut session);
    let Pdu::SpawnV2(spawn) = pdu else {
        panic!("boundary-sized creation")
    };
    assert_eq!(spawn.workspace.len(), 256);
    assert_eq!(spawn.size.rows * spawn.size.cols, wire::MAX_SHELL_CELLS);
}

#[test]
fn missing_clock_incarnation_and_legacy_are_pre_admission_refusals() {
    let mut session = Session::new();
    assert_eq!(
        session.create_shell("default", 24, 80, INCARNATION),
        Err(ShellCreationError::NotReady)
    );
    assert!(matches!(take_one(&mut session).1, Pdu::GetCodecVersion(_)));
    let mut session = registered(74);
    take_one(&mut session); // The outstanding inventory has not supplied an incarnation.
    assert!(session.advance_time(0));
    assert_eq!(
        session.create_shell("default", 24, 80, INCARNATION),
        Err(ShellCreationError::Stale)
    );
    assert!(session.take_outgoing().is_empty());
    let mut session = empty_session(74, false);
    assert_eq!(
        session.create_shell("default", 24, 80, INCARNATION),
        Err(ShellCreationError::ClockUnavailable)
    );
    assert!(session.take_outgoing().is_empty());
    assert!(session.advance_time(0));
    assert_eq!(session.create_shell("default", 24, 80, INCARNATION), Ok(4));
    for version in [46, 47] {
        let mut harness = Harness::new(4, 30);
        harness.server.codec_vers = version;
        harness.attach();
        assert!(harness.session.advance_time(0));
        assert_eq!(
            harness.session.create_shell("default", 24, 80, INCARNATION),
            Err(ShellCreationError::Unsupported)
        );
        assert!(harness.session.is_ready());
        assert!(harness.session.take_outgoing().is_empty());
    }
}

#[test]
fn only_one_creation_or_undelivered_result_can_be_owned() {
    let mut session = empty_session(74, true);
    let first = session.create_shell("first", 24, 80, INCARNATION).unwrap();
    assert_eq!(
        session.create_shell("duplicate", 24, 80, INCARNATION),
        Err(ShellCreationError::Busy)
    );
    assert_eq!(take_one(&mut session).0, first);
    reply(&mut session, 8, first, &receipt());
    assert_eq!(
        session.create_shell("unread result", 24, 80, INCARNATION),
        Err(ShellCreationError::Busy)
    );
    assert!(session.take_outgoing().is_empty());
    assert!(
        matches!(session.next_event(), Some(Event::ShellCreated { request, .. }) if request == first)
    );
    assert_eq!(
        session.create_shell("second", 24, 80, INCARNATION),
        Ok(first + 1)
    );
}

#[test]
fn malformed_creation_receipts_are_unknown_and_do_not_release_another_effect() {
    for fault in 0..10 {
        for compress in [false, true] {
            let mut session = empty_session(74, true);
            let request = session
                .create_shell("default", 24, 80, INCARNATION)
                .unwrap();
            take_one(&mut session);
            let mut response = receipt();
            match fault {
                5 => response.size.rows = 0,
                6 => response.pane_id = usize::MAX,
                7 => response.tab_id = usize::MAX,
                8 => response.window_id = usize::MAX,
                _ => {}
            }
            let mut body = varbincode::serialize(&response).unwrap();
            let mut ident = 8;
            match fault {
                0 => {
                    body.pop();
                }
                1 => body.push(0),
                2 => body.resize(wire::MAX_SPAWN_RESPONSE_BYTES + 1, 0),
                3 => {
                    ident = 10;
                    body.clear();
                }
                4 => {
                    ident = 0;
                    body = varbincode::serialize(&codec::ErrorResponse::new(
                        35,
                        MuxErrorCode::POLICY_REJECTED,
                        None,
                        MuxErrorEffect::NOT_APPLIED,
                        MuxErrorRetry::NEVER,
                    ))
                    .unwrap();
                }
                9 => {
                    ident = 124;
                    body.clear();
                }
                _ => {}
            }
            session.receive(&server_frame(ident, request, &body, compress));
            assert_unknown_before_failure(&mut session, request);
        }
    }
}

#[test]
fn only_validated_explicit_no_effect_authority_can_report_a_refusal() {
    for unknown in 0..8 {
        let mut session = empty_session(74, true);
        let request = session
            .create_shell("default", 24, 80, INCARNATION)
            .unwrap();
        take_one(&mut session);
        let mut error = codec::ErrorResponse::new(
            36,
            MuxErrorCode::DOMAIN_NOT_FOUND,
            None,
            MuxErrorEffect::NOT_APPLIED,
            MuxErrorRetry::NEVER,
        );
        match unknown {
            0 => error.validate().unwrap(),
            1 => {
                error = codec::ErrorResponse::new(
                    36,
                    MuxErrorCode::BACKEND_FAILURE,
                    None,
                    MuxErrorEffect::NOT_APPLIED,
                    MuxErrorRetry::SAFE_AFTER_BACKOFF,
                )
            }
            2 => {
                error = codec::ErrorResponse::new(
                    36,
                    MuxErrorCode::INDETERMINATE_MUTATION,
                    None,
                    MuxErrorEffect::INDETERMINATE,
                    MuxErrorRetry::RECONCILE_BEFORE_RETRY,
                )
            }
            3 => error.schema_version = 2,
            4 => error.code = MuxErrorCode(999),
            5 => error.retry = MuxErrorRetry(0),
            6 => {
                error.object = Some(codec::MuxErrorObject {
                    kind: codec::MuxErrorObjectKind::PANE,
                    id: 7,
                })
            }
            7 => error.effect = MuxErrorEffect(0),
            _ => unreachable!(),
        }
        reply(&mut session, 0, request, &error);
        assert!(
            session.is_ready(),
            "a bounded rejection does not end viewing"
        );
        let events = drain(&mut session);
        assert!(
            matches!(&events[..], [Event::ShellCreationFailed { request: got, uncertain, reason }]
            if *got == request && *uncertain == (unknown >= 2) && reason.len() <= 512)
        );
        assert_eq!(session.next_input_wake_ns(), None);
        assert!(
            session.take_outgoing().is_empty(),
            "even safe-after-backoff never resends creation"
        );
    }
}

#[test]
fn unsolicited_or_cross_request_spawn_receipts_cannot_settle_creation() {
    for unsolicited in [false, true] {
        let mut session = empty_session(74, true);
        let request = session
            .create_shell("default", 24, 80, INCARNATION)
            .unwrap();
        take_one(&mut session);
        let wrong_request = if unsolicited {
            0
        } else {
            session.refresh_panes();
            let (rpc, pdu) = take_one(&mut session);
            assert!(matches!(pdu, Pdu::ListPanes(_)));
            assert_ne!(rpc, request);
            rpc
        };
        reply(&mut session, 8, wrong_request, &receipt());
        assert_unknown_before_failure(&mut session, request);
    }
}

#[test]
fn original_deadline_and_disconnect_preserve_unknown_creation_without_replay() {
    for disconnect in [false, true] {
        let mut session = empty_session(74, true);
        let request = session
            .create_shell("default", 24, 80, INCARNATION)
            .unwrap();
        take_one(&mut session);
        // An unrelated RPC serial cannot consume the creation receipt.
        reply(&mut session, 8, request + 100, &receipt());
        assert!(drain(&mut session).is_empty());
        assert_eq!(
            session.next_input_wake_ns(),
            Some(SHELL_CREATION_DEADLINE_NS)
        );
        assert!(session.advance_time(SHELL_CREATION_DEADLINE_NS - 1));
        assert!(session.take_outgoing().is_empty());
        if disconnect {
            session.closed();
        } else {
            assert!(!session.advance_time(SHELL_CREATION_DEADLINE_NS));
        }
        assert_unknown_before_failure(&mut session, request);
        reply(&mut session, 8, request, &receipt());
        assert!(
            drain(&mut session).is_empty(),
            "late receipts cannot revive a closed operation"
        );
        assert_eq!(
            session.create_shell("default", 24, 80, INCARNATION),
            Err(ShellCreationError::NotReady)
        );
        assert!(session.take_outgoing().is_empty());
    }
}
