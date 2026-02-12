//! Smart pane priority classification — intelligent resource allocation.
//!
//! Classifies panes into priority levels based on pattern detections, output
//! rate, and activity tier. Higher-priority panes get more resources (faster
//! polling, preferential connection pool access, last to be shed under
//! backpressure).
//!
//! # Priority levels
//!
//! | Priority   | Value | Typical triggers                          |
//! |------------|-------|-------------------------------------------|
//! | Critical   | 4     | Errors, user attention needed              |
//! | High       | 3     | Active output above rate threshold         |
//! | Medium     | 2     | Thinking/processing, moderate output       |
//! | Low        | 1     | Idle, waiting for input                    |
//! | Background | 0     | Rate-limited, dormant, completed tasks     |
//!
//! # Integration
//!
//! Consumes signals from:
//! - [`PaneTierClassifier`](crate::pane_tiers::PaneTierClassifier) — activity tiers
//! - [`PatternEngine`](crate::patterns::PatternEngine) — detected events
//! - [`OutputRateTracker`] — EWMA output rate with exponential decay
//!
//! Feeds into:
//! - Capture scheduler (tailer) — polling interval selection
//! - Connection pool — priority queue ordering
//! - Backpressure shedding — Background shed first, Critical last

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::concurrent_map::PaneMap;
use crate::pane_tiers::PaneTier;

// =============================================================================
// Priority enum
// =============================================================================

/// Resource allocation priority for a pane.
///
/// Ordered from lowest (Background = 0) to highest (Critical = 4).
/// Manual overrides take precedence over automatic classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanePriority {
    /// Rate-limited, dormant, or completed — minimal resources.
    Background = 0,
    /// Idle or waiting for input — reduced resources.
    Low = 1,
    /// Thinking or moderate output — standard resources.
    Medium = 2,
    /// Active with high output rate — elevated resources.
    High = 3,
    /// Error state or needs user attention — maximum resources.
    Critical = 4,
}

impl PanePriority {
    /// Numeric value (0–4) for metrics and comparison.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Create from numeric value, clamping to valid range.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Background,
            1 => Self::Low,
            2 => Self::Medium,
            3 => Self::High,
            _ => Self::Critical,
        }
    }

    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for priority classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityConfig {
    /// EWMA half-life for output rate decay (seconds). Default: 10.
    pub rate_half_life_secs: f64,
    /// Lines/sec threshold to qualify as High priority. Default: 10.0.
    pub high_rate_threshold: f64,
    /// Lines/sec threshold for Medium (below high). Default: 1.0.
    pub medium_rate_threshold: f64,
    /// Seconds since last error detection to keep Critical. Default: 30.
    pub error_retention_secs: f64,
}

impl Default for PriorityConfig {
    fn default() -> Self {
        Self {
            rate_half_life_secs: 10.0,
            high_rate_threshold: 10.0,
            medium_rate_threshold: 1.0,
            error_retention_secs: 30.0,
        }
    }
}

// =============================================================================
// Output rate tracker (EWMA with exponential decay)
// =============================================================================

/// Tracks output rate using exponential weighted moving average.
///
/// The rate decays over time: after one half-life of silence the rate
/// drops to 50%, after three half-lives to ~12.5%. This ensures stale
/// high rates don't persist when a pane goes quiet.
#[derive(Debug, Clone)]
pub struct OutputRateTracker {
    /// EWMA of lines per second.
    ewma_lps: f64,
    /// When the last sample was recorded.
    last_sample: Instant,
    /// Decay half-life.
    half_life: Duration,
    /// Total lines observed (lifetime).
    total_lines: u64,
}

impl OutputRateTracker {
    /// Create a new tracker with the given half-life.
    pub fn new(half_life: Duration) -> Self {
        Self {
            ewma_lps: 0.0,
            last_sample: Instant::now(),
            half_life,
            total_lines: 0,
        }
    }

    /// Create with a specific start time (for testing).
    pub fn with_start(half_life: Duration, start: Instant) -> Self {
        Self {
            ewma_lps: 0.0,
            last_sample: start,
            half_life,
            total_lines: 0,
        }
    }

