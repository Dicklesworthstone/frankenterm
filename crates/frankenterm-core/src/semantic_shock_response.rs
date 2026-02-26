//! Semantic Shock Response & Operator Alerts — translates conformal anomaly
//! shocks into actionable user protection.
//!
//! Bead: ft-344j8.15
//!
//! Consumes `Event::PatternDetected` events (with `event_type == "semantic_anomaly"`)
//! from the `EventBus` and:
//!
//! 1. **Alert mode**: Records the shock, increments counters, surfaces it for
//!    TUI dashboard display, and optionally triggers desktop notifications.
//! 2. **Pause mode**: Additionally creates a `TraumaDecision` that revokes
//!    command execution privileges for the affected pane until the operator
//!    clears the anomaly.
//!
//! The operator clears shocks via `clear_pane()` or `clear_all()`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::patterns::Detection;
use crate::semantic_anomaly::ConformalShock;
use crate::trauma_guard::TraumaDecision;

// =============================================================================
// Configuration
// =============================================================================

/// Action to take when a semantic anomaly is detected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShockAction {
    /// Alert the operator but do NOT block commands.
    #[default]
    Alert,
    /// Alert the operator AND pause command execution in the affected pane.
    Pause,
}

/// Configuration for the semantic shock response system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SemanticShockConfig {
    /// Enable/disable the semantic shock response system.
    pub enabled: bool,
    /// Action to take on shock detection.
    pub action: ShockAction,
    /// Minimum p-value to trigger a shock response. Shocks with p_value above
    /// this threshold are ignored. Default: 0.01 (1%).
    pub p_value_threshold: f64,
    /// Maximum number of active shocks to track per pane. Oldest are evicted
    /// when exceeded. Default: 10.
    pub max_shocks_per_pane: usize,
    /// Auto-clear shocks older than this many seconds. 0 = never auto-clear.
    /// Default: 300 (5 minutes).
    pub auto_clear_seconds: u64,
    /// Minimum time between notifications for the same pane (seconds).
    /// Default: 30.
    pub notification_cooldown_seconds: u64,
}

impl Default for SemanticShockConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            action: ShockAction::Alert,
            p_value_threshold: 0.01,
            max_shocks_per_pane: 10,
            auto_clear_seconds: 300,
            notification_cooldown_seconds: 30,
        }
    }
}

// =============================================================================
// Shock record
// =============================================================================

/// A recorded semantic shock for operator review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShockRecord {
    /// Pane where the anomaly was detected.
    pub pane_id: u64,
    /// The conformal shock details.
    pub shock: ConformalShock,
    /// Size of the triggering segment (bytes).
    pub segment_len: usize,
    /// Rule ID from the detection.
    pub rule_id: String,
    /// Confidence score from detection.
    pub confidence: f64,
    /// Monotonic record index.
    pub sequence: u64,
    /// Milliseconds since the responder was created.
    pub age_ms: u64,
}

/// Summary of active shocks for a single pane (for TUI display).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneShockSummary {
    /// Pane ID.
    pub pane_id: u64,
    /// Number of active (un-cleared) shocks.
    pub active_count: usize,
    /// Most recent shock.
    pub latest: Option<ShockRecord>,
    /// Whether the pane is currently paused due to shocks.
    pub is_paused: bool,
}

// =============================================================================
// Metrics
// =============================================================================

/// Atomic counters for shock response activity.
#[derive(Debug)]
pub struct ShockResponseMetrics {
    /// Total detections received (including filtered).
    pub detections_received: AtomicU64,
    /// Detections filtered (p_value above threshold).
    pub detections_filtered: AtomicU64,
    /// Shocks recorded.
    pub shocks_recorded: AtomicU64,
    /// Panes paused.
    pub panes_paused: AtomicU64,
    /// Panes cleared by operator.
    pub panes_cleared: AtomicU64,
    /// Notifications sent.
    pub notifications_sent: AtomicU64,
    /// Notifications suppressed (cooldown).
    pub notifications_suppressed: AtomicU64,
    /// Auto-cleared shocks.
    pub auto_cleared: AtomicU64,
}

