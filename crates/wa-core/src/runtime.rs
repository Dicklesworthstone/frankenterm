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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, instrument, warn};

use crate::config::PaneFilterConfig;
use crate::error::Result;
use crate::ingest::{persist_captured_segment, CapturedSegment, PaneCursor, PaneRegistry};
use crate::patterns::{Detection, DetectionContext, PatternEngine};
use crate::storage::{StorageHandle, StoredEvent};
use crate::wezterm::{PaneInfo, WeztermClient};

/// Configuration for the observation runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Polling interval for pane discovery
    pub discovery_interval: Duration,
    /// Polling interval for content capture
    pub capture_interval: Duration,
    /// Delta extraction overlap window size
    pub overlap_size: usize,
    /// Pane filter configuration
    pub pane_filter: PaneFilterConfig,
    /// Channel buffer size for internal queues
    pub channel_buffer: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            discovery_interval: Duration::from_secs(5),
            capture_interval: Duration::from_millis(200),
            overlap_size: 4096,
            pane_filter: PaneFilterConfig::default(),
            channel_buffer: 1024,
        }
    }
}

/// Internal message for segment capture events.
#[derive(Debug)]
struct CaptureEvent {
    segment: CapturedSegment,
    #[allow(dead_code)]
    pane_info: PaneInfo,
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
    /// Shutdown signal sender
    shutdown_tx: Option<mpsc::Sender<()>>,
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

        Self {
            config,
            storage: Arc::new(tokio::sync::Mutex::new(storage)),
            pattern_engine: Arc::new(pattern_engine),
            registry: Arc::new(RwLock::new(registry)),
            cursors: Arc::new(RwLock::new(HashMap::new())),
            detection_contexts: Arc::new(RwLock::new(HashMap::new())),
            shutdown_tx: None,
        }
    }

    /// Start the observation runtime.
    ///
    /// Returns handles for the spawned tasks. Call `shutdown()` to stop.
    #[instrument(skip(self))]
    pub async fn start(&mut self) -> Result<RuntimeHandle> {
        info!("Starting observation runtime");

        let (shutdown_tx, _shutdown_rx) = mpsc::channel::<()>(1);
        let (capture_tx, capture_rx) = mpsc::channel::<CaptureEvent>(self.config.channel_buffer);

        self.shutdown_tx = Some(shutdown_tx.clone());

        // Spawn discovery task
        let discovery_handle = self.spawn_discovery_task(shutdown_tx.clone());

        // Spawn capture tasks (will be dynamically managed based on discovered panes)
        let capture_handle = self.spawn_capture_task(capture_tx, shutdown_tx.clone());

        // Spawn persistence and detection task
        let persistence_handle = self.spawn_persistence_task(capture_rx, shutdown_tx.clone());

        info!("Observation runtime started");

        Ok(RuntimeHandle {
            discovery: discovery_handle,
            capture: capture_handle,
            persistence: persistence_handle,
            shutdown_tx,
        })
    }

    /// Spawn the pane discovery task.
    fn spawn_discovery_task(&self, _shutdown_tx: mpsc::Sender<()>) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let detection_contexts = Arc::clone(&self.detection_contexts);
        let storage = Arc::clone(&self.storage);
        let interval = self.config.discovery_interval;

        tokio::spawn(async move {
            // Create a fresh WezTerm client for this task
            let wezterm = WeztermClient::new();
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

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

                            debug!(pane_id = pane_id, "Restarted observing pane (new generation)");
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

    /// Spawn the content capture task.
    fn spawn_capture_task(
        &self,
        capture_tx: mpsc::Sender<CaptureEvent>,
        _shutdown_tx: mpsc::Sender<()>,
    ) -> JoinHandle<()> {
        let registry = Arc::clone(&self.registry);
        let cursors = Arc::clone(&self.cursors);
        let interval = self.config.capture_interval;
        let overlap_size = self.config.overlap_size;

        tokio::spawn(async move {
            // Create a fresh WezTerm client for this task
            let wezterm = WeztermClient::new();
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

                // Get list of observed panes and their info
                let reg = registry.read().await;
                let observed_panes: Vec<(u64, PaneInfo)> = reg
                    .observed_pane_ids()
                    .into_iter()
                    .filter_map(|id| reg.get_entry(id).map(|e| (id, e.info.clone())))
                    .collect();
                drop(reg);

                for (pane_id, pane_info) in observed_panes {
                    // Get pane content (no escapes)
                    match wezterm.get_text(pane_id, false).await {
                        Ok(content) => {
                            // Try to extract delta
                            let mut cursors = cursors.write().await;
                            if let Some(cursor) = cursors.get_mut(&pane_id) {
                                if let Some(segment) =
                                    cursor.capture_snapshot(&content, overlap_size)
                                {
                                    let event = CaptureEvent {
                                        segment,
                                        pane_info: pane_info.clone(),
                                    };

                                    if capture_tx.send(event).await.is_err() {
                                        error!(pane_id = pane_id, "Capture channel closed");
                                        return;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            debug!(pane_id = pane_id, error = %e, "Failed to get pane text");
                        }
                    }
                }
            }
        })
    }

    /// Spawn the persistence and detection task.
    fn spawn_persistence_task(
        &self,
        mut capture_rx: mpsc::Receiver<CaptureEvent>,
        _shutdown_tx: mpsc::Sender<()>,
    ) -> JoinHandle<()> {
        let storage = Arc::clone(&self.storage);
        let pattern_engine = Arc::clone(&self.pattern_engine);
        let detection_contexts = Arc::clone(&self.detection_contexts);

        tokio::spawn(async move {
            while let Some(event) = capture_rx.recv().await {
                let pane_id = event.segment.pane_id;
                let content = event.segment.content.clone();

                // Persist the segment
                let storage_guard = storage.lock().await;
                match persist_captured_segment(&storage_guard, &event.segment).await {
                    Ok(persisted) => {
                        debug!(
                            pane_id = pane_id,
                            seq = persisted.segment.seq,
                            has_gap = persisted.gap.is_some(),
                            "Persisted segment"
                        );

                        // Run pattern detection on the content
                        #[allow(clippy::significant_drop_tightening)]
                        let detections = {
                            let mut contexts = detection_contexts.write().await;
                            let ctx = contexts.entry(pane_id).or_insert_with(|| {
                                let mut c = DetectionContext::new();
                                c.pane_id = Some(pane_id);
                                c
                            });
                            pattern_engine.detect_with_context(&content, ctx)
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

                                if let Err(e) = storage_guard.record_event(stored_event).await {
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
                    Err(e) => {
                        error!(pane_id = pane_id, error = %e, "Failed to persist segment");
                    }
                }
                drop(storage_guard);
            }
        })
    }

    /// Request graceful shutdown.
    ///
    /// Note: This signals tasks to stop but does not shut down storage.
    /// The caller should separately shut down the storage handle if needed.
    pub async fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
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
    /// Shutdown signal sender
    pub shutdown_tx: mpsc::Sender<()>,
}

impl RuntimeHandle {
    /// Wait for all tasks to complete.
    pub async fn join(self) {
        let _ = self.discovery.await;
        let _ = self.capture.await;
        let _ = self.persistence.await;
    }

    /// Request graceful shutdown.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(()).await;
        self.join().await;
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
}
