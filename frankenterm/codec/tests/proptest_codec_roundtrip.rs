use chrono::TimeZone;
use codec::{
    ActivatePaneDirection, AdjustPaneSize, CreateFloatingPane, CycleStack, EraseScrollbackRequest,
    ErrorResponse, GetClientList, GetClientListResponse, GetCodecVersion, GetCodecVersionResponse,
    GetImageCell, GetImageCellResponse, GetLines, GetLinesResponse, GetPaneDirection,
    GetPaneDirectionResponse, GetPaneRenderChanges, GetPaneRenderChangesResponse,
    GetPaneRenderableDimensions, GetPaneRenderableDimensionsResponse, GetTlsCreds,
    GetTlsCredsResponse, KillPane, ListPanes, LivenessResponse, MoveFloatingPane,
    MovePaneToNewTabResponse, PaneFocused, PaneRemoved, Pdu, Ping, Pong, RemoveFloatingPane,
    RenameWorkspace, Resize, SearchScrollbackRequest, SearchScrollbackResponse, SelectStackPane,
    SendKeyDown, SendKeyUp, SendMouseEvent, SendPaste, SerializedLines, SetActiveWorkspace,
    SetClientId, SetClipboard, SetFloatingPaneZ, SetFocusedPane, SetLayoutCycle, SetPaneZoomed,
    SetWindowWorkspace, SpawnResponse, SplitPane, SwapToLayout, TabAddedToWindow, TabResized,
    TabTitleChanged, ToggleFloatingPane, UnitResponse, UpdatePaneConstraints, WindowTitleChanged,
    WindowWorkspaceChanged, WriteToPane,
};
use config::keyassignment::{PaneDirection, ScrollbackEraseMode, SpawnTabDomain};
use frankenterm_term::{ClipboardSelection, TerminalSize};
use mux::client::{ClientId, ClientInfo};
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions, StableCursorPosition};
use mux::tab::{FloatingPaneRect, SplitDirection, SplitRequest, SplitSize};
use proptest::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::image::ImageData;
use termwiz::input::{KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..128)
}

fn arb_terminal_size() -> impl Strategy<Value = TerminalSize> {
    (
        0usize..=512,
        0usize..=512,
        0usize..=8192,
        0usize..=8192,
        0u32..=960,
    )
        .prop_map(
            |(rows, cols, pixel_width, pixel_height, dpi)| TerminalSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
                dpi,
            },
        )
}

fn arb_path_buf() -> impl Strategy<Value = PathBuf> {
    proptest::collection::vec("[a-zA-Z0-9._-]{1,12}", 1..=4).prop_map(|segments| {
        let joined = format!("/{}", segments.join("/"));
        PathBuf::from(joined)
    })
}

fn arb_clipboard_selection() -> impl Strategy<Value = ClipboardSelection> {
    prop_oneof![
        Just(ClipboardSelection::Clipboard),
        Just(ClipboardSelection::PrimarySelection),
    ]
}

fn arb_optional_usize() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![Just(None), (0usize..=4096).prop_map(Some),]
}

fn arb_floating_rect() -> impl Strategy<Value = FloatingPaneRect> {
    (0usize..=4096, 0usize..=4096, 1usize..=512, 1usize..=512).prop_map(
        |(left, top, width, height)| FloatingPaneRect {
            left,
            top,
            width,
            height,
        },
    )
}

fn arb_create_floating_pane() -> impl Strategy<Value = CreateFloatingPane> {
    (0u64..=4096, 0u64..=4096, arb_floating_rect()).prop_map(|(tab_id, pane_id, rect)| {
        CreateFloatingPane {
            tab_id,
            pane_id,
            rect,
        }
    })
}

fn arb_set_clipboard() -> impl Strategy<Value = SetClipboard> {
    (
        0u64..=4096,
        prop_oneof![Just(None), arb_small_string().prop_map(Some),],
        arb_clipboard_selection(),
    )
        .prop_map(|(pane_id, clipboard, selection)| SetClipboard {
            pane_id,
            clipboard,
            selection,
        })
}

fn arb_update_pane_constraints() -> impl Strategy<Value = UpdatePaneConstraints> {
    (
        0u64..=4096,
        arb_optional_usize(),
        arb_optional_usize(),
        arb_optional_usize(),
        arb_optional_usize(),
    )
        .prop_map(|(pane_id, min_width, max_width, min_height, max_height)| {
            UpdatePaneConstraints {
                pane_id,
                min_width,
                max_width,
                min_height,
                max_height,
            }
        })
}

fn arb_layout_names() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(arb_small_string(), 0..8)
}

