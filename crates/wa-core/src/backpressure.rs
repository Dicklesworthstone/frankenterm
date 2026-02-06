//! Backpressure policy for the wa watcher pipeline.
//!
//! Monitors capture channel and storage write queue depths, classifies the
//! system into four tiers (Green / Yellow / Red / Black), and provides
//! actionable signals that upstream tasks use to shed load gracefully.
//!
//! See `docs/backpressure-policy.md` for the full design specification.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ─── Tier ────────────────────────────────────────────────────────────

/// Backpressure severity tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureTier {
    /// All queues below warning thresholds.
    Green,
    /// Capture ≥ yellow threshold OR write ≥ yellow threshold.
    Yellow,
    /// Capture ≥ red threshold OR write ≥ red threshold.
    Red,
    /// Queue near saturation.
    Black,
}

impl std::fmt::Display for BackpressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "GREEN"),
            Self::Yellow => write!(f, "YELLOW"),
            Self::Red => write!(f, "RED"),
            Self::Black => write!(f, "BLACK"),
        }
    }
}

impl BackpressureTier {
    /// Numeric value for gauge metrics (0–3).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Green => 0,
            Self::Yellow => 1,
            Self::Red => 2,
            Self::Black => 3,
        }
    }
}

// ─── Configuration ───────────────────────────────────────────────────

/// Backpressure policy configuration.
///
/// All thresholds are expressed as fractions (0.0–1.0) of the respective
/// queue capacity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackpressureConfig {
    /// Enable the backpressure policy.
    pub enabled: bool,

    /// How often to sample queue depths (milliseconds).
    pub check_interval_ms: u64,

    // ── Capture channel thresholds ──
    /// Fraction of capture channel capacity that triggers Yellow.
    pub yellow_capture: f64,
    /// Fraction of capture channel capacity that triggers Red.
    pub red_capture: f64,

    // ── Write queue thresholds ──
    /// Fraction of write queue capacity that triggers Yellow.
    pub yellow_write: f64,
    /// Fraction of write queue capacity that triggers Red.
    pub red_write: f64,

    /// Minimum time (ms) in an elevated tier before allowing downgrade.
    pub hysteresis_ms: u64,

    // ── Yellow tier actions ──
    /// Idle pane poll interval multiplier.
    pub idle_poll_backoff_factor: f64,
    /// Fraction of lowest-priority panes to skip for pattern detection.
    pub skip_detection_ratio: f64,

    // ── Red tier actions ──
    /// Fraction of lowest-priority panes to pause.
    pub pause_ratio: f64,
    /// Maximum segments buffered in the persistence task before dropping.
    pub max_buffered_segments: usize,

    // ── Recovery ──
    /// Resume one paused pane every N milliseconds during recovery.
    pub recovery_resume_interval_ms: u64,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_ms: 500,
            yellow_capture: 0.50,
            red_capture: 0.75,
            yellow_write: 0.60,
            red_write: 0.80,
            hysteresis_ms: 2000,
            idle_poll_backoff_factor: 2.0,
            skip_detection_ratio: 0.25,
            pause_ratio: 0.50,
            max_buffered_segments: 100,
            recovery_resume_interval_ms: 500,
        }
    }
}

// ─── Queue Observation ───────────────────────────────────────────────

/// A point-in-time reading of queue depths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDepths {
    pub capture_depth: usize,
    pub capture_capacity: usize,
    pub write_depth: usize,
    pub write_capacity: usize,
}

impl QueueDepths {
    /// Capture queue fill ratio (0.0–1.0).
    #[must_use]
    pub fn capture_ratio(&self) -> f64 {
        if self.capture_capacity == 0 {
            return 0.0;
        }
        self.capture_depth as f64 / self.capture_capacity as f64
    }

    /// Write queue fill ratio (0.0–1.0).
    #[must_use]
    pub fn write_ratio(&self) -> f64 {
        if self.write_capacity == 0 {
            return 0.0;
        }
        self.write_depth as f64 / self.write_capacity as f64
    }
}

