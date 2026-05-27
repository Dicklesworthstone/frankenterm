//! Property-based tests for the capture-scheduler snapshot types in
//! `frankenterm_core::tailer`. `proptest_tailer.rs` covers scheduler
//! behavior, but these public serde types — `CapturePriorityTier`,
//! `CaptureSkipReason`, `CaptureStarvationWarning`, `PaneSchedulerSnapshot`,
//! and `SchedulerTierSnapshot` — had no coverage. Pins:
//!
//! - `CapturePriorityTier::from_priority` boundary (priority <= 50 → High).
//! - serde round-trips for every snapshot type and enum (snake_case rename).

use proptest::prelude::*;

use frankenterm_core::tailer::{
    CapturePriorityTier, CaptureSkipReason, CaptureStarvationWarning, PaneSchedulerSnapshot,
    SchedulerTierSnapshot, TailerMode,
};

fn arb_priority_tier() -> impl Strategy<Value = CapturePriorityTier> {
    prop_oneof![Just(CapturePriorityTier::High), Just(CapturePriorityTier::Low)]
}

fn arb_tailer_mode() -> impl Strategy<Value = TailerMode> {
    prop_oneof![Just(TailerMode::Polling), Just(TailerMode::Streaming)]
}

fn arb_starvation_warning() -> impl Strategy<Value = CaptureStarvationWarning> {
    prop_oneof![
        Just(CaptureStarvationWarning::None),
        Just(CaptureStarvationWarning::StrictInvariantFailed),
        Just(CaptureStarvationWarning::BenchmarkTargetMissed),
        Just(CaptureStarvationWarning::NotProven),
    ]
}

fn arb_skip_reason() -> impl Strategy<Value = CaptureSkipReason> {
    prop::sample::select(vec![
        CaptureSkipReason::NotObserved,
        CaptureSkipReason::StreamingMode,
        CaptureSkipReason::StreamingFallback,
        CaptureSkipReason::NotDue,
        CaptureSkipReason::AlreadyCapturing,
        CaptureSkipReason::GlobalCaptureBudgetExhausted,
        CaptureSkipReason::GlobalByteBudgetExhausted,
        CaptureSkipReason::NoPermit,
        CaptureSkipReason::SendBackpressure,
        CaptureSkipReason::OverflowGapPending,
        CaptureSkipReason::OverflowGapEmitted,
        CaptureSkipReason::NoCursor,
        CaptureSkipReason::ChannelClosed,
        CaptureSkipReason::CaptureTimeout,
        CaptureSkipReason::CaptureCircuitOpen,
        CaptureSkipReason::CaptureError,
        CaptureSkipReason::NoChange,
        CaptureSkipReason::Changed,
        CaptureSkipReason::Shutdown,
    ])
}

fn arb_pane_snapshot() -> impl Strategy<Value = PaneSchedulerSnapshot> {
    (
        any::<u64>(),
        any::<u32>(),
        arb_priority_tier(),
        arb_tailer_mode(),
        any::<bool>(),
        any::<Option<u64>>(),
        any::<u64>(),
        any::<u64>(),
        arb_skip_reason(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<bool>(),
        arb_starvation_warning(),
    )
        .prop_map(
            |(
                pane_id,
                priority,
                tier,
                mode,
                stale,
                last_successful_capture_age_ms,
                last_poll_age_ms,
                current_interval_ms,
                last_reason_code,
                selection_opportunities,
                selected_count,
                skipped_count,
                consecutive_backpressure,
                overflow_gap_pending,
                starvation_warning,
            )| PaneSchedulerSnapshot {
                pane_id,
                priority,
                tier,
                mode,
                stale,
                last_successful_capture_age_ms,
                last_poll_age_ms,
                current_interval_ms,
                last_reason_code,
                selection_opportunities,
                selected_count,
                skipped_count,
                consecutive_backpressure,
                overflow_gap_pending,
                starvation_warning,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// from_priority partitions at 50: priority <= 50 → High, else Low.
    #[test]
    fn capture_priority_tier_from_priority_boundary(priority in any::<u32>()) {
        let tier = CapturePriorityTier::from_priority(priority);
        let expected = if priority <= 50 {
            CapturePriorityTier::High
        } else {
            CapturePriorityTier::Low
        };
        prop_assert_eq!(tier, expected);
    }

    /// Explicit boundary: 50 is the last High, 51 is the first Low.
    #[test]
    fn capture_priority_tier_boundary_exact(_dummy in 0u8..1) {
        prop_assert_eq!(CapturePriorityTier::from_priority(0), CapturePriorityTier::High);
        prop_assert_eq!(CapturePriorityTier::from_priority(50), CapturePriorityTier::High);
        prop_assert_eq!(CapturePriorityTier::from_priority(51), CapturePriorityTier::Low);
        prop_assert_eq!(CapturePriorityTier::from_priority(u32::MAX), CapturePriorityTier::Low);
    }

    #[test]
    fn priority_tier_serde_roundtrip(tier in arb_priority_tier()) {
        let back: CapturePriorityTier =
            serde_json::from_str(&serde_json::to_string(&tier).unwrap()).unwrap();
        prop_assert_eq!(tier, back);
    }

    #[test]
    fn skip_reason_serde_roundtrip(reason in arb_skip_reason()) {
        let back: CaptureSkipReason =
            serde_json::from_str(&serde_json::to_string(&reason).unwrap()).unwrap();
        prop_assert_eq!(reason, back);
    }

    #[test]
    fn starvation_warning_serde_roundtrip(warning in arb_starvation_warning()) {
        let back: CaptureStarvationWarning =
            serde_json::from_str(&serde_json::to_string(&warning).unwrap()).unwrap();
        prop_assert_eq!(warning, back);
    }

    #[test]
    fn pane_scheduler_snapshot_serde_roundtrip(snap in arb_pane_snapshot()) {
        let back: PaneSchedulerSnapshot =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn scheduler_tier_snapshot_serde_roundtrip(
        tier in arb_priority_tier(),
        opps in any::<u64>(),
        selected in any::<u64>(),
        skipped in any::<u64>(),
    ) {
        let snap = SchedulerTierSnapshot {
            tier,
            selection_opportunities: opps,
            selected_count: selected,
            skipped_count: skipped,
        };
        let back: SchedulerTierSnapshot =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        prop_assert_eq!(snap, back);
    }
}
