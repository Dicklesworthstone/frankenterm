use codec::{
    CreateFloatingPane, ErrorResponse, GetClientList, GetCodecVersion, GetCodecVersionResponse,
    GetPaneDirectionResponse, GetTlsCreds, GetTlsCredsResponse, KillPane, ListPanes, PaneFocused,
    PaneRemoved, Pdu, Ping, Pong, RenameWorkspace, SelectStackPane, SendPaste, SetActiveWorkspace,
    SetClipboard, SetFocusedPane, SetLayoutCycle, SetWindowWorkspace, TabTitleChanged,
    UnitResponse, UpdatePaneConstraints, WindowTitleChanged, WindowWorkspaceChanged, WriteToPane,
};
use frankenterm_term::ClipboardSelection;
use mux::tab::FloatingPaneRect;
use proptest::prelude::*;
use std::path::PathBuf;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..128)
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
