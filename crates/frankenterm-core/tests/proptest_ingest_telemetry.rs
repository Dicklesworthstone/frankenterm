//! Property-based tests for ingest pipeline telemetry counters (ft-3kxe.8).
//!
//! Validates:
//! 1. IngestTelemetry counters are monotonically increasing
//! 2. Counters accurately reflect discovery_tick() outcomes
//! 3. Snapshot serialization roundtrips correctly
//! 4. StreamIngester telemetry snapshot consistency
//! 5. Counters remain consistent across pane lifecycle events

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::ingest::{
    IngestTelemetry, IngestTelemetrySnapshot, PaneRegistry, StreamEvent, StreamIngester,
    StreamIngesterTelemetrySnapshot,
};
use frankenterm_core::wezterm::PaneInfo;

// =============================================================================
// Helpers
// =============================================================================

fn make_pane(pane_id: u64, title: &str) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 1,
        window_id: 1,
        domain_id: None,
        domain_name: None,
        workspace: Some("default".to_string()),
        size: None,
        rows: None,
        cols: None,
        title: Some(title.to_string()),
        cwd: Some("/tmp".to_string()),
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: true,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

// =============================================================================
// IngestTelemetry unit properties
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let t = IngestTelemetry::new();
    let snap = t.snapshot();
    assert_eq!(snap.discovery_ticks, 0);
    assert_eq!(snap.panes_discovered, 0);
    assert_eq!(snap.panes_closed, 0);
    assert_eq!(snap.generation_changes, 0);
    assert_eq!(snap.metadata_changes, 0);
    assert_eq!(snap.panes_filtered, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = IngestTelemetrySnapshot {
        discovery_ticks: 42,
        panes_discovered: 10,
        panes_closed: 3,
        generation_changes: 7,
        metadata_changes: 5,
        panes_filtered: 2,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: IngestTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// PaneRegistry telemetry integration
// =============================================================================

#[test]
fn discovery_tick_increments_counter() {
    let mut reg = PaneRegistry::new();
    let panes = vec![make_pane(1, "bash"), make_pane(2, "vim")];

    reg.discovery_tick(panes);
    let snap = reg.telemetry().snapshot();

    assert_eq!(snap.discovery_ticks, 1);
    assert_eq!(snap.panes_discovered, 2);
    assert_eq!(snap.panes_closed, 0);
}

#[test]
fn pane_close_counted() {
    let mut reg = PaneRegistry::new();

    // Discover two panes
    reg.discovery_tick(vec![make_pane(1, "bash"), make_pane(2, "vim")]);

    // Close pane 2 by omitting it
    reg.discovery_tick(vec![make_pane(1, "bash")]);

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.discovery_ticks, 2);
    assert_eq!(snap.panes_discovered, 2);
    assert_eq!(snap.panes_closed, 1);
}

#[test]
fn generation_change_counted() {
    let mut reg = PaneRegistry::new();

    // Discover pane
    reg.discovery_tick(vec![make_pane(1, "bash")]);

    // Title change triggers generation change
    reg.discovery_tick(vec![make_pane(1, "vim")]);

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.discovery_ticks, 2);
    assert_eq!(snap.generation_changes, 1);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn counters_monotonically_increase(
        pane_counts in prop::collection::vec(0u64..8, 1..10),
    ) {
        let mut reg = PaneRegistry::new();
        let mut prev_ticks = 0u64;
        let mut prev_discovered = 0u64;
        let mut prev_closed = 0u64;

        for (tick_idx, count) in pane_counts.iter().enumerate() {
            let panes: Vec<PaneInfo> = (0..*count)
                .map(|id| make_pane(id, &format!("pane-{id}")))
                .collect();

            reg.discovery_tick(panes);
            let snap = reg.telemetry().snapshot();

            prop_assert!(
                snap.discovery_ticks > prev_ticks,
                "discovery_ticks must increase: tick_idx={}, prev={}, cur={}",
                tick_idx, prev_ticks, snap.discovery_ticks
            );
            prop_assert!(
                snap.panes_discovered >= prev_discovered,
                "panes_discovered must not decrease: prev={}, cur={}",
                prev_discovered, snap.panes_discovered
            );
            prop_assert!(
                snap.panes_closed >= prev_closed,
                "panes_closed must not decrease: prev={}, cur={}",
                prev_closed, snap.panes_closed
            );

            prev_ticks = snap.discovery_ticks;
            prev_discovered = snap.panes_discovered;
            prev_closed = snap.panes_closed;
        }
    }

    #[test]
    fn discovered_equals_sum_of_new_panes(
        tick_panes in prop::collection::vec(
            prop::collection::vec(1u64..100, 0..6),
            1..8,
        ),
    ) {
        let mut reg = PaneRegistry::new();
        let mut total_new = 0u64;

        for pane_ids in &tick_panes {
            let panes: Vec<PaneInfo> = pane_ids
                .iter()
                .map(|&id| make_pane(id, "shell"))
                .collect();

            let diff = reg.discovery_tick(panes);
            total_new += diff.new_panes.len() as u64;
        }

        let snap = reg.telemetry().snapshot();
        prop_assert_eq!(snap.panes_discovered, total_new);
    }

    #[test]
    fn closed_equals_sum_of_closed_panes(
        tick_panes in prop::collection::vec(
            prop::collection::vec(1u64..20, 0..5),
            2..6,
        ),
    ) {
        let mut reg = PaneRegistry::new();
        let mut total_closed = 0u64;

        for pane_ids in &tick_panes {
            let panes: Vec<PaneInfo> = pane_ids
                .iter()
                .map(|&id| make_pane(id, "shell"))
                .collect();

            let diff = reg.discovery_tick(panes);
            total_closed += diff.closed_panes.len() as u64;
        }

        let snap = reg.telemetry().snapshot();
        prop_assert_eq!(snap.panes_closed, total_closed);
    }

    #[test]
    fn snapshot_roundtrip_via_serde(
        ticks in 0u64..100,
        discovered in 0u64..1000,
        closed in 0u64..500,
        gen_changes in 0u64..200,
        meta_changes in 0u64..200,
        filtered in 0u64..100,
    ) {
        let snap = IngestTelemetrySnapshot {
            discovery_ticks: ticks,
            panes_discovered: discovered,
            panes_closed: closed,
            generation_changes: gen_changes,
            metadata_changes: meta_changes,
            panes_filtered: filtered,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: IngestTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}

// =============================================================================
// StreamIngester telemetry
// =============================================================================

#[test]
fn stream_ingester_telemetry_starts_zero() {
    let ingester = StreamIngester::new();
    let snap = ingester.telemetry_snapshot();
    assert_eq!(snap.active_panes, 0);
    assert_eq!(snap.segments_emitted, 0);
    assert_eq!(snap.gaps_emitted, 0);
    assert_eq!(snap.overflow_pending, 0);
}

#[test]
fn stream_ingester_counts_segments() {
    let mut ingester = StreamIngester::new();

    ingester.process(StreamEvent::OutputData {
        pane_id: 1,
        data: "hello\n".to_string(),
        received_at: 1_000_000,
        overflow: false,
    });

    let snap = ingester.telemetry_snapshot();
    assert_eq!(snap.active_panes, 1);
    assert_eq!(snap.segments_emitted, 1);
    assert_eq!(snap.gaps_emitted, 0);
}

#[test]
fn stream_ingester_counts_gaps_on_overflow() {
    let mut ingester = StreamIngester::new();

    // First event establishes pane
    ingester.process(StreamEvent::OutputData {
        pane_id: 1,
        data: "hello\n".to_string(),
        received_at: 1_000_000,
        overflow: false,
    });

    // Overflow event: produces GAP + delta = 2 segments, 1 gap
    ingester.process(StreamEvent::OutputData {
        pane_id: 1,
        data: "world\n".to_string(),
        received_at: 2_000_000,
        overflow: true,
    });

    let snap = ingester.telemetry_snapshot();
    assert_eq!(snap.segments_emitted, 3); // 1 initial + 1 gap + 1 delta
    assert_eq!(snap.gaps_emitted, 1);
}

#[test]
fn stream_ingester_snapshot_serde_roundtrip() {
    let snap = StreamIngesterTelemetrySnapshot {
        active_panes: 5,
        segments_emitted: 100,
        gaps_emitted: 3,
        overflow_pending: 1,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: StreamIngesterTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn stream_segments_monotonic(
        events in prop::collection::vec(
            (1u64..5, "[a-z]{1,20}"),
            1..20,
        ),
    ) {
        let mut ingester = StreamIngester::new();
        let mut prev_segments = 0u64;
        let mut prev_gaps = 0u64;

        for (pane_id, data) in &events {
            ingester.process(StreamEvent::OutputData {
                pane_id: *pane_id,
                data: data.clone(),
                received_at: 1_000_000,
                overflow: false,
            });

            let snap = ingester.telemetry_snapshot();
            prop_assert!(
                snap.segments_emitted >= prev_segments,
                "segments must not decrease"
            );
            prop_assert!(
                snap.gaps_emitted >= prev_gaps,
                "gaps must not decrease"
            );
            prev_segments = snap.segments_emitted;
            prev_gaps = snap.gaps_emitted;
        }
    }
}