impl ShockResponseMetrics {
    fn new() -> Self {
        Self {
            detections_received: AtomicU64::new(0),
            detections_filtered: AtomicU64::new(0),
            shocks_recorded: AtomicU64::new(0),
            panes_paused: AtomicU64::new(0),
            panes_cleared: AtomicU64::new(0),
            notifications_sent: AtomicU64::new(0),
            notifications_suppressed: AtomicU64::new(0),
            auto_cleared: AtomicU64::new(0),
        }
    }

    /// Snapshot metrics into a serializable form.
    pub fn snapshot(&self) -> ShockResponseMetricsSnapshot {
        ShockResponseMetricsSnapshot {
            detections_received: self.detections_received.load(Ordering::Relaxed),
            detections_filtered: self.detections_filtered.load(Ordering::Relaxed),
            shocks_recorded: self.shocks_recorded.load(Ordering::Relaxed),
            panes_paused: self.panes_paused.load(Ordering::Relaxed),
            panes_cleared: self.panes_cleared.load(Ordering::Relaxed),
            notifications_sent: self.notifications_sent.load(Ordering::Relaxed),
            notifications_suppressed: self.notifications_suppressed.load(Ordering::Relaxed),
            auto_cleared: self.auto_cleared.load(Ordering::Relaxed),
        }
    }
}

/// Serializable snapshot of shock response metrics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShockResponseMetricsSnapshot {
    pub detections_received: u64,
    pub detections_filtered: u64,
    pub shocks_recorded: u64,
    pub panes_paused: u64,
    pub panes_cleared: u64,
    pub notifications_sent: u64,
    pub notifications_suppressed: u64,
    pub auto_cleared: u64,
}

// =============================================================================
// Per-pane state
// =============================================================================

/// Per-pane shock tracking state.
#[derive(Debug)]
struct PaneShockState {
    /// Active (un-cleared) shock records, newest last.
    shocks: Vec<ShockRecord>,
    /// Whether the pane is currently paused.
    paused: bool,
    /// Instant of last notification sent for this pane.
    last_notification: Option<Instant>,
}

impl PaneShockState {
    fn new() -> Self {
        Self {
            shocks: Vec::new(),
            paused: false,
            last_notification: None,
        }
    }

    /// Evict shocks that have auto-expired.
    fn auto_clear(&mut self, auto_clear_ms: u64, now_ms: u64) -> usize {
        if auto_clear_ms == 0 {
            return 0;
        }
        let before = self.shocks.len();
        self.shocks.retain(|s| {
            // Keep if the shock is younger than the auto-clear window.
            now_ms.saturating_sub(s.age_ms) < auto_clear_ms
        });
        let cleared = before - self.shocks.len();
        if self.shocks.is_empty() {
            self.paused = false;
        }
        cleared
    }

    fn summary(&self, pane_id: u64) -> PaneShockSummary {
        PaneShockSummary {
            pane_id,
            active_count: self.shocks.len(),
            latest: self.shocks.last().cloned(),
            is_paused: self.paused,
        }
    }
}

// =============================================================================
// Responder
// =============================================================================

/// Notification callback result.
#[derive(Debug, Clone)]
pub struct ShockNotification {
    /// Pane affected.
    pub pane_id: u64,
    /// The shock record.
    pub record: ShockRecord,
    /// Whether the pane was paused.
    pub paused: bool,
}

/// The semantic shock responder — consumes detections and manages operator
/// alerts and policy enforcement.
///
/// Thread-safe via interior `RwLock`.
pub struct SemanticShockResponder {
    config: SemanticShockConfig,
    /// Per-pane shock state.
    panes: RwLock<HashMap<u64, PaneShockState>>,
    /// Global metrics.
    metrics: Arc<ShockResponseMetrics>,
    /// Monotonic sequence counter.
    sequence: AtomicU64,
    /// Creation instant for relative timing.
    created_at: Instant,
}