    /// Record new output and update the EWMA.
    pub fn record_output(&mut self, line_count: usize, now: Instant) {
        if line_count == 0 {
            return;
        }
        let elapsed = now.duration_since(self.last_sample);
        let elapsed_secs = elapsed.as_secs_f64();

        let decay = self.decay_factor(elapsed_secs);
        self.ewma_lps *= decay;

        if elapsed_secs > 1e-9 {
            let instant_rate = line_count as f64 / elapsed_secs;
            let alpha = 1.0 - decay;
            self.ewma_lps += alpha * instant_rate;
        }

        self.last_sample = now;
        self.total_lines += line_count as u64;
    }

    /// Current rate with time-decay applied.
    pub fn lines_per_second(&self, now: Instant) -> f64 {
        let elapsed = now.duration_since(self.last_sample).as_secs_f64();
        let decay = self.decay_factor(elapsed);
        self.ewma_lps * decay
    }

    /// Total lines observed over lifetime.
    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    /// Instant of last recorded output.
    pub fn last_sample_time(&self) -> Instant {
        self.last_sample
    }

    fn decay_factor(&self, elapsed_secs: f64) -> f64 {
        if self.half_life.as_secs_f64() < 1e-9 {
            return 0.0;
        }
        let exponent = -elapsed_secs * (2.0_f64.ln() / self.half_life.as_secs_f64());
        exponent.exp().clamp(0.0, 1.0)
    }
}

// =============================================================================
// Detected event signal
// =============================================================================

/// A detected event that influences priority classification.
///
/// Constructed from [`Detection`](crate::patterns::Detection) results.
#[derive(Debug, Clone)]
pub struct PrioritySignal {
    /// The event type from pattern detection (e.g. "rate_limited", "error").
    pub event_type: String,
    /// Severity: 0 = info, 1 = warning, 2 = critical.
    pub severity: u8,
    /// When this signal was observed.
    pub observed_at: Instant,
}

// =============================================================================
// Per-pane state
// =============================================================================

#[derive(Debug)]
struct PaneClassification {
    /// Current computed priority.
    priority: PanePriority,
    /// Activity tier from PaneTierClassifier.
    tier: PaneTier,
    /// Output rate tracker.
    rate: OutputRateTracker,
    /// Most recent error/critical signal timestamp.
    last_critical_signal: Option<Instant>,
    /// Most recent rate-limited signal timestamp.
    last_rate_limited_signal: Option<Instant>,
    /// Manual override (if set, bypasses automatic classification).
    manual_override: Option<PanePriority>,
}

// =============================================================================
// Priority classifier
// =============================================================================

/// Aggregate metrics for the priority system.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PriorityMetrics {
    /// Pane count per priority level.
    pub counts: HashMap<String, usize>,
    /// Total classification calls.
    pub total_classifications: u64,
    /// Total manual overrides active.
    pub override_count: usize,
    /// Total panes tracked.
    pub tracked_panes: usize,
}

/// Classifies panes into priority levels for resource allocation.
///
/// Thread-safe: per-pane state is distributed across a sharded `PaneMap`.
pub struct PriorityClassifier {
    config: PriorityConfig,
    panes: PaneMap<PaneClassification>,
    total_classifications: AtomicU64,
}

impl std::fmt::Debug for PriorityClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriorityClassifier")
            .field("config", &self.config)
            .finish()
    }
}

