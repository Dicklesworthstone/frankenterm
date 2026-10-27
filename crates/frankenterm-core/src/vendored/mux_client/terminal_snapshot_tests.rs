// Included inside mux_client::tests to reuse its actual Unix-socket harness.
mod terminal_snapshot_transport {
    use super::*;
    use crate::web::terminal_projection::{TerminalProjectionError, scope_for_identity};
    use frankenterm_terminal_wire as wire;

    #[derive(Clone, Copy)]
    enum Scenario {
        Stable,
        ReplaceAfterRows,
        RestartAfterRows,
        DurableChangesAfterRows,
        PaletteChangesEveryAttempt,
        CursorChangesEveryAttempt,
        GeometryChangesEveryAttempt,
        PaletteChangesOnce,
        MisplacedRow,
    }

    #[derive(Default, Debug)]
    struct Trace {
        identity_queries: usize,
        render_queries: usize,
        row_queries: usize,
        listing_queries: usize,
        requested_ranges: Vec<std::ops::Range<isize>>,
    }

    fn native_identity() -> codec::PaneIdentityV1 {
        codec::PaneIdentityV1 {
            session_incarnation: mux::MuxSessionIncarnation::from_bytes([0x11; 16]),
            durable_pane_id: [0x22; 16],
            pane_registration: codec::ReliablePaneRegistrationIdentityV1::from_bytes([0x33; 16]),
        }
    }

    fn snapshot_scope() -> wire::Scope {
        scope_for_identity(wire::Id::new("socket-fixture-host").unwrap(), &native_identity(), wire::WireU64(5)).unwrap()
    }

    fn metadata() -> codec::PaneRenderMetadataV1 {
        codec::PaneRenderMetadataV1 {
            seqno: 7,
            dimensions: mux::renderable::RenderableDimensions {
                cols: 80, viewport_rows: 65, scrollback_rows: 65,
                physical_top: 100, scrollback_top: 100,
                dpi: 96, pixel_width: 640, pixel_height: 1040, reverse_video: false,
            },
            cursor: mux::renderable::StableCursorPosition {
                x: 3, y: 164, shape: termwiz::surface::CursorShape::SteadyBlock,
                visibility: termwiz::surface::CursorVisibility::Visible,
            },
            mouse_grabbed: false, alt_screen_active: false,
            bracketed_paste: true, focus_tracking: true, palette_generation: 4,
            palette: frankenterm_term::color::ColorPalette::default(),
        }
    }

