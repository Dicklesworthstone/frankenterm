use frankenterm_terminal_wire::*;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn dimensions(columns: u16) -> Dimensions {
    Dimensions {
        columns,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn scope() -> Scope {
    Scope {
        identity: PaneIdentity {
            host: id("host-a"),
            pane: id("20000000-0000-4000-8000-000000000001"),
            incarnation: id("30000000-0000-4000-8000-000000000002"),
        },
        epoch: WireU64(9_007_199_254_740_993),
    }
}

fn capabilities() -> Vec<Capability> {
    vec![
        Capability::TerminalState,
        Capability::Input,
        Capability::Resize,
        Capability::Control,
        Capability::Events,
        Capability::ResourceReferences,
    ]
}

fn hello(required: Vec<Capability>) -> Hello {
    Hello {
        host: id("host-a"),
        hello_id: id("hello-a"),
        asset: Sha256::new(&"a".repeat(64)).unwrap(),
        required: required.try_into().unwrap(),
        optional: Capabilities::new(),
    }
}

fn pair(required: Vec<Capability>) -> (ServerSession, BrowserSession) {
    let hello = hello(required);
    let mut server = ServerSession::new(
        id("host-a"),
        id("session-a"),
        hello.asset.clone(),
        hello.required.clone(),
    )
    .unwrap();
    let mut browser = BrowserSession::new(hello).unwrap();
    let welcome = server.negotiate(&browser.hello_frame().unwrap()).unwrap();
    browser.accept_welcome(&welcome).unwrap();
    (server, browser)
}

fn bind(server: &mut ServerSession, browser: &mut BrowserSession, sequence: u64) {
    server
        .bind_scope(scope(), dimensions(80), WireU64(sequence))
        .unwrap();
    browser
        .bind_scope(scope(), dimensions(80), WireU64(sequence))
        .unwrap();
}

fn effect(scope: Scope, fence: u64) -> Mutation {
    Mutation {
        scope,
        request_id: id("request-a"),
        lease_fence: WireU64(fence),
    }
}

fn modifiers() -> Modifiers {
    Modifiers {
        shift: false,
        alt: false,
        control: false,
        super_key: false,
    }
}

fn request(message: ClientMessage) -> Vec<u8> {
    encode_client(&ClientFrame::new(Some(id("session-a")), message).unwrap()).unwrap()
}

fn ack(sequence: u64) -> ClientMessage {
    ClientMessage::AppliedAck(AppliedAck {
        scope: scope(),
        sequence: WireU64(sequence),
    })
}

fn patch(columns: u16) -> GridPatch {
    let style = CellStyle {
        foreground: Color::Default,
        background: Color::Default,
        underline_color: Color::Default,
        underline: Underline::None,
        attributes: 0,
    };
    let cells = (0..columns)
        .map(|column| Cell {
            column,
            text: Text::new("x").unwrap(),
            width: 1,
            style: style.clone(),
            hyperlink: None,
            image: None,
        })
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    GridPatch {
        dimensions: dimensions(columns),
        cursor: Cursor {
            column: 0,
            row: 0,
            visible: true,
            blink: false,
            shape: CursorShape::Block,
        },
        lines: vec![Line {
            row: 0,
            stable_row: WireI64(-1),
            cells,
        }]
        .try_into()
        .unwrap(),
        hyperlinks: BoundedVec::new(),
        images: BoundedVec::new(),
    }
}

fn delta(columns: u16, base: u64, sequence: u64) -> ServerMessage {
    ServerMessage::Delta(Delta {
        scope: scope(),
        base_sequence: WireU64(base),
        sequence: WireU64(sequence),
        modes: TerminalModes {
            alternate_screen: false,
            mouse_tracking: false,
            bracketed_paste: true,
        },
        patch: patch(columns),
    })
}

#[test]
fn handshake_failure_does_not_open_or_reset_a_session() {
    let configuration = hello(capabilities());
    let mut server = ServerSession::new(
        id("host-a"),
        id("session-a"),
        configuration.asset.clone(),
        configuration.required.clone(),
    )
    .unwrap();
    assert_eq!(
        server.decode_request(&request(ack(0))),
        Err(WireError::NotNegotiated)
    );
    assert_eq!(
        server.bind_scope(scope(), dimensions(80), WireU64(0)),
        Err(WireError::NotNegotiated)
    );

    let mut wrong = configuration.clone();
    wrong.asset = Sha256::new(&"b".repeat(64)).unwrap();
    let bad = encode_client(&ClientFrame::new(None, ClientMessage::Hello(wrong)).unwrap()).unwrap();
    assert_eq!(server.negotiate(&bad), Err(WireError::AssetMismatch));
    assert_eq!(
        server.decode_request(&request(ack(0))),
        Err(WireError::NotNegotiated)
    );

    let mut browser = BrowserSession::new(configuration).unwrap();
    let hello_bytes = browser.hello_frame().unwrap();
    let welcome_bytes = server.negotiate(&hello_bytes).unwrap();
    let mut wrong_welcome = decode_server(&welcome_bytes).unwrap();
    let ServerMessage::Welcome(welcome) = &mut wrong_welcome.message else {
        panic!("welcome");
    };
    welcome.hello_id = id("other-hello");
    assert_eq!(
        browser.accept_welcome(&encode_server(&wrong_welcome).unwrap()),
        Err(WireError::IdentityMismatch)
    );
    browser.accept_welcome(&welcome_bytes).unwrap();
    assert_eq!(
        server.negotiate(&hello_bytes),
        Err(WireError::AlreadyNegotiated)
    );
    assert_eq!(
        browser.accept_welcome(&welcome_bytes),
        Err(WireError::AlreadyNegotiated)
    );
    assert_eq!(browser.hello_frame(), Err(WireError::AlreadyNegotiated));
    let ping = browser
        .encode_request(ClientMessage::Ping(Ping {
            nonce: WireU64(u64::MAX),
        }))
        .unwrap();
    assert!(server.decode_request(&ping).is_ok());
}

#[test]
fn negotiation_requires_requested_capabilities_and_pins_the_session() {
    let configuration = hello(vec![Capability::TerminalState, Capability::Input]);
    let mut server = ServerSession::new(
        id("host-a"),
        id("session-a"),
        configuration.asset.clone(),
        vec![Capability::TerminalState].try_into().unwrap(),
    )
    .unwrap();
    let bytes =
        encode_client(&ClientFrame::new(None, ClientMessage::Hello(configuration)).unwrap())
            .unwrap();
    assert_eq!(
        server.negotiate(&bytes),
        Err(WireError::UnsupportedCapability)
    );

    let (mut server, mut browser) = pair(vec![Capability::TerminalState]);
    bind(&mut server, &mut browser, 10);
    let wrong =
        encode_client(&ClientFrame::new(Some(id("different-session")), ack(0)).unwrap()).unwrap();
    assert_eq!(
        server.decode_request(&wrong),
        Err(WireError::IdentityMismatch)
    );
    let input = ClientMessage::Paste(PasteRequest {
        effect: effect(scope(), 1),
        text: Text::new("x").unwrap(),
    });
    assert_eq!(
        server.decode_request(&request(input)),
        Err(WireError::UnsupportedCapability)
    );
    assert_eq!(
        server.record_lease(&scope(), WireU64(1), true),
        Err(WireError::UnsupportedCapability)
    );
}

#[test]
fn every_scoped_request_rejects_a_stale_epoch_before_it_can_be_dispatched() {
    let (mut server, mut browser) = pair(capabilities());
    bind(&mut server, &mut browser, 10);
    server.record_lease(&scope(), WireU64(7), true).unwrap();
    let mut stale = scope();
    stale.epoch = WireU64(1);
    let read = ReadRequest {
        scope: stale.clone(),
        request_id: id("request-a"),
    };
    let lease = LeaseRequest {
        scope: stale.clone(),
        request_id: id("request-a"),
        lease_fence: WireU64(7),
    };
    let messages = vec![
        ClientMessage::SnapshotRequest(read.clone()),
        ClientMessage::AcquireControl(read),
        ClientMessage::AppliedAck(AppliedAck {
            scope: stale.clone(),
            sequence: WireU64(0),
        }),
        ClientMessage::Resync(Resync {
            scope: stale.clone(),
            last_applied: WireU64(0),
            reason: ResyncReason::Gap,
        }),
        ClientMessage::Unsubscribe(Unsubscribe {
            request_id: id("request-a"),
            scopes: vec![stale.clone()].try_into().unwrap(),
        }),
        ClientMessage::Key(KeyRequest {
            effect: effect(stale.clone(), 7),
            key: Key::Named(NamedKey::Enter),
            modifiers: modifiers(),
            action: KeyAction::Press,
        }),
        ClientMessage::Paste(PasteRequest {
            effect: effect(stale.clone(), 7),
            text: Text::new("x").unwrap(),
        }),
        ClientMessage::Mouse(MouseRequest {
            effect: effect(stale.clone(), 7),
            kind: MouseKind::Move,
            button: MouseButton::None,
            column: 0,
            row: 0,
            wheel_rows: 0,
            modifiers: modifiers(),
        }),
        ClientMessage::Resize(ResizeRequest {
            effect: effect(stale, 7),
            dimensions: dimensions(100),
        }),
        ClientMessage::RenewControl(lease.clone()),
        ClientMessage::ReleaseControl(lease),
    ];
    for message in messages {
        assert_eq!(
            server.decode_request(&request(message)),
            Err(WireError::StaleScope)
        );
    }
    for field in ["host", "pane", "incarnation"] {
        let mut wrong = scope();
        match field {
            "host" => wrong.identity.host = id("other-host"),
            "pane" => wrong.identity.pane = id("other-pane"),
            _ => wrong.identity.incarnation = id("other-incarnation"),
        }
        let bytes = request(ClientMessage::AppliedAck(AppliedAck {
            scope: wrong,
            sequence: WireU64(0),
        }));
        assert_eq!(
            server.decode_request(&bytes),
            Err(if field == "host" {
                WireError::IdentityMismatch
            } else {
                WireError::StaleScope
            })
        );
    }
    let foreign = ClientMessage::Subscribe(Subscribe {
        request_id: id("subscribe"),
        panes: vec![PaneIdentity {
            host: id("other-host"),
            pane: id("pane"),
            incarnation: id("incarnation"),
        }]
        .try_into()
        .unwrap(),
    });
    assert_eq!(
        server.decode_request(&request(foreign)),
        Err(WireError::IdentityMismatch)
    );
    assert!(server.decode_request(&request(ack(0))).is_ok());
}

#[test]
fn encoding_and_decoding_do_not_advance_delivery_or_issue_an_applied_ack() {
    let (mut server, mut browser) = pair(capabilities());
    bind(&mut server, &mut browser, 10);
    let bytes = server.encode_response(delta(80, 10, 11)).unwrap();
    assert!(browser.decode_response(&bytes).is_ok());
    assert_eq!(
        server.decode_request(&request(ack(11))),
        Err(WireError::StaleSequence)
    );
    assert_eq!(
        browser.encode_request(ack(11)),
        Err(WireError::StaleSequence)
    );
    server
        .record_delivery(&scope(), WireU64(10), WireU64(11))
        .unwrap();
    browser
        .record_delivery(&scope(), WireU64(10), WireU64(11))
        .unwrap();
    let applied = browser.encode_request(ack(11)).unwrap();
    assert!(server.decode_request(&applied).is_ok());
    assert!(
        server.decode_request(&applied).is_ok(),
        "duplicate applied ACK is idempotent"
    );
    assert_eq!(
        server.decode_request(&request(ack(10))),
        Err(WireError::StaleSequence)
    );
    assert_eq!(
        server.record_delivery(&scope(), WireU64(10), WireU64(12)),
        Err(WireError::StaleSequence)
    );
    assert_eq!(
        browser.decode_response(&bytes),
        Err(WireError::StaleSequence)
    );
    assert_eq!(WireU64(u64::MAX).next(), Err(WireError::StaleSequence));
}

#[test]
fn retired_lease_fences_cannot_resurrect_and_delayed_revokes_cannot_retire_a_new_lease() {
    let (mut server, mut browser) = pair(capabilities());
    bind(&mut server, &mut browser, 10);
    for active in [true, false] {
        server.record_lease(&scope(), WireU64(7), active).unwrap();
        browser.record_lease(&scope(), WireU64(7), active).unwrap();
    }
    assert_eq!(
        server.record_lease(&scope(), WireU64(7), true),
        Err(WireError::StaleLease)
    );
    let grant = ServerMessage::LeaseState(LeaseState {
        scope: scope(),
        request_id: id("lease"),
        disposition: LeaseDisposition::Granted,
        fence: Some(WireU64(7)),
        ttl_ms: 1000,
    });
    assert_eq!(
        server.encode_response(grant.clone()),
        Err(WireError::StaleLease)
    );
    let untrusted =
        encode_server(&ServerFrame::new(Some(id("session-a")), grant).unwrap()).unwrap();
    assert_eq!(
        browser.decode_response(&untrusted),
        Err(WireError::StaleLease)
    );

    server.record_lease(&scope(), WireU64(8), true).unwrap();
    browser.record_lease(&scope(), WireU64(8), true).unwrap();
    let old_revoke = ServerMessage::LeaseState(LeaseState {
        scope: scope(),
        request_id: id("old-revoke"),
        disposition: LeaseDisposition::Revoked,
        fence: Some(WireU64(7)),
        ttl_ms: 0,
    });
    let bytes =
        encode_server(&ServerFrame::new(Some(id("session-a")), old_revoke).unwrap()).unwrap();
    assert_eq!(browser.decode_response(&bytes), Err(WireError::StaleLease));
    let input = browser
        .encode_request(ClientMessage::Paste(PasteRequest {
            effect: effect(scope(), 8),
            text: Text::new("only-current-fence").unwrap(),
        }))
        .unwrap();
    assert!(server.decode_request(&input).is_ok());
    for active in [true, false] {
        server
            .record_lease(&scope(), WireU64(u64::MAX), active)
            .unwrap();
    }
    assert_eq!(
        server.record_lease(&scope(), WireU64(u64::MAX), true),
        Err(WireError::StaleLease)
    );
    assert_eq!(
        server.record_lease(&scope(), WireU64(0), true),
        Err(WireError::StaleLease)
    );
}

#[test]
fn resize_updates_geometry_without_refunding_ack_or_lease_watermarks() {
    let (mut server, mut browser) = pair(capabilities());
    bind(&mut server, &mut browser, 10);
    server.record_lease(&scope(), WireU64(9), true).unwrap();
    browser.record_lease(&scope(), WireU64(9), true).unwrap();
    let applied = browser.encode_request(ack(8)).unwrap();
    server.decode_request(&applied).unwrap();
    let resize = browser
        .encode_request(ClientMessage::Resize(ResizeRequest {
            effect: effect(scope(), 9),
            dimensions: dimensions(120),
        }))
        .unwrap();
    server.decode_request(&resize).unwrap();
    assert_eq!(
        server.encode_response(delta(120, 10, 11)),
        Err(WireError::StaleScope)
    );
    server.update_dimensions(&scope(), dimensions(120)).unwrap();
    let bytes = server.encode_response(delta(120, 10, 11)).unwrap();
    assert_eq!(browser.decode_response(&bytes), Err(WireError::StaleScope));
    browser
        .update_dimensions(&scope(), dimensions(120))
        .unwrap();
    assert!(browser.decode_response(&bytes).is_ok());
    assert_eq!(
        server.decode_request(&request(ack(7))),
        Err(WireError::StaleSequence)
    );
    assert_eq!(
        browser.encode_request(ack(7)),
        Err(WireError::StaleSequence)
    );
    assert_eq!(
        server.record_lease(&scope(), WireU64(8), true),
        Err(WireError::StaleLease)
    );
    let mouse = browser
        .encode_request(ClientMessage::Mouse(MouseRequest {
            effect: effect(scope(), 9),
            kind: MouseKind::Move,
            button: MouseButton::None,
            column: 100,
            row: 0,
            wheel_rows: 0,
            modifiers: modifiers(),
        }))
        .unwrap();
    assert!(server.decode_request(&mouse).is_ok());
    let mut invalid = dimensions(120);
    invalid.columns = 0;
    assert_eq!(
        server.update_dimensions(&scope(), invalid),
        Err(WireError::InvalidBounds)
    );
    assert!(
        server.decode_request(&mouse).is_ok(),
        "invalid resize must not corrupt geometry"
    );
}

#[test]
fn resize_receipts_work_without_negotiating_keyboard_input() {
    let (mut server, mut browser) = pair(vec![
        Capability::TerminalState,
        Capability::Control,
        Capability::Resize,
    ]);
    bind(&mut server, &mut browser, 10);
    server.record_lease(&scope(), WireU64(1), true).unwrap();
    browser.record_lease(&scope(), WireU64(1), true).unwrap();
    let resize = browser
        .encode_request(ClientMessage::Resize(ResizeRequest {
            effect: effect(scope(), 1),
            dimensions: dimensions(120),
        }))
        .unwrap();
    server.decode_request(&resize).unwrap();
    let receipt = server
        .encode_response(ServerMessage::ResizeReceipt(ResizeReceipt {
            effect: effect(scope(), 1),
            disposition: ReceiptDisposition::Accepted,
            dimensions: Some(dimensions(120)),
            reason: None,
        }))
        .unwrap();
    assert!(matches!(
        browser.decode_response(&receipt).unwrap().message,
        ServerMessage::ResizeReceipt(_)
    ));
    assert_eq!(
        server.encode_response(ServerMessage::InputReceipt(InputReceipt {
            effect: effect(scope(), 1),
            disposition: ReceiptDisposition::Accepted,
            input_serial: Some(WireU64(1)),
            reason: None,
        })),
        Err(WireError::UnsupportedCapability)
    );
}

#[test]
fn subscription_bound_and_explicit_incarnation_replacement_preserve_other_scopes() {
    let (mut server, _) = pair(capabilities());
    for pane in 0..MAX_SUBSCRIPTIONS {
        let mut next = scope();
        next.identity.pane = id(&format!("pane-{pane}"));
        server.bind_scope(next, dimensions(80), WireU64(0)).unwrap();
    }
    assert_eq!(
        server.bind_scope(scope(), dimensions(80), WireU64(0)),
        Err(WireError::LimitExceeded)
    );
    let mut old = scope();
    old.identity.pane = id("pane-0");
    let mut replacement = old.clone();
    replacement.identity.incarnation = id("replacement");
    assert_eq!(
        server.bind_scope(replacement.clone(), dimensions(80), WireU64(0)),
        Err(WireError::StaleScope)
    );
    server.unbind_scope(&old).unwrap();
    server
        .bind_scope(replacement, dimensions(80), WireU64(0))
        .unwrap();
    let stale = ClientMessage::SnapshotRequest(ReadRequest {
        scope: old,
        request_id: id("old"),
    });
    assert_eq!(
        server.decode_request(&request(stale)),
        Err(WireError::StaleScope)
    );
    let mut unaffected = scope();
    unaffected.identity.pane = id("pane-1");
    assert!(
        server
            .decode_request(&request(ClientMessage::SnapshotRequest(ReadRequest {
                scope: unaffected,
                request_id: id("still-live"),
            })))
            .is_ok()
    );
}
