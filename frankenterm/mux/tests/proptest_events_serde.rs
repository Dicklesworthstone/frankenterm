use mux::events::{EventType, HandlerPriority};
use proptest::prelude::*;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_event_type() -> impl Strategy<Value = EventType> {
    prop_oneof![
        Just(EventType::UpdateStatus),
        Just(EventType::UserVarChanged),
        Just(EventType::PaneOutput),
        Just(EventType::WindowResized),
        Just(EventType::PaneFocused),
        Just(EventType::PaneAdded),
        Just(EventType::PaneRemoved),
        Just(EventType::ConfigReloaded),
        arb_small_string().prop_map(EventType::Custom),
    ]
}

fn arb_handler_priority() -> impl Strategy<Value = HandlerPriority> {
    prop_oneof![
        Just(HandlerPriority::Native),
        Just(HandlerPriority::Wasm),
        Just(HandlerPriority::Lua),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn event_type_json_roundtrip(value in arb_event_type()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: EventType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn handler_priority_json_roundtrip(value in arb_handler_priority()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: HandlerPriority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn custom_event_type_display_prefixes_name(name in arb_small_string()) {
        let value = EventType::Custom(name.clone());
        prop_assert_eq!(value.to_string(), format!("custom:{name}"));
    }

    #[test]
    fn handler_priority_order_stays_native_then_wasm_then_lua(
        left in arb_handler_priority(),
        right in arb_handler_priority(),
    ) {
        let rank = |value: HandlerPriority| match value {
            HandlerPriority::Native => 0u8,
            HandlerPriority::Wasm => 1u8,
            HandlerPriority::Lua => 2u8,
        };

        prop_assert_eq!(left.cmp(&right), rank(left).cmp(&rank(right)));
    }
}