    async fn fixture(
        scenario: Scenario,
    ) -> (tempfile::TempDir, PathBuf, task::JoinHandle<()>, Arc<StdMutex<Trace>>) {
        let trace = Arc::new(StdMutex::new(Trace::default()));
        let server_trace = Arc::clone(&trace);
        let (dir, path, server) = render_read_server(1, move |connection, request| {
            assert_eq!(connection, 0, "one transaction may not migrate to a new connection");
            let mut trace = server_trace.lock().unwrap();
            let reply = match request {
                Pdu::GetPaneIdentityV1(request) => {
                    assert_eq!(request.pane_id, 7);
                    trace.identity_queries += 1;
                    assert!(trace.identity_queries <= 6, "at most three complete attempts");
                    let after_rows = trace.identity_queries % 2 == 0;
                    let mut identity = native_identity();
                    let mut facts = metadata();
                    match scenario {
                        Scenario::ReplaceAfterRows if after_rows => {
                            identity.pane_registration = codec::ReliablePaneRegistrationIdentityV1::from_bytes([0x44; 16]);
                        }
                        Scenario::RestartAfterRows if after_rows => {
                            identity.session_incarnation = mux::MuxSessionIncarnation::from_bytes([0x55; 16]);
                        }
                        Scenario::DurableChangesAfterRows if after_rows => {
                            identity.durable_pane_id = [0x66; 16];
                        }
                        Scenario::PaletteChangesEveryAttempt if after_rows => {
                            facts.palette_generation += 1;
                            facts.palette.foreground = frankenterm_term::color::SrgbaTuple::from((1_u8, 2_u8, 3_u8));
                        }
                        Scenario::PaletteChangesOnce if trace.identity_queries == 2 => {
                            facts.palette_generation += 1;
                            facts.palette.foreground = frankenterm_term::color::SrgbaTuple::from((1_u8, 2_u8, 3_u8));
                        }
                        Scenario::CursorChangesEveryAttempt if after_rows => facts.cursor.x += 1,
                        Scenario::GeometryChangesEveryAttempt if after_rows => facts.dimensions.cols += 1,
                        _ => {}
                    }
                    Pdu::GetPaneIdentityV1Response(codec::GetPaneIdentityV1Response {
                        pane_id: 7,
                        outcome: codec::PaneIdentityOutcomeV1::Available {
                            identity, render_facts: Some(Box::new(facts)),
                        },
                    })
                }
                Pdu::GetPaneRenderStateV1(request) => {
                    assert_eq!(request.pane_id, 7);
                    trace.render_queries += 1;
                    assert!(trace.render_queries <= 6);
                    let mut facts = metadata();
                    if trace.render_queries % 2 == 0 {
                        match scenario {
                            Scenario::CursorChangesEveryAttempt => facts.cursor.x += 1,
                            Scenario::GeometryChangesEveryAttempt => facts.dimensions.cols += 1,
                            _ => {}
                        }
                    }
                    let mut response = test_render_change(7, facts.seqno, "socket-snapshot");
                    response.dimensions = facts.dimensions;
                    response.cursor_position = facts.cursor;
                    response.mouse_grabbed = facts.mouse_grabbed;
                    response.alt_screen_active = facts.alt_screen_active;
                    response.dirty_lines = Vec::new();
                    Pdu::GetPaneRenderChangesResponse(response)
                }
                Pdu::GetLinesAtLayout(request) => {
                    assert_eq!(request.pane_id, 7);
                    assert_eq!(request.layout.seqno, 7);
                    assert_eq!(request.layout.dimensions, metadata().dimensions);
                    assert_eq!(request.lines.len(), 1);
                    let range = request.lines[0].clone();
                    assert!(range.start >= 100 && range.end <= 165);
                    let count = usize::try_from(range.end - range.start).unwrap();
                    assert!(count > 0 && count <= 64);
                    assert!(count * 80 <= wire::MAX_CHUNK_CELLS);
                    trace.row_queries += 1;
                    trace.requested_ranges.push(range.clone());
                    let attempt = trace.identity_queries.div_ceil(2);
                    let mut lines = range.map(|row| {
                        (row, frankenterm_term::Line::from_text(
                            &format!("{attempt}-{:03}", row - 100),
                            &termwiz::cell::CellAttributes::default(), 7, None,
                        ))
                    }).collect::<Vec<_>>();
                    if matches!(scenario, Scenario::MisplacedRow) {
                        // Structurally valid native data, but not the exact
                        // row requested under this layout witness.
                        lines[0].0 += 1000;
                    }
                    Pdu::GetLinesAtLayoutResponse(codec::GetLinesAtLayoutResponse {
                        pane_id: 7, layout: request.layout, lines: lines.into(),
                    })
                }
                Pdu::ListPanes(_) => {
                    trace.listing_queries += 1;
                    empty_list_panes_response()
                }
                other => panic!("snapshot emitted unexpected PDU {}", other.pdu_name()),
            };
            (Vec::new(), Some(reply), Vec::new())
        }).await;
        (dir, path, server, trace)
    }

    fn assert_rows(snapshot: &wire::FrozenSnapshot, generation: usize) {
        assert_eq!(snapshot.scope, snapshot_scope());
        assert_eq!(snapshot.sequence, wire::WireU64(7));
        assert!(snapshot.modes.bracketed_paste);
        assert_eq!(snapshot.title.as_str(), "socket-snapshot");
        assert_eq!((snapshot.dimensions.columns, snapshot.dimensions.rows), (80, 65));
        assert_eq!((snapshot.cursor.column, snapshot.cursor.row), (3, 64));
        assert_eq!(snapshot.lines.len(), 65);
        for (row, line) in snapshot.lines.as_slice().iter().enumerate() {
            assert_eq!(line.stable_row, wire::WireI64(100 + row as i64));
            assert_eq!(line.cells.len(), 80);
            let text: String = line.cells.as_slice().iter().take(5).map(|cell| cell.text.as_str()).collect();
            assert_eq!(text, format!("{generation}-{row:03}"));
        }
    }

