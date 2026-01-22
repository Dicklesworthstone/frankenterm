//! Pane content tailing with adaptive polling.
//!
//! This module provides the TailerSupervisor for managing per-pane content
//! capture with adaptive polling intervals.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use crate::ingest::{CapturedSegment, PaneCursor};
use crate::wezterm::{PaneInfo, PaneTextSource};

/// Configuration for the tailer supervisor.
#[derive(Debug, Clone)]
pub struct TailerConfig {
    /// Minimum polling interval (fast polling for active panes)
    pub min_interval: Duration,
    /// Maximum polling interval (slow polling for idle panes)
    pub max_interval: Duration,
    /// Multiplier for backoff when pane is idle
    pub backoff_multiplier: f64,
    /// Maximum number of concurrent captures
    pub max_concurrent: usize,
    /// Overlap size for delta extraction
    pub overlap_size: usize,
    /// Timeout for sending to channel
    pub send_timeout: Duration,
}

impl Default for TailerConfig {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_millis(50),
            max_interval: Duration::from_secs(1),
            backoff_multiplier: 1.5,
            max_concurrent: 10,
            overlap_size: 256,
            send_timeout: Duration::from_millis(100),
        }
    }
}

/// Metrics for tailer supervisor.
#[derive(Debug, Default)]
pub struct TailerMetrics {
    /// Total capture events sent
    pub events_sent: u64,
    /// Number of send timeouts
    pub send_timeouts: u64,
    /// Number of captures that found no changes
    pub no_change_captures: u64,
}

/// Metrics for supervisor operations.
#[derive(Debug, Default)]
pub struct SupervisorMetrics {
    /// Number of tailers started
    pub tailers_started: u64,
    /// Number of tailers stopped
    pub tailers_stopped: u64,
    /// Total sync operations
    pub sync_count: u64,
}

/// A captured segment event for persistence.
#[derive(Debug, Clone)]
pub struct CaptureEvent {
    /// The captured segment (includes pane_id, seq, content, kind, captured_at)
    pub segment: CapturedSegment,
}

/// Per-pane tailer state.
struct PaneTailer {
    /// Pane ID (retained for debugging/logging)
    #[allow(dead_code)]
    pane_id: u64,
    /// Current polling interval
    current_interval: Duration,
    /// Last poll time
    last_poll: Instant,
    /// Whether changes were detected in last poll
    had_changes: bool,
}

impl PaneTailer {
    fn new(pane_id: u64, initial_interval: Duration) -> Self {
        Self {
            pane_id,
            current_interval: initial_interval,
            last_poll: Instant::now(),
            had_changes: false,
        }
    }

    fn should_poll(&self) -> bool {
        self.last_poll.elapsed() >= self.current_interval
    }

    fn record_poll(&mut self, had_changes: bool, config: &TailerConfig) {
        self.last_poll = Instant::now();
        self.had_changes = had_changes;

        // Adaptive interval: speed up if changes, slow down if idle
        if had_changes {
            self.current_interval = config.min_interval;
        } else {
            let new_interval = Duration::from_secs_f64(
                self.current_interval.as_secs_f64() * config.backoff_multiplier,
            );
            self.current_interval = new_interval.min(config.max_interval);
        }
    }
}

/// Supervisor for managing multiple pane tailers.
pub struct TailerSupervisor<S>
where
    S: PaneTextSource + Send + Sync + 'static,
{
    /// Configuration
    config: TailerConfig,
    /// Channel for sending capture events (will be used when actual polling is implemented)
    #[allow(dead_code)]
    tx: mpsc::Sender<CaptureEvent>,
    /// Per-pane cursors (shared with runtime)
    cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Shutdown flag
    shutdown_flag: Arc<AtomicBool>,
    /// Pane text source (WezTerm client or test double)
    source: Arc<S>,
    /// Concurrency limiter for in-flight polls
    semaphore: Arc<Semaphore>,
    /// Per-pane tailer state
    tailers: HashMap<u64, PaneTailer>,
    /// Metrics
    metrics: TailerMetrics,
    /// Supervisor metrics
    supervisor_metrics: SupervisorMetrics,
}

