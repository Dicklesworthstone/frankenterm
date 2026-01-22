//! Observation Runtime for the watcher daemon.
//!
//! This module orchestrates the passive observation loop:
//! - Pane discovery and content tailers
//! - Delta extraction and storage persistence
//! - Pattern detection and event emission
//!
//! # Architecture
//!
//! ```text
//! WezTerm CLI ──┬──► PaneRegistry (discovery)
//!               │
//!               └──► PaneCursor (deltas) ──┬──► StorageHandle (segments)
//!                                          │
//!                                          └──► PatternEngine ──► StorageHandle (events)
//! ```
//!
//! The runtime explicitly enforces that the observation loop never calls any
//! send/act APIs - it is purely passive.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{RwLock, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, instrument, warn};

use crate::config::{HotReloadableConfig, PaneFilterConfig};
use crate::crash::{HealthSnapshot, ShutdownSummary};
use crate::error::Result;
use crate::ingest::{PaneCursor, PaneRegistry, persist_captured_segment};
use crate::patterns::{Detection, DetectionContext, PatternEngine};
use crate::storage::{StorageHandle, StoredEvent};
use crate::tailer::{CaptureEvent, TailerConfig, TailerSupervisor};
use crate::wezterm::{PaneInfo, WeztermClient};

/// Configuration for the observation runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Polling interval for pane discovery
    pub discovery_interval: Duration,
    /// Maximum polling interval for content capture (idle panes)
    pub capture_interval: Duration,
    /// Minimum polling interval for content capture (active panes)
    pub min_capture_interval: Duration,
    /// Delta extraction overlap window size
    pub overlap_size: usize,
    /// Pane filter configuration
    pub pane_filter: PaneFilterConfig,
    /// Channel buffer size for internal queues
    pub channel_buffer: usize,
    /// Maximum concurrent capture operations
    pub max_concurrent_captures: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            discovery_interval: Duration::from_secs(5),
            capture_interval: Duration::from_millis(200),
            min_capture_interval: Duration::from_millis(50),
            overlap_size: 4096,
            pane_filter: PaneFilterConfig::default(),
            channel_buffer: 1024,
            max_concurrent_captures: 10,
        }
    }
}

/// Runtime metrics for health snapshots and shutdown summaries.
#[derive(Debug)]
pub struct RuntimeMetrics {
    /// Count of segments persisted
    segments_persisted: AtomicU64,
    /// Count of events recorded
    events_recorded: AtomicU64,
    /// Timestamp when runtime started (epoch ms)
    started_at: AtomicU64,
    /// Last DB write timestamp (epoch ms)
    last_db_write_at: AtomicU64,
    /// Sum of ingest lag samples (for averaging)
    ingest_lag_sum_ms: AtomicU64,
    /// Count of ingest lag samples
    ingest_lag_count: AtomicU64,
    /// Maximum ingest lag observed
    ingest_lag_max_ms: AtomicU64,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            segments_persisted: AtomicU64::new(0),
            events_recorded: AtomicU64::new(0),
            started_at: AtomicU64::new(0),
            last_db_write_at: AtomicU64::new(0),
            ingest_lag_sum_ms: AtomicU64::new(0),
            ingest_lag_count: AtomicU64::new(0),
            ingest_lag_max_ms: AtomicU64::new(0),
        }
    }
}

impl RuntimeMetrics {
    /// Record an ingest lag sample.
    pub fn record_ingest_lag(&self, lag_ms: u64) {
        self.ingest_lag_sum_ms.fetch_add(lag_ms, Ordering::SeqCst);
        self.ingest_lag_count.fetch_add(1, Ordering::SeqCst);

        // Update max using compare-and-swap loop
        let mut current_max = self.ingest_lag_max_ms.load(Ordering::SeqCst);
        while lag_ms > current_max {
            match self.ingest_lag_max_ms.compare_exchange_weak(
                current_max,
                lag_ms,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(v) => current_max = v,
            }
        }
    }

    /// Record a successful DB write.
    pub fn record_db_write(&self) {
        self.last_db_write_at
            .store(epoch_ms_u64(), Ordering::SeqCst);
    }