// ─── Snapshot ────────────────────────────────────────────────────────

/// Serialisable snapshot of the current backpressure state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackpressureSnapshot {
    pub tier: BackpressureTier,
    pub timestamp_epoch_ms: u64,
    pub capture_depth: usize,
    pub capture_capacity: usize,
    pub write_depth: usize,
    pub write_capacity: usize,
    pub duration_in_tier_ms: u64,
    pub transitions: u64,
    pub paused_panes: Vec<u64>,
}

// ─── Metrics ─────────────────────────────────────────────────────────

/// Counters tracked across the lifetime of the backpressure manager.
#[derive(Debug, Default)]
pub struct BackpressureMetrics {
    pub yellow_entries: AtomicU64,
    pub red_entries: AtomicU64,
    pub black_entries: AtomicU64,
    pub segments_dropped: AtomicU64,
    pub gaps_emitted: AtomicU64,
    pub detection_skipped: AtomicU64,
    pub fts_deferred: AtomicU64,
}

// ─── Manager ─────────────────────────────────────────────────────────

/// Evaluates queue depths and manages tier transitions with hysteresis.
pub struct BackpressureManager {
    config: BackpressureConfig,
    current_tier: RwLock<BackpressureTier>,
    tier_entered_at: RwLock<Instant>,
    transition_count: AtomicU64,
    paused_panes: Arc<RwLock<HashSet<u64>>>,
    pub metrics: BackpressureMetrics,
}

impl BackpressureManager {
    /// Create a new manager with the given configuration.
    #[must_use]
    pub fn new(config: BackpressureConfig) -> Self {
        Self {
            config,
            current_tier: RwLock::new(BackpressureTier::Green),
            tier_entered_at: RwLock::new(Instant::now()),
            transition_count: AtomicU64::new(0),
            paused_panes: Arc::new(RwLock::new(HashSet::new())),
            metrics: BackpressureMetrics::default(),
        }
    }

    /// Whether the policy is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Current tier (lock-free read when only an approximate value is needed).
    #[must_use]
    pub fn current_tier(&self) -> BackpressureTier {
        *self
            .current_tier
            .read()
            .expect("backpressure lock poisoned")
    }

    /// Classify queue depths into a tier without applying any state change.
    #[must_use]
    pub fn classify(&self, depths: &QueueDepths) -> BackpressureTier {
        let cr = depths.capture_ratio();
        let wr = depths.write_ratio();

        // Black: near saturation (within 5 slots of full, or ratio ≥ 0.995).
        let capture_saturated = depths.capture_capacity > 0
            && depths.capture_depth >= depths.capture_capacity.saturating_sub(5);
        let write_saturated = depths.write_capacity > 0
            && depths.write_depth >= depths.write_capacity.saturating_sub(100);

        if capture_saturated || write_saturated {
            BackpressureTier::Black
        } else if cr >= self.config.red_capture || wr >= self.config.red_write {
            BackpressureTier::Red
        } else if cr >= self.config.yellow_capture || wr >= self.config.yellow_write {
            BackpressureTier::Yellow
        } else {
            BackpressureTier::Green
        }
    }