impl PriorityClassifier {
    /// Create a new classifier with the given config.
    pub fn new(config: PriorityConfig) -> Self {
        Self {
            config,
            panes: PaneMap::new(),
            total_classifications: AtomicU64::new(0),
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(PriorityConfig::default())
    }

    /// Register a pane for priority tracking.
    pub fn register_pane(&self, pane_id: u64) {
        let half_life = Duration::from_secs_f64(self.config.rate_half_life_secs);
        self.panes.insert_if_absent(pane_id, PaneClassification {
            priority: PanePriority::Medium,
            tier: PaneTier::Active,
            rate: OutputRateTracker::new(half_life),
            last_critical_signal: None,
            last_rate_limited_signal: None,
            manual_override: None,
        });
    }

    /// Unregister a pane.
    pub fn unregister_pane(&self, pane_id: u64) {
        self.panes.remove(pane_id);
    }

    /// Record output lines for a pane.
    pub fn record_output(&self, pane_id: u64, line_count: usize) {
        self.record_output_at(pane_id, line_count, Instant::now());
    }

    /// Record output with explicit timestamp (for testing).
    pub fn record_output_at(&self, pane_id: u64, line_count: usize, now: Instant) {
        self.panes.write_with(pane_id, |state| {
            state.rate.record_output(line_count, now);
        });
    }

    /// Update the activity tier for a pane (from PaneTierClassifier).
    pub fn update_tier(&self, pane_id: u64, tier: PaneTier) {
        self.panes.write_with(pane_id, |state| {
            state.tier = tier;
        });
    }

    /// Feed a detection signal into the classifier.
    pub fn observe_signal(&self, pane_id: u64, signal: &PrioritySignal) {
        self.panes.write_with(pane_id, |state| {
            if signal.severity >= 2
                || signal.event_type == "error"
                || signal.event_type == "needs_attention"
            {
                state.last_critical_signal = Some(signal.observed_at);
            }
            if signal.event_type == "rate_limited" {
                state.last_rate_limited_signal = Some(signal.observed_at);
            }
        });
    }

    /// Set a manual priority override (takes precedence over auto).
    pub fn set_override(&self, pane_id: u64, priority: PanePriority) {
        self.panes.write_with(pane_id, |state| {
            state.manual_override = Some(priority);
        });
    }

    /// Clear a manual override, returning to automatic classification.
    pub fn clear_override(&self, pane_id: u64) {
        self.panes.write_with(pane_id, |state| {
            state.manual_override = None;
        });
    }

    /// Classify a single pane and return its priority.
    pub fn classify(&self, pane_id: u64) -> PanePriority {
        self.classify_at(pane_id, Instant::now())
    }

    /// Classify with explicit timestamp (for testing).
    pub fn classify_at(&self, pane_id: u64, now: Instant) -> PanePriority {
        self.total_classifications.fetch_add(1, Ordering::Relaxed);
        self.panes
            .write_with(pane_id, |state| {
                let priority = self.compute_priority(state, now);
                state.priority = priority;
                priority
            })
            .unwrap_or(PanePriority::Low)
    }

    /// Classify all tracked panes.
    pub fn classify_all(&self) -> HashMap<u64, PanePriority> {
        self.classify_all_at(Instant::now())
    }

    /// Classify all with explicit timestamp.
    pub fn classify_all_at(&self, now: Instant) -> HashMap<u64, PanePriority> {
        self.panes
            .map_all_mut(|_pane_id, state| {
                self.total_classifications
                    .fetch_add(1, Ordering::Relaxed);
                let priority = self.compute_priority(state, now);
                state.priority = priority;
                priority
            })
            .into_iter()
            .collect()
    }

    /// Get the current cached priority without reclassifying.
    pub fn current_priority(&self, pane_id: u64) -> PanePriority {
        self.panes
            .read_with(pane_id, |s| s.priority)
            .unwrap_or(PanePriority::Low)
    }

    /// Get output rate for a pane (lines/sec with decay).
    pub fn output_rate(&self, pane_id: u64) -> f64 {
        self.output_rate_at(pane_id, Instant::now())
    }

    /// Get output rate at explicit timestamp.
    pub fn output_rate_at(&self, pane_id: u64, now: Instant) -> f64 {
        self.panes
            .read_with(pane_id, |s| s.rate.lines_per_second(now))
            .unwrap_or(0.0)
    }

    /// Check if a pane has a manual override set.
    pub fn has_override(&self, pane_id: u64) -> bool {
        self.panes
            .read_with(pane_id, |s| s.manual_override.is_some())
            .unwrap_or(false)
    }

    /// Return aggregate metrics.
    pub fn metrics(&self) -> PriorityMetrics {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut override_count = 0;

        self.panes.for_each_mut(|_pane_id, state| {
            *counts
                .entry(state.priority.label().to_string())
                .or_insert(0) += 1;
            if state.manual_override.is_some() {
                override_count += 1;
            }
        });

        PriorityMetrics {
            counts,
            total_classifications: self.total_classifications.load(Ordering::Relaxed),
            override_count,
            tracked_panes: self.panes.len(),
        }
    }

    /// Number of tracked panes.
    pub fn tracked_pane_count(&self) -> usize {
        self.panes.len()
    }

    // -------------------------------------------------------------------------
    // Internal classification logic
    // -------------------------------------------------------------------------

    fn compute_priority(&self, state: &PaneClassification, now: Instant) -> PanePriority {
        // Manual override always wins.
        if let Some(p) = state.manual_override {
            return p;
        }

        // Check for recent error/critical signal → Critical.
        if let Some(ts) = state.last_critical_signal {
            let age = now.duration_since(ts).as_secs_f64();
            if age < self.config.error_retention_secs {
                return PanePriority::Critical;
            }
        }

        // Check rate-limited signal → Background.
        if let Some(ts) = state.last_rate_limited_signal {
            let age = now.duration_since(ts).as_secs_f64();
            // Rate-limited decays faster (half the retention).
            if age < self.config.error_retention_secs / 2.0 {
                return PanePriority::Background;
            }
        }

        let rate = state.rate.lines_per_second(now);

        // Compose tier + rate into priority.
        match state.tier {
            PaneTier::Active if rate >= self.config.high_rate_threshold => PanePriority::High,
            PaneTier::Active => PanePriority::Medium,
            PaneTier::Thinking => PanePriority::Medium,
            PaneTier::Idle => PanePriority::Low,
            PaneTier::Background => PanePriority::Background,
            PaneTier::Dormant => PanePriority::Background,
        }
    }
}

// =============================================================================
// Suggested shedding order (for backpressure integration)
// =============================================================================

/// Return pane IDs sorted by priority (ascending), suitable for shedding.
///
/// Under backpressure, shed from the front of the returned list first
/// (Background, then Low, etc.).
pub fn shedding_order<S: BuildHasher>(priorities: &HashMap<u64, PanePriority, S>) -> Vec<u64> {
    let mut panes: Vec<(u64, PanePriority)> = priorities.iter().map(|(&id, &p)| (id, p)).collect();
    panes.sort_by_key(|&(id, p)| (p, id)); // stable: same priority → sort by id
    panes.into_iter().map(|(id, _)| id).collect()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> PriorityConfig {
        PriorityConfig {
            rate_half_life_secs: 10.0,
            high_rate_threshold: 10.0,
            medium_rate_threshold: 1.0,
            error_retention_secs: 30.0,
        }
    }

    // -- PanePriority enum tests --

    #[test]
    fn priority_ordering() {
        assert!(PanePriority::Critical > PanePriority::High);
        assert!(PanePriority::High > PanePriority::Medium);
        assert!(PanePriority::Medium > PanePriority::Low);
        assert!(PanePriority::Low > PanePriority::Background);
    }

    #[test]
    fn priority_as_u8_roundtrip() {
        for p in [
            PanePriority::Background,
            PanePriority::Low,
            PanePriority::Medium,
            PanePriority::High,
            PanePriority::Critical,
        ] {
            assert_eq!(PanePriority::from_u8(p.as_u8()), p);
        }
    }

    #[test]
    fn priority_from_u8_clamps() {
        assert_eq!(PanePriority::from_u8(255), PanePriority::Critical);
        assert_eq!(PanePriority::from_u8(100), PanePriority::Critical);
    }

    #[test]
    fn priority_labels() {
        assert_eq!(PanePriority::Background.label(), "background");
        assert_eq!(PanePriority::Critical.label(), "critical");
    }

    #[test]
    fn priority_serde_roundtrip() {
        let p = PanePriority::High;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"high\"");
        let back: PanePriority = serde_json::from_str(&json).unwrap();
        assert_eq!(back, PanePriority::High);
    }