fn arb_send_paste() -> impl Strategy<Value = SendPaste> {
    (0u64..=4096, arb_small_string()).prop_map(|(pane_id, data)| SendPaste { pane_id, data })
}

fn arb_set_layout_cycle() -> impl Strategy<Value = SetLayoutCycle> {
    (0u64..=4096, arb_layout_names()).prop_map(|(tab_id, layout_names)| SetLayoutCycle {
        tab_id,
        layout_names,
    })
}

fn arb_swap_to_layout() -> impl Strategy<Value = SwapToLayout> {
    (0u64..=4096, 0usize..=128).prop_map(|(tab_id, layout_index)| SwapToLayout {
        tab_id,
        layout_index,
    })
}

fn arb_cycle_stack() -> impl Strategy<Value = CycleStack> {
    (0u64..=4096, 0usize..=128, any::<bool>()).prop_map(|(tab_id, slot_index, forward)| {
        CycleStack {
            tab_id,
            slot_index,
            forward,
        }
    })
}

fn arb_select_stack_pane() -> impl Strategy<Value = SelectStackPane> {
    (0u64..=4096, 0usize..=128, 0usize..=128).prop_map(|(tab_id, slot_index, pane_index)| {
        SelectStackPane {
            tab_id,
            slot_index,
            pane_index,
        }
    })
}

fn arb_error_response() -> impl Strategy<Value = ErrorResponse> {
    arb_small_string().prop_map(|reason| ErrorResponse { reason })
}

fn arb_get_codec_version_response() -> impl Strategy<Value = GetCodecVersionResponse> {
    (
        0usize..=4096,
        arb_small_string(),
        arb_path_buf(),
        prop_oneof![Just(None), arb_path_buf().prop_map(Some),],
    )
        .prop_map(
            |(codec_vers, version_string, executable_path, config_file_path)| {
                GetCodecVersionResponse {
                    codec_vers,
                    version_string,
                    executable_path,
                    config_file_path,
                }
            },
        )
}

fn arb_get_tls_creds_response() -> impl Strategy<Value = GetTlsCredsResponse> {
    (arb_small_string(), arb_small_string()).prop_map(|(ca_cert_pem, client_cert_pem)| {
        GetTlsCredsResponse {
            ca_cert_pem,
            client_cert_pem,
        }
    })
}

fn arb_write_to_pane() -> impl Strategy<Value = WriteToPane> {
    (0u64..=4096, arb_small_bytes()).prop_map(|(pane_id, data)| WriteToPane { pane_id, data })
}

fn arb_pane_removed() -> impl Strategy<Value = PaneRemoved> {
    (0u64..=4096).prop_map(|pane_id| PaneRemoved { pane_id })
}

fn arb_kill_pane() -> impl Strategy<Value = KillPane> {
    (0u64..=4096).prop_map(|pane_id| KillPane { pane_id })
}

fn arb_set_focused_pane() -> impl Strategy<Value = SetFocusedPane> {
    (0u64..=4096).prop_map(|pane_id| SetFocusedPane { pane_id })
}

fn arb_get_pane_direction_response() -> impl Strategy<Value = GetPaneDirectionResponse> {
    prop_oneof![
        Just(GetPaneDirectionResponse { pane_id: None }),
        (0u64..=4096).prop_map(|pane_id| GetPaneDirectionResponse {
            pane_id: Some(pane_id),
        }),
    ]
}

fn arb_pane_focused() -> impl Strategy<Value = PaneFocused> {
    (0u64..=4096).prop_map(|pane_id| PaneFocused { pane_id })
}

fn arb_window_workspace_changed() -> impl Strategy<Value = WindowWorkspaceChanged> {
    (0u64..=4096, arb_small_string()).prop_map(|(window_id, workspace)| WindowWorkspaceChanged {
        window_id,
        workspace,
    })
}

fn arb_client_id() -> impl Strategy<Value = ClientId> {
    (
        arb_small_string(),
        arb_small_string(),
        any::<u32>(),
        any::<u64>(),
        0usize..=4096,
        prop_oneof![Just(None), arb_small_string().prop_map(Some)],
    )
        .prop_map(
            |(hostname, username, pid, epoch, id, ssh_auth_sock)| ClientId {
                hostname,
                username,
                pid,
                epoch,
                id,
                ssh_auth_sock,
            },
        )
}

fn arb_set_client_id() -> impl Strategy<Value = SetClientId> {
    (arb_client_id(), any::<bool>()).prop_map(|(client_id, is_proxy)| SetClientId {
        client_id,
        is_proxy,
    })
}