    /// Evaluate queue depths and apply a tier transition if warranted.
    ///
    /// Returns `Some((old, new))` when the tier changes, `None` otherwise.
    pub fn evaluate(&self, depths: &QueueDepths) -> Option<(BackpressureTier, BackpressureTier)> {
        if !self.config.enabled {
            return None;
        }

        let proposed = self.classify(depths);
        let current = self.current_tier();

        if proposed == current {
            return None;
        }

        // Upgrades (toward Black) are immediate.
        // Downgrades require hysteresis.
        if proposed < current {
            let entered = *self.tier_entered_at.read().expect("lock poisoned");
            let elapsed_ms = entered.elapsed().as_millis() as u64;
            if elapsed_ms < self.config.hysteresis_ms {
                return None; // too soon to downgrade
            }
        }

        // Apply transition.
        *self.current_tier.write().expect("lock poisoned") = proposed;
        *self.tier_entered_at.write().expect("lock poisoned") = Instant::now();
        self.transition_count.fetch_add(1, Ordering::Relaxed);

        match proposed {
            BackpressureTier::Yellow => {
                self.metrics.yellow_entries.fetch_add(1, Ordering::Relaxed);
            }
            BackpressureTier::Red => {
                self.metrics.red_entries.fetch_add(1, Ordering::Relaxed);
            }
            BackpressureTier::Black => {
                self.metrics.black_entries.fetch_add(1, Ordering::Relaxed);
            }
            BackpressureTier::Green => {}
        }

        tracing::warn!(
            old_tier = %current,
            new_tier = %proposed,
            capture_ratio = format_args!("{:.1}%", depths.capture_ratio() * 100.0),
            write_ratio = format_args!("{:.1}%", depths.write_ratio() * 100.0),
            "backpressure tier transition"
        );

        Some((current, proposed))
    }

    // ── Pane pause management ────────────────────────────────────────

    /// Mark a pane as paused due to backpressure.
    pub fn pause_pane(&self, pane_id: u64) {
        self.paused_panes
            .write()
            .expect("lock poisoned")
            .insert(pane_id);
    }

    /// Resume a previously paused pane.
    pub fn resume_pane(&self, pane_id: u64) {
        self.paused_panes
            .write()
            .expect("lock poisoned")
            .remove(&pane_id);
    }

    /// Resume all paused panes (e.g. on recovery to Green).
    pub fn resume_all_panes(&self) {
        self.paused_panes.write().expect("lock poisoned").clear();
    }

    /// Check if a pane is currently paused.
    #[must_use]
    pub fn is_pane_paused(&self, pane_id: u64) -> bool {
        self.paused_panes
            .read()
            .expect("lock poisoned")
            .contains(&pane_id)
    }

