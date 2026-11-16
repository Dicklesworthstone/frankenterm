#![cfg(feature = "use_serde")]

use frankenterm_color_types::SrgbaTuple;
use proptest::prelude::*;

fn arb_srgba_tuple() -> impl Strategy<Value = SrgbaTuple> {
    (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b, a)| {
        SrgbaTuple(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            f32::from(a) / 255.0,
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn srgba_tuple_json_roundtrip(value in arb_srgba_tuple()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SrgbaTuple = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn srgba_tuple_u8_projection_survives_json_roundtrip(value in arb_srgba_tuple()) {
        let before = value.as_rgba_u8();
        let json = serde_json::to_string(&value).unwrap();
        let back: SrgbaTuple = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.as_rgba_u8(), before);
    }
}