fn arb_client_info() -> impl Strategy<Value = ClientInfo> {
    (
        arb_client_id(),
        0i64..=4_102_444_800,
        prop::option::of(arb_small_string()),
        0i64..=4_102_444_800,
        prop::option::of(0u64..=4096),
    )
        .prop_map(
            |(client_id, connected_at, active_workspace, last_input, focused_pane_id)| ClientInfo {
                client_id: Arc::new(client_id),
                connected_at: chrono::Utc.timestamp_opt(connected_at, 0).unwrap(),
                active_workspace,
                last_input: chrono::Utc.timestamp_opt(last_input, 0).unwrap(),
                focused_pane_id,
            },
        )
}

fn arb_get_client_list_response() -> impl Strategy<Value = GetClientListResponse> {
    proptest::collection::vec(arb_client_info(), 0..=8)
        .prop_map(|clients| GetClientListResponse { clients })
}

fn arb_move_pane_to_new_tab_response() -> impl Strategy<Value = MovePaneToNewTabResponse> {
    (0u64..=4096, 0u64..=4096)
        .prop_map(|(tab_id, window_id)| MovePaneToNewTabResponse { tab_id, window_id })
}

fn arb_spawn_response() -> impl Strategy<Value = SpawnResponse> {
    (0u64..=4096, 0u64..=4096, 0u64..=4096, arb_terminal_size()).prop_map(
        |(tab_id, pane_id, window_id, size)| SpawnResponse {
            tab_id,
            pane_id,
            window_id,
            size,
        },
    )
}

fn arb_spawn_tab_domain() -> impl Strategy<Value = SpawnTabDomain> {
    prop_oneof![
        Just(SpawnTabDomain::DefaultDomain),
        Just(SpawnTabDomain::CurrentPaneDomain),
        arb_small_string().prop_map(SpawnTabDomain::DomainName),
        (0usize..=1024).prop_map(SpawnTabDomain::DomainId),
    ]
}

fn arb_split_size() -> impl Strategy<Value = SplitSize> {
    prop_oneof![
        (0usize..=256).prop_map(SplitSize::Cells),
        (0u8..=100).prop_map(SplitSize::Percent),
    ]
}

fn arb_split_request() -> impl Strategy<Value = SplitRequest> {
    (
        prop_oneof![
            Just(SplitDirection::Horizontal),
            Just(SplitDirection::Vertical),
        ],
        any::<bool>(),
        any::<bool>(),
        arb_split_size(),
    )
        .prop_map(
            |(direction, target_is_second, top_level, size)| SplitRequest {
                direction,
                target_is_second,
                top_level,
                size,
            },
        )
}

fn arb_split_pane() -> impl Strategy<Value = SplitPane> {
    (
        0u64..=4096,
        arb_split_request(),
        prop::option::of(arb_small_string()),
        arb_spawn_tab_domain(),
        prop::option::of(0u64..=4096),
    )
        .prop_map(
            |(pane_id, split_request, command_dir, domain, move_pane_id)| SplitPane {
                pane_id,
                split_request,
                command: None,
                command_dir,
                domain,
                move_pane_id,
            },
        )
}

fn arb_key_code() -> impl Strategy<Value = KeyCode> {
    prop_oneof![
        Just(KeyCode::Enter),
        Just(KeyCode::Escape),
        Just(KeyCode::Tab),
        Just(KeyCode::LeftArrow),
        Just(KeyCode::RightArrow),
        any::<char>().prop_map(KeyCode::Char),
    ]
}

fn arb_modifiers() -> impl Strategy<Value = Modifiers> {
    prop_oneof![
        Just(Modifiers::NONE),
        Just(Modifiers::SHIFT),
        Just(Modifiers::ALT),
        Just(Modifiers::CTRL),
        Just(Modifiers::SHIFT | Modifiers::ALT),
        Just(Modifiers::SHIFT | Modifiers::CTRL),
    ]
}

fn arb_key_event() -> impl Strategy<Value = KeyEvent> {
    (arb_key_code(), arb_modifiers()).prop_map(|(key, modifiers)| KeyEvent { key, modifiers })
}

fn arb_send_key_down() -> impl Strategy<Value = SendKeyDown> {
    (0u64..=4096, arb_key_event(), any::<u64>()).prop_map(|(pane_id, event, millis)| SendKeyDown {
        pane_id,
        event,
        input_serial: (std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)).into(),
    })
}

fn arb_send_key_up() -> impl Strategy<Value = SendKeyUp> {
    (0u64..=4096, arb_key_event()).prop_map(|(pane_id, event)| SendKeyUp { pane_id, event })
}

fn arb_mouse_buttons() -> impl Strategy<Value = MouseButtons> {
    prop_oneof![
        Just(MouseButtons::NONE),
        Just(MouseButtons::LEFT),
        Just(MouseButtons::RIGHT),
        Just(MouseButtons::MIDDLE),
        Just(MouseButtons::LEFT | MouseButtons::RIGHT),
        Just(MouseButtons::VERT_WHEEL),
        Just(MouseButtons::VERT_WHEEL | MouseButtons::WHEEL_POSITIVE),
    ]
}