    // -- OutputRateTracker tests --

    #[test]
    fn rate_tracker_starts_at_zero() {
        let now = Instant::now();
        let tracker = OutputRateTracker::with_start(Duration::from_secs(10), now);
        assert_eq!(tracker.lines_per_second(now), 0.0);
        assert_eq!(tracker.total_lines(), 0);
    }

    #[test]
    fn rate_tracker_records_output() {
        let start = Instant::now();
        let mut tracker = OutputRateTracker::with_start(Duration::from_secs(10), start);
        let t1 = start + Duration::from_secs(1);
        tracker.record_output(100, t1);
        // EWMA with alpha ≈ 0.067 (1s elapsed, 10s half-life):
        // rate = alpha * 100/1 ≈ 6.7 lps
        let rate = tracker.lines_per_second(t1);
        assert!(rate > 1.0, "rate={rate} should be >1");
        assert!(rate < 20.0, "rate={rate} should be <20 (EWMA smoothing)");
        assert_eq!(tracker.total_lines(), 100);
    }

    #[test]
    fn rate_tracker_zero_lines_ignored() {
        let start = Instant::now();
        let mut tracker = OutputRateTracker::with_start(Duration::from_secs(10), start);
        tracker.record_output(0, start + Duration::from_secs(1));
        assert_eq!(tracker.total_lines(), 0);
    }

