use proptest::prelude::*;

use frankenterm_core::render_snapshot_guard::{
    LockWaitClassification, RenderFrameTiming, classify_lock_wait,
};
use frankenterm_core::render_snapshot_jsonl::{
    RenderFrameJsonRow, parse_render_frame_trace, render_frame_trace_to_jsonl,
};

fn arb_scenario() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,31}"
}

fn arb_timing() -> impl Strategy<Value = RenderFrameTiming> {
    (any::<u64>(), any::<u64>(), any::<u64>(), any::<u32>()).prop_map(
        |(ts_ns, acquire_ns, hold_ns, dirty_lines_observed)| RenderFrameTiming {
            ts_ns,
            acquire_ns,
            hold_ns,
            dirty_lines_observed,
        },
    )
}

fn label_for_acquire_ns(acquire_ns: u64) -> &'static str {
    match classify_lock_wait(acquire_ns) {
        LockWaitClassification::Green => "green",
        LockWaitClassification::Yellow => "yellow",
        LockWaitClassification::Red => "red",
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_render_snapshot_jsonl_row_from_timing_preserves_fields(
        scenario in arb_scenario(),
        frame_index in any::<u32>(),
        timing in arb_timing(),
    ) {
        let row = RenderFrameJsonRow::from_timing(scenario.clone(), frame_index, &timing);

        prop_assert_eq!(row.scenario, scenario);
        prop_assert_eq!(row.frame_index, frame_index);
        prop_assert_eq!(row.ts_ns, timing.ts_ns);
        prop_assert_eq!(row.acquire_ns, timing.acquire_ns);
        prop_assert_eq!(row.hold_ns, timing.hold_ns);
        prop_assert_eq!(row.dirty_lines_observed, timing.dirty_lines_observed);
        prop_assert_eq!(row.classification, label_for_acquire_ns(timing.acquire_ns));
    }

    #[test]
    fn proptest_render_snapshot_jsonl_render_parse_roundtrip_indexes_frames(
        scenario in arb_scenario(),
        timings in prop::collection::vec(arb_timing(), 0..128),
    ) {
        let jsonl = render_frame_trace_to_jsonl(&scenario, &timings);

        prop_assert_eq!(jsonl.is_empty(), timings.is_empty());
        if !timings.is_empty() {
            prop_assert!(jsonl.ends_with('\n'));
            prop_assert_eq!(jsonl.lines().count(), timings.len());
        }

        let parsed = parse_render_frame_trace(&jsonl).expect("rendered JSONL parses");
        prop_assert_eq!(parsed.len(), timings.len());
        for (idx, (row, timing)) in parsed.iter().zip(timings.iter()).enumerate() {
            prop_assert_eq!(row, &RenderFrameJsonRow::from_timing(&scenario, idx as u32, timing));
            prop_assert_eq!(row.scenario, scenario);
            prop_assert_eq!(row.frame_index, idx as u32);
            prop_assert_eq!(row.classification, label_for_acquire_ns(timing.acquire_ns));
        }
    }

    #[test]
    fn proptest_render_snapshot_jsonl_parse_skips_blank_lines(
        scenario in arb_scenario(),
        first in arb_timing(),
        second in arb_timing(),
    ) {
        let first_row = RenderFrameJsonRow::from_timing(&scenario, 0, &first);
        let second_row = RenderFrameJsonRow::from_timing(&scenario, 1, &second);
        let jsonl = format!(
            "\n{}\n   \n\n{}\n\t\n",
            serde_json::to_string(&first_row).unwrap(),
            serde_json::to_string(&second_row).unwrap(),
        );

        let parsed = parse_render_frame_trace(&jsonl).expect("blank lines are skipped");
        prop_assert_eq!(parsed, vec![first_row, second_row]);
    }

    #[test]
    fn proptest_render_snapshot_jsonl_row_serde_roundtrip_is_lossless(
        scenario in arb_scenario(),
        frame_index in any::<u32>(),
        timing in arb_timing(),
    ) {
        let row = RenderFrameJsonRow::from_timing(&scenario, frame_index, &timing);
        let encoded = serde_json::to_string(&row).unwrap();
        let decoded: RenderFrameJsonRow = serde_json::from_str(&encoded).unwrap();

        prop_assert_eq!(decoded, row);
        prop_assert!(!encoded.contains('\n'));
    }

    #[test]
    fn proptest_render_snapshot_jsonl_classification_label_matches_public_helper(
        acquire_ns in any::<u64>(),
    ) {
        let classification = classify_lock_wait(acquire_ns);
        let helper_label = RenderFrameJsonRow::classification_label(classification);

        prop_assert_eq!(helper_label, label_for_acquire_ns(acquire_ns));
        prop_assert!(matches!(helper_label, "green" | "yellow" | "red"));
    }
}
