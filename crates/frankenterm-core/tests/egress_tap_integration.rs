//! Integration tests for egress tap points (ft-oegrb.2.3).
//!
//! Verifies that the `EgressTap` fires correctly when integrated with
//! `TailerSupervisor` for delta captures, gap captures, and overflow gaps.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use frankenterm_core::ingest::{CapturedSegmentKind, PaneCursor, PaneRegistry};
use frankenterm_core::recording::{
    EgressEvent, EgressNoopTap, EgressTap, RecorderSegmentKind, SharedEgressTap,
    captured_kind_to_segment,
};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder, RwLock, mpsc};
use frankenterm_core::tailer::{CaptureEvent, TailerConfig, TailerPollTaskSet, TailerSupervisor};
use frankenterm_core::wezterm::{PaneInfo, PaneTextSource};

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
}

impl EgressTap for TestEgressTap {
    fn on_egress(&self, event: EgressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[derive(Debug, Clone)]
struct FakePaneSource {
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
    type Fut<'a> = Pin<Box<dyn Future<Output = frankenterm_core::Result<String>> + Send + 'a>>;

    fn get_text(&self, pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
        let texts = self.texts.clone();
        Box::pin(async move {
            let map = texts.read().await;
            match map.get(&pane_id) {
                Some(text) => Ok(text.clone()),
                None => Err(frankenterm_core::Error::runtime_backend(
                    "fake_pane_source_get_text",
                    format!("pane {pane_id} not found"),
                )),
            }
        })
    }
}

fn test_pane_info(pane_id: u64) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title: None,
        cwd: None,
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: false,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

fn fast_config() -> TailerConfig {
    TailerConfig {
        // A zero interval is the deterministic readiness driver for these
        // tests. It avoids wall-clock sleeps while leaving production cadence
        // behavior to TailerSupervisor's dedicated timing tests.
        min_interval: Duration::ZERO,
        max_interval: Duration::from_millis(100),
        backoff_multiplier: 1.5,
        max_concurrent: 4,
        overlap_size: 50,
        send_timeout: Duration::from_secs(1),
        capture_timeout: Duration::from_secs(1),
    }
}

fn pane_map(ids: &[u64]) -> HashMap<u64, PaneInfo> {
    ids.iter().map(|&id| (id, test_pane_info(id))).collect()
}

fn run_async_test<F>(future: F)
where
    F: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("failed to build runtime_async current-thread runtime");
    CompatRuntime::block_on(&runtime, future);
}

#[test]
fn egress_tap_fires_on_delta_capture() {
    run_async_test(async {
        let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
        let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(FakePaneSource::new());

        source.set_text(1, "$ prompt\nline1\nline2\n").await;
        {
            cursors.write().await.insert(1, PaneCursor::new(1));
        }

        let tap = Arc::new(TestEgressTap::new());
        let mut tailer = TailerSupervisor::new(
            fast_config(),
            tx,
            Arc::clone(&cursors),
            Arc::clone(&registry),
            shutdown.clone(),
            Arc::clone(&source),
        );
        tailer.set_egress_tap(tap.clone());
        tailer.sync_tailers(&pane_map(&[1]));

        let mut poll_tasks = TailerPollTaskSet::new();
        tailer.spawn_ready(&mut poll_tasks);
        while let Some((pid, out)) = poll_tasks.join_next().await {
            tailer.handle_poll_result(pid, out);
        }
        while rx.try_recv().is_ok() {}

        let first = tap.events();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].pane_id, 1);
        assert_eq!(first[0].sequence, 0);
        assert_eq!(first[0].segment_kind, RecorderSegmentKind::Delta);
        assert_eq!(first[0].text, "$ prompt\nline1\nline2\n");
        assert!(!first[0].is_gap);
        assert_eq!(first[0].gap_reason, None);
        assert!(first[0].occurred_at_ms > 0);

        source
            .set_text(1, "$ prompt\nline1\nline2\nnew output\n")
            .await;

        let mut poll_tasks = TailerPollTaskSet::new();
        tailer.spawn_ready(&mut poll_tasks);
        while let Some((pid, out)) = poll_tasks.join_next().await {
            tailer.handle_poll_result(pid, out);
        }
        while rx.try_recv().is_ok() {}

        let all = tap.events();
        assert_eq!(all.len(), 2);
        let last = &all[1];
        assert_eq!(last.pane_id, 1);
        assert_eq!(last.sequence, 1);
        assert_eq!(last.segment_kind, RecorderSegmentKind::Delta);
        assert_eq!(last.text, "new output\n");
        assert!(!last.is_gap);
        assert_eq!(last.gap_reason, None);
    });
}