    #[test]
    fn rate_tracker_decays_over_time() {
        let start = Instant::now();
        let half_life = Duration::from_secs(10);
        let mut tracker = OutputRateTracker::with_start(half_life, start);

        // Record 100 lines/sec burst.
        let t1 = start + Duration::from_secs(1);
        tracker.record_output(100, t1);
        let rate_at_burst = tracker.lines_per_second(t1);

        // After one half-life, rate should be ~50%.
        let t2 = t1 + half_life;
        let rate_after_one_half_life = tracker.lines_per_second(t2);
        let ratio = rate_after_one_half_life / rate_at_burst;
        assert!((ratio - 0.5).abs() < 0.05, "ratio={ratio} should be ~0.5");

        // After three half-lives, rate should be ~12.5%.
        let t3 = t1 + half_life * 3;
        let rate_after_three_half_lives = tracker.lines_per_second(t3);
        let ratio3 = rate_after_three_half_lives / rate_at_burst;
        assert!(
            (ratio3 - 0.125).abs() < 0.03,
            "ratio3={ratio3} should be ~0.125"
        );
    }

    #[test]
    fn rate_tracker_monotonic_decay() {
        let start = Instant::now();
        let mut tracker = OutputRateTracker::with_start(Duration::from_secs(10), start);
        tracker.record_output(50, start + Duration::from_millis(500));

        let mut prev_rate = f64::MAX;
        for i in 1..20 {
            let t = start + Duration::from_secs(i);
            let rate = tracker.lines_per_second(t);
            assert!(
                rate <= prev_rate + f64::EPSILON,
                "rate should monotonically decrease: {prev_rate} -> {rate}"
            );
            prev_rate = rate;
        }
    }

    // -- PriorityClassifier tests --