fn arb_mouse_event() -> impl Strategy<Value = MouseEvent> {
    (
        any::<u16>(),
        any::<u16>(),
        arb_mouse_buttons(),
        arb_modifiers(),
    )
        .prop_map(|(x, y, mouse_buttons, modifiers)| MouseEvent {
            x,
            y,
            mouse_buttons,
            modifiers,
        })
}

fn arb_send_mouse_event() -> impl Strategy<Value = SendMouseEvent> {
    (0u64..=4096, arb_mouse_event()).prop_map(|(pane_id, event)| SendMouseEvent { pane_id, event })
}

fn arb_liveness_response() -> impl Strategy<Value = LivenessResponse> {
    (0u64..=4096, any::<bool>())
        .prop_map(|(pane_id, is_alive)| LivenessResponse { pane_id, is_alive })
}

fn arb_resize() -> impl Strategy<Value = Resize> {
    (0u64..=4096, 0u64..=4096, arb_terminal_size()).prop_map(
        |(containing_tab_id, pane_id, size)| Resize {
            containing_tab_id,
            pane_id,
            size,
        },
    )
}

fn arb_set_pane_zoomed() -> impl Strategy<Value = SetPaneZoomed> {
    (0u64..=4096, 0u64..=4096, any::<bool>()).prop_map(|(containing_tab_id, pane_id, zoomed)| {
        SetPaneZoomed {
            containing_tab_id,
            pane_id,
            zoomed,
        }
    })
}

fn arb_pane_direction() -> impl Strategy<Value = PaneDirection> {
    prop_oneof![
        Just(PaneDirection::Up),
        Just(PaneDirection::Down),
        Just(PaneDirection::Left),
        Just(PaneDirection::Right),
        Just(PaneDirection::Next),
        Just(PaneDirection::Prev),
    ]
}

fn arb_get_pane_direction() -> impl Strategy<Value = GetPaneDirection> {
    (0u64..=4096, arb_pane_direction())
        .prop_map(|(pane_id, direction)| GetPaneDirection { pane_id, direction })
}

fn arb_adjust_pane_size() -> impl Strategy<Value = AdjustPaneSize> {
    (0u64..=4096, arb_pane_direction(), 0usize..=256).prop_map(|(pane_id, direction, amount)| {
        AdjustPaneSize {
            pane_id,
            direction,
            amount,
        }
    })
}

fn arb_activate_pane_direction() -> impl Strategy<Value = ActivatePaneDirection> {
    (0u64..=4096, arb_pane_direction())
        .prop_map(|(pane_id, direction)| ActivatePaneDirection { pane_id, direction })
}

fn arb_get_pane_render_changes() -> impl Strategy<Value = GetPaneRenderChanges> {
    (0u64..=4096).prop_map(|pane_id| GetPaneRenderChanges { pane_id })
}

fn arb_get_pane_renderable_dimensions() -> impl Strategy<Value = GetPaneRenderableDimensions> {
    (0u64..=4096).prop_map(|pane_id| GetPaneRenderableDimensions { pane_id })
}

fn arb_get_pane_renderable_dimensions_response()
-> impl Strategy<Value = GetPaneRenderableDimensionsResponse> {
    (
        0u64..=4096,
        any::<bool>(),
        any::<bool>(),
        0usize..=4096,
        0usize..=4096,
        0usize..=4096,
        0u32..=960,
    )
        .prop_map(
            |(
                pane_id,
                reverse_video,
                tiering_enabled,
                cols,
                viewport_rows,
                scrollback_rows,
                dpi,
            )| {
                let cursor_position = StableCursorPosition::default();
                let dimensions = RenderableDimensions {
                    cols,
                    viewport_rows,
                    scrollback_rows,
                    dpi,
                    reverse_video,
                    ..RenderableDimensions::default()
                };
                let tiered_scrollback_status = Some(PaneTieredScrollbackStatus {
                    tiering_enabled,
                    ..PaneTieredScrollbackStatus::default()
                });

                GetPaneRenderableDimensionsResponse {
                    pane_id,
                    cursor_position,
                    dimensions,
                    tiered_scrollback_status,
                }
            },
        )
}

fn arb_rename_workspace() -> impl Strategy<Value = RenameWorkspace> {
    (arb_small_string(), arb_small_string()).prop_map(|(old_workspace, new_workspace)| {
        RenameWorkspace {
            old_workspace,
            new_workspace,
        }
    })
}