    #[test]
    fn native_socket_snapshot_reads_bounded_rows_and_returns_one_exact_screen() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let (_dir, path, server, trace) = fixture(Scenario::Stable).await;
            let mut client = DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(path)).await.unwrap();
            let snapshot = client.get_terminal_snapshot_with_cx(&cx, 7, &native_identity(), snapshot_scope(), wire::WireU64(1)).await.unwrap();
            assert_rows(&snapshot, 1);
            assert!(!client.is_connection_poisoned());
            drop(client);
            timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
            let trace = trace.lock().unwrap();
            assert_eq!((trace.identity_queries, trace.render_queries, trace.row_queries), (2, 2, 3));
            assert_eq!(trace.requested_ranges, vec![100..125, 125..150, 150..165]);
        });
    }

    #[test]
    fn native_socket_snapshot_rejects_registration_or_server_replacement_after_rows() {
        for scenario in [Scenario::ReplaceAfterRows, Scenario::RestartAfterRows, Scenario::DurableChangesAfterRows] {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let (_dir, path, server, trace) = fixture(scenario).await;
                let mut client = DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(path)).await.unwrap();
                let result = client.get_terminal_snapshot_with_cx(&cx, 7, &native_identity(), snapshot_scope(), wire::WireU64(1)).await;
                assert!(matches!(result, Err(TerminalProjectionError::IdentityChanged)));
                drop(client);
                timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
                let trace = trace.lock().unwrap();
                assert_eq!((trace.identity_queries, trace.render_queries, trace.row_queries), (2, 2, 3), "replacement must not be retried as a new source");
            });
        }
    }

    #[test]
    fn native_socket_snapshot_discards_partial_generation_before_retry() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let (_dir, path, server, trace) = fixture(Scenario::PaletteChangesOnce).await;
            let mut client = DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(path)).await.unwrap();
            let snapshot = client.get_terminal_snapshot_with_cx(&cx, 7, &native_identity(), snapshot_scope(), wire::WireU64(1)).await.unwrap();
            assert_rows(&snapshot, 2);
            drop(client);
            timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
            let trace = trace.lock().unwrap();
            assert_eq!((trace.identity_queries, trace.render_queries, trace.row_queries), (4, 4, 6));
        });
    }

    #[test]
    fn native_socket_metadata_churn_at_same_sequence_exhausts_finite_attempts() {
        for scenario in [Scenario::PaletteChangesEveryAttempt, Scenario::CursorChangesEveryAttempt, Scenario::GeometryChangesEveryAttempt] {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let (_dir, path, server, trace) = fixture(scenario).await;
                let mut client = DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(path)).await.unwrap();
                let result = client.get_terminal_snapshot_with_cx(&cx, 7, &native_identity(), snapshot_scope(), wire::WireU64(1)).await;
                assert!(matches!(result, Err(TerminalProjectionError::SnapshotChanged)));
                assert!(!client.is_connection_poisoned());
                client.list_panes_with_cx(&cx).await.unwrap();
                drop(client);
                timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
                let trace = trace.lock().unwrap();
                assert_eq!((trace.identity_queries, trace.render_queries, trace.row_queries), (6, 6, 9));
                assert_eq!(trace.listing_queries, 1, "aligned stream remains usable");
            });
        }
    }

    #[test]
    fn native_socket_snapshot_abandon_or_cancel_retires_borrowed_stream() {
        use std::future::{Future, poll_fn};

        for cancel in [false, true] {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let dir = tempfile::tempdir_in("/tmp").unwrap();
                let path = dir.path().join("snapshot-pending.sock");
                let listener = compat_unix::bind(&path).await.unwrap();
                let (request_seen_tx, request_seen_rx) = crate::runtime_async::oneshot::channel();
                let server = task::spawn(async move {
                    let mut stream = accept_direct_mux_handshake(
                        listener, CODEC_VERSION, CODEC_VERSION_MIN_SUPPORTED,
                    ).await;
                    let mut buffered = StreamingPduBuffer::new();
                    let request = read_test_request_pdu(&mut stream, &mut buffered).await;
                    assert!(matches!(request.pdu, Pdu::GetPaneIdentityV1(request) if request.pane_id == 7));
                    assert!(buffered.is_empty());
                    request_seen_tx.send(()).unwrap();
                    // Deliberately keep the request unanswered. The client must
                    // retire this stream rather than send a later request into
                    // a response whose framing is no longer owned by a future.
                    let mut byte = [0_u8; 1];
                    match timeout(Duration::from_secs(5), unix_stream_read(&mut stream, &mut byte)).await {
                        Ok(Ok(0)) => {}
                        Ok(Err(error)) if is_disconnected_io_kind(error.kind()) => {}
                        other => panic!("abandoned snapshot emitted more bytes or failed to retire: {other:?}"),
                    }
                });
                let mut client = DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(path)).await.unwrap();
                let identity = native_identity();
                {
                    let mut pending = Box::pin(client.get_terminal_snapshot_with_cx(
                        &cx, 7, &identity, snapshot_scope(), wire::WireU64(1),
                    ));
                    let mut observed = Box::pin(crate::runtime_async::oneshot_recv(request_seen_rx));
                    timeout(Duration::from_secs(2), poll_fn(|context| {
                        assert!(pending.as_mut().poll(context).is_pending(), "server has not replied");
                        observed.as_mut().poll(context)
                    })).await.unwrap().unwrap();
                    if cancel {
                        cx.cancel_with(crate::outcome::CancelKind::User, Some("cancel pending native snapshot"));
                        let result = timeout(Duration::from_secs(2), pending).await.unwrap();
                        assert!(matches!(result, Err(TerminalProjectionError::Native(ref error)) if error.is_cancelled()));
                    }
                    // Otherwise the still-pending borrowed future is dropped
                    // here while its caller retains the DirectMuxClient.
                }
                assert!(client.is_connection_poisoned());
                let live_cx = crate::cx::for_testing();
                assert!(matches!(client.list_panes_with_cx(&live_cx).await, Err(DirectMuxError::Disconnected)));
                drop(client);
                timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
            });
        }
    }

    #[test]
    fn native_socket_snapshot_rejects_wrong_rows_without_another_read_attempt() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let (_dir, path, server, trace) = fixture(Scenario::MisplacedRow).await;
            let mut client = DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(path)).await.unwrap();
            assert!(client.get_terminal_snapshot_with_cx(&cx, 7, &native_identity(), snapshot_scope(), wire::WireU64(1)).await.is_err());
            drop(client);
            timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
            let trace = trace.lock().unwrap();
            assert_eq!(trace.identity_queries, 1);
            assert_eq!(trace.row_queries, 1);
        });
    }

    #[test]
    fn native_socket_snapshot_precancelled_or_foreign_scope_never_reads_pane() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let (_dir, path, server, trace) = fixture(Scenario::Stable).await;
            let mut client = DirectMuxClient::connect_with_cx(&cx, direct_mux_client_config(path)).await.unwrap();
            let cancelled = crate::cx::for_testing();
            cancelled.cancel_with(crate::outcome::CancelKind::User, Some("snapshot negative control"));
            let result = client.get_terminal_snapshot_with_cx(&cancelled, 7, &native_identity(), snapshot_scope(), wire::WireU64(1)).await;
            assert!(matches!(result, Err(TerminalProjectionError::Native(ref error)) if error.is_cancelled()));
            let mut foreign = snapshot_scope();
            foreign.identity.pane = wire::Id::new("different-durable-pane").unwrap();
            assert!(matches!(client.get_terminal_snapshot_with_cx(&cx, 7, &native_identity(), foreign, wire::WireU64(1)).await, Err(TerminalProjectionError::IdentityChanged)));
            assert!(!client.is_connection_poisoned());
            client.list_panes_with_cx(&cx).await.unwrap();
            drop(client);
            timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
            let trace = trace.lock().unwrap();
            assert_eq!((trace.identity_queries, trace.render_queries, trace.row_queries), (0, 0, 0));
            assert_eq!(trace.listing_queries, 1);
        });
    }
}
