use codec::{
    CreateFloatingPane, ErrorResponse, GetCodecVersionResponse, GetTlsCredsResponse, Pdu,
    SelectStackPane, SendPaste, SetClipboard, SetLayoutCycle, UpdatePaneConstraints,
};
use frankenterm_term::ClipboardSelection;
use mux::tab::FloatingPaneRect;
use proptest::prelude::*;
use std::path::PathBuf;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
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
}