#[test]
fn egress_tap_captures_gap_segments() {
    run_async_test(async {
        let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
        let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(FakePaneSource::new());

        source.set_text(1, "initial content").await;
        {
            cursors.write().await.insert(1, PaneCursor::new(1));
        }

        let tap = Arc::new(TestEgressTap::new());
        let mut tailer = TailerSupervisor::new(
            fast_config(),
            tx,
            Arc::clone(&cursors),
            Arc::clone(&registry),
            shutdown.clone(),
            Arc::clone(&source),
        );
        tailer.set_egress_tap(tap.clone());
        tailer.sync_tailers(&pane_map(&[1]));

        let mut poll_tasks = TailerPollTaskSet::new();
        tailer.spawn_ready(&mut poll_tasks);
        while let Some((pid, out)) = poll_tasks.join_next().await {
            tailer.handle_poll_result(pid, out);
        }
        while rx.try_recv().is_ok() {}

        source
            .set_text(1, "completely different text that shares no overlap")
            .await;

        let mut poll_tasks = TailerPollTaskSet::new();
        tailer.spawn_ready(&mut poll_tasks);
        while let Some((pid, out)) = poll_tasks.join_next().await {
            tailer.handle_poll_result(pid, out);
        }
        while rx.try_recv().is_ok() {}

        let events = tap.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 0);
        let gap = &events[1];
        assert_eq!(gap.pane_id, 1);
        assert_eq!(gap.sequence, 1);
        assert!(gap.is_gap);
        assert_eq!(gap.gap_reason.as_deref(), Some("overlap_not_found"));
        assert_eq!(gap.segment_kind, RecorderSegmentKind::Gap);
        assert_eq!(gap.text, "completely different text that shares no overlap");
    });
}

#[test]
fn egress_noop_tap_compiles_as_shared() {
    let _tap: SharedEgressTap = Arc::new(EgressNoopTap);
}

#[test]
fn egress_tap_multi_pane() {
    run_async_test(async {
        let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
        let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(FakePaneSource::new());

        source.set_text(10, "pane ten content").await;
        source.set_text(20, "pane twenty content").await;
        {
            let mut g = cursors.write().await;
            g.insert(10, PaneCursor::new(10));
            g.insert(20, PaneCursor::new(20));
        }

        let tap = Arc::new(TestEgressTap::new());
        let mut tailer = TailerSupervisor::new(
            fast_config(),
            tx,
            Arc::clone(&cursors),
            Arc::clone(&registry),
            shutdown.clone(),
            Arc::clone(&source),
        );
        tailer.set_egress_tap(tap.clone());
        tailer.sync_tailers(&pane_map(&[10, 20]));

        let mut poll_tasks = TailerPollTaskSet::new();
        tailer.spawn_ready(&mut poll_tasks);
        while let Some((pid, out)) = poll_tasks.join_next().await {
            tailer.handle_poll_result(pid, out);
        }
        while rx.try_recv().is_ok() {}

        let mut events = tap.events();
        events.sort_by_key(|event| event.pane_id);
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].pane_id, events[0].sequence), (10, 0));
        assert_eq!((events[1].pane_id, events[1].sequence), (20, 0));
        assert_eq!(events[0].text, "pane ten content");
        assert_eq!(events[1].text, "pane twenty content");
        assert_eq!(events[0].segment_kind, RecorderSegmentKind::Delta);
        assert_eq!(events[1].segment_kind, RecorderSegmentKind::Delta);
    });
}