    /// Get average ingest lag in milliseconds.
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_ingest_lag_ms(&self) -> f64 {
        let sum = self.ingest_lag_sum_ms.load(Ordering::SeqCst);
        let count = self.ingest_lag_count.load(Ordering::SeqCst);
        if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        }
    }

    /// Get maximum ingest lag in milliseconds.
    pub fn max_ingest_lag_ms(&self) -> u64 {
        self.ingest_lag_max_ms.load(Ordering::SeqCst)
    }

    /// Get last DB write timestamp (epoch ms), or None if never written.
    pub fn last_db_write(&self) -> Option<u64> {
        let ts = self.last_db_write_at.load(Ordering::SeqCst);
        if ts == 0 { None } else { Some(ts) }
    }
}

/// The observation runtime orchestrates passive monitoring.
///
/// This runtime:
/// 1. Discovers panes via WezTerm CLI
/// 2. Captures content deltas from observed panes
/// 3. Persists segments and gaps to storage
/// 4. Runs pattern detection on new content
/// 5. Persists detection events to storage
///
/// The runtime is explicitly **read-only** - it never sends input to panes.
pub struct ObservationRuntime {
    /// Runtime configuration
    config: RuntimeConfig,
    /// Storage handle for persistence (wrapped for async sharing)
    storage: Arc<tokio::sync::Mutex<StorageHandle>>,
    /// Pattern detection engine
    pattern_engine: Arc<PatternEngine>,
    /// Pane registry for discovery and tracking
    registry: Arc<RwLock<PaneRegistry>>,
    /// Per-pane cursors for delta extraction
    cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Per-pane detection contexts for deduplication
    detection_contexts: Arc<RwLock<HashMap<u64, DetectionContext>>>,
    /// Shutdown flag for signaling tasks
    shutdown_flag: Arc<AtomicBool>,
    /// Runtime metrics for health/shutdown
    metrics: Arc<RuntimeMetrics>,
    /// Hot-reloadable config sender (for broadcasting updates to tasks)
    config_tx: watch::Sender<HotReloadableConfig>,
    /// Hot-reloadable config receiver (for tasks to receive updates)
    config_rx: watch::Receiver<HotReloadableConfig>,
}

impl ObservationRuntime {
    /// Create a new observation runtime.
    ///
    /// # Arguments
    /// * `config` - Runtime configuration
    /// * `storage` - Storage handle for persistence
    /// * `pattern_engine` - Pattern detection engine
    #[must_use]
    pub fn new(
        config: RuntimeConfig,
        storage: StorageHandle,
        pattern_engine: PatternEngine,
    ) -> Self {
        let registry = PaneRegistry::with_filter(config.pane_filter.clone());
        let metrics = Arc::new(RuntimeMetrics::default());
        metrics.started_at.store(epoch_ms_u64(), Ordering::SeqCst);

        // Initialize hot-reload config channel with current values
        let hot_config = HotReloadableConfig {
            log_level: "info".to_string(), // Default, will be overridden
            poll_interval_ms: duration_ms_u64(config.capture_interval),
            min_poll_interval_ms: duration_ms_u64(config.min_capture_interval),
            retention_days: 30,
            retention_max_mb: 0,
            checkpoint_interval_secs: 60,
            pattern_packs: vec![],
            workflows_enabled: vec![],
            auto_run_allowlist: vec![],
        };
        let (config_tx, config_rx) = watch::channel(hot_config);

        Self {
            config,
            storage: Arc::new(tokio::sync::Mutex::new(storage)),
            pattern_engine: Arc::new(pattern_engine),
            registry: Arc::new(RwLock::new(registry)),
            cursors: Arc::new(RwLock::new(HashMap::new())),
            detection_contexts: Arc::new(RwLock::new(HashMap::new())),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            metrics,
            config_tx,
            config_rx,
        }
    }

    /// Start the observation runtime.
    ///
    /// Returns handles for the spawned tasks. Call `shutdown()` to stop.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<RuntimeHandle> {
        info!("Starting observation runtime");

        let (capture_tx, capture_rx) = mpsc::channel::<CaptureEvent>(self.config.channel_buffer);

        // Spawn discovery task
        let discovery_handle = self.spawn_discovery_task();

        // Spawn capture tasks (will be dynamically managed based on discovered panes)
        let capture_handle = self.spawn_capture_task(capture_tx);

