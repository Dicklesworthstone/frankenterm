#![cfg(feature = "use_serde")]

use proptest::prelude::*;
use serde_json::{from_str, to_string};
use termwiz::terminal::{Blocking, ScreenSize};

fn arb_screen_size() -> impl Strategy<Value = ScreenSize> {
    (0usize..=4096, 0usize..=4096, 0usize..=8192, 0usize..=8192).prop_map(
        |(rows, cols, xpixel, ypixel)| ScreenSize {
            rows,
            cols,
            xpixel,
            ypixel,
        },
    )
}

fn arb_blocking() -> impl Strategy<Value = Blocking> {
    prop_oneof![Just(Blocking::DoNotWait), Just(Blocking::Wait)]
}

proptest! {
    #[test]
    fn screen_size_json_roundtrip(screen_size in arb_screen_size()) {
        let json = to_string(&screen_size)?;
        let decoded: ScreenSize = from_str(&json)?;
        prop_assert_eq!(decoded, screen_size);
    }

    #[test]
    fn blocking_json_roundtrip(blocking in arb_blocking()) {
        let json = to_string(&blocking)?;
        let decoded: Blocking = from_str(&json)?;
        prop_assert_eq!(decoded, blocking);
    }
}