#[test]
fn independent_supervisors_emit_deterministic_pane_local_sequences() {
    run_async_test(async {
        let (left_tx, mut left_rx) = mpsc::channel::<CaptureEvent>(8);
        let (right_tx, mut right_rx) = mpsc::channel::<CaptureEvent>(8);
        let left_cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let right_cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let left_registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let right_registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(FakePaneSource::new());
        let left_pane = 101;
        let right_pane = 202;

        source.set_text(left_pane, "left supervisor output").await;
        source.set_text(right_pane, "right supervisor output").await;
        left_cursors
            .write()
            .await
            .insert(left_pane, PaneCursor::new(left_pane));
        right_cursors
            .write()
            .await
            .insert(right_pane, PaneCursor::new(right_pane));

        let left_tap = Arc::new(TestEgressTap::new());
        let right_tap = Arc::new(TestEgressTap::new());
        let mut left = TailerSupervisor::new(
            fast_config(),
            left_tx,
            Arc::clone(&left_cursors),
            left_registry,
            Arc::clone(&shutdown),
            Arc::clone(&source),
        );
        let mut right = TailerSupervisor::new(
            fast_config(),
            right_tx,
            Arc::clone(&right_cursors),
            right_registry,
            shutdown,
            source,
        );
        left.set_egress_tap(left_tap.clone());
        right.set_egress_tap(right_tap.clone());
        left.sync_tailers(&pane_map(&[left_pane]));
        right.sync_tailers(&pane_map(&[right_pane]));

        // Both independent supervisors contribute poll futures to the same
        // task set, so completion order is intentionally unconstrained.
        let mut poll_tasks = TailerPollTaskSet::new();
        left.spawn_ready(&mut poll_tasks);
        right.spawn_ready(&mut poll_tasks);
        while let Some((pane_id, outcome)) = poll_tasks.join_next().await {
            if pane_id == left_pane {
                left.handle_poll_result(pane_id, outcome);
            } else {
                assert_eq!(pane_id, right_pane);
                right.handle_poll_result(pane_id, outcome);
            }
        }
        while left_rx.try_recv().is_ok() {}
        while right_rx.try_recv().is_ok() {}

        let left_events = left_tap.events();
        let right_events = right_tap.events();
        assert_eq!(left_events.len(), 1);
        assert_eq!(right_events.len(), 1);
        assert_eq!((left_events[0].pane_id, left_events[0].sequence), (101, 0));
        assert_eq!(
            (right_events[0].pane_id, right_events[0].sequence),
            (202, 0)
        );

        let mut canonical = vec![
            (left_events[0].pane_id, left_events[0].sequence),
            (right_events[0].pane_id, right_events[0].sequence),
        ];
        canonical.sort_unstable();
        assert_eq!(canonical, vec![(101, 0), (202, 0)]);
    });
}

#[test]
fn egress_tap_not_set_still_works() {
    run_async_test(async {
        let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
        let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(FakePaneSource::new());

        source.set_text(1, "some text").await;
        {
            cursors.write().await.insert(1, PaneCursor::new(1));
        }

        let mut tailer = TailerSupervisor::new(
            fast_config(),
            tx,
            Arc::clone(&cursors),
            Arc::clone(&registry),
            shutdown.clone(),
            Arc::clone(&source),
        );
        tailer.sync_tailers(&pane_map(&[1]));

        let mut poll_tasks = TailerPollTaskSet::new();
        tailer.spawn_ready(&mut poll_tasks);
        while let Some((pid, out)) = poll_tasks.join_next().await {
            tailer.handle_poll_result(pid, out);
        }

        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(
            count, 1,
            "capture must proceed when no observer is installed"
        );
    });
}

#[test]
fn egress_monotonic_sequence() {
    run_async_test(async {
        let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
        let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(FakePaneSource::new());

        source.set_text(1, "aaaa\nbbbb\ncccc\ndddd\neeee\n").await;
        {
            cursors.write().await.insert(1, PaneCursor::new(1));
        }

        let tap = Arc::new(TestEgressTap::new());
        let mut tailer = TailerSupervisor::new(
            fast_config(),
            tx,
            Arc::clone(&cursors),
            Arc::clone(&registry),
            shutdown.clone(),
            Arc::clone(&source),
        );
        tailer.set_egress_tap(tap.clone());
        tailer.sync_tailers(&pane_map(&[1]));

        for i in 0..3 {
            source
                .set_text(1, &format!("aaaa\nbbbb\ncccc\ndddd\neeee\nout-{i}\n"))
                .await;
            let mut poll_tasks = TailerPollTaskSet::new();
            tailer.spawn_ready(&mut poll_tasks);
            while let Some((pid, out)) = poll_tasks.join_next().await {
                tailer.handle_poll_result(pid, out);
            }
            while rx.try_recv().is_ok() {}
        }

        let all = tap.events();
        let sequences: Vec<u64> = all.iter().map(|event| event.sequence).collect();
        assert_eq!(sequences, vec![0, 1, 2]);
    });
}

#[test]
fn captured_kind_maps_correctly() {
    let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Delta);
    assert_eq!(kind, RecorderSegmentKind::Delta);
    assert!(!is_gap);

    let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Gap {
        reason: "overlap_failed".to_string(),
    });
    assert_eq!(kind, RecorderSegmentKind::Gap);
    assert!(is_gap);
}
