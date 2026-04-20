use config::keyassignment::{PaneDirection, ScrollbackEraseMode, SpawnTabDomain};
use proptest::prelude::*;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_spawn_tab_domain() -> impl Strategy<Value = SpawnTabDomain> {
    prop_oneof![
        Just(SpawnTabDomain::DefaultDomain),
        Just(SpawnTabDomain::CurrentPaneDomain),
        arb_small_string().prop_map(SpawnTabDomain::DomainName),
        (0usize..=4096).prop_map(SpawnTabDomain::DomainId),
    ]
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

fn arb_scrollback_erase_mode() -> impl Strategy<Value = ScrollbackEraseMode> {
    prop_oneof![
        Just(ScrollbackEraseMode::ScrollbackOnly),
        Just(ScrollbackEraseMode::ScrollbackAndViewport),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn spawn_tab_domain_json_roundtrip(value in arb_spawn_tab_domain()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SpawnTabDomain = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn pane_direction_json_roundtrip(value in arb_pane_direction()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PaneDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn scrollback_erase_mode_json_roundtrip(value in arb_scrollback_erase_mode()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: ScrollbackEraseMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