        // Spawn persistence and detection task
        let persistence_handle = self.spawn_persistence_task(capture_rx);

        info!("Observation runtime started");

        Ok(RuntimeHandle {
            discovery: discovery_handle,
            capture: capture_handle,
            persistence: persistence_handle,
            shutdown_flag: Arc::clone(&self.shutdown_flag),
            storage: Arc::clone(&self.storage),
            metrics: Arc::clone(&self.metrics),
            registry: Arc::clone(&self.registry),
            cursors: Arc::clone(&self.cursors),
            start_time: Instant::now(),
            config_tx: self.config_tx.clone(),
        })
    }

    /// Spawn the pane discovery task.
    fn spawn_discovery_task(&self) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let storage = Arc::clone(&self.storage);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let initial_interval = self.config.discovery_interval;
        let mut config_rx = self.config_rx.clone();

        tokio::spawn(async move {
            // Create a fresh WezTerm client for this task
            let wezterm = WeztermClient::new();
            let mut current_interval = initial_interval;

            loop {
                // Use sleep instead of interval to support dynamic interval changes
                tokio::time::sleep(current_interval).await;

                // Check shutdown flag
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Discovery task: shutdown signal received");
                    break;
                }

                // Check for config updates (non-blocking)
                if config_rx.has_changed().unwrap_or(false) {
                    let new_config = config_rx.borrow_and_update().clone();
                    let new_interval = Duration::from_millis(new_config.poll_interval_ms);
                    if new_interval != current_interval {
                        info!(
                            old_ms = duration_ms_u64(current_interval),
                            new_ms = duration_ms_u64(new_interval),
                            "Discovery interval updated via hot reload"
                        );
                        current_interval = new_interval;
                    }
                }

                match wezterm.list_panes().await {
                    Ok(panes) => {
                        let mut reg = registry.write().await;
                        let diff = reg.discovery_tick(panes);

                        // Handle new panes
                        for pane_id in &diff.new_panes {
                            if let Some(entry) = reg.get_entry(*pane_id) {
                                // Upsert pane in storage
                                let record = entry.to_pane_record();
                                let storage_guard = storage.lock().await;
                                if let Err(e) = storage_guard.upsert_pane(record).await {
                                    error!(pane_id = pane_id, error = %e, "Failed to upsert pane");
                                }
                                drop(storage_guard);

                                // Create cursor if observed
                                if entry.should_observe() {
                                    {
                                        let mut cursors = cursors.write().await;
                                        cursors.insert(*pane_id, PaneCursor::new(*pane_id));
                                    }

                                    {
                                        let mut contexts = detection_contexts.write().await;
                                        let mut ctx = DetectionContext::new();
                                        ctx.pane_id = Some(*pane_id);
                                        contexts.insert(*pane_id, ctx);
                                    }

                                    debug!(pane_id = pane_id, "Started observing pane");
                                } else if let Some(reason) = entry.observation.ignore_reason() {
                                    info!(
                                        pane_id = pane_id,
                                        reason = reason,
                                        "Pane ignored by observation filter"
                                    );
                                }
                            }
                        }

                        // Handle closed panes
                        for pane_id in &diff.closed_panes {
                            {
                                let mut cursors = cursors.write().await;
                                cursors.remove(pane_id);
                            }

                            {
                                let mut contexts = detection_contexts.write().await;
                                contexts.remove(pane_id);
                            }

                            debug!(pane_id = pane_id, "Stopped observing pane (closed)");
                        }

                        // Handle new generations (pane restarted)
                        for pane_id in &diff.new_generations {
                            {
                                let mut cursors = cursors.write().await;
                                cursors.insert(*pane_id, PaneCursor::new(*pane_id));
                            }

                            {
                                let mut contexts = detection_contexts.write().await;
                                let mut ctx = DetectionContext::new();
                                ctx.pane_id = Some(*pane_id);
                                contexts.insert(*pane_id, ctx);
                            }

                            debug!(
                                pane_id = pane_id,
                                "Restarted observing pane (new generation)"
                            );
                        }

                        if !diff.new_panes.is_empty()
                            || !diff.closed_panes.is_empty()
                            || !diff.new_generations.is_empty()
                        {
                            debug!(
                                new = diff.new_panes.len(),
                                closed = diff.closed_panes.len(),
                                restarted = diff.new_generations.len(),
                                "Pane discovery tick"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to list panes");
                    }
                }
            }
        })
    }

    /// Spawn the content capture task using TailerSupervisor with adaptive polling.
    ///
    /// This task manages per-pane tailers that:
    /// - Poll fast when output is changing (min_capture_interval)
    /// - Poll slow when idle (capture_interval)
    /// - Respect concurrency limits (max_concurrent_captures)
    /// - Handle backpressure from downstream
    fn spawn_capture_task(&self, capture_tx: mpsc::Sender<CaptureEvent>) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let discovery_interval = self.config.discovery_interval;

        // Create tailer config from runtime config
        let tailer_config = TailerConfig {
            min_interval: self.config.min_capture_interval,
            max_interval: self.config.capture_interval,
            backoff_multiplier: 1.5,
            max_concurrent: self.config.max_concurrent_captures,
            overlap_size: self.config.overlap_size,
            send_timeout: Duration::from_millis(100),
        };

        tokio::spawn(async move {
            let poll_interval = tailer_config.min_interval;
            let source = Arc::new(WeztermClient::new());
            // Create tailer supervisor
            let mut supervisor = TailerSupervisor::new(
                tailer_config,
                capture_tx,
                cursors,
                Arc::clone(&shutdown_flag),
                source,
            );

            // Sync tailers periodically with discovery interval
            let mut sync_tick = tokio::time::interval(discovery_interval);
            let mut poll_tick = tokio::time::interval(poll_interval);

            loop {
                tokio::select! {
                    _ = sync_tick.tick() => {
                        if shutdown_flag.load(Ordering::SeqCst) {
                            debug!("Capture task: shutdown signal received");
                            break;
                        }

                        // Get current observed panes from registry
                        let observed_panes: HashMap<u64, PaneInfo> = {
                            let reg = registry.read().await;
                            reg.observed_pane_ids()
                                .into_iter()
                                .filter_map(|id| reg.get_entry(id).map(|e| (id, e.info.clone())))
                                .collect()
                        };

                        supervisor.sync_tailers(&observed_panes);

                        debug!(
                            active_tailers = supervisor.active_count(),
                            observed_panes = observed_panes.len(),
                            "Tailer sync tick"
                        );
                    }
                    _ = poll_tick.tick() => {
                        supervisor.poll_once().await;
                    }
                }
            }

            // Graceful shutdown of all tailers
            supervisor.shutdown().await;
        })
    }

    /// Spawn the persistence and detection task.
    fn spawn_persistence_task(
        &self,
        mut capture_rx: mpsc::Receiver<CaptureEvent>,
    ) -> JoinHandle<()> {
        let storage = Arc::clone(&self.storage);
        let pattern_engine = Arc::clone(&self.pattern_engine);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let metrics = Arc::clone(&self.metrics);

        tokio::spawn(async move {
            // Process events until channel closes or shutdown
            while let Some(event) = capture_rx.recv().await {
                // Check shutdown flag - if set, drain remaining events quickly
                if shutdown_flag.load(Ordering::SeqCst) {
                    debug!("Persistence task: shutdown signal received, draining remaining events");
                    // Continue to drain but don't block forever
                }
                let pane_id = event.segment.pane_id;
                let content = event.segment.content.clone();
                let captured_at = event.segment.captured_at;

                // Persist the segment
                let storage_guard = storage.lock().await;
                match persist_captured_segment(&storage_guard, &event.segment).await {
                    Ok(persisted) => {
                        // Track metrics
                        metrics.segments_persisted.fetch_add(1, Ordering::SeqCst);

                        // Record ingest lag (time from capture to persistence)
                        let now = epoch_ms();
                        let lag_ms = u64::try_from((now - captured_at).max(0)).unwrap_or(0);
                        metrics.record_ingest_lag(lag_ms);
                        metrics.record_db_write();

                        debug!(
                            pane_id = pane_id,
                            seq = persisted.segment.seq,
                            has_gap = persisted.gap.is_some(),
                            "Persisted segment"
                        );

                        // Run pattern detection on the content
                        let detections = {
                            let mut contexts = detection_contexts.write().await;
                            let ctx = contexts.entry(pane_id).or_insert_with(|| {
                                let mut c = DetectionContext::new();
                                c.pane_id = Some(pane_id);
                                c
                            });
                            let detections = pattern_engine.detect_with_context(&content, ctx);
                            drop(contexts);
                            detections
                        };

                        if !detections.is_empty() {
                            debug!(
                                pane_id = pane_id,
                                count = detections.len(),
                                "Pattern detections"
                            );

                            // Persist each detection as an event
                            for detection in detections {
                                let stored_event = detection_to_stored_event(
                                    pane_id,
                                    &detection,
                                    Some(persisted.segment.id),
                                );

                                match storage_guard.record_event(stored_event).await {
                                    Ok(_) => {
                                        metrics.events_recorded.fetch_add(1, Ordering::SeqCst);
                                    }
                                    Err(e) => {
                                        error!(
                                            pane_id = pane_id,
                                            rule_id = detection.rule_id,
                                            error = %e,
                                            "Failed to record event"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(pane_id = pane_id, error = %e, "Failed to persist segment");
                    }
                }
                drop(storage_guard);
            }
        })
    }

    /// Signal tasks to begin shutdown.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
    }

    /// Take ownership of the storage handle for external shutdown.
    ///
    /// Returns the storage handle wrapped in Arc<Mutex>. The caller is
    /// responsible for shutdown. This invalidates the runtime.
    #[must_use]
    pub fn take_storage(self) -> Arc<tokio::sync::Mutex<StorageHandle>> {
        self.storage
    }
}

/// Handle to the running observation runtime.
pub struct RuntimeHandle {
    /// Discovery task handle
    pub discovery: JoinHandle<()>,
    /// Capture task handle
    pub capture: JoinHandle<()>,
    /// Persistence task handle
    pub persistence: JoinHandle<()>,
    /// Shutdown flag for signaling tasks
    pub shutdown_flag: Arc<AtomicBool>,
    /// Storage handle for external access
    pub storage: Arc<tokio::sync::Mutex<StorageHandle>>,
    /// Runtime metrics
    pub metrics: Arc<RuntimeMetrics>,
    /// Pane registry
    pub registry: Arc<RwLock<PaneRegistry>>,
    /// Per-pane cursors
    pub cursors: Arc<RwLock<HashMap<u64, PaneCursor>>>,
    /// Runtime start time
    pub start_time: Instant,
    /// Hot-reload config sender for broadcasting updates
    config_tx: watch::Sender<HotReloadableConfig>,
}

impl RuntimeHandle {
    /// Wait for all tasks to complete.
    pub async fn join(self) {
        let _ = self.discovery.await;
        let _ = self.capture.await;
        let _ = self.persistence.await;
    }

    /// Request graceful shutdown and collect a summary.
    ///
    /// This method:
    /// 1. Sets the shutdown flag to signal all tasks
    /// 2. Waits for tasks to complete (with timeout)
    /// 3. Flushes storage
    /// 4. Collects and returns a shutdown summary
    pub async fn shutdown_with_summary(self) -> ShutdownSummary {
        let elapsed_secs = self.start_time.elapsed().as_secs();
        let mut warnings = Vec::new();

        // Signal shutdown
        self.shutdown_flag.store(true, Ordering::SeqCst);
        info!("Shutdown signal sent");

        // Wait for tasks with timeout
        let timeout = Duration::from_secs(5);
        let join_result = tokio::time::timeout(timeout, async {
            let _ = self.discovery.await;
            let _ = self.capture.await;
            let _ = self.persistence.await;
        })
        .await;

        let clean = if join_result.is_err() {
            warnings.push("Tasks did not complete within timeout".to_string());
            false
        } else {
            true
        };

        // Get final metrics
        let segments_persisted = self.metrics.segments_persisted.load(Ordering::SeqCst);
        let events_recorded = self.metrics.events_recorded.load(Ordering::SeqCst);

        // Get last seq per pane
        let last_seq_by_pane: Vec<(u64, i64)> = {
            let cursors = self.cursors.read().await;
            cursors
                .iter()
                .map(|(pane_id, cursor)| (*pane_id, cursor.last_seq()))
                .collect()
        };

        // Flush storage
        {
            let storage_guard = self.storage.lock().await;
            if let Err(e) = storage_guard.shutdown().await {
                warnings.push(format!("Storage shutdown error: {e}"));
            }
        }

        ShutdownSummary {
            elapsed_secs,
            final_capture_queue: 0, // Channel is consumed
            final_write_queue: 0,
            segments_persisted,
            events_recorded,
            last_seq_by_pane,
            clean,
            warnings,
        }
    }

    /// Request graceful shutdown.
    ///
    /// Sets the shutdown flag and waits for tasks to complete.
    pub async fn shutdown(self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        self.join().await;
    }

    /// Signal shutdown without waiting.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
    }

    /// Update the global health snapshot from current runtime state.
    ///
    /// Call this periodically (e.g., every 30s) to keep crash reports useful.
    pub async fn update_health_snapshot(&self) {
        let observed_panes = {
            let reg = self.registry.read().await;
            reg.observed_pane_ids().len()
        };

        let last_seq_by_pane: Vec<(u64, i64)> = {
            let cursors = self.cursors.read().await;
            cursors
                .iter()
                .map(|(pane_id, cursor)| (*pane_id, cursor.last_seq()))
                .collect()
        };

        // Check DB writability with a lightweight health check
        let db_writable = {
            let storage_guard = self.storage.lock().await;
            storage_guard.is_writable().await
        };

        let snapshot = HealthSnapshot {
            timestamp: epoch_ms_u64(),
            observed_panes,
            capture_queue_depth: 0, // Not easily accessible after start
            write_queue_depth: 0,
            last_seq_by_pane,
            warnings: vec![],
            ingest_lag_avg_ms: self.metrics.avg_ingest_lag_ms(),
            ingest_lag_max_ms: self.metrics.max_ingest_lag_ms(),
            db_writable,
            db_last_write_at: self.metrics.last_db_write(),
        };

        HealthSnapshot::update_global(snapshot);
    }

    /// Take ownership of the storage handle for external shutdown.
    ///
    /// The caller is responsible for shutdown. This invalidates the runtime.
    #[must_use]
    pub fn take_storage(self) -> Arc<tokio::sync::Mutex<StorageHandle>> {
        self.storage
    }

    /// Apply a hot-reloadable config update.
    ///
    /// Broadcasts the new config to all running tasks. Returns `Ok(())` if the
    /// update was sent successfully, or an error if the channel is closed.
    ///
    /// # Arguments
    /// * `new_config` - The new hot-reloadable configuration values
    ///
    /// # Errors
    /// Returns an error if the config channel is closed (runtime shutting down).
    pub fn apply_config_update(&self, new_config: HotReloadableConfig) -> Result<()> {
        self.config_tx
            .send(new_config)
            .map_err(|e| crate::Error::Runtime(format!("Failed to send config update: {e}")))
    }

    /// Get the current hot-reloadable config.
    #[must_use]
    pub fn current_config(&self) -> HotReloadableConfig {
        self.config_tx.borrow().clone()
    }
}

/// Get current time as epoch milliseconds.
fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn epoch_ms_u64() -> u64 {
    u64::try_from(epoch_ms()).unwrap_or(0)
}

fn duration_ms_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Convert a Detection to a StoredEvent for persistence.
fn detection_to_stored_event(
    pane_id: u64,
    detection: &Detection,
    segment_id: Option<i64>,
) -> StoredEvent {
    StoredEvent {
        id: 0, // Will be assigned by storage
        pane_id,
        rule_id: detection.rule_id.clone(),
        agent_type: format!("{:?}", detection.agent_type),
        event_type: detection.event_type.clone(),
        severity: format!("{:?}", detection.severity),
        confidence: detection.confidence,
        extracted: Some(detection.extracted.clone()),
        matched_text: Some(detection.matched_text.clone()),
        segment_id,
        detected_at: epoch_ms(),
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::PaneRecord;
    use tempfile::TempDir;

    fn temp_db_path() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db").to_string_lossy().to_string();
        (dir, path)
    }

    #[allow(dead_code)]
    fn test_pane_record(pane_id: u64) -> PaneRecord {
        PaneRecord {
            pane_id,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("test".to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            first_seen_at: epoch_ms(),
            last_seen_at: epoch_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    #[test]
    fn detection_to_stored_event_converts_correctly() {
        use crate::patterns::{AgentType, Severity};

        let detection = Detection {
            rule_id: "test.rule".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "test_event".to_string(),
            severity: Severity::Info,
            confidence: 0.95,
            extracted: serde_json::json!({"key": "value"}),
            matched_text: "matched text".to_string(),
        };

        let event = detection_to_stored_event(42, &detection, Some(123));

        assert_eq!(event.pane_id, 42);
        assert_eq!(event.rule_id, "test.rule");
        assert_eq!(event.event_type, "test_event");
        assert!((event.confidence - 0.95).abs() < f64::EPSILON);
        assert_eq!(event.segment_id, Some(123));
        assert!(event.handled_at.is_none());
    }

    #[tokio::test]
    async fn runtime_config_defaults_are_reasonable() {
        let config = RuntimeConfig::default();

        assert_eq!(config.discovery_interval, Duration::from_secs(5));
        assert_eq!(config.capture_interval, Duration::from_millis(200));
        assert_eq!(config.overlap_size, 4096);
        assert_eq!(config.channel_buffer, 1024);
    }

    #[tokio::test]
    async fn runtime_can_be_created() {
        let (_dir, db_path) = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();
        let engine = PatternEngine::new();

        let config = RuntimeConfig::default();
        let _runtime = ObservationRuntime::new(config, storage, engine);
    }

    #[test]
    fn runtime_metrics_records_ingest_lag() {
        let metrics = RuntimeMetrics::default();

        // Initially no samples
        assert!((metrics.avg_ingest_lag_ms() - 0.0).abs() < f64::EPSILON);
        assert_eq!(metrics.max_ingest_lag_ms(), 0);

        // Record some samples
        metrics.record_ingest_lag(10);
        metrics.record_ingest_lag(20);
        metrics.record_ingest_lag(30);

        // Verify average
        assert!((metrics.avg_ingest_lag_ms() - 20.0).abs() < f64::EPSILON);

        // Verify max
        assert_eq!(metrics.max_ingest_lag_ms(), 30);
    }

    #[test]
    fn runtime_metrics_tracks_max_correctly_with_decreasing_values() {
        let metrics = RuntimeMetrics::default();

        // Record high value first
        metrics.record_ingest_lag(100);
        assert_eq!(metrics.max_ingest_lag_ms(), 100);

        // Lower values shouldn't change max
        metrics.record_ingest_lag(50);
        metrics.record_ingest_lag(25);
        assert_eq!(metrics.max_ingest_lag_ms(), 100);

        // Higher value should update max
        metrics.record_ingest_lag(150);
        assert_eq!(metrics.max_ingest_lag_ms(), 150);
    }

    #[test]
    fn runtime_metrics_last_db_write() {
        let metrics = RuntimeMetrics::default();

        // Initially no writes
        assert!(metrics.last_db_write().is_none());

        // Record a write
        metrics.record_db_write();

        // Should now have a timestamp
        assert!(metrics.last_db_write().is_some());
        assert!(metrics.last_db_write().unwrap() > 0);
    }

    #[test]
    fn health_snapshot_reflects_runtime_metrics() {
        use crate::crash::HealthSnapshot;

        let metrics = RuntimeMetrics::default();
        metrics.record_ingest_lag(10);
        metrics.record_ingest_lag(50);
        metrics.record_db_write();

        let snapshot = HealthSnapshot {
            timestamp: 0,
            observed_panes: 2,
            capture_queue_depth: 0,
            write_queue_depth: 0,
            last_seq_by_pane: vec![],
            warnings: vec![],
            ingest_lag_avg_ms: metrics.avg_ingest_lag_ms(),
            ingest_lag_max_ms: metrics.max_ingest_lag_ms(),
            db_writable: true,
            db_last_write_at: metrics.last_db_write(),
        };

        // Verify metrics are correctly reflected in snapshot
        assert!((snapshot.ingest_lag_avg_ms - 30.0).abs() < f64::EPSILON);
        assert_eq!(snapshot.ingest_lag_max_ms, 50);
        assert!(snapshot.db_writable);
        assert!(snapshot.db_last_write_at.is_some());
    }
}