    /// List currently paused pane IDs.
    #[must_use]
    pub fn paused_pane_ids(&self) -> Vec<u64> {
        let guard = self.paused_panes.read().expect("lock poisoned");
        let mut ids: Vec<u64> = guard.iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    // ── Configuration access ─────────────────────────────────────────

    /// Idle poll backoff factor for Yellow tier.
    #[must_use]
    pub fn idle_poll_backoff_factor(&self) -> f64 {
        self.config.idle_poll_backoff_factor
    }

    /// Fraction of low-priority panes to skip for detection in Yellow.
    #[must_use]
    pub fn skip_detection_ratio(&self) -> f64 {
        self.config.skip_detection_ratio
    }

    /// Fraction of low-priority panes to pause in Red.
    #[must_use]
    pub fn pause_ratio(&self) -> f64 {
        self.config.pause_ratio
    }

    /// Maximum buffered segments in the persistence task.
    #[must_use]
    pub fn max_buffered_segments(&self) -> usize {
        self.config.max_buffered_segments
    }

    /// Interval between resuming individual panes during recovery.
    #[must_use]
    pub fn recovery_resume_interval_ms(&self) -> u64 {
        self.config.recovery_resume_interval_ms
    }

    // ── Snapshot ─────────────────────────────────────────────────────

    /// Produce a serialisable snapshot of the current state.
    #[must_use]
    pub fn snapshot(&self, depths: &QueueDepths) -> BackpressureSnapshot {
        let tier = self.current_tier();
        let entered = *self.tier_entered_at.read().expect("lock poisoned");
        let now_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        BackpressureSnapshot {
            tier,
            timestamp_epoch_ms: now_epoch_ms,
            capture_depth: depths.capture_depth,
            capture_capacity: depths.capture_capacity,
            write_depth: depths.write_depth,
            write_capacity: depths.write_capacity,
            duration_in_tier_ms: entered.elapsed().as_millis() as u64,
            transitions: self.transition_count.load(Ordering::Relaxed),
            paused_panes: self.paused_pane_ids(),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_manager() -> BackpressureManager {
        BackpressureManager::new(BackpressureConfig::default())
    }

    fn depths(capture: usize, cap_cap: usize, write: usize, wr_cap: usize) -> QueueDepths {
        QueueDepths {
            capture_depth: capture,
            capture_capacity: cap_cap,
            write_depth: write,
            write_capacity: wr_cap,
        }
    }

    #[test]
    fn initial_state_is_green() {
        let m = default_manager();
        assert_eq!(m.current_tier(), BackpressureTier::Green);
    }

    #[test]
    fn classify_green() {
        let m = default_manager();
        let d = depths(100, 1024, 500, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Green);
    }

    #[test]
    fn classify_yellow_capture() {
        let m = default_manager();
        // 512/1024 = 50% → Yellow
        let d = depths(512, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Yellow);
    }

    #[test]
    fn classify_yellow_write() {
        let m = default_manager();
        // 6000/10000 = 60% → Yellow
        let d = depths(0, 1024, 6000, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Yellow);
    }

    #[test]
    fn classify_red_capture() {
        let m = default_manager();
        // 768/1024 = 75% → Red
        let d = depths(768, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Red);
    }

    #[test]
    fn classify_red_write() {
        let m = default_manager();
        // 8000/10000 = 80% → Red
        let d = depths(0, 1024, 8000, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Red);
    }

    #[test]
    fn classify_black_capture_saturated() {
        let m = default_manager();
        // 1022/1024 (within 5 of capacity) → Black
        let d = depths(1022, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Black);
    }

    #[test]
    fn classify_black_write_saturated() {
        let m = default_manager();
        // 9950/10000 (within 100 of capacity) → Black
        let d = depths(0, 1024, 9950, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Black);
    }

    #[test]
    fn classify_zero_capacity_is_green() {
        let m = default_manager();
        let d = depths(0, 0, 0, 0);
        assert_eq!(m.classify(&d), BackpressureTier::Green);
    }

    #[test]
    fn evaluate_upgrades_immediately() {
        let m = default_manager();
        let d = depths(768, 1024, 0, 10_000); // Red
        let result = m.evaluate(&d);
        assert_eq!(
            result,
            Some((BackpressureTier::Green, BackpressureTier::Red))
        );
        assert_eq!(m.current_tier(), BackpressureTier::Red);
    }

    #[test]
    fn evaluate_downgrade_blocked_by_hysteresis() {
        let mut config = BackpressureConfig::default();
        config.hysteresis_ms = 60_000; // 60 seconds
        let m = BackpressureManager::new(config);

        // First: upgrade to Red
        let d_red = depths(768, 1024, 0, 10_000);
        m.evaluate(&d_red);
        assert_eq!(m.current_tier(), BackpressureTier::Red);

        // Attempt downgrade to Green: should be blocked by hysteresis
        let d_green = depths(10, 1024, 100, 10_000);
        let result = m.evaluate(&d_green);
        assert!(result.is_none());
        assert_eq!(m.current_tier(), BackpressureTier::Red);
    }

    #[test]
    fn evaluate_no_change_returns_none() {
        let m = default_manager();
        let d = depths(10, 1024, 100, 10_000); // Green
        let result = m.evaluate(&d);
        assert!(result.is_none());
    }

    #[test]
    fn evaluate_disabled_returns_none() {
        let mut config = BackpressureConfig::default();
        config.enabled = false;
        let m = BackpressureManager::new(config);

        let d = depths(1024, 1024, 0, 10_000); // Would be Black
        let result = m.evaluate(&d);
        assert!(result.is_none());
        assert_eq!(m.current_tier(), BackpressureTier::Green);
    }

    #[test]
    fn pane_pause_lifecycle() {
        let m = default_manager();

        assert!(!m.is_pane_paused(1));
        assert!(m.paused_pane_ids().is_empty());

        m.pause_pane(1);
        m.pause_pane(3);
        assert!(m.is_pane_paused(1));
        assert!(m.is_pane_paused(3));
        assert!(!m.is_pane_paused(2));
        assert_eq!(m.paused_pane_ids(), vec![1, 3]);

        m.resume_pane(1);
        assert!(!m.is_pane_paused(1));
        assert!(m.is_pane_paused(3));

        m.resume_all_panes();
        assert!(m.paused_pane_ids().is_empty());
    }

    #[test]
    fn snapshot_reflects_state() {
        let m = default_manager();
        let d = depths(768, 1024, 2000, 10_000);
        m.evaluate(&d); // → Red

        m.pause_pane(5);
        m.pause_pane(9);

        let snap = m.snapshot(&d);
        assert_eq!(snap.tier, BackpressureTier::Red);
        assert_eq!(snap.capture_depth, 768);
        assert_eq!(snap.capture_capacity, 1024);
        assert_eq!(snap.paused_panes, vec![5, 9]);
        assert!(snap.transitions >= 1);
    }

    #[test]
    fn tier_ordering() {
        assert!(BackpressureTier::Green < BackpressureTier::Yellow);
        assert!(BackpressureTier::Yellow < BackpressureTier::Red);
        assert!(BackpressureTier::Red < BackpressureTier::Black);
    }

    #[test]
    fn tier_display() {
        assert_eq!(BackpressureTier::Green.to_string(), "GREEN");
        assert_eq!(BackpressureTier::Yellow.to_string(), "YELLOW");
        assert_eq!(BackpressureTier::Red.to_string(), "RED");
        assert_eq!(BackpressureTier::Black.to_string(), "BLACK");
    }

    #[test]
    fn tier_as_u8() {
        assert_eq!(BackpressureTier::Green.as_u8(), 0);
        assert_eq!(BackpressureTier::Yellow.as_u8(), 1);
        assert_eq!(BackpressureTier::Red.as_u8(), 2);
        assert_eq!(BackpressureTier::Black.as_u8(), 3);
    }

    #[test]
    fn queue_depths_ratios() {
        let d = depths(512, 1024, 5000, 10_000);
        assert!((d.capture_ratio() - 0.5).abs() < f64::EPSILON);
        assert!((d.write_ratio() - 0.5).abs() < f64::EPSILON);

        let zero = depths(0, 0, 0, 0);
        assert!((zero.capture_ratio()).abs() < f64::EPSILON);
        assert!((zero.write_ratio()).abs() < f64::EPSILON);
    }

    #[test]
    fn config_default_thresholds() {
        let c = BackpressureConfig::default();
        assert!(c.enabled);
        assert!((c.yellow_capture - 0.50).abs() < f64::EPSILON);
        assert!((c.red_capture - 0.75).abs() < f64::EPSILON);
        assert!((c.yellow_write - 0.60).abs() < f64::EPSILON);
        assert!((c.red_write - 0.80).abs() < f64::EPSILON);
        assert_eq!(c.hysteresis_ms, 2000);
    }

    #[test]
    fn metrics_increment() {
        let m = default_manager();

        // Green → Yellow
        let d = depths(512, 1024, 0, 10_000);
        m.evaluate(&d);
        assert_eq!(m.metrics.yellow_entries.load(Ordering::Relaxed), 1);

        // Yellow → Red (upgrade is immediate)
        let d = depths(768, 1024, 0, 10_000);
        m.evaluate(&d);
        assert_eq!(m.metrics.red_entries.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let snap = BackpressureSnapshot {
            tier: BackpressureTier::Yellow,
            timestamp_epoch_ms: 1_700_000_000_000,
            capture_depth: 500,
            capture_capacity: 1024,
            write_depth: 100,
            write_capacity: 10_000,
            duration_in_tier_ms: 5000,
            transitions: 3,
            paused_panes: vec![1, 5],
        };

        let json = serde_json::to_string(&snap).unwrap();
        let parsed: BackpressureSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tier, BackpressureTier::Yellow);
        assert_eq!(parsed.capture_depth, 500);
        assert_eq!(parsed.paused_panes, vec![1, 5]);
    }
}