fn arb_set_window_workspace() -> impl Strategy<Value = SetWindowWorkspace> {
    (0u64..=4096, arb_small_string()).prop_map(|(window_id, workspace)| SetWindowWorkspace {
        window_id,
        workspace,
    })
}

fn arb_set_active_workspace() -> impl Strategy<Value = SetActiveWorkspace> {
    arb_small_string().prop_map(|workspace| SetActiveWorkspace { workspace })
}

fn arb_tab_title_changed() -> impl Strategy<Value = TabTitleChanged> {
    (0u64..=4096, arb_small_string()).prop_map(|(tab_id, title)| TabTitleChanged { tab_id, title })
}

fn arb_window_title_changed() -> impl Strategy<Value = WindowTitleChanged> {
    (0u64..=4096, arb_small_string())
        .prop_map(|(window_id, title)| WindowTitleChanged { window_id, title })
}

fn arb_tab_added_to_window() -> impl Strategy<Value = TabAddedToWindow> {
    (0u64..=4096, 0u64..=4096)
        .prop_map(|(tab_id, window_id)| TabAddedToWindow { tab_id, window_id })
}

fn arb_tab_resized() -> impl Strategy<Value = TabResized> {
    (0u64..=4096).prop_map(|tab_id| TabResized { tab_id })
}

fn arb_set_floating_pane_z() -> impl Strategy<Value = SetFloatingPaneZ> {
    (0u64..=4096, any::<u32>()).prop_map(|(pane_id, z_order)| SetFloatingPaneZ { pane_id, z_order })
}

fn arb_toggle_floating_pane() -> impl Strategy<Value = ToggleFloatingPane> {
    (0u64..=4096, any::<bool>())
        .prop_map(|(pane_id, visible)| ToggleFloatingPane { pane_id, visible })
}

fn arb_move_floating_pane() -> impl Strategy<Value = MoveFloatingPane> {
    (0u64..=4096, arb_floating_rect())
        .prop_map(|(pane_id, rect)| MoveFloatingPane { pane_id, rect })
}

fn arb_remove_floating_pane() -> impl Strategy<Value = RemoveFloatingPane> {
    (0u64..=4096).prop_map(|pane_id| RemoveFloatingPane { pane_id })
}

fn arb_pattern() -> impl Strategy<Value = mux::pane::Pattern> {
    prop_oneof![
        arb_small_string().prop_map(mux::pane::Pattern::CaseSensitiveString),
        arb_small_string().prop_map(mux::pane::Pattern::CaseInSensitiveString),
        arb_small_string().prop_map(mux::pane::Pattern::Regex),
    ]
}

fn arb_scrollback_erase_mode() -> impl Strategy<Value = ScrollbackEraseMode> {
    prop_oneof![
        Just(ScrollbackEraseMode::ScrollbackOnly),
        Just(ScrollbackEraseMode::ScrollbackAndViewport),
    ]
}

fn arb_erase_scrollback_request() -> impl Strategy<Value = EraseScrollbackRequest> {
    (0u64..=4096, arb_scrollback_erase_mode()).prop_map(|(pane_id, erase_mode)| {
        EraseScrollbackRequest {
            pane_id,
            erase_mode,
        }
    })
}

fn arb_search_scrollback_request() -> impl Strategy<Value = SearchScrollbackRequest> {
    (
        0u64..=4096,
        arb_pattern(),
        -100_000isize..=100_000isize,
        -100_000isize..=100_000isize,
        prop_oneof![Just(None), any::<u32>().prop_map(Some)],
    )
        .prop_map(|(pane_id, pattern, a, b, limit)| SearchScrollbackRequest {
            pane_id,
            pattern,
            range: a.min(b)..a.max(b),
            limit,
        })
}

fn arb_search_result() -> impl Strategy<Value = mux::pane::SearchResult> {
    (
        any::<i64>(),
        0usize..=1024,
        any::<i64>(),
        0usize..=1024,
        0usize..=1024,
    )
        .prop_map(
            |(start_y, start_x, end_y, end_x, match_id)| mux::pane::SearchResult {
                start_y,
                start_x,
                end_y,
                end_x,
                match_id,
            },
        )
}

fn arb_search_scrollback_response() -> impl Strategy<Value = SearchScrollbackResponse> {
    proptest::collection::vec(arb_search_result(), 0..=16)
        .prop_map(|results| SearchScrollbackResponse { results })
}

fn arb_get_lines() -> impl Strategy<Value = GetLines> {
    (
        0u64..=4096,
        proptest::collection::vec(
            (-100_000isize..=100_000isize, -100_000isize..=100_000isize),
            0..=16,
        ),
    )
        .prop_map(|(pane_id, lines)| GetLines {
            pane_id,
            lines: lines.into_iter().map(|(a, b)| a.min(b)..a.max(b)).collect(),
        })
}