    #[test]
    fn classifier_register_and_unregister() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        assert_eq!(c.tracked_pane_count(), 1);
        c.unregister_pane(1);
        assert_eq!(c.tracked_pane_count(), 0);
    }

    #[test]
    fn classifier_unregistered_pane_returns_low() {
        let c = PriorityClassifier::new(make_config());
        assert_eq!(c.classify(999), PanePriority::Low);
    }

    #[test]
    fn classifier_default_is_medium() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        // Default tier is Active + 0 rate → Medium.
        assert_eq!(c.classify(1), PanePriority::Medium);
    }

    #[test]
    fn classifier_high_rate_produces_high() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);

        let now = Instant::now();
        // Simulate high output.
        c.record_output_at(1, 200, now + Duration::from_secs(1));
        c.update_tier(1, PaneTier::Active);
        assert_eq!(
            c.classify_at(1, now + Duration::from_secs(1)),
            PanePriority::High
        );
    }

    #[test]
    fn classifier_idle_tier_produces_low() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.update_tier(1, PaneTier::Idle);
        assert_eq!(c.classify(1), PanePriority::Low);
    }

    #[test]
    fn classifier_dormant_tier_produces_background() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.update_tier(1, PaneTier::Dormant);
        assert_eq!(c.classify(1), PanePriority::Background);
    }

    #[test]
    fn classifier_background_tier_produces_background() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.update_tier(1, PaneTier::Background);
        assert_eq!(c.classify(1), PanePriority::Background);
    }

    #[test]
    fn classifier_thinking_tier_produces_medium() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.update_tier(1, PaneTier::Thinking);
        assert_eq!(c.classify(1), PanePriority::Medium);
    }

    #[test]
    fn classifier_error_signal_produces_critical() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        let now = Instant::now();
        c.observe_signal(
            1,
            &PrioritySignal {
                event_type: "error".to_string(),
                severity: 2,
                observed_at: now,
            },
        );
        assert_eq!(c.classify_at(1, now), PanePriority::Critical);
    }

    #[test]
    fn classifier_error_signal_decays() {
        let config = make_config();
        let retention = config.error_retention_secs;
        let c = PriorityClassifier::new(config);
        c.register_pane(1);
        let now = Instant::now();
        c.observe_signal(
            1,
            &PrioritySignal {
                event_type: "error".to_string(),
                severity: 2,
                observed_at: now,
            },
        );
        // Still Critical within retention window.
        assert_eq!(
            c.classify_at(1, now + Duration::from_secs(5)),
            PanePriority::Critical
        );
        // Past retention → falls back to tier-based.
        let past = now + Duration::from_secs_f64(retention + 1.0);
        assert_ne!(c.classify_at(1, past), PanePriority::Critical);
    }

    #[test]
    fn classifier_rate_limited_signal_produces_background() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        let now = Instant::now();
        c.observe_signal(
            1,
            &PrioritySignal {
                event_type: "rate_limited".to_string(),
                severity: 1,
                observed_at: now,
            },
        );
        assert_eq!(c.classify_at(1, now), PanePriority::Background);
    }

    #[test]
    fn classifier_manual_override_wins() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.update_tier(1, PaneTier::Dormant); // Would be Background.
        c.set_override(1, PanePriority::Critical);
        assert_eq!(c.classify(1), PanePriority::Critical);
        assert!(c.has_override(1));
    }

    #[test]
    fn classifier_clear_override() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.set_override(1, PanePriority::Critical);
        c.clear_override(1);
        assert!(!c.has_override(1));
        c.update_tier(1, PaneTier::Dormant);
        assert_eq!(c.classify(1), PanePriority::Background);
    }

    #[test]
    fn classifier_classify_all() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.register_pane(2);
        c.register_pane(3);
        c.update_tier(1, PaneTier::Active);
        c.update_tier(2, PaneTier::Idle);
        c.update_tier(3, PaneTier::Dormant);

        let all = c.classify_all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[&1], PanePriority::Medium);
        assert_eq!(all[&2], PanePriority::Low);
        assert_eq!(all[&3], PanePriority::Background);
    }

    #[test]
    fn classifier_current_priority_cached() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.update_tier(1, PaneTier::Idle);
        c.classify(1);
        assert_eq!(c.current_priority(1), PanePriority::Low);
    }

    #[test]
    fn classifier_metrics() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        c.register_pane(2);
        c.set_override(1, PanePriority::Critical);
        c.classify(1);
        c.classify(2);

        let m = c.metrics();
        assert_eq!(m.tracked_panes, 2);
        assert_eq!(m.override_count, 1);
        assert_eq!(m.total_classifications, 2);
    }

    #[test]
    fn classifier_output_rate() {
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        let now = Instant::now();
        c.record_output_at(1, 50, now + Duration::from_secs(1));
        let rate = c.output_rate_at(1, now + Duration::from_secs(1));
        assert!(rate > 0.0);
    }

    #[test]
    fn classifier_error_over_rate_limited() {
        // Error signal should take precedence over rate-limited signal.
        let c = PriorityClassifier::new(make_config());
        c.register_pane(1);
        let now = Instant::now();
        c.observe_signal(
            1,
            &PrioritySignal {
                event_type: "rate_limited".to_string(),
                severity: 1,
                observed_at: now,
            },
        );
        c.observe_signal(
            1,
            &PrioritySignal {
                event_type: "error".to_string(),
                severity: 2,
                observed_at: now,
            },
        );
        // Critical wins over Background.
        assert_eq!(c.classify_at(1, now), PanePriority::Critical);
    }

    // -- Shedding order tests --

    #[test]
    fn shedding_order_sorts_by_priority() {
        let mut priorities = HashMap::new();
        priorities.insert(1, PanePriority::Critical);
        priorities.insert(2, PanePriority::Background);
        priorities.insert(3, PanePriority::High);
        priorities.insert(4, PanePriority::Low);
        priorities.insert(5, PanePriority::Medium);

        let order = shedding_order(&priorities);
        // Should be: Background(2), Low(4), Medium(5), High(3), Critical(1)
        assert_eq!(order, vec![2, 4, 5, 3, 1]);
    }

    #[test]
    fn shedding_order_stable_for_same_priority() {
        let mut priorities = HashMap::new();
        priorities.insert(10, PanePriority::Medium);
        priorities.insert(5, PanePriority::Medium);
        priorities.insert(20, PanePriority::Medium);

        let order = shedding_order(&priorities);
        // Same priority → sorted by pane id.
        assert_eq!(order, vec![5, 10, 20]);
    }

    // -- Proptest --

    #[cfg(test)]
    mod proptest_priority {
        use super::*;
        use proptest::prelude::*;

        fn arb_pane_tier() -> impl Strategy<Value = PaneTier> {
            prop_oneof![
                Just(PaneTier::Active),
                Just(PaneTier::Thinking),
                Just(PaneTier::Idle),
                Just(PaneTier::Background),
                Just(PaneTier::Dormant),
            ]
        }

        fn arb_priority() -> impl Strategy<Value = PanePriority> {
            prop_oneof![
                Just(PanePriority::Background),
                Just(PanePriority::Low),
                Just(PanePriority::Medium),
                Just(PanePriority::High),
                Just(PanePriority::Critical),
            ]
        }

        proptest! {
            /// Error state always outranks idle.
            #[test]
            fn error_always_outranks_idle(
                tier_a in arb_pane_tier(),
                _tier_b in arb_pane_tier(),
            ) {
                let c = PriorityClassifier::new(PriorityConfig::default());
                c.register_pane(1);
                c.register_pane(2);
                let now = Instant::now();

                // Pane 1 has error signal.
                c.observe_signal(1, &PrioritySignal {
                    event_type: "error".to_string(),
                    severity: 2,
                    observed_at: now,
                });
                c.update_tier(1, tier_a);

                // Pane 2 is idle with no signals.
                c.update_tier(2, PaneTier::Idle);

                let p1 = c.classify_at(1, now);
                let p2 = c.classify_at(2, now);
                prop_assert!(p1 >= p2, "error pane ({:?}) should be >= idle pane ({:?})", p1, p2);
            }

            /// Manual override always respected.
            #[test]
            fn override_always_wins(
                tier in arb_pane_tier(),
                override_p in arb_priority(),
                has_error in proptest::bool::ANY,
            ) {
                let c = PriorityClassifier::new(PriorityConfig::default());
                c.register_pane(1);
                let now = Instant::now();

                c.update_tier(1, tier);
                if has_error {
                    c.observe_signal(1, &PrioritySignal {
                        event_type: "error".to_string(),
                        severity: 2,
                        observed_at: now,
                    });
                }
                c.set_override(1, override_p);

                let result = c.classify_at(1, now);
                prop_assert_eq!(result, override_p);
            }

            /// Decay is monotonically non-increasing.
            #[test]
            fn decay_monotonic(
                lines in 1u32..1000u32,
                samples in 2u32..50u32,
            ) {
                let start = Instant::now();
                let mut tracker = OutputRateTracker::with_start(
                    Duration::from_secs(10),
                    start,
                );
                tracker.record_output(lines as usize, start + Duration::from_millis(500));

                let mut prev = f64::MAX;
                for i in 1..=samples {
                    let t = start + Duration::from_secs(i as u64);
                    let rate = tracker.lines_per_second(t);
                    prop_assert!(
                        rate <= prev + f64::EPSILON,
                        "decay must be monotonic: {prev} -> {rate}"
                    );
                    prev = rate;
                }
            }

            /// Priority is a total order (transitivity).
            #[test]
            fn total_order(
                a in arb_priority(),
                b in arb_priority(),
                c in arb_priority(),
            ) {
                // If a <= b and b <= c, then a <= c.
                if a <= b && b <= c {
                    prop_assert!(a <= c);
                }
            }

            /// from_u8(as_u8(p)) == p for all priorities.
            #[test]
            fn u8_roundtrip(p in arb_priority()) {
                prop_assert_eq!(PanePriority::from_u8(p.as_u8()), p);
            }
        }
    }
}
