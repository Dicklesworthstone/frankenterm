use mux::pane::{PaneConstraints, Pattern, SearchResult};
use proptest::prelude::*;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..32).prop_map(|chars| chars.into_iter().collect())
}

fn arb_optional_usize() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![Just(None), (0usize..=4096).prop_map(Some),]
}

fn arb_pane_constraints() -> impl Strategy<Value = PaneConstraints> {
    (
        0usize..=4096,
        0usize..=4096,
        arb_optional_usize(),
        arb_optional_usize(),
        arb_optional_usize(),
        arb_optional_usize(),
        any::<bool>(),
    )
        .prop_map(
            |(
                min_width,
                min_height,
                max_width,
                max_height,
                preferred_width,
                preferred_height,
                fixed,
            )| PaneConstraints {
                min_width,
                min_height,
                max_width,
                max_height,
                preferred_width,
                preferred_height,
                fixed,
            },
        )
}

fn arb_search_result() -> impl Strategy<Value = SearchResult> {
    (
        -100_000isize..=100_000isize,
        0usize..=4096,
        -100_000isize..=100_000isize,
        0usize..=4096,
        0usize..=4096,
    )
        .prop_map(|(start_y, start_x, end_y, end_x, match_id)| SearchResult {
            start_y,
            start_x,
            end_y,
            end_x,
            match_id,
        })
}

fn arb_pattern() -> impl Strategy<Value = Pattern> {
    prop_oneof![
        arb_small_string().prop_map(Pattern::CaseSensitiveString),
        arb_small_string().prop_map(Pattern::CaseInSensitiveString),
        arb_small_string().prop_map(Pattern::Regex),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn pane_constraints_json_roundtrip(value in arb_pane_constraints()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PaneConstraints = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn search_result_json_roundtrip(value in arb_search_result()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[test]
    fn pattern_json_roundtrip(value in arb_pattern()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: Pattern = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }
}