fn arb_get_image_cell() -> impl Strategy<Value = GetImageCell> {
    (
        0u64..=4096,
        any::<i64>(),
        0usize..=1024,
        prop::array::uniform32(any::<u8>()),
    )
        .prop_map(|(pane_id, line_idx, cell_idx, data_hash)| GetImageCell {
            pane_id,
            line_idx,
            cell_idx,
            data_hash,
        })
}

fn arb_get_image_cell_response() -> impl Strategy<Value = GetImageCellResponse> {
    (
        0u64..=4096,
        prop::option::of(proptest::collection::vec(any::<u8>(), 0..=32)),
    )
        .prop_map(|(pane_id, data)| GetImageCellResponse {
            pane_id,
            data: data.map(|bytes| Arc::new(ImageData::with_raw_data(bytes))),
        })
}

fn arb_get_lines_response() -> impl Strategy<Value = GetLinesResponse> {
    (0u64..=4096).prop_map(|pane_id| GetLinesResponse {
        pane_id,
        lines: SerializedLines::default(),
    })
}

fn arb_get_pane_render_changes_response() -> impl Strategy<Value = GetPaneRenderChangesResponse> {
    (
        0u64..=4096,
        any::<bool>(),
        any::<bool>(),
        arb_small_string(),
        any::<usize>(),
    )
        .prop_map(
            |(pane_id, mouse_grabbed, alt_screen_active, title, seqno)| {
                GetPaneRenderChangesResponse {
                    pane_id,
                    mouse_grabbed,
                    alt_screen_active,
                    cursor_position: StableCursorPosition::default(),
                    dimensions: RenderableDimensions::default(),
                    tiered_scrollback_status: None,
                    dirty_lines: vec![],
                    title,
                    working_dir: None,
                    bonus_lines: SerializedLines::default(),
                    input_serial: None,
                    seqno,
                }
            },
        )
}

