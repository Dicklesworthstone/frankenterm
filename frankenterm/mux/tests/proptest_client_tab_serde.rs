use chrono::{TimeZone, Utc};
use mux::client::{ClientId, ClientInfo};
use mux::tab::FloatingPaneRect;
use proptest::prelude::*;
use std::sync::Arc;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_timestamp() -> impl Strategy<Value = chrono::DateTime<Utc>> {
    (0i64..=4_102_444_800).prop_map(|secs| Utc.timestamp_opt(secs, 0).single().unwrap())
}

fn arb_client_id() -> impl Strategy<Value = ClientId> {
    (
        arb_small_string(),
        arb_small_string(),
        any::<u32>(),
        any::<u64>(),
        0usize..=4096,
        prop_oneof![Just(None), arb_small_string().prop_map(Some),],
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

fn arb_client_info() -> impl Strategy<Value = ClientInfo> {
    (
        arb_client_id(),
        arb_timestamp(),
        prop_oneof![Just(None), arb_small_string().prop_map(Some),],
        arb_timestamp(),
        prop_oneof![Just(None), (0usize..=4096).prop_map(Some),],
    )
        .prop_map(
            |(client_id, connected_at, active_workspace, last_input, focused_pane_id)| ClientInfo {
                client_id: Arc::new(client_id),
                connected_at,
                active_workspace,
                last_input,
                focused_pane_id,
            },
        )
}

fn arb_floating_pane_rect() -> impl Strategy<Value = FloatingPaneRect> {
    (0usize..=4096, 0usize..=4096, 0usize..=1024, 0usize..=1024).prop_map(
        |(left, top, width, height)| FloatingPaneRect {
            left,
            top,
            width,
            height,
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn client_id_json_roundtrip(id in arb_client_id()) {
        let json = serde_json::to_string(&id).unwrap();
        let back: ClientId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, id);
    }

    #[test]
    fn client_info_json_roundtrip(info in arb_client_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: ClientInfo = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(back.client_id.as_ref(), info.client_id.as_ref());
        prop_assert_eq!(back.connected_at, info.connected_at);
        prop_assert_eq!(back.active_workspace, info.active_workspace);
        prop_assert_eq!(back.last_input, info.last_input);
        prop_assert_eq!(back.focused_pane_id, info.focused_pane_id);
    }

    #[test]
    fn floating_pane_rect_json_roundtrip(rect in arb_floating_pane_rect()) {
        let json = serde_json::to_string(&rect).unwrap();
        let back: FloatingPaneRect = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, rect);
    }
}
