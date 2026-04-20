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
}