impl std::fmt::Debug for SemanticShockResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticShockResponder")
            .field("config", &self.config)
            .field(
                "panes_tracked",
                &self.panes.read().map(|p| p.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl SemanticShockResponder {
    /// Create a new responder with the given configuration.
    pub fn new(config: SemanticShockConfig) -> Self {
        Self {
            config,
            panes: RwLock::new(HashMap::new()),
            metrics: Arc::new(ShockResponseMetrics::new()),
            sequence: AtomicU64::new(0),
            created_at: Instant::now(),
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &SemanticShockConfig {
        &self.config
    }

    /// Get the metrics handle.
    pub fn metrics(&self) -> &ShockResponseMetrics {
        &self.metrics
    }

    /// Get a metrics snapshot.
    pub fn metrics_snapshot(&self) -> ShockResponseMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Process a detection event from the EventBus.
    ///
    /// Returns `Some(ShockNotification)` if a notification should be sent,
    /// `None` if the detection was filtered, not a semantic anomaly, or
    /// suppressed by cooldown.
    pub fn handle_detection(
        &self,
        pane_id: u64,
        detection: &Detection,
    ) -> Option<ShockNotification> {
        if !self.config.enabled {
            return None;
        }

        self.metrics
            .detections_received
            .fetch_add(1, Ordering::Relaxed);

        // Only handle semantic anomaly detections.
        if detection.event_type != "semantic_anomaly" {
            return None;
        }

        // Extract the shock from the detection's extracted data.
        let shock = extract_shock_from_detection(detection)?;

        // Filter by p-value threshold.
        if shock.p_value > self.config.p_value_threshold {
            self.metrics
                .detections_filtered
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // Extract segment_len if available.
        let segment_len = detection
            .extracted
            .get("segment_len")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        let age_ms = u64::try_from(self.created_at.elapsed().as_millis()).unwrap_or(u64::MAX);

        let record = ShockRecord {
            pane_id,
            shock,
            segment_len,
            rule_id: detection.rule_id.clone(),
            confidence: detection.confidence,
            sequence: seq,
            age_ms,
        };

        let now = Instant::now();
        let cooldown = std::time::Duration::from_secs(self.config.notification_cooldown_seconds);
        let auto_clear_ms = self.config.auto_clear_seconds * 1000;

        let mut panes = self.panes.write().ok()?;
        let state = panes.entry(pane_id).or_insert_with(PaneShockState::new);

        // Auto-clear expired shocks.
        let auto_cleared = state.auto_clear(auto_clear_ms, age_ms);
        if auto_cleared > 0 {
            self.metrics
                .auto_cleared
                .fetch_add(auto_cleared as u64, Ordering::Relaxed);
        }

        // Add the new shock.
        state.shocks.push(record.clone());
        self.metrics.shocks_recorded.fetch_add(1, Ordering::Relaxed);

        // Enforce max shocks per pane.
        while state.shocks.len() > self.config.max_shocks_per_pane {
            state.shocks.remove(0);
        }

        // Pause the pane if configured.
        let paused = if self.config.action == ShockAction::Pause && !state.paused {
            state.paused = true;
            self.metrics.panes_paused.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            state.paused
        };

        // Check notification cooldown.
        let should_notify = match state.last_notification {
            Some(last) if now.duration_since(last) < cooldown => {
                self.metrics
                    .notifications_suppressed
                    .fetch_add(1, Ordering::Relaxed);
                false
            }
            _ => {
                state.last_notification = Some(now);
                self.metrics
                    .notifications_sent
                    .fetch_add(1, Ordering::Relaxed);
                true
            }
        };

        if should_notify {
            Some(ShockNotification {
                pane_id,
                record,
                paused,
            })
        } else {
            None
        }
    }

    /// Build a `TraumaDecision` for policy engine integration.
    ///
    /// Returns a decision with `should_intervene = true` if the pane is paused.
    pub fn trauma_decision_for_pane(&self, pane_id: u64) -> TraumaDecision {
        let paused = self
            .panes
            .read()
            .ok()
            .and_then(|panes| panes.get(&pane_id).map(|s| s.paused))
            .unwrap_or(false);

        if paused {
            TraumaDecision {
                should_intervene: true,
                reason_code: Some("semantic_anomaly_pause".to_string()),
                command_hash: 0,
                repeat_count: 0,
                recurring_signatures: vec!["semantic_anomaly".to_string()],
            }
        } else {
            TraumaDecision {
                should_intervene: false,
                reason_code: None,
                command_hash: 0,
                repeat_count: 0,
                recurring_signatures: Vec::new(),
            }
        }
    }

    /// Check if a pane is currently paused due to semantic shocks.
    pub fn is_pane_paused(&self, pane_id: u64) -> bool {
        self.panes
            .read()
            .ok()
            .and_then(|panes| panes.get(&pane_id).map(|s| s.paused))
            .unwrap_or(false)
    }

    /// Get summary for a specific pane.
    pub fn pane_summary(&self, pane_id: u64) -> Option<PaneShockSummary> {
        self.panes
            .read()
            .ok()
            .and_then(|panes| panes.get(&pane_id).map(|s| s.summary(pane_id)))
    }

    /// Get summaries for all panes with active shocks.
    pub fn all_summaries(&self) -> Vec<PaneShockSummary> {
        self.panes
            .read()
            .ok()
            .map(|panes| {
                panes
                    .iter()
                    .filter(|(_, s)| !s.shocks.is_empty())
                    .map(|(&pid, s)| s.summary(pid))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clear all shocks for a pane (operator review complete).
    ///
    /// Returns `true` if the pane was found and cleared.
    pub fn clear_pane(&self, pane_id: u64) -> bool {
        if let Ok(mut panes) = self.panes.write() {
            if let Some(state) = panes.get_mut(&pane_id) {
                state.shocks.clear();
                if state.paused {
                    state.paused = false;
                    self.metrics.panes_cleared.fetch_add(1, Ordering::Relaxed);
                }
                return true;
            }
        }
        false
    }

    /// Clear all shocks for all panes.
    pub fn clear_all(&self) -> usize {
        let mut cleared = 0;
        if let Ok(mut panes) = self.panes.write() {
            for state in panes.values_mut() {
                if !state.shocks.is_empty() || state.paused {
                    state.shocks.clear();
                    if state.paused {
                        state.paused = false;
                        self.metrics.panes_cleared.fetch_add(1, Ordering::Relaxed);
                    }
                    cleared += 1;
                }
            }
        }
        cleared
    }

    /// Number of panes currently tracked.
    pub fn tracked_pane_count(&self) -> usize {
        self.panes.read().ok().map(|p| p.len()).unwrap_or(0)
    }

    /// Number of panes currently paused.
    pub fn paused_pane_count(&self) -> usize {
        self.panes
            .read()
            .ok()
            .map(|panes| panes.values().filter(|s| s.paused).count())
            .unwrap_or(0)
    }

    /// Run auto-clear on all panes, removing expired shocks.
    pub fn gc_expired_shocks(&self) -> usize {
        let auto_clear_ms = self.config.auto_clear_seconds * 1000;
        if auto_clear_ms == 0 {
            return 0;
        }
        let now_ms = u64::try_from(self.created_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut total = 0;
        if let Ok(mut panes) = self.panes.write() {
            for state in panes.values_mut() {
                total += state.auto_clear(auto_clear_ms, now_ms);
            }
        }
        if total > 0 {
            self.metrics
                .auto_cleared
                .fetch_add(total as u64, Ordering::Relaxed);
        }
        total
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Extract a `ConformalShock` from a detection's `extracted` JSON field.
fn extract_shock_from_detection(detection: &Detection) -> Option<ConformalShock> {
    let extracted = &detection.extracted;
    let p_value = extracted.get("p_value")?.as_f64()?;
    let distance = extracted.get("distance")?.as_f64()? as f32;
    let alpha = extracted.get("alpha")?.as_f64()?;
    let calibration_count = extracted.get("calibration_count")?.as_u64()? as usize;
    let calibration_median = extracted.get("calibration_median")?.as_f64()? as f32;

    Some(ConformalShock {
        distance,
        p_value,
        alpha,
        calibration_count,
        calibration_median,
    })
}

/// Build a `NotificationPayload` from a shock notification.
///
/// This can be passed to the existing `NotificationPipeline` or `DesktopNotifier`.
pub fn build_notification_payload(
    notification: &ShockNotification,
) -> crate::notifications::NotificationPayload {
    let record = &notification.record;
    let pause_note = if notification.paused {
        " — pane PAUSED (clear to resume)"
    } else {
        ""
    };
    crate::notifications::NotificationPayload {
        event_type: record.rule_id.clone(),
        pane_id: notification.pane_id,
        timestamp: chrono::Utc::now().to_rfc3339(),
        summary: format!(
            "Semantic anomaly in pane {}{}",
            notification.pane_id, pause_note
        ),
        description: format!(
            "Conformal shock: p={:.4}, distance={:.3}, calibration={}, segment={}B",
            record.shock.p_value,
            record.shock.distance,
            record.shock.calibration_count,
            record.segment_len,
        ),
        severity: "critical".to_string(),
        agent_type: "unknown".to_string(),
        confidence: record.confidence,
        quick_fix: if notification.paused {
            Some("ft shock clear <pane_id>".to_string())
        } else {
            None
        },
        suppressed_since_last: 0,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Severity};

    fn make_semantic_detection(p_value: f64, distance: f64) -> Detection {
        Detection {
            rule_id: "core.semantic_anomaly:conformal_shock".to_string(),
            agent_type: AgentType::Unknown,
            event_type: "semantic_anomaly".to_string(),
            severity: Severity::Critical,
            confidence: 1.0 - p_value,
            extracted: serde_json::json!({
                "p_value": p_value,
                "distance": distance,
                "alpha": 0.05,
                "calibration_count": 200,
                "calibration_median": 0.12,
                "segment_len": 1024,
            }),
            matched_text: format!("Semantic anomaly: p={p_value:.4}, distance={distance:.3}"),
            span: (0, 0),
        }
    }

    fn make_non_semantic_detection() -> Detection {
        Detection {
            rule_id: "core.codex:error_loop".to_string(),
            agent_type: AgentType::Codex,
            event_type: "error_loop".to_string(),
            severity: Severity::Warning,
            confidence: 0.85,
            extracted: serde_json::json!({}),
            matched_text: "error loop detected".to_string(),
            span: (0, 0),
        }
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn config_defaults() {
        let config = SemanticShockConfig::default();
        assert!(config.enabled);
        assert_eq!(config.action, ShockAction::Alert);
        assert!((config.p_value_threshold - 0.01).abs() < 1e-10);
        assert_eq!(config.max_shocks_per_pane, 10);
        assert_eq!(config.auto_clear_seconds, 300);
        assert_eq!(config.notification_cooldown_seconds, 30);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = SemanticShockConfig {
            enabled: false,
            action: ShockAction::Pause,
            p_value_threshold: 0.05,
            max_shocks_per_pane: 5,
            auto_clear_seconds: 600,
            notification_cooldown_seconds: 60,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: SemanticShockConfig = serde_json::from_str(&json).unwrap();
        assert!(!restored.enabled);
        assert_eq!(restored.action, ShockAction::Pause);
        assert!((restored.p_value_threshold - 0.05).abs() < 1e-10);
    }

    #[test]
    fn shock_action_serde() {
        let json_alert = serde_json::to_string(&ShockAction::Alert).unwrap();
        assert_eq!(json_alert, "\"alert\"");
        let json_pause = serde_json::to_string(&ShockAction::Pause).unwrap();
        assert_eq!(json_pause, "\"pause\"");
        let restored: ShockAction = serde_json::from_str("\"pause\"").unwrap();
        assert_eq!(restored, ShockAction::Pause);
    }

    // =========================================================================
    // Shock extraction tests
    // =========================================================================

    #[test]
    fn extract_shock_from_valid_detection() {
        let det = make_semantic_detection(0.001, 0.95);
        let shock = extract_shock_from_detection(&det).unwrap();
        assert!((shock.p_value - 0.001).abs() < 1e-10);
        assert!((shock.distance - 0.95).abs() < 1e-4);
        assert!((shock.alpha - 0.05).abs() < 1e-10);
        assert_eq!(shock.calibration_count, 200);
    }

    #[test]
    fn extract_shock_from_missing_fields() {
        let det = Detection {
            rule_id: "test".to_string(),
            agent_type: AgentType::Unknown,
            event_type: "semantic_anomaly".to_string(),
            severity: Severity::Critical,
            confidence: 0.9,
            extracted: serde_json::json!({"p_value": 0.01}), // missing distance etc.
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(extract_shock_from_detection(&det).is_none());
    }

    // =========================================================================
    // Responder creation tests
    // =========================================================================

    #[test]
    fn responder_new() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        assert_eq!(r.tracked_pane_count(), 0);
        assert_eq!(r.paused_pane_count(), 0);
        let snap = r.metrics_snapshot();
        assert_eq!(snap.detections_received, 0);
    }

    #[test]
    fn responder_debug() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        let dbg = format!("{r:?}");
        assert!(dbg.contains("SemanticShockResponder"));
    }

    // =========================================================================
    // Detection handling tests
    // =========================================================================

    #[test]
    fn handle_non_semantic_detection_ignored() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        let det = make_non_semantic_detection();
        let result = r.handle_detection(1, &det);
        assert!(result.is_none());
        assert_eq!(r.metrics_snapshot().detections_received, 1);
        assert_eq!(r.metrics_snapshot().shocks_recorded, 0);
    }

    #[test]
    fn handle_semantic_detection_alert_mode() {
        let config = SemanticShockConfig {
            action: ShockAction::Alert,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        let notif = r.handle_detection(1, &det).unwrap();

        assert_eq!(notif.pane_id, 1);
        assert!(!notif.paused);
        assert!(!r.is_pane_paused(1));
        assert_eq!(r.metrics_snapshot().shocks_recorded, 1);
        assert_eq!(r.metrics_snapshot().notifications_sent, 1);
    }

    #[test]
    fn handle_semantic_detection_pause_mode() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        let notif = r.handle_detection(1, &det).unwrap();

        assert!(notif.paused);
        assert!(r.is_pane_paused(1));
        assert_eq!(r.paused_pane_count(), 1);
        assert_eq!(r.metrics_snapshot().panes_paused, 1);
    }

    #[test]
    fn handle_detection_filters_high_p_value() {
        let config = SemanticShockConfig {
            p_value_threshold: 0.01,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.05, 0.5); // p_value > threshold
        let result = r.handle_detection(1, &det);

        assert!(result.is_none());
        assert_eq!(r.metrics_snapshot().detections_filtered, 1);
        assert_eq!(r.metrics_snapshot().shocks_recorded, 0);
    }

    #[test]
    fn handle_detection_disabled() {
        let config = SemanticShockConfig {
            enabled: false,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        assert!(r.handle_detection(1, &det).is_none());
        assert_eq!(r.metrics_snapshot().detections_received, 0);
    }

    #[test]
    fn notification_cooldown() {
        let config = SemanticShockConfig {
            notification_cooldown_seconds: 60,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        // First detection: notification sent.
        let n1 = r.handle_detection(1, &det);
        assert!(n1.is_some());

        // Second detection immediately: suppressed.
        let n2 = r.handle_detection(1, &det);
        assert!(n2.is_none());

        let snap = r.metrics_snapshot();
        assert_eq!(snap.notifications_sent, 1);
        assert_eq!(snap.notifications_suppressed, 1);
        assert_eq!(snap.shocks_recorded, 2); // Both recorded.
    }

    // =========================================================================
    // Trauma decision tests
    // =========================================================================

    #[test]
    fn trauma_decision_not_paused() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        let decision = r.trauma_decision_for_pane(1);
        assert!(!decision.should_intervene);
        assert!(decision.reason_code.is_none());
    }

    #[test]
    fn trauma_decision_paused() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det);

        let decision = r.trauma_decision_for_pane(1);
        assert!(decision.should_intervene);
        assert_eq!(
            decision.reason_code.as_deref(),
            Some("semantic_anomaly_pause")
        );
        assert_eq!(decision.recurring_signatures, vec!["semantic_anomaly"]);
    }

    #[test]
    fn trauma_decision_different_pane_not_affected() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det); // Pause pane 1.

        assert!(r.is_pane_paused(1));
        assert!(!r.is_pane_paused(2)); // Pane 2 unaffected.
    }

    // =========================================================================
    // Clear tests
    // =========================================================================

    #[test]
    fn clear_pane_removes_shocks_and_unpauses() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det);
        assert!(r.is_pane_paused(1));

        let cleared = r.clear_pane(1);
        assert!(cleared);
        assert!(!r.is_pane_paused(1));
        assert_eq!(r.metrics_snapshot().panes_cleared, 1);

        // Decision should no longer intervene.
        let decision = r.trauma_decision_for_pane(1);
        assert!(!decision.should_intervene);
    }

    #[test]
    fn clear_nonexistent_pane() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        assert!(!r.clear_pane(999));
    }

    #[test]
    fn clear_all_panes() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det);
        r.handle_detection(2, &det);
        assert_eq!(r.paused_pane_count(), 2);

        let cleared = r.clear_all();
        assert_eq!(cleared, 2);
        assert_eq!(r.paused_pane_count(), 0);
    }

    // =========================================================================
    // Summary tests
    // =========================================================================

    #[test]
    fn pane_summary() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det);
        r.handle_detection(1, &det);

        let summary = r.pane_summary(1).unwrap();
        assert_eq!(summary.pane_id, 1);
        assert_eq!(summary.active_count, 2);
        assert!(!summary.is_paused);
        assert!(summary.latest.is_some());
    }

    #[test]
    fn all_summaries() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det);
        r.handle_detection(2, &det);

        let summaries = r.all_summaries();
        assert_eq!(summaries.len(), 2);
    }

    #[test]
    fn pane_summary_nonexistent() {
        let r = SemanticShockResponder::new(SemanticShockConfig::default());
        assert!(r.pane_summary(999).is_none());
    }

    // =========================================================================
    // Max shocks per pane eviction
    // =========================================================================

    #[test]
    fn evicts_oldest_when_over_max() {
        let config = SemanticShockConfig {
            max_shocks_per_pane: 3,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);

        for i in 1..=5 {
            let det = make_semantic_detection(0.001 * i as f64, 0.9);
            // Use pane 1 for all.
            r.handle_detection(1, &det);
        }

        let summary = r.pane_summary(1).unwrap();
        assert_eq!(summary.active_count, 3);
        // Oldest 2 should have been evicted. Latest should be the 5th.
        let latest = summary.latest.unwrap();
        assert_eq!(latest.sequence, 4); // 0-indexed, so 5th is seq 4.
    }

    // =========================================================================
    // Shock record serde
    // =========================================================================

    #[test]
    fn shock_record_serde() {
        let record = ShockRecord {
            pane_id: 42,
            shock: ConformalShock {
                distance: 0.95,
                p_value: 0.001,
                alpha: 0.05,
                calibration_count: 200,
                calibration_median: 0.12,
            },
            segment_len: 1024,
            rule_id: "test.rule".to_string(),
            confidence: 0.99,
            sequence: 7,
            age_ms: 5000,
        };
        let json = serde_json::to_string(&record).unwrap();
        let restored: ShockRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pane_id, 42);
        assert_eq!(restored.sequence, 7);
        assert_eq!(restored.segment_len, 1024);
    }

    // =========================================================================
    // Metrics snapshot serde
    // =========================================================================

    #[test]
    fn metrics_snapshot_serde() {
        let snap = ShockResponseMetricsSnapshot {
            detections_received: 10,
            detections_filtered: 3,
            shocks_recorded: 7,
            panes_paused: 2,
            panes_cleared: 1,
            notifications_sent: 5,
            notifications_suppressed: 2,
            auto_cleared: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let restored: ShockResponseMetricsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, snap);
    }

    // =========================================================================
    // Notification payload builder
    // =========================================================================

    #[test]
    fn notification_payload_builder() {
        let notif = ShockNotification {
            pane_id: 7,
            record: ShockRecord {
                pane_id: 7,
                shock: ConformalShock {
                    distance: 0.95,
                    p_value: 0.001,
                    alpha: 0.05,
                    calibration_count: 200,
                    calibration_median: 0.12,
                },
                segment_len: 1024,
                rule_id: "core.semantic_anomaly:conformal_shock".to_string(),
                confidence: 0.999,
                sequence: 0,
                age_ms: 100,
            },
            paused: true,
        };
        let payload = build_notification_payload(&notif);
        assert_eq!(payload.pane_id, 7);
        assert!(payload.summary.contains("PAUSED"));
        assert_eq!(payload.severity, "critical");
        assert!(payload.quick_fix.is_some());
    }

    #[test]
    fn notification_payload_alert_mode() {
        let notif = ShockNotification {
            pane_id: 3,
            record: ShockRecord {
                pane_id: 3,
                shock: ConformalShock {
                    distance: 0.8,
                    p_value: 0.005,
                    alpha: 0.05,
                    calibration_count: 100,
                    calibration_median: 0.1,
                },
                segment_len: 512,
                rule_id: "test".to_string(),
                confidence: 0.995,
                sequence: 0,
                age_ms: 50,
            },
            paused: false,
        };
        let payload = build_notification_payload(&notif);
        assert!(!payload.summary.contains("PAUSED"));
        assert!(payload.quick_fix.is_none());
    }

    // =========================================================================
    // Multiple panes isolation
    // =========================================================================

    #[test]
    fn multiple_panes_independent_pauses() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        r.handle_detection(1, &det);
        r.handle_detection(2, &det);
        r.handle_detection(3, &det);

        assert_eq!(r.paused_pane_count(), 3);

        // Clear only pane 2.
        r.clear_pane(2);
        assert!(r.is_pane_paused(1));
        assert!(!r.is_pane_paused(2));
        assert!(r.is_pane_paused(3));
        assert_eq!(r.paused_pane_count(), 2);
    }

    // =========================================================================
    // GC expired shocks
    // =========================================================================

    #[test]
    fn gc_expired_no_auto_clear() {
        let config = SemanticShockConfig {
            auto_clear_seconds: 0, // Disabled.
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);
        r.handle_detection(1, &det);

        let cleared = r.gc_expired_shocks();
        assert_eq!(cleared, 0);
    }

    // =========================================================================
    // PaneShockSummary serde
    // =========================================================================

    #[test]
    fn pane_shock_summary_serde() {
        let summary = PaneShockSummary {
            pane_id: 1,
            active_count: 3,
            latest: None,
            is_paused: true,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let restored: PaneShockSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pane_id, 1);
        assert_eq!(restored.active_count, 3);
        assert!(restored.is_paused);
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn handle_detection_with_boundary_p_value() {
        let config = SemanticShockConfig {
            p_value_threshold: 0.01,
            notification_cooldown_seconds: 0, // Disable cooldown for this test.
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);

        // Above threshold — filtered (not recorded).
        let det_above = make_semantic_detection(0.02, 0.5);
        assert!(r.handle_detection(1, &det_above).is_none());
        assert_eq!(r.metrics_snapshot().detections_filtered, 1);
        assert_eq!(r.metrics_snapshot().shocks_recorded, 0);

        // Exactly at threshold — passes (only strictly above is filtered).
        let det_at = make_semantic_detection(0.01, 0.5);
        assert!(r.handle_detection(2, &det_at).is_some());
        assert_eq!(r.metrics_snapshot().shocks_recorded, 1);

        // Below threshold — passes.
        let det_below = make_semantic_detection(0.009, 0.5);
        assert!(r.handle_detection(3, &det_below).is_some());
        assert_eq!(r.metrics_snapshot().shocks_recorded, 2);
    }

    #[test]
    fn repeated_pause_same_pane_only_counts_once() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0, // No cooldown.
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        r.handle_detection(1, &det);
        r.handle_detection(1, &det);
        r.handle_detection(1, &det);

        // panes_paused should only count the first pause.
        assert_eq!(r.metrics_snapshot().panes_paused, 1);
        assert_eq!(r.metrics_snapshot().shocks_recorded, 3);
    }

    #[test]
    fn clear_and_re_pause() {
        let config = SemanticShockConfig {
            action: ShockAction::Pause,
            notification_cooldown_seconds: 0,
            ..Default::default()
        };
        let r = SemanticShockResponder::new(config);
        let det = make_semantic_detection(0.001, 0.95);

        r.handle_detection(1, &det);
        assert!(r.is_pane_paused(1));

        r.clear_pane(1);
        assert!(!r.is_pane_paused(1));

        // Re-detection should re-pause.
        r.handle_detection(1, &det);
        assert!(r.is_pane_paused(1));
        assert_eq!(r.metrics_snapshot().panes_paused, 2);
        assert_eq!(r.metrics_snapshot().panes_cleared, 1);
    }
}
