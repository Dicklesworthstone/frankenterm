use mux::layout::{LayoutArrangement, SwapLayout};
use mux::renderable::StableCursorPosition;
use mux::tab::{
    PaneEntry, PaneNode, SerdeUrl, SplitDirection, SplitDirectionAndSize, SplitRequest, SplitSize,
};
use proptest::prelude::*;
use url::Url;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_url() -> impl Strategy<Value = Url> {
    ("[a-z0-9]{1,12}", "[a-z0-9-]{1,16}", "[a-z0-9/_-]{0,24}").prop_map(
        |(subdomain, host, path)| {
            let path = path.trim_matches('/');
            let candidate = if path.is_empty() {
                format!("https://{subdomain}.{host}.example/")
            } else {
                format!("https://{subdomain}.{host}.example/{path}")
            };
            Url::parse(&candidate).unwrap()
        },
    )
}

fn arb_serde_url() -> impl Strategy<Value = SerdeUrl> {
    arb_url().prop_map(SerdeUrl::from)
}

fn arb_terminal_size() -> impl Strategy<Value = frankenterm_term::TerminalSize> {
    (
        1usize..=512,
        1usize..=512,
        0usize..=8192,
        0usize..=8192,
        0u32..=960,
    )
        .prop_map(|(rows, cols, pixel_width, pixel_height, dpi)| {
            frankenterm_term::TerminalSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
                dpi,
            }
        })
}

fn arb_pane_entry() -> impl Strategy<Value = PaneEntry> {
    (
        0usize..=4096,
        0usize..=4096,
        0usize..=4096,
        arb_small_string(),
        arb_terminal_size(),
        prop_oneof![Just(None), arb_serde_url().prop_map(Some),],
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        arb_small_string(),
        0usize..=4096,
        -10_000isize..=10_000isize,
        0usize..=4096,
        0usize..=4096,
        prop_oneof![Just(None), arb_small_string().prop_map(Some),],
    )
        .prop_map(
            |(
                window_id,
                tab_id,
                pane_id,
                title,
                size,
                working_dir,
                alt_screen_active,
                is_active_pane,
                is_zoomed_pane,
                workspace,
                cursor_x,
                physical_top,
                top_row,
                left_col,
                tty_name,
            )| PaneEntry {
                window_id,
                tab_id,
                pane_id,
                title,
                size,
                working_dir,
                alt_screen_active,
                is_active_pane,
                is_zoomed_pane,
                workspace,
                cursor_pos: StableCursorPosition {
                    x: cursor_x,
                    ..StableCursorPosition::default()
                },
                physical_top,
                top_row,
                left_col,
                tty_name,
            },
        )
}

fn arb_pane_node() -> impl Strategy<Value = PaneNode> {
    prop_oneof![
        Just(PaneNode::Empty),
        arb_pane_entry().prop_map(PaneNode::Leaf),
    ]
}

fn arb_split_direction() -> impl Strategy<Value = SplitDirection> {
    prop_oneof![
        Just(SplitDirection::Horizontal),
        Just(SplitDirection::Vertical),
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
        arb_split_direction(),
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

fn arb_split_direction_and_size() -> impl Strategy<Value = SplitDirectionAndSize> {
    (
        arb_split_direction(),
        arb_terminal_size(),
        arb_terminal_size(),
    )
        .prop_map(|(direction, first, second)| SplitDirectionAndSize {
            direction,
            first,
            second,
        })
}

fn arb_layout_arrangement() -> impl Strategy<Value = LayoutArrangement> {
    let leaf = any::<bool>().prop_map(|is_main| LayoutArrangement::Slot { is_main });
    leaf.prop_recursive(4, 32, 2, |inner| {
        (arb_split_direction(), 0.0f64..=1.0f64, inner.clone(), inner).prop_map(
            |(direction, ratio, first, second)| LayoutArrangement::Split {
                direction,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            },
        )
    })
}

fn arb_swap_layout() -> impl Strategy<Value = SwapLayout> {
    (
        arb_small_string(),
        prop::option::of(arb_small_string()),
        arb_layout_arrangement(),
    )
        .prop_map(|(name, description, arrangement)| SwapLayout {
            name,
            description,
            arrangement,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn serde_url_json_roundtrip(value in arb_serde_url()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SerdeUrl = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn pane_entry_json_roundtrip(value in arb_pane_entry()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PaneEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn pane_node_json_roundtrip(value in arb_pane_node()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PaneNode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn split_request_json_roundtrip(value in arb_split_request()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SplitRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn split_direction_and_size_json_roundtrip(value in arb_split_direction_and_size()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SplitDirectionAndSize = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn layout_arrangement_json_roundtrip(value in arb_layout_arrangement()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: LayoutArrangement = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn swap_layout_json_roundtrip(value in arb_swap_layout()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SwapLayout = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
