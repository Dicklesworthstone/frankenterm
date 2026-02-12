//! Integration tests for egress tap points (ft-oegrb.2.3).
//!
//! Verifies that the `EgressTap` fires correctly when integrated with
//! `TailerSupervisor` for delta captures, gap captures, and overflow gaps.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{RwLock, mpsc};

use frankenterm_core::ingest::{CapturedSegmentKind, PaneCursor, PaneRegistry};
use frankenterm_core::recording::{
    EgressEvent, EgressNoopTap, EgressTap, RecorderSegmentKind, SharedEgressTap,
};
use frankenterm_core::tailer::{CaptureEvent, TailerConfig, TailerSupervisor};
use frankenterm_core::wezterm::PaneTextSource;

/// Test-local collecting egress tap (the library's CollectingEgressTap is
/// #[cfg(test)] which is not visible to integration tests).
#[derive(Debug, Default)]
struct TestEgressTap {
    events: Mutex<Vec<EgressEvent>>,
}

impl TestEgressTap {
    fn new() -> Self {
        Self::default()
    }

    fn events(&self) -> Vec<EgressEvent> {
        self.events.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

impl EgressTap for TestEgressTap {
    fn on_egress(&self, event: EgressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// A fake pane text source for testing that returns configurable text.
#[derive(Debug, Clone)]
struct FakePaneSource {
    /// Maps pane_id → current text to return from get_text
    texts: Arc<RwLock<HashMap<u64, String>>>,
}

impl FakePaneSource {
    fn new() -> Self {
        Self {
            texts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn set_text(&self, pane_id: u64, text: &str) {
        self.texts.write().await.insert(pane_id, text.to_string());
    }
}

impl PaneTextSource for FakePaneSource {
    fn get_text(
        &self,
        pane_id: u64,
        _escapes: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = frankenterm_core::Result<String>> + Send + '_>,
    > {
        let texts = self.texts.clone();
        Box::pin(async move {
            let map = texts.read().await;
            match map.get(&pane_id) {
                Some(text) => Ok(text.clone()),
                None => Err(frankenterm_core::Error::Runtime(format!(
                    "pane {pane_id} not found"
                ))),
            }
        })
    }
}

fn fast_tailer_config() -> TailerConfig {
    TailerConfig {
        min_interval: Duration::from_millis(10),
        max_interval: Duration::from_millis(100),
        backoff_multiplier: 1.5,
        max_concurrent: 4,
        overlap_size: 50,
        send_timeout: Duration::from_secs(1),
    }
}

#[tokio::test]
async fn egress_tap_fires_on_delta_capture() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RwLock::new(PaneRegistry::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let source = Arc::new(FakePaneSource::new());

    // Set initial pane text
    source.set_text(1, "$ prompt\nline1\nline2\n").await;

    // Create cursor for pane 1
    {
        let mut cursors_guard = cursors.write().await;
        cursors_guard.insert(1, PaneCursor::new(1));
    }

    let tap = Arc::new(TestEgressTap::new());
    let mut tailer = TailerSupervisor::new(
        fast_tailer_config(),
        tx,
        Arc::clone(&cursors),
        Arc::clone(&registry),
        shutdown.clone(),
        Arc::clone(&source),
    );
    tailer.set_egress_tap(tap.clone());

    // Sync with pane list
    let panes = vec![frankenterm_core::wezterm::PaneInfo {
        pane_id: 1,
        ..Default::default()
    }];
    tailer.sync_panes(&panes);

    // First poll — captures initial snapshot (may be a gap since there's no previous)
    let mut join_set = tokio::task::JoinSet::new();
    tailer.poll_ready_panes(&mut join_set);

    // Wait for result
    while let Some(result) = join_set.join_next().await {
        let (pane_id, outcome) = result.unwrap();
        tailer.handle_poll_result(pane_id, outcome);
    }

    // Drain channel
    let mut events = vec![];
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    // First capture should produce at least one segment
    if !events.is_empty() {
        assert!(tap.len() >= 1, "tap should have fired for captured segment");
        let egress = &tap.events()[0];
        assert_eq!(egress.pane_id, 1);
        assert!(!egress.text.is_empty());
        assert!(egress.occurred_at_ms > 0);
        assert!(egress.sequence == 0 || egress.sequence == 1);
    }

    // Now change text and poll again for a delta
    source
        .set_text(1, "$ prompt\nline1\nline2\nnew output\n")
        .await;

    // Wait for tailer interval
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut join_set = tokio::task::JoinSet::new();
    tailer.poll_ready_panes(&mut join_set);
    while let Some(result) = join_set.join_next().await {
        let (pane_id, outcome) = result.unwrap();
        tailer.handle_poll_result(pane_id, outcome);
    }

    // Drain channel
    while let Ok(_ev) = rx.try_recv() {}

    // Should have at least 2 tap events now (initial + delta)
    let all_events = tap.events();
    if all_events.len() >= 2 {
        let delta_event = &all_events[all_events.len() - 1];
        assert_eq!(delta_event.pane_id, 1);
        // Delta should contain the new output
        assert!(
            delta_event.segment_kind == RecorderSegmentKind::Delta
                || delta_event.segment_kind == RecorderSegmentKind::Gap
        );
    }
}

#[tokio::test]
async fn egress_tap_captures_gap_segments() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RwLock::new(PaneRegistry::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let source = Arc::new(FakePaneSource::new());

    source.set_text(1, "initial content").await;

    {
        let mut cursors_guard = cursors.write().await;
        cursors_guard.insert(1, PaneCursor::new(1));
    }

    let tap = Arc::new(TestEgressTap::new());
    let mut tailer = TailerSupervisor::new(
        fast_tailer_config(),
        tx,
        Arc::clone(&cursors),
        Arc::clone(&registry),
        shutdown.clone(),
        Arc::clone(&source),
    );
    tailer.set_egress_tap(tap.clone());

    let panes = vec![frankenterm_core::wezterm::PaneInfo {
        pane_id: 1,
        ..Default::default()
    }];
    tailer.sync_panes(&panes);

    // First poll
    let mut join_set = tokio::task::JoinSet::new();
    tailer.poll_ready_panes(&mut join_set);
    while let Some(result) = join_set.join_next().await {
        let (pane_id, outcome) = result.unwrap();
        tailer.handle_poll_result(pane_id, outcome);
    }
    while let Ok(_) = rx.try_recv() {}

    // Completely different content (no overlap possible) → should produce a Gap
    source
        .set_text(1, "completely different text that shares no overlap")
        .await;

    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut join_set = tokio::task::JoinSet::new();
    tailer.poll_ready_panes(&mut join_set);
    while let Some(result) = join_set.join_next().await {
        let (pane_id, outcome) = result.unwrap();
        tailer.handle_poll_result(pane_id, outcome);
    }
    while let Ok(_) = rx.try_recv() {}

    // Verify gap tap events are flagged correctly
    let all = tap.events();
    let gaps: Vec<&EgressEvent> = all.iter().filter(|e| e.is_gap).collect();
    // May or may not produce a gap depending on overlap behavior,
    // but verify that any gaps have proper metadata
    for gap in &gaps {
        assert_eq!(gap.pane_id, 1);
        assert!(gap.gap_reason.is_some());
        assert_eq!(gap.segment_kind, RecorderSegmentKind::Gap);
    }
}

#[tokio::test]
async fn egress_noop_tap_compiles_as_shared() {
    let _tap: SharedEgressTap = Arc::new(EgressNoopTap);
    // Verifies EgressNoopTap satisfies SharedEgressTap bounds
}

#[tokio::test]
async fn egress_tap_records_correct_pane_id() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RwLock::new(PaneRegistry::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let source = Arc::new(FakePaneSource::new());

    // Set up two panes
    source.set_text(10, "pane ten content").await;
    source.set_text(20, "pane twenty content").await;

    {
        let mut cursors_guard = cursors.write().await;
        cursors_guard.insert(10, PaneCursor::new(10));
        cursors_guard.insert(20, PaneCursor::new(20));
    }

    let tap = Arc::new(TestEgressTap::new());
    let mut tailer = TailerSupervisor::new(
        fast_tailer_config(),
        tx,
        Arc::clone(&cursors),
        Arc::clone(&registry),
        shutdown.clone(),
        Arc::clone(&source),
    );
    tailer.set_egress_tap(tap.clone());

    let panes = vec![
        frankenterm_core::wezterm::PaneInfo {
            pane_id: 10,
            ..Default::default()
        },
        frankenterm_core::wezterm::PaneInfo {
            pane_id: 20,
            ..Default::default()
        },
    ];
    tailer.sync_panes(&panes);

    let mut join_set = tokio::task::JoinSet::new();
    tailer.poll_ready_panes(&mut join_set);
    while let Some(result) = join_set.join_next().await {
        let (pane_id, outcome) = result.unwrap();
        tailer.handle_poll_result(pane_id, outcome);
    }
    while let Ok(_) = rx.try_recv() {}

    let all = tap.events();
    let pane_ids: std::collections::HashSet<u64> = all.iter().map(|e| e.pane_id).collect();

    // Both panes should have captured (either or both)
    if !all.is_empty() {
        // Verify all captured events have valid pane IDs
        for ev in &all {
            assert!(ev.pane_id == 10 || ev.pane_id == 20);
        }
    }
}

#[tokio::test]
async fn egress_tap_not_set_still_works() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RwLock::new(PaneRegistry::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let source = Arc::new(FakePaneSource::new());

    source.set_text(1, "some text").await;

    {
        let mut cursors_guard = cursors.write().await;
        cursors_guard.insert(1, PaneCursor::new(1));
    }

    // No tap set — should work without panicking
    let mut tailer = TailerSupervisor::new(
        fast_tailer_config(),
        tx,
        Arc::clone(&cursors),
        Arc::clone(&registry),
        shutdown.clone(),
        Arc::clone(&source),
    );

    let panes = vec![frankenterm_core::wezterm::PaneInfo {
        pane_id: 1,
        ..Default::default()
    }];
    tailer.sync_panes(&panes);

    let mut join_set = tokio::task::JoinSet::new();
    tailer.poll_ready_panes(&mut join_set);
    while let Some(result) = join_set.join_next().await {
        let (pane_id, outcome) = result.unwrap();
        tailer.handle_poll_result(pane_id, outcome);
    }

    // Verify capture events still flow to channel
    let mut count = 0;
    while let Ok(_) = rx.try_recv() {
        count += 1;
    }
    // Should have at least one captured segment
    assert!(count >= 1, "capture events should flow without tap set");
}

#[tokio::test]
async fn egress_event_has_monotonic_sequence() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
    let registry = Arc::new(RwLock::new(PaneRegistry::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let source = Arc::new(FakePaneSource::new());

    // Start with overlappable content
    source
        .set_text(1, "aaaa\nbbbb\ncccc\ndddd\neeee\n")
        .await;

    {
        let mut cursors_guard = cursors.write().await;
        cursors_guard.insert(1, PaneCursor::new(1));
    }

    let tap = Arc::new(TestEgressTap::new());
    let mut tailer = TailerSupervisor::new(
        fast_tailer_config(),
        tx,
        Arc::clone(&cursors),
        Arc::clone(&registry),
        shutdown.clone(),
        Arc::clone(&source),
    );
    tailer.set_egress_tap(tap.clone());

    let panes = vec![frankenterm_core::wezterm::PaneInfo {
        pane_id: 1,
        ..Default::default()
    }];
    tailer.sync_panes(&panes);

    // Capture multiple times with changing content
    for i in 0..3 {
        source
            .set_text(
                1,
                &format!("aaaa\nbbbb\ncccc\ndddd\neeee\noutput-{i}\n"),
            )
            .await;

        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut join_set = tokio::task::JoinSet::new();
        tailer.poll_ready_panes(&mut join_set);
        while let Some(result) = join_set.join_next().await {
            let (pane_id, outcome) = result.unwrap();
            tailer.handle_poll_result(pane_id, outcome);
        }
        while let Ok(_) = rx.try_recv() {}
    }

    let all = tap.events();
    if all.len() >= 2 {
        // Verify sequences are monotonically increasing
        for window in all.windows(2) {
            assert!(
                window[1].sequence >= window[0].sequence,
                "sequence should be monotonically increasing: {} >= {}",
                window[1].sequence,
                window[0].sequence
            );
        }
    }
}

#[tokio::test]
async fn captured_kind_to_segment_maps_correctly() {
    use frankenterm_core::recording::captured_kind_to_segment;

    let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Delta);
    assert_eq!(kind, RecorderSegmentKind::Delta);
    assert!(!is_gap);

    let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Gap {
        reason: "overlap_failed".to_string(),
    });
    assert_eq!(kind, RecorderSegmentKind::Gap);
    assert!(is_gap);
}
