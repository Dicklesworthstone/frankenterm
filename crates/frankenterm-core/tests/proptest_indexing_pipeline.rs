//! Property-based tests for ContentIndexingPipeline (ft-dr6zv.1.5).

use proptest::prelude::*;

use frankenterm_core::search::{
    ContentIndexingPipeline, IndexingConfig, PaneWatermark, PipelineConfig, PipelineSkipReason,
    PipelineState, ScrollbackLine, SearchIndex,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_pipeline_state() -> impl Strategy<Value = PipelineState> {
    prop_oneof![
        Just(PipelineState::Running),
        Just(PipelineState::Paused),
        Just(PipelineState::Stopped),
    ]
}

fn test_index(dir: &std::path::Path) -> SearchIndex {
    SearchIndex::open(IndexingConfig {
        index_dir: dir.to_path_buf(),
        max_index_size_bytes: 10 * 1024 * 1024,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 5,
        max_docs_per_second: 1000,
    })
    .unwrap()
}

// ---------------------------------------------------------------------------
// PIP-1: Pipeline state transitions are always valid
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn pip_1_state_transitions_valid(
        ops in proptest::collection::vec(0u8..6, 1..30),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        for op in ops {
            match op {
                0 => pipeline.pause(),
                1 => pipeline.resume(),
                2 => pipeline.stop(),
                3 => pipeline.restart(),
                _ => { /* no-op */ }
            }
            // State must always be one of the three valid states.
            let state = pipeline.state();
            prop_assert!(
                state == PipelineState::Running
                    || state == PipelineState::Paused
                    || state == PipelineState::Stopped,
                "invalid state after op {}: {:?}", op, state
            );
        }
    }
}

