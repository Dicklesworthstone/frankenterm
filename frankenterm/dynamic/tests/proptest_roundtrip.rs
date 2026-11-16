use frankenterm_dynamic::{FromDynamic, ToDynamic};
use proptest::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, FromDynamic, ToDynamic)]
struct RoundtripStruct {
    name: String,
    enabled: bool,
    count: u16,
    note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, FromDynamic, ToDynamic)]
enum RoundtripEnum {
    Idle,
    Named { tag: String, level: u8 },
    Tuple(String, bool),
}

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..24).prop_map(|chars| chars.into_iter().collect())
}

fn arb_roundtrip_struct() -> impl Strategy<Value = RoundtripStruct> {
    (
        arb_small_string(),
        any::<bool>(),
        0u16..=4096,
        prop_oneof![Just(None), arb_small_string().prop_map(Some)],
    )
        .prop_map(|(name, enabled, count, note)| RoundtripStruct {
            name,
            enabled,
            count,
            note,
        })
}

fn arb_roundtrip_enum() -> impl Strategy<Value = RoundtripEnum> {
    prop_oneof![
        Just(RoundtripEnum::Idle),
        (arb_small_string(), any::<u8>())
            .prop_map(|(tag, level)| RoundtripEnum::Named { tag, level }),
        (arb_small_string(), any::<bool>())
            .prop_map(|(name, enabled)| RoundtripEnum::Tuple(name, enabled)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn struct_to_dynamic_from_dynamic_roundtrip(value in arb_roundtrip_struct()) {
        let dynamic = value.to_dynamic();
        let back = RoundtripStruct::from_dynamic(&dynamic, Default::default()).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn enum_to_dynamic_from_dynamic_roundtrip(value in arb_roundtrip_enum()) {
        let dynamic = value.to_dynamic();
        let back = RoundtripEnum::from_dynamic(&dynamic, Default::default()).unwrap();
        prop_assert_eq!(back, value);
    }
}