impl<S> TailerSupervisor<S>
where
    S: PaneTextSource + Send + Sync + 'static,
{
    /// Create a new tailer supervisor.
    pub fn new(
        config: TailerConfig,
        tx: mpsc::Sender<CaptureEvent>,
        cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
        shutdown_flag: Arc<AtomicBool>,
        source: Arc<S>,
    ) -> Self {
        let max_concurrent = config.max_concurrent.max(1);
        Self {
            config,
            tx,
            cursors,
            shutdown_flag,
            source,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            tailers: HashMap::new(),
            metrics: TailerMetrics::default(),
            supervisor_metrics: SupervisorMetrics::default(),
        }
    }

    /// Number of active tailers.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.tailers.len()
    }

    /// Sync tailers with the current set of observed panes.
    ///
    /// Adds tailers for new panes, removes tailers for departed panes.
    pub fn sync_tailers(&mut self, observed_panes: &HashMap<u64, PaneInfo>) {
        self.supervisor_metrics.sync_count += 1;

        // Remove tailers for panes no longer observed
        let to_remove: Vec<u64> = self
            .tailers
            .keys()
            .filter(|id| !observed_panes.contains_key(*id))
            .copied()
            .collect();

        for pane_id in to_remove {
            self.tailers.remove(&pane_id);
            self.supervisor_metrics.tailers_stopped += 1;
            debug!(pane_id, "Removed tailer for departed pane");
        }

        // Add tailers for new panes
        for &pane_id in observed_panes.keys() {
            if !self.tailers.contains_key(&pane_id) {
                self.tailers
                    .insert(pane_id, PaneTailer::new(pane_id, self.config.min_interval));
                self.supervisor_metrics.tailers_started += 1;
                debug!(pane_id, "Added tailer for new pane");
            }
        }
    }

    /// Perform one poll cycle, capturing from panes that are ready.
    pub async fn poll_once(&mut self) {
        if self.shutdown_flag.load(Ordering::SeqCst) {
            return;
        }

        // Find panes ready for polling
        let ready_panes: Vec<u64> = self
            .tailers
            .iter()
            .filter(|(_, t)| t.should_poll())
            .map(|(id, _)| *id)
            .collect();

        if ready_panes.is_empty() {
            return;
        }

        let mut join_set = JoinSet::new();

        for pane_id in ready_panes {
            let tx = self.tx.clone();
            let cursors = Arc::clone(&self.cursors);
            let source = Arc::clone(&self.source);
            let semaphore = Arc::clone(&self.semaphore);
            let overlap_size = self.config.overlap_size;
            let send_timeout = self.config.send_timeout;

            join_set.spawn(async move {
                let Ok(_permit) = semaphore.acquire_owned().await else {
                    return (pane_id, PollOutcome::Backpressure);
                };

                let has_cursor = {
                    let cursors = cursors.read().await;
                    cursors.contains_key(&pane_id)
                };

                if !has_cursor {
                    return (pane_id, PollOutcome::NoCursor);
                }

                let permit = match timeout(send_timeout, tx.reserve()).await {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => return (pane_id, PollOutcome::ChannelClosed),
                    Err(_) => return (pane_id, PollOutcome::Backpressure),
                };

                let text = match source.get_text(pane_id, false).await {
                    Ok(text) => text,
                    Err(err) => {
                        drop(permit);
                        return (pane_id, PollOutcome::Error(err.to_string()));
                    }
                };

                let captured = {
                    let mut cursors = cursors.write().await;
                    cursors
                        .get_mut(&pane_id)
                        .and_then(|cursor| cursor.capture_snapshot(&text, overlap_size))
                };

                if let Some(segment) = captured {
                    permit.send(CaptureEvent { segment });
                    (pane_id, PollOutcome::Changed)
                } else {
                    drop(permit);
                    (pane_id, PollOutcome::NoChange)
                }
            });
        }

        while let Some(result) = join_set.join_next().await {
            let (pane_id, outcome) = match result {
                Ok(value) => value,
                Err(e) => {
                    warn!(error = %e, "Tailer poll task failed");
                    continue;
                }
            };

            if let Some(tailer) = self.tailers.get_mut(&pane_id) {
                match outcome {
                    PollOutcome::Changed => {
                        tailer.record_poll(true, &self.config);
                        self.metrics.events_sent += 1;
                    }
                    PollOutcome::NoChange => {
                        tailer.record_poll(false, &self.config);
                        self.metrics.no_change_captures += 1;
                        trace!(pane_id, "Tailer poll no change");
                    }
                    PollOutcome::Backpressure => {
                        tailer.record_poll(false, &self.config);
                        self.metrics.send_timeouts += 1;
                        warn!(pane_id, "Tailer backpressure: capture queue full");
                    }
                    PollOutcome::NoCursor => {
                        tailer.record_poll(false, &self.config);
                        trace!(pane_id, "Tailer poll skipped (no cursor)");
                    }
                    PollOutcome::ChannelClosed => {
                        tailer.record_poll(false, &self.config);
                        warn!(pane_id, "Tailer channel closed");
                    }
                    PollOutcome::Error(error) => {
                        tailer.record_poll(false, &self.config);
                        warn!(pane_id, error = %error, "Tailer poll failed");
                    }
                }
            }
        }
    }

    /// Graceful shutdown of all tailers.
    pub async fn shutdown(&mut self) {
        debug!(
            active_count = self.tailers.len(),
            "Shutting down tailer supervisor"
        );
        self.tailers.clear();
    }

    /// Get current metrics.
    #[must_use]
    pub fn metrics(&self) -> &TailerMetrics {
        &self.metrics
    }

    /// Get supervisor metrics.
    #[must_use]
    pub fn supervisor_metrics(&self) -> &SupervisorMetrics {
        &self.supervisor_metrics
    }
}