// ---------------------------------------------------------------------------
// PIP-2: Paused or stopped pipeline always skips ticks
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn pip_2_paused_stopped_skip(
        initial_state in arb_pipeline_state(),
        line_count in 0usize..10,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        match initial_state {
            PipelineState::Running => { /* default */ }
            PipelineState::Paused => pipeline.pause(),
            PipelineState::Stopped => pipeline.stop(),
        }

        let lines: Vec<ScrollbackLine> = (0..line_count)
            .map(|i| ScrollbackLine {
                text: format!("line {}", i),
                captured_at_ms: 1000 + i as i64 * 100,
                pane_id: None,
                session_id: None,
            })
            .collect();
        let panes = if lines.is_empty() {
            vec![]
        } else {
            vec![(1u64, None, lines)]
        };

        let report = pipeline.tick(&panes, 5000, false, None);

        match initial_state {
            PipelineState::Paused => {
                prop_assert_eq!(
                    report.skipped_reason,
                    Some(PipelineSkipReason::Paused),
                    "paused pipeline must skip with Paused reason"
                );
                prop_assert_eq!(report.panes_processed, 0);
            }
            PipelineState::Stopped => {
                prop_assert_eq!(
                    report.skipped_reason,
                    Some(PipelineSkipReason::Stopped),
                    "stopped pipeline must skip with Stopped reason"
                );
                prop_assert_eq!(report.panes_processed, 0);
            }
            PipelineState::Running => {
                // Running pipeline should NOT have Paused or Stopped skip reason.
                if let Some(reason) = report.skipped_reason {
                    prop_assert!(
                        reason != PipelineSkipReason::Paused
                            && reason != PipelineSkipReason::Stopped,
                        "running pipeline must not skip with Paused/Stopped"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PIP-3: Watermark monotonically increases
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pip_3_watermark_monotonic(
        tick_count in 1usize..6,
        base_ms in 1000i64..5000,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        let mut prev_wm = i64::MIN;

        for tick_idx in 0..tick_count {
            let ts_base = base_ms + (tick_idx as i64) * 1000;
            let lines = vec![
                ScrollbackLine {
                    text: format!("tick {} output", tick_idx),
                    captured_at_ms: ts_base,
                    pane_id: None,
                    session_id: None,
                },
                ScrollbackLine {
                    text: format!("tick {} more", tick_idx),
                    captured_at_ms: ts_base + 50,
                    pane_id: None,
                    session_id: None,
                },
            ];
            let panes = vec![(1u64, None, lines)];
            pipeline.tick(&panes, ts_base + 500, false, None);

            if let Some(wm) = pipeline.watermark(1) {
                prop_assert!(
                    wm.last_indexed_at_ms >= prev_wm,
                    "watermark must not decrease: {} < {}",
                    wm.last_indexed_at_ms,
                    prev_wm
                );
                prev_wm = wm.last_indexed_at_ms;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PIP-4: Resize storm flag prevents indexing when configured
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn pip_4_resize_storm_respected(
        pause_on_storm in any::<bool>(),
        storm_active in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut config = PipelineConfig::default();
        config.pause_on_resize_storm = pause_on_storm;
        let mut pipeline = ContentIndexingPipeline::new(config, index);

        let lines = vec![ScrollbackLine {
            text: "data".to_string(),
            captured_at_ms: 1000,
            pane_id: None,
            session_id: None,
        }];
        let panes = vec![(1u64, None, lines)];
        let report = pipeline.tick(&panes, 2000, storm_active, None);

        if pause_on_storm && storm_active {
            prop_assert_eq!(
                report.skipped_reason,
                Some(PipelineSkipReason::ResizeStorm),
                "resize storm + pause_on_storm must skip"
            );
            prop_assert_eq!(report.panes_processed, 0);
        } else {
            prop_assert!(
                report.skipped_reason != Some(PipelineSkipReason::ResizeStorm),
                "should not skip due to resize storm when disabled or not active"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// PIP-5: max_panes_per_tick is respected
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pip_5_pane_limit_respected(
        pane_count in 1usize..20,
        max_panes in 1usize..10,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut config = PipelineConfig::default();
        config.max_panes_per_tick = max_panes;
        let mut pipeline = ContentIndexingPipeline::new(config, index);

        let panes: Vec<(u64, Option<String>, Vec<ScrollbackLine>)> = (0..pane_count)
            .map(|i| {
                (
                    i as u64,
                    None,
                    vec![ScrollbackLine {
                        text: format!("pane {} data", i),
                        captured_at_ms: 1000 + i as i64 * 100,
                        pane_id: None,
                        session_id: None,
                    }],
                )
            })
            .collect();

        let report = pipeline.tick(&panes, 5000, false, None);

        // processed + skipped must not exceed max_panes (only max_panes are iterated)
        let touched = report.panes_processed + report.panes_skipped;
        prop_assert!(
            touched <= max_panes,
            "touched panes {} exceeds max_panes {}", touched, max_panes
        );
    }
}

// ---------------------------------------------------------------------------
// PIP-6: Duplicate content below watermark is skipped
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pip_6_duplicate_below_watermark_skipped(
        text in "[a-z]{3,20}",
        ts in 1000i64..9000,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        let lines = vec![ScrollbackLine {
            text: text.clone(),
            captured_at_ms: ts,
            pane_id: None,
            session_id: None,
        }];
        let panes = vec![(1u64, None, lines.clone())];

        // First tick should process.
        let r1 = pipeline.tick(&panes, ts + 1000, false, None);
        prop_assert_eq!(r1.panes_processed, 1);

        // Second tick with identical content (same timestamps) should skip.
        let r2 = pipeline.tick(&panes, ts + 2000, false, None);
        prop_assert_eq!(r2.panes_skipped, 1, "identical content must be skipped");
        prop_assert_eq!(r2.panes_processed, 0);
    }
}

// ---------------------------------------------------------------------------
// PIP-7: remove_pane clears watermark, enabling re-index
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pip_7_remove_pane_clears_watermark(
        pane_id in 1u64..100,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        let lines = vec![ScrollbackLine {
            text: "data".to_string(),
            captured_at_ms: 1000,
            pane_id: None,
            session_id: None,
        }];
        let panes = vec![(pane_id, None, lines)];
        pipeline.tick(&panes, 2000, false, None);

        prop_assert!(pipeline.watermark(pane_id).is_some());

        let removed = pipeline.remove_pane(pane_id);
        prop_assert!(removed.is_some());
        prop_assert!(pipeline.watermark(pane_id).is_none());
    }
}

// ---------------------------------------------------------------------------
// PIP-8: reset_all_watermarks sets all to i64::MIN
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pip_8_reset_all_watermarks(
        pane_count in 1usize..8,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        // Index several panes.
        let panes: Vec<(u64, Option<String>, Vec<ScrollbackLine>)> = (0..pane_count)
            .map(|i| {
                (
                    i as u64,
                    None,
                    vec![ScrollbackLine {
                        text: format!("pane {}", i),
                        captured_at_ms: 1000 + i as i64 * 100,
                        pane_id: None,
                        session_id: None,
                    }],
                )
            })
            .collect();
        pipeline.tick(&panes, 5000, false, None);

        pipeline.reset_all_watermarks();

        for i in 0..pane_count {
            if let Some(wm) = pipeline.watermark(i as u64) {
                prop_assert_eq!(
                    wm.last_indexed_at_ms,
                    i64::MIN,
                    "watermark for pane {} must be i64::MIN after reset", i
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PIP-9: PaneWatermark serde roundtrip preserves all fields
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn pip_9_watermark_serde_roundtrip(
        pane_id in 0u64..1000,
        last_indexed_at_ms in -10_000i64..10_000,
        total_docs_indexed in 0u64..10_000,
        session_id in proptest::option::of("[a-z]{4,12}"),
    ) {
        let wm = PaneWatermark {
            pane_id,
            last_indexed_at_ms,
            total_docs_indexed,
            session_id,
        };
        let json = serde_json::to_string(&wm).unwrap();
        let wm2: PaneWatermark = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wm, wm2, "PaneWatermark must roundtrip through JSON");
    }
}

// ---------------------------------------------------------------------------
// PIP-10: PipelineState serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn pip_10_state_serde_roundtrip(
        state in arb_pipeline_state(),
    ) {
        let json = serde_json::to_string(&state).unwrap();
        let state2: PipelineState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, state2);
    }
}

// ---------------------------------------------------------------------------
// PIP-11: tick report doc counts are consistent
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pip_11_report_consistency(
        line_count in 1usize..15,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        let lines: Vec<ScrollbackLine> = (0..line_count)
            .map(|i| ScrollbackLine {
                text: format!("line number {}", i),
                captured_at_ms: 1000 + i as i64 * 100,
                pane_id: None,
                session_id: None,
            })
            .collect();
        let panes = vec![(1u64, None, lines)];
        let report = pipeline.tick(&panes, 5000, false, None);

        // accepted_docs <= submitted_docs
        prop_assert!(
            report.ingest_report.accepted_docs <= report.ingest_report.submitted_docs,
            "accepted ({}) must be <= submitted ({})",
            report.ingest_report.accepted_docs,
            report.ingest_report.submitted_docs
        );

        // total_lines_consumed <= line_count
        prop_assert!(
            report.total_lines_consumed <= line_count,
            "consumed ({}) must be <= input ({})",
            report.total_lines_consumed,
            line_count
        );
    }
}

// ---------------------------------------------------------------------------
// PIP-12: Multi-pane concurrent indexing with watermark isolation
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn pip_12_multi_pane_watermark_isolation(
        pane_count in 2usize..6,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

        // Each pane has different timestamps.
        let panes: Vec<(u64, Option<String>, Vec<ScrollbackLine>)> = (0..pane_count)
            .map(|i| {
                let base = 1000 + (i as i64) * 5000;
                (
                    i as u64,
                    Some(format!("sess-{}", i)),
                    vec![
                        ScrollbackLine {
                            text: format!("pane {} line 1", i),
                            captured_at_ms: base,
                            pane_id: None,
                            session_id: None,
                        },
                        ScrollbackLine {
                            text: format!("pane {} line 2", i),
                            captured_at_ms: base + 100,
                            pane_id: None,
                            session_id: None,
                        },
                    ],
                )
            })
            .collect();

        pipeline.tick(&panes, 50_000, false, None);

        // Each pane should have its own watermark.
        for i in 0..pane_count {
            let wm = pipeline.watermark(i as u64);
            prop_assert!(wm.is_some(), "pane {} must have watermark", i);
            let wm = wm.unwrap();
            let expected_session = format!("sess-{}", i);
            prop_assert_eq!(
                wm.session_id.as_deref(),
                Some(expected_session.as_str()),
                "pane {} session_id mismatch", i
            );
        }

        // Watermarks should be independent — different timestamps.
        if pane_count >= 2 {
            let wm0 = pipeline.watermark(0).unwrap();
            let wm1 = pipeline.watermark(1).unwrap();
            prop_assert!(
                wm0.last_indexed_at_ms != wm1.last_indexed_at_ms,
                "different panes should have different watermarks"
            );
        }
    }
}