fn assert_pdu_roundtrip(serial: u64, pdu: Pdu) {
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, serial).unwrap();

    let decoded = Pdu::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded.serial, serial);
    assert_eq!(decoded.pdu, pdu);

    let mut streaming = encoded.clone();
    let streamed = Pdu::stream_decode(&mut streaming).unwrap().unwrap();
    assert_eq!(streamed.serial, serial);
    assert_eq!(streamed.pdu, pdu);
    assert!(streaming.is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn create_floating_pane_json_and_pdu_roundtrip(
        payload in arb_create_floating_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: CreateFloatingPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::CreateFloatingPane(payload));
    }

    #[test]
    fn set_clipboard_json_and_pdu_roundtrip(
        payload in arb_set_clipboard(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetClipboard = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetClipboard(payload));
    }

    #[test]
    fn update_pane_constraints_json_and_pdu_roundtrip(
        payload in arb_update_pane_constraints(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: UpdatePaneConstraints = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::UpdatePaneConstraints(payload));
    }

    #[test]
    fn send_paste_json_and_pdu_roundtrip(
        payload in arb_send_paste(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SendPaste = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SendPaste(payload));
    }

    #[test]
    fn set_layout_cycle_json_and_pdu_roundtrip(
        payload in arb_set_layout_cycle(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetLayoutCycle = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetLayoutCycle(payload));
    }

    #[test]
    fn swap_to_layout_json_and_pdu_roundtrip(
        payload in arb_swap_to_layout(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SwapToLayout = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SwapToLayout(payload));
    }

    #[test]
    fn cycle_stack_json_and_pdu_roundtrip(
        payload in arb_cycle_stack(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: CycleStack = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::CycleStack(payload));
    }

    #[test]
    fn select_stack_pane_json_and_pdu_roundtrip(
        payload in arb_select_stack_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SelectStackPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SelectStackPane(payload));
    }

    #[test]
    fn error_response_json_and_pdu_roundtrip(
        payload in arb_error_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: ErrorResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::ErrorResponse(payload));
    }

    #[test]
    fn get_codec_version_response_json_and_pdu_roundtrip(
        payload in arb_get_codec_version_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetCodecVersionResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetCodecVersionResponse(payload));
    }

    #[test]
    fn get_tls_creds_response_json_and_pdu_roundtrip(
        payload in arb_get_tls_creds_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetTlsCredsResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetTlsCredsResponse(payload));
    }

    #[test]
    fn unit_response_pdu_roundtrip_preserves_serial(serial in any::<u64>()) {
        assert_pdu_roundtrip(serial, Pdu::UnitResponse(UnitResponse {}));
    }

    #[test]
    fn get_codec_version_request_pdu_roundtrip_preserves_serial(serial in any::<u64>()) {
        assert_pdu_roundtrip(serial, Pdu::GetCodecVersion(GetCodecVersion {}));
    }

    #[test]
    fn get_tls_creds_request_pdu_roundtrip_preserves_serial(serial in any::<u64>()) {
        assert_pdu_roundtrip(serial, Pdu::GetTlsCreds(GetTlsCreds {}));
    }

    #[test]
    fn ping_request_pdu_roundtrip_preserves_serial(serial in any::<u64>()) {
        assert_pdu_roundtrip(serial, Pdu::Ping(Ping {}));
    }

    #[test]
    fn pong_response_pdu_roundtrip_preserves_serial(serial in any::<u64>()) {
        assert_pdu_roundtrip(serial, Pdu::Pong(Pong {}));
    }

    #[test]
    fn list_panes_request_pdu_roundtrip_preserves_serial(serial in any::<u64>()) {
        assert_pdu_roundtrip(serial, Pdu::ListPanes(ListPanes {}));
    }

    #[test]
    fn get_client_list_request_pdu_roundtrip_preserves_serial(serial in any::<u64>()) {
        assert_pdu_roundtrip(serial, Pdu::GetClientList(GetClientList {}));
    }

    #[test]
    fn get_client_list_response_json_and_pdu_roundtrip(
        payload in arb_get_client_list_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetClientListResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetClientListResponse(payload));
    }

    #[test]
    fn move_pane_to_new_tab_response_json_and_pdu_roundtrip(
        payload in arb_move_pane_to_new_tab_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: MovePaneToNewTabResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::MovePaneToNewTabResponse(payload));
    }

    #[test]
    fn spawn_response_json_and_pdu_roundtrip(
        payload in arb_spawn_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SpawnResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SpawnResponse(payload));
    }

    #[test]
    fn send_key_down_json_and_pdu_roundtrip(
        payload in arb_send_key_down(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SendKeyDown = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SendKeyDown(payload));
    }

    #[test]
    fn send_key_up_json_and_pdu_roundtrip(
        payload in arb_send_key_up(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SendKeyUp = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SendKeyUp(payload));
    }

    #[test]
    fn send_mouse_event_json_and_pdu_roundtrip(
        payload in arb_send_mouse_event(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SendMouseEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SendMouseEvent(payload));
    }

    #[test]
    fn split_pane_json_and_pdu_roundtrip(
        payload in arb_split_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SplitPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SplitPane(payload));
    }

    #[test]
    fn pane_removed_json_and_pdu_roundtrip(
        payload in arb_pane_removed(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: PaneRemoved = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::PaneRemoved(payload));
    }

    #[test]
    fn kill_pane_json_and_pdu_roundtrip(
        payload in arb_kill_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: KillPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::KillPane(payload));
    }

    #[test]
    fn set_focused_pane_json_and_pdu_roundtrip(
        payload in arb_set_focused_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetFocusedPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetFocusedPane(payload));
    }

    #[test]
    fn get_pane_direction_response_json_and_pdu_roundtrip(
        payload in arb_get_pane_direction_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetPaneDirectionResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetPaneDirectionResponse(payload));
    }

    #[test]
    fn pane_focused_json_and_pdu_roundtrip(
        payload in arb_pane_focused(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: PaneFocused = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::PaneFocused(payload));
    }

    #[test]
    fn window_workspace_changed_json_and_pdu_roundtrip(
        payload in arb_window_workspace_changed(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: WindowWorkspaceChanged = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::WindowWorkspaceChanged(payload));
    }

    #[test]
    fn set_client_id_json_and_pdu_roundtrip(
        payload in arb_set_client_id(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetClientId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetClientId(payload));
    }

    #[test]
    fn liveness_response_json_and_pdu_roundtrip(
        payload in arb_liveness_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: LivenessResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::LivenessResponse(payload));
    }

    #[test]
    fn resize_json_and_pdu_roundtrip(
        payload in arb_resize(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: Resize = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::Resize(payload));
    }

    #[test]
    fn set_pane_zoomed_json_and_pdu_roundtrip(
        payload in arb_set_pane_zoomed(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetPaneZoomed = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetPaneZoomed(payload));
    }

    #[test]
    fn tab_added_to_window_json_and_pdu_roundtrip(
        payload in arb_tab_added_to_window(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: TabAddedToWindow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::TabAddedToWindow(payload));
    }

    #[test]
    fn tab_resized_json_and_pdu_roundtrip(
        payload in arb_tab_resized(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: TabResized = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::TabResized(payload));
    }

    #[test]
    fn get_pane_direction_json_and_pdu_roundtrip(
        payload in arb_get_pane_direction(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetPaneDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetPaneDirection(payload));
    }

    #[test]
    fn adjust_pane_size_json_and_pdu_roundtrip(
        payload in arb_adjust_pane_size(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: AdjustPaneSize = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::AdjustPaneSize(payload));
    }

    #[test]
    fn set_floating_pane_z_json_and_pdu_roundtrip(
        payload in arb_set_floating_pane_z(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetFloatingPaneZ = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetFloatingPaneZ(payload));
    }

    #[test]
    fn toggle_floating_pane_json_and_pdu_roundtrip(
        payload in arb_toggle_floating_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: ToggleFloatingPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::ToggleFloatingPane(payload));
    }

    #[test]
    fn move_floating_pane_json_and_pdu_roundtrip(
        payload in arb_move_floating_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: MoveFloatingPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::MoveFloatingPane(payload));
    }

    #[test]
    fn remove_floating_pane_json_and_pdu_roundtrip(
        payload in arb_remove_floating_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: RemoveFloatingPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::RemoveFloatingPane(payload));
    }

    #[test]
    fn activate_pane_direction_json_and_pdu_roundtrip(
        payload in arb_activate_pane_direction(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: ActivatePaneDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::ActivatePaneDirection(payload));
    }

    #[test]
    fn get_pane_render_changes_json_and_pdu_roundtrip(
        payload in arb_get_pane_render_changes(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetPaneRenderChanges = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetPaneRenderChanges(payload));
    }

    #[test]
    fn get_pane_renderable_dimensions_json_and_pdu_roundtrip(
        payload in arb_get_pane_renderable_dimensions(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetPaneRenderableDimensions = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetPaneRenderableDimensions(payload));
    }

    #[test]
    fn get_pane_renderable_dimensions_response_json_and_pdu_roundtrip(
        payload in arb_get_pane_renderable_dimensions_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetPaneRenderableDimensionsResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetPaneRenderableDimensionsResponse(payload));
    }

    #[test]
    fn erase_scrollback_request_json_and_pdu_roundtrip(
        payload in arb_erase_scrollback_request(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: EraseScrollbackRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::EraseScrollbackRequest(payload));
    }

    #[test]
    fn search_scrollback_request_json_and_pdu_roundtrip(
        payload in arb_search_scrollback_request(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SearchScrollbackRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SearchScrollbackRequest(payload));
    }

    #[test]
    fn search_scrollback_response_json_and_pdu_roundtrip(
        payload in arb_search_scrollback_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SearchScrollbackResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SearchScrollbackResponse(payload));
    }

    #[test]
    fn get_lines_json_and_pdu_roundtrip(
        payload in arb_get_lines(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetLines = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetLines(payload));
    }

    #[test]
    fn get_image_cell_json_and_pdu_roundtrip(
        payload in arb_get_image_cell(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetImageCell = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetImageCell(payload));
    }

    #[test]
    fn get_image_cell_response_json_and_pdu_roundtrip(
        payload in arb_get_image_cell_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetImageCellResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetImageCellResponse(payload));
    }

    #[test]
    fn get_lines_response_json_and_pdu_roundtrip(
        payload in arb_get_lines_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetLinesResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetLinesResponse(payload));
    }

    #[test]
    fn get_pane_render_changes_response_json_and_pdu_roundtrip(
        payload in arb_get_pane_render_changes_response(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: GetPaneRenderChangesResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::GetPaneRenderChangesResponse(payload));
    }

    #[test]
    fn write_to_pane_json_and_pdu_roundtrip(
        payload in arb_write_to_pane(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: WriteToPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::WriteToPane(payload));
    }

    #[test]
    fn rename_workspace_json_and_pdu_roundtrip(
        payload in arb_rename_workspace(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: RenameWorkspace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::RenameWorkspace(payload));
    }

    #[test]
    fn set_window_workspace_json_and_pdu_roundtrip(
        payload in arb_set_window_workspace(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetWindowWorkspace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetWindowWorkspace(payload));
    }

    #[test]
    fn set_active_workspace_json_and_pdu_roundtrip(
        payload in arb_set_active_workspace(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SetActiveWorkspace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::SetActiveWorkspace(payload));
    }

    #[test]
    fn tab_title_changed_json_and_pdu_roundtrip(
        payload in arb_tab_title_changed(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: TabTitleChanged = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::TabTitleChanged(payload));
    }

    #[test]
    fn window_title_changed_json_and_pdu_roundtrip(
        payload in arb_window_title_changed(),
        serial in any::<u64>(),
    ) {
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: WindowTitleChanged = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload);

        assert_pdu_roundtrip(serial, Pdu::WindowTitleChanged(payload));
    }
}
