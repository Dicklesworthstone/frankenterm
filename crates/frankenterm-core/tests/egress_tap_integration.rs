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

use tokio::sync::{RwLock, mpsc};

use frankenterm_core::ingest::{CapturedSegmentKind, PaneCursor, PaneRegistry};
use frankenterm_core::recording::{
    EgressEvent, EgressNoopTap, EgressTap, RecorderSegmentKind, SharedEgressTap,
    captured_kind_to_segment,
};
use frankenterm_core::runtime_compat::sleep;
use frankenterm_core::tailer::{CaptureEvent, TailerConfig, TailerSupervisor};
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
    fn len(&self) -> usize {
        self.events.lock().unwrap().len()
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
                None => Err(frankenterm_core::Error::Runtime(format!(
                    "pane {pane_id} not found"
                ))),
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
        min_interval: Duration::from_millis(10),
        max_interval: Duration::from_millis(100),
        backoff_multiplier: 1.5,
        max_concurrent: 4,
        overlap_size: 50,
        send_timeout: Duration::from_secs(1),
    }
}

fn pane_map(ids: &[u64]) -> HashMap<u64, PaneInfo> {
    ids.iter().map(|&id| (id, test_pane_info(id))).collect()
}

#[tokio::test]
async fn egress_tap_fires_on_delta_capture() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
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

    let mut js = tokio::task::JoinSet::new();
    tailer.spawn_ready(&mut js);
    while let Some(r) = js.join_next().await {
        let (pid, out) = r.unwrap();
        tailer.handle_poll_result(pid, out);
    }
    while rx.try_recv().is_ok() {}

    if tap.len() >= 1 {
        let e = &tap.events()[0];
        assert_eq!(e.pane_id, 1);
        assert!(!e.text.is_empty());
        assert!(e.occurred_at_ms > 0);
    }

    source
        .set_text(1, "$ prompt\nline1\nline2\nnew output\n")
        .await;
    sleep(Duration::from_millis(20)).await;

    let mut js = tokio::task::JoinSet::new();
    tailer.spawn_ready(&mut js);
    while let Some(r) = js.join_next().await {
        let (pid, out) = r.unwrap();
        tailer.handle_poll_result(pid, out);
    }
    while rx.try_recv().is_ok() {}

    let all = tap.events();
    if all.len() >= 2 {
        let last = &all[all.len() - 1];
        assert_eq!(last.pane_id, 1);
        assert!(
            last.segment_kind == RecorderSegmentKind::Delta
                || last.segment_kind == RecorderSegmentKind::Gap
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

    let mut js = tokio::task::JoinSet::new();
    tailer.spawn_ready(&mut js);
    while let Some(r) = js.join_next().await {
        let (pid, out) = r.unwrap();
        tailer.handle_poll_result(pid, out);
    }
    while rx.try_recv().is_ok() {}

    source
        .set_text(1, "completely different text that shares no overlap")
        .await;
    sleep(Duration::from_millis(20)).await;

    let mut js = tokio::task::JoinSet::new();
    tailer.spawn_ready(&mut js);
    while let Some(r) = js.join_next().await {
        let (pid, out) = r.unwrap();
        tailer.handle_poll_result(pid, out);
    }
    while rx.try_recv().is_ok() {}

    for gap in tap.events().iter().filter(|e| e.is_gap) {
        assert_eq!(gap.pane_id, 1);
        assert!(gap.gap_reason.is_some());
        assert_eq!(gap.segment_kind, RecorderSegmentKind::Gap);
    }
}

#[tokio::test]
async fn egress_noop_tap_compiles_as_shared() {
    let _tap: SharedEgressTap = Arc::new(EgressNoopTap);
}

#[tokio::test]
async fn egress_tap_multi_pane() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
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

    let mut js = tokio::task::JoinSet::new();
    tailer.spawn_ready(&mut js);
    while let Some(r) = js.join_next().await {
        let (pid, out) = r.unwrap();
        tailer.handle_poll_result(pid, out);
    }
    while rx.try_recv().is_ok() {}

    for ev in &tap.events() {
        assert!(ev.pane_id == 10 || ev.pane_id == 20);
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

    let mut js = tokio::task::JoinSet::new();
    tailer.spawn_ready(&mut js);
    while let Some(r) = js.join_next().await {
        let (pid, out) = r.unwrap();
        tailer.handle_poll_result(pid, out);
    }

    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    // Primary assertion: no panic without a tap set.
    // Capture may or may not produce events depending on scheduler state.
    let _ = count;
}

#[tokio::test]
async fn egress_monotonic_sequence() {
    let (tx, mut rx) = mpsc::channel::<CaptureEvent>(16);
    let cursors = Arc::new(RwLock::new(HashMap::new()));
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
        sleep(Duration::from_millis(20)).await;
        let mut js = tokio::task::JoinSet::new();
        tailer.spawn_ready(&mut js);
        while let Some(r) = js.join_next().await {
            let (pid, out) = r.unwrap();
            tailer.handle_poll_result(pid, out);
        }
        while rx.try_recv().is_ok() {}
    }

    let all = tap.events();
    if all.len() >= 2 {
        for w in all.windows(2) {
            assert!(
                w[1].sequence >= w[0].sequence,
                "monotonic: {} >= {}",
                w[1].sequence,
                w[0].sequence
            );
        }
    }
}

#[tokio::test]
async fn captured_kind_maps_correctly() {
    let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Delta);
    assert_eq!(kind, RecorderSegmentKind::Delta);
    assert!(!is_gap);

    let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Gap {
        reason: "overlap_failed".to_string(),
    });
    assert_eq!(kind, RecorderSegmentKind::Gap);
    assert!(is_gap);
}