#[derive(Debug)]
enum PollOutcome {
    Changed,
    NoChange,
    Backpressure,
    NoCursor,
    ChannelClosed,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_pane(id: u64) -> PaneInfo {
        PaneInfo {
            pane_id: id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: Some(24),
            cols: Some(80),
            cwd: None,
            title: None,
            tty_name: None,
            cursor_x: Some(0),
            cursor_y: Some(0),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: id == 1,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    #[derive(Default)]
    struct StaticSource;

    impl PaneTextSource for StaticSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[derive(Default)]
    struct FixedSource;

    impl PaneTextSource for FixedSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            Box::pin(async move { Ok(format!("pane-{pane_id}")) })
        }
    }

    struct CountingSource {
        active: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl CountingSource {
        fn new(active: Arc<AtomicUsize>, max: Arc<AtomicUsize>, delay: Duration) -> Self {
            Self { active, max, delay }
        }
    }

    impl PaneTextSource for CountingSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let active = Arc::clone(&self.active);
            let max = Arc::clone(&self.max);
            let delay = self.delay;
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                loop {
                    let observed = max.load(Ordering::SeqCst);
                    if current <= observed {
                        break;
                    }
                    if max
                        .compare_exchange(observed, current, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                }

                tokio::time::sleep(delay).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(format!("pane-{pane_id}-tick"))
            })
        }
    }

    #[test]
    fn tailer_config_default() {
        let config = TailerConfig::default();
        assert_eq!(config.min_interval, Duration::from_millis(50));
        assert_eq!(config.max_interval, Duration::from_secs(1));
        assert!(config.backoff_multiplier > 1.0);
    }

    #[test]
    fn pane_tailer_adaptive_interval() {
        let config = TailerConfig {
            min_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(1),
            backoff_multiplier: 2.0,
            ..Default::default()
        };

        let mut tailer = PaneTailer::new(1, config.min_interval);

        // No changes - interval should increase
        tailer.record_poll(false, &config);
        assert_eq!(tailer.current_interval, Duration::from_millis(200));

        // Still no changes - continue increasing
        tailer.record_poll(false, &config);
        assert_eq!(tailer.current_interval, Duration::from_millis(400));

        // Changes detected - snap back to min
        tailer.record_poll(true, &config);
        assert_eq!(tailer.current_interval, config.min_interval);
    }

    #[test]
    fn pane_tailer_interval_capped_at_max() {
        let config = TailerConfig {
            min_interval: Duration::from_millis(100),
            max_interval: Duration::from_millis(500),
            backoff_multiplier: 10.0,
            ..Default::default()
        };

        let mut tailer = PaneTailer::new(1, config.min_interval);

        // Should cap at max_interval
        tailer.record_poll(false, &config);
        assert_eq!(tailer.current_interval, config.max_interval);
    }

    #[tokio::test]
    async fn supervisor_sync_tailers() {
        let config = TailerConfig::default();
        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let source = Arc::new(StaticSource);

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, shutdown, source);

        assert_eq!(supervisor.active_count(), 0);

        // Add some panes
        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        panes.insert(2, make_pane(2));

        supervisor.sync_tailers(&panes);
        assert_eq!(supervisor.active_count(), 2);

        // Remove a pane
        panes.remove(&1);
        supervisor.sync_tailers(&panes);
        assert_eq!(supervisor.active_count(), 1);
    }

    #[tokio::test]
    async fn supervisor_respects_concurrency_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingSource::new(
            Arc::clone(&active),
            Arc::clone(&max),
            Duration::from_millis(20),
        ));

        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 2,
            send_timeout: Duration::from_millis(50),
            ..Default::default()
        };

        let (tx, _rx) = mpsc::channel(10);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let mut cursor_guard = cursors.write().await;
            for pane_id in 1..=4 {
                cursor_guard.insert(pane_id, PaneCursor::new(pane_id));
            }
        }

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, shutdown, source);

        let mut panes = HashMap::new();
        for pane_id in 1..=4 {
            panes.insert(pane_id, make_pane(pane_id));
        }
        supervisor.sync_tailers(&panes);

        tokio::time::sleep(Duration::from_millis(2)).await;
        supervisor.poll_once().await;

        let max_seen = max.load(Ordering::SeqCst);
        assert!(max_seen <= 2, "max concurrency observed: {max_seen}");
    }

    #[tokio::test]
    async fn supervisor_backpressure_records_timeout() {
        let source = Arc::new(FixedSource);
        let config = TailerConfig {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(50),
            max_concurrent: 2,
            send_timeout: Duration::from_millis(10),
            ..Default::default()
        };

        let (tx, _rx) = mpsc::channel(1);
        let cursors = Arc::new(RwLock::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        {
            let mut cursor_guard = cursors.write().await;
            cursor_guard.insert(1, PaneCursor::new(1));
            cursor_guard.insert(2, PaneCursor::new(2));
        }

        let mut supervisor = TailerSupervisor::new(config, tx, cursors, shutdown, source);

        let mut panes = HashMap::new();
        panes.insert(1, make_pane(1));
        panes.insert(2, make_pane(2));
        supervisor.sync_tailers(&panes);

        tokio::time::sleep(Duration::from_millis(2)).await;
        supervisor.poll_once().await;

        assert!(supervisor.metrics().send_timeouts >= 1);
    }

    #[test]
    fn capture_event_wraps_captured_segment() {
        use std::time::{SystemTime, UNIX_EPOCH};

        #[allow(clippy::cast_possible_truncation)]
        // Epoch millis won't overflow i64 until year 292 million
        let captured_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let seg = CapturedSegment {
            pane_id: 42,
            seq: 10,
            content: "test content".to_string(),
            kind: crate::ingest::CapturedSegmentKind::Delta,
            captured_at,
        };

        let event = CaptureEvent { segment: seg };

        assert_eq!(event.segment.pane_id, 42);
        assert_eq!(event.segment.seq, 10);
        assert_eq!(event.segment.content, "test content");
        assert!(event.segment.captured_at > 0);
    }
}
