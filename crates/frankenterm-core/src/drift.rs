//! ADWIN-based pattern drift detection (Bifet & Gavalda 2007).
//!
//! Detects when pattern detection rates change significantly, signaling
//! that regex rules may have become stale due to agent version updates.
//!
//! # Algorithm
//!
//! ADWIN maintains a variable-length window of observations.  After each
//! new observation it tests whether the window can be split into two
//! sub-windows W₀ (old) and W₁ (new) such that:
//!
//!   |μ(W₀) - μ(W₁)| ≥ ε_cut
//!
//! where ε_cut depends on sub-window sizes and a confidence parameter δ.
//! If a valid split is found, W₀ is dropped (drift detected).
//!
//! # Usage
//!
//! ```text
//! Pattern Engine ──► DriftMonitor ──► DriftEvent
//!   (detection        (per-rule        (rate drop/spike
//!    counts)           ADWIN)           alerts)
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// =============================================================================
// ADWIN Window
// =============================================================================

/// ADWIN (ADaptive WINdowing) detector for a single stream of observations.
///
/// Maintains a variable-length window that automatically shrinks when
/// a statistically significant change in the mean is detected.
#[derive(Debug, Clone)]
pub struct AdwinWindow {
    /// All observations in the current window.
    window: Vec<f64>,
    /// Running sum of the window (for fast mean computation).
    sum: f64,
    /// Confidence parameter δ (lower = more conservative, fewer false alarms).
    delta: f64,
}

impl AdwinWindow {
    /// Create a new ADWIN window with the given confidence parameter.
    ///
    /// - `delta`: Confidence parameter (typical: 0.002 to 0.05).
    ///   Lower values reduce false positives but increase detection delay.
    #[must_use]
    pub fn new(delta: f64) -> Self {
        Self {
            window: Vec::new(),
            sum: 0.0,
            delta: delta.clamp(1e-10, 1.0),
        }
    }

    /// Add a new observation and check for drift.
    ///
    /// Returns `Some(DriftInfo)` if drift was detected (old data dropped),
    /// or `None` if no drift.
    pub fn push(&mut self, value: f64) -> Option<DriftInfo> {
        self.window.push(value);
        self.sum += value;

        self.check_and_shrink()
    }

    /// Number of observations in the current window.
    #[must_use]
    pub fn len(&self) -> usize {
        self.window.len()
    }

    /// Whether the window is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Mean of the current window.
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.window.is_empty() {
            return 0.0;
        }
        self.sum / self.window.len() as f64
    }

    /// Variance of the current window (population variance).
    #[must_use]
    pub fn variance(&self) -> f64 {
        if self.window.len() < 2 {
            return 0.0;
        }
        let mean = self.mean();
        let sum_sq: f64 = self.window.iter().map(|x| (x - mean).powi(2)).sum();
        sum_sq / self.window.len() as f64
    }

    /// Reset the window, discarding all observations.
    pub fn reset(&mut self) {
        self.window.clear();
        self.sum = 0.0;
    }

    /// Current confidence parameter.
    #[must_use]
    pub fn delta(&self) -> f64 {
        self.delta
    }

    /// Internal: check all possible splits and shrink if drift found.
    fn check_and_shrink(&mut self) -> Option<DriftInfo> {
        let n = self.window.len();
        if n < 4 {
            return None; // Need at least 2 elements per sub-window
        }

        // Try splits from the oldest data forward.
        // For each split point, test if |mean(W0) - mean(W1)| >= epsilon_cut.
        let mut prefix_sum = 0.0;

        for split in 1..n {
            prefix_sum += self.window[split - 1];

            let n0 = split as f64;
            let n1 = (n - split) as f64;

            // Need at least 2 observations in each sub-window for meaningful test
            if split < 2 || (n - split) < 2 {
                continue;
            }

            let mu0 = prefix_sum / n0;
            let mu1 = (self.sum - prefix_sum) / n1;
            let diff = (mu0 - mu1).abs();

            let eps = self.epsilon_cut(n0, n1);

            if diff >= eps {
                // Drift detected — drop W0 (old data)
                let old_mean = mu0;
                let new_mean = mu1;
                let dropped = split;

                self.window.drain(..split);
                self.sum = self.window.iter().sum();

                return Some(DriftInfo {
                    old_mean,
                    new_mean,
                    dropped_count: dropped,
                    remaining_count: self.window.len(),
                    mean_diff: diff,
                    threshold: eps,
                });
            }
        }

        None
    }

    /// Compute the Hoeffding-bound based epsilon_cut for a given split.
    ///
    /// ε_cut = sqrt( (1/(2·m)) · ln(4·n/δ) )
    ///
    /// where m = 1/(1/n₀ + 1/n₁)  (harmonic-ish combination)
    ///       n = n₀ + n₁ (total window size)
    fn epsilon_cut(&self, n0: f64, n1: f64) -> f64 {
        let n = n0 + n1;
        let m = 1.0 / (1.0 / n0 + 1.0 / n1);
        let ln_term = (4.0 * n / self.delta).ln();
        if ln_term <= 0.0 {
            return f64::MAX; // Degenerate: never trigger
        }
        (ln_term / (2.0 * m)).sqrt()
    }
}

// =============================================================================
// Drift Info
// =============================================================================

/// Information about a detected drift event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftInfo {
    /// Mean of the old (dropped) sub-window.
    pub old_mean: f64,
    /// Mean of the new (retained) sub-window.
    pub new_mean: f64,
    /// Number of observations dropped from the old sub-window.
    pub dropped_count: usize,
    /// Number of observations remaining after shrink.
    pub remaining_count: usize,
    /// Absolute difference between old and new means.
    pub mean_diff: f64,
    /// Epsilon threshold that was exceeded.
    pub threshold: f64,
}

impl DriftInfo {
    /// Whether this represents a rate drop (new mean < old mean).
    #[must_use]
    pub fn is_drop(&self) -> bool {
        self.new_mean < self.old_mean
    }

    /// Whether this represents a rate spike (new mean > old mean).
    #[must_use]
    pub fn is_spike(&self) -> bool {
        self.new_mean > self.old_mean
    }

    /// Relative change as a fraction of the old mean.
    /// Returns `None` if old mean is zero.
    #[must_use]
    pub fn relative_change(&self) -> Option<f64> {
        if self.old_mean.abs() < f64::EPSILON {
            return None;
        }
        Some((self.new_mean - self.old_mean) / self.old_mean)
    }
}

// =============================================================================
// Drift Event
// =============================================================================

/// Type of drift detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftType {
    /// Detection rate dropped significantly.
    RateDrop,
    /// Detection rate spiked significantly.
    RateSpike,
}

/// A drift event for a specific pattern rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftEvent {
    /// The pattern rule that drifted.
    pub rule_id: String,
    /// Type of drift detected.
    pub drift_type: DriftType,
    /// Drift details.
    pub info: DriftInfo,
    /// Suggested action.
    pub suggestion: String,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the drift detection system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftConfig {
    /// Whether drift detection is enabled.
    pub enabled: bool,
    /// ADWIN confidence parameter δ (default: 0.01).
    pub confidence: f64,
    /// Minimum observations in the ADWIN window before drift can be detected.
    pub min_window_size: usize,
    /// Maximum window size to bound memory usage.
    pub max_window_size: usize,
    /// Minimum absolute mean difference to report as drift.
    /// Prevents noisy alerts on tiny changes.
    pub min_mean_diff: f64,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            confidence: 0.01,
            min_window_size: 10,
            max_window_size: 2000,
            min_mean_diff: 0.5,
        }
    }
}

// =============================================================================
// Per-Rule Monitor
// =============================================================================

/// Tracks detection rate for a single pattern rule using ADWIN.
#[derive(Debug, Clone)]
pub struct RuleMonitor {
    rule_id: String,
    window: AdwinWindow,
    total_observations: usize,
    total_drifts: usize,
    last_drift: Option<DriftInfo>,
}

impl RuleMonitor {
    /// Create a new monitor for a specific rule.
    #[must_use]
    pub fn new(rule_id: String, delta: f64) -> Self {
        Self {
            rule_id,
            window: AdwinWindow::new(delta),
            total_observations: 0,
            total_drifts: 0,
            last_drift: None,
        }
    }

    /// Record a detection rate observation and check for drift.
    ///
    /// Returns `Some(DriftEvent)` if drift was detected.
    pub fn observe(&mut self, rate: f64, config: &DriftConfig) -> Option<DriftEvent> {
        self.total_observations += 1;

        // Enforce max window size by dropping oldest if at capacity
        if self.window.len() >= config.max_window_size {
            // Manually shrink: remove oldest quarter
            let to_drop = config.max_window_size / 4;
            self.window.window.drain(..to_drop);
            self.window.sum = self.window.window.iter().sum();
        }

        let drift = self.window.push(rate)?;

        // Skip if below minimum window or minimum diff
        if self.total_observations < config.min_window_size {
            return None;
        }
        if drift.mean_diff < config.min_mean_diff {
            return None;
        }

        self.total_drifts += 1;
        self.last_drift = Some(drift.clone());

        let drift_type = if drift.is_drop() {
            DriftType::RateDrop
        } else {
            DriftType::RateSpike
        };

        let suggestion = match drift_type {
            DriftType::RateDrop => format!(
                "Rule '{}' detection rate dropped from {:.1}/period to {:.1}/period. \
                 Consider capturing current agent output to check if the pattern changed.",
                self.rule_id, drift.old_mean, drift.new_mean
            ),
            DriftType::RateSpike => format!(
                "Rule '{}' detection rate increased from {:.1}/period to {:.1}/period. \
                 May indicate agent version change or new false positives.",
                self.rule_id, drift.old_mean, drift.new_mean
            ),
        };

        Some(DriftEvent {
            rule_id: self.rule_id.clone(),
            drift_type,
            info: drift,
            suggestion,
        })
    }

    /// Number of observations processed.
    #[must_use]
    pub fn total_observations(&self) -> usize {
        self.total_observations
    }

    /// Number of drift events detected.
    #[must_use]
    pub fn total_drifts(&self) -> usize {
        self.total_drifts
    }

    /// Current window size.
    #[must_use]
    pub fn window_size(&self) -> usize {
        self.window.len()
    }

    /// Current window mean (latest detection rate estimate).
    #[must_use]
    pub fn current_rate(&self) -> f64 {
        self.window.mean()
    }

    /// Last detected drift, if any.
    #[must_use]
    pub fn last_drift(&self) -> Option<&DriftInfo> {
        self.last_drift.as_ref()
    }

    /// Rule ID.
    #[must_use]
    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    /// Reset the monitor.
    pub fn reset(&mut self) {
        self.window.reset();
        self.total_observations = 0;
        self.total_drifts = 0;
        self.last_drift = None;
    }
}

// =============================================================================
// Drift Monitor (multi-rule)
// =============================================================================

/// Multi-rule drift monitor managing ADWIN instances for all tracked patterns.
#[derive(Debug, Clone)]
pub struct DriftMonitor {
    config: DriftConfig,
    monitors: HashMap<String, RuleMonitor>,
}

/// Summary of all monitored rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftSummary {
    pub total_rules: usize,
    pub total_drifts: usize,
    pub rules: Vec<RuleSummary>,
}

/// Summary for a single rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSummary {
    pub rule_id: String,
    pub window_size: usize,
    pub current_rate: f64,
    pub total_observations: usize,
    pub total_drifts: usize,
    pub last_drift: Option<DriftInfo>,
}

impl DriftMonitor {
    /// Create a new multi-rule drift monitor.
    #[must_use]
    pub fn new(config: DriftConfig) -> Self {
        Self {
            config,
            monitors: HashMap::new(),
        }
    }

    /// Register a rule for drift monitoring.
    pub fn register_rule(&mut self, rule_id: &str) {
        self.monitors.entry(rule_id.to_string()).or_insert_with(|| {
            RuleMonitor::new(rule_id.to_string(), self.config.confidence)
        });
    }

    /// Record a detection rate observation for a rule.
    ///
    /// Auto-registers the rule if not already tracked.
    /// Returns `Some(DriftEvent)` if drift was detected.
    pub fn observe(&mut self, rule_id: &str, rate: f64) -> Option<DriftEvent> {
        if !self.config.enabled {
            return None;
        }

        let monitor = self
            .monitors
            .entry(rule_id.to_string())
            .or_insert_with(|| RuleMonitor::new(rule_id.to_string(), self.config.confidence));

        monitor.observe(rate, &self.config)
    }

    /// Get the monitor for a specific rule.
    #[must_use]
    pub fn rule_monitor(&self, rule_id: &str) -> Option<&RuleMonitor> {
        self.monitors.get(rule_id)
    }

    /// Number of monitored rules.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.monitors.len()
    }

    /// Produce a summary of all monitored rules.
    #[must_use]
    pub fn summary(&self) -> DriftSummary {
        let mut rules: Vec<RuleSummary> = self
            .monitors
            .values()
            .map(|m| RuleSummary {
                rule_id: m.rule_id().to_string(),
                window_size: m.window_size(),
                current_rate: m.current_rate(),
                total_observations: m.total_observations(),
                total_drifts: m.total_drifts(),
                last_drift: m.last_drift().cloned(),
            })
            .collect();
        rules.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));

        let total_drifts = rules.iter().map(|r| r.total_drifts).sum();

        DriftSummary {
            total_rules: rules.len(),
            total_drifts,
            rules,
        }
    }

    /// Get the config.
    #[must_use]
    pub fn config(&self) -> &DriftConfig {
        &self.config
    }

    /// Reset all rule monitors.
    pub fn reset(&mut self) {
        for monitor in self.monitors.values_mut() {
            monitor.reset();
        }
    }

    /// Remove a rule from monitoring.
    pub fn unregister_rule(&mut self, rule_id: &str) -> bool {
        self.monitors.remove(rule_id).is_some()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── ADWIN Window basics ─────────────────────────────────────────────

    #[test]
    fn empty_window() {
        let w = AdwinWindow::new(0.01);
        assert!(w.is_empty());
        assert_eq!(w.len(), 0);
        assert!((w.mean() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn single_observation() {
        let mut w = AdwinWindow::new(0.01);
        let drift = w.push(5.0);
        assert!(drift.is_none());
        assert_eq!(w.len(), 1);
        assert!((w.mean() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn mean_tracks_observations() {
        let mut w = AdwinWindow::new(0.01);
        w.push(10.0);
        w.push(20.0);
        w.push(30.0);
        assert!((w.mean() - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn variance_correct() {
        let mut w = AdwinWindow::new(0.001); // Very conservative delta to avoid drift
        // Use values close enough that ADWIN won't detect drift
        for v in [4.5, 5.5, 4.5, 5.5, 4.5, 5.5, 4.5, 5.5] {
            w.push(v);
        }
        // Mean = 5.0, variance = 0.25
        assert!(
            (w.mean() - 5.0).abs() < 0.1,
            "mean={}, expected ~5.0",
            w.mean()
        );
        assert!(
            (w.variance() - 0.25).abs() < 0.1,
            "variance={}, expected ~0.25",
            w.variance()
        );
    }

    #[test]
    fn stationary_no_drift() {
        let mut w = AdwinWindow::new(0.01);
        let mut drifts = 0;
        // Feed a constant signal — should never trigger drift
        for _ in 0..200 {
            if w.push(5.0).is_some() {
                drifts += 1;
            }
        }
        assert_eq!(drifts, 0, "constant signal should produce no drift");
    }

    #[test]
    fn detects_mean_shift() {
        let mut w = AdwinWindow::new(0.01);
        let mut drift_detected = false;

        // Regime 1: mean ≈ 5.0
        for _ in 0..100 {
            w.push(5.0);
        }

        // Regime 2: mean ≈ 50.0 (large shift)
        for _ in 0..100 {
            if w.push(50.0).is_some() {
                drift_detected = true;
                break;
            }
        }

        assert!(drift_detected, "should detect 5.0 → 50.0 mean shift");
    }

    #[test]
    fn drift_info_direction() {
        let mut w = AdwinWindow::new(0.05); // More sensitive

        // Build up low values
        for _ in 0..50 {
            w.push(1.0);
        }

        // Switch to high values
        let mut info = None;
        for _ in 0..50 {
            if let Some(d) = w.push(20.0) {
                info = Some(d);
                break;
            }
        }

        let drift = info.expect("should detect drift");
        assert!(drift.is_spike(), "new mean should be higher");
        assert!(!drift.is_drop());
    }

    #[test]
    fn drift_drops_old_data() {
        let mut w = AdwinWindow::new(0.01);

        for _ in 0..100 {
            w.push(5.0);
        }
        let size_before = w.len();

        // Trigger drift with large shift
        let mut triggered = false;
        for _ in 0..100 {
            if let Some(info) = w.push(100.0) {
                assert!(
                    info.dropped_count > 0,
                    "drift should drop old observations"
                );
                assert!(
                    w.len() < size_before,
                    "window should shrink after drift"
                );
                triggered = true;
                break;
            }
        }
        assert!(triggered);
    }

    #[test]
    fn window_reset() {
        let mut w = AdwinWindow::new(0.01);
        for _ in 0..50 {
            w.push(10.0);
        }
        assert!(!w.is_empty());

        w.reset();
        assert!(w.is_empty());
        assert_eq!(w.len(), 0);
        assert!((w.mean() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn delta_clamped() {
        let w = AdwinWindow::new(-1.0);
        assert!(w.delta() > 0.0);

        let w2 = AdwinWindow::new(2.0);
        assert!(w2.delta() <= 1.0);
    }

    // ── DriftInfo ───────────────────────────────────────────────────────

    #[test]
    fn drift_info_relative_change() {
        let info = DriftInfo {
            old_mean: 10.0,
            new_mean: 5.0,
            dropped_count: 50,
            remaining_count: 50,
            mean_diff: 5.0,
            threshold: 3.0,
        };
        let rc = info.relative_change().unwrap();
        assert!((rc - (-0.5)).abs() < f64::EPSILON);
    }

    #[test]
    fn drift_info_zero_old_mean() {
        let info = DriftInfo {
            old_mean: 0.0,
            new_mean: 5.0,
            dropped_count: 10,
            remaining_count: 10,
            mean_diff: 5.0,
            threshold: 3.0,
        };
        assert!(info.relative_change().is_none());
    }

    #[test]
    fn drift_info_serde_roundtrip() {
        let info = DriftInfo {
            old_mean: 8.5,
            new_mean: 2.1,
            dropped_count: 30,
            remaining_count: 20,
            mean_diff: 6.4,
            threshold: 1.5,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: DriftInfo = serde_json::from_str(&json).unwrap();
        assert!((parsed.old_mean - 8.5).abs() < f64::EPSILON);
        assert_eq!(parsed.dropped_count, 30);
    }

    // ── DriftConfig ─────────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = DriftConfig::default();
        assert!(c.enabled);
        assert!((c.confidence - 0.01).abs() < f64::EPSILON);
        assert_eq!(c.min_window_size, 10);
        assert_eq!(c.max_window_size, 2000);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = DriftConfig {
            enabled: false,
            confidence: 0.05,
            min_window_size: 20,
            max_window_size: 500,
            min_mean_diff: 1.0,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: DriftConfig = serde_json::from_str(&json).unwrap();
        assert!(!parsed.enabled);
        assert!((parsed.confidence - 0.05).abs() < f64::EPSILON);
        assert_eq!(parsed.max_window_size, 500);
    }

    // ── RuleMonitor ─────────────────────────────────────────────────────

    #[test]
    fn rule_monitor_stable_rate() {
        let config = DriftConfig::default();
        let mut mon = RuleMonitor::new("test.rule".to_string(), 0.01);

        for _ in 0..100 {
            let event = mon.observe(3.0, &config);
            assert!(event.is_none(), "stable rate should not trigger drift");
        }

        assert_eq!(mon.total_observations(), 100);
        assert_eq!(mon.total_drifts(), 0);
        assert!((mon.current_rate() - 3.0).abs() < 0.1);
    }

    #[test]
    fn rule_monitor_detects_rate_drop() {
        let config = DriftConfig {
            min_window_size: 5,
            min_mean_diff: 0.5,
            ..Default::default()
        };
        let mut mon = RuleMonitor::new("error.pattern".to_string(), 0.01);

        // High rate period
        for _ in 0..80 {
            mon.observe(10.0, &config);
        }

        // Rate drops
        let mut detected = false;
        for _ in 0..80 {
            if let Some(event) = mon.observe(0.5, &config) {
                assert_eq!(event.drift_type, DriftType::RateDrop);
                assert!(event.info.is_drop());
                assert!(event.suggestion.contains("error.pattern"));
                detected = true;
                break;
            }
        }
        assert!(detected, "should detect rate drop from 10.0 to 0.5");
        assert!(mon.total_drifts() > 0);
    }

    #[test]
    fn rule_monitor_detects_rate_spike() {
        let config = DriftConfig {
            min_window_size: 5,
            min_mean_diff: 0.5,
            ..Default::default()
        };
        let mut mon = RuleMonitor::new("rate_limit.pattern".to_string(), 0.01);

        // Low rate period
        for _ in 0..80 {
            mon.observe(0.5, &config);
        }

        // Rate spikes
        let mut detected = false;
        for _ in 0..80 {
            if let Some(event) = mon.observe(15.0, &config) {
                assert_eq!(event.drift_type, DriftType::RateSpike);
                assert!(event.info.is_spike());
                detected = true;
                break;
            }
        }
        assert!(detected, "should detect rate spike from 0.5 to 15.0");
    }

    #[test]
    fn rule_monitor_respects_min_mean_diff() {
        let config = DriftConfig {
            min_window_size: 5,
            min_mean_diff: 5.0, // High threshold
            confidence: 0.1,    // Very sensitive ADWIN
            ..Default::default()
        };
        let mut mon = RuleMonitor::new("tiny.change".to_string(), 0.1);

        // Small shift: 10.0 → 10.5
        for _ in 0..50 {
            mon.observe(10.0, &config);
        }
        for _ in 0..50 {
            let event = mon.observe(10.5, &config);
            // Even if ADWIN detects it, min_mean_diff filters it out
            if let Some(e) = event {
                assert!(
                    e.info.mean_diff >= 5.0,
                    "should not report drifts below min_mean_diff"
                );
            }
        }
    }

    #[test]
    fn rule_monitor_reset() {
        let config = DriftConfig::default();
        let mut mon = RuleMonitor::new("test".to_string(), 0.01);

        for _ in 0..20 {
            mon.observe(5.0, &config);
        }
        assert!(mon.total_observations() > 0);

        mon.reset();
        assert_eq!(mon.total_observations(), 0);
        assert_eq!(mon.total_drifts(), 0);
        assert_eq!(mon.window_size(), 0);
    }

    // ── DriftMonitor (multi-rule) ───────────────────────────────────────

    #[test]
    fn drift_monitor_register_and_observe() {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        dm.register_rule("rule_a");
        dm.register_rule("rule_b");
        assert_eq!(dm.rule_count(), 2);

        dm.observe("rule_a", 5.0);
        dm.observe("rule_b", 3.0);

        assert!(dm.rule_monitor("rule_a").is_some());
        assert!(dm.rule_monitor("rule_b").is_some());
    }

    #[test]
    fn drift_monitor_auto_registers() {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        assert_eq!(dm.rule_count(), 0);
        dm.observe("new_rule", 5.0);
        assert_eq!(dm.rule_count(), 1);
    }

    #[test]
    fn drift_monitor_disabled() {
        let config = DriftConfig {
            enabled: false,
            ..Default::default()
        };
        let mut dm = DriftMonitor::new(config);

        // Should never detect drift when disabled
        for _ in 0..100 {
            dm.observe("rule", 5.0);
        }
        for _ in 0..100 {
            let event = dm.observe("rule", 100.0);
            assert!(event.is_none(), "disabled monitor should never fire");
        }
    }

    #[test]
    fn drift_monitor_summary() {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        dm.observe("alpha", 5.0);
        dm.observe("beta", 10.0);
        dm.observe("alpha", 5.0);

        let summary = dm.summary();
        assert_eq!(summary.total_rules, 2);
        // Rules sorted alphabetically
        assert_eq!(summary.rules[0].rule_id, "alpha");
        assert_eq!(summary.rules[1].rule_id, "beta");
        assert_eq!(summary.rules[0].total_observations, 2);
    }

    #[test]
    fn drift_monitor_summary_serializes() {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        dm.observe("test_rule", 5.0);
        let summary = dm.summary();

        let json = serde_json::to_string_pretty(&summary).unwrap();
        let parsed: DriftSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_rules, 1);
    }

    #[test]
    fn drift_monitor_unregister() {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        dm.register_rule("to_remove");
        dm.register_rule("keep");
        assert_eq!(dm.rule_count(), 2);

        assert!(dm.unregister_rule("to_remove"));
        assert_eq!(dm.rule_count(), 1);

        assert!(!dm.unregister_rule("nonexistent"));
    }

    #[test]
    fn drift_monitor_reset() {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for _ in 0..50 {
            dm.observe("rule1", 5.0);
        }

        dm.reset();

        let mon = dm.rule_monitor("rule1").unwrap();
        assert_eq!(mon.total_observations(), 0);
    }

    // ── DriftEvent ──────────────────────────────────────────────────────

    #[test]
    fn drift_event_serde_roundtrip() {
        let event = DriftEvent {
            rule_id: "test.pattern".to_string(),
            drift_type: DriftType::RateDrop,
            info: DriftInfo {
                old_mean: 10.0,
                new_mean: 1.0,
                dropped_count: 50,
                remaining_count: 30,
                mean_diff: 9.0,
                threshold: 2.0,
            },
            suggestion: "Check the pattern.".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DriftEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.rule_id, "test.pattern");
        assert_eq!(parsed.drift_type, DriftType::RateDrop);
    }

    // ── Window invariants ───────────────────────────────────────────────

    #[test]
    fn window_size_always_positive_after_push() {
        let mut w = AdwinWindow::new(0.05);
        for i in 0..500 {
            let val = if i < 200 { 5.0 } else { 50.0 };
            w.push(val);
            assert!(w.len() >= 1, "window should never be empty after push");
        }
    }

    #[test]
    fn window_mean_bounded_by_observations() {
        let mut w = AdwinWindow::new(0.01);
        let values = [1.0, 5.0, 2.0, 8.0, 3.0, 7.0, 4.0, 6.0];
        for &v in &values {
            w.push(v);
            let mean = w.mean();
            // After any drift-induced shrink, the remaining window's mean
            // should still be within the global observation range
            assert!(
                mean >= 1.0 - f64::EPSILON && mean <= 8.0 + f64::EPSILON,
                "mean {} out of bounds",
                mean
            );
        }
    }

    // ── Sensitivity vs delta ────────────────────────────────────────────

    #[test]
    fn lower_delta_fewer_drifts() {
        let run_with_delta = |delta: f64| -> usize {
            let mut w = AdwinWindow::new(delta);
            let mut drifts = 0;
            // Gradual shift
            for i in 0..200 {
                let val = 5.0 + (i as f64) * 0.05;
                if w.push(val).is_some() {
                    drifts += 1;
                }
            }
            drifts
        };

        let conservative = run_with_delta(0.001);
        let sensitive = run_with_delta(0.1);
        assert!(
            conservative <= sensitive,
            "lower delta ({conservative}) should produce <= drifts than higher delta ({sensitive})"
        );
    }

    // ── Max window size enforcement ─────────────────────────────────────

    #[test]
    fn max_window_enforced() {
        let config = DriftConfig {
            max_window_size: 50,
            ..Default::default()
        };
        let mut mon = RuleMonitor::new("bounded".to_string(), 0.01);

        for _ in 0..200 {
            mon.observe(5.0, &config);
        }

        assert!(
            mon.window_size() <= 50,
            "window {} should be bounded by max {}",
            mon.window_size(),
            50
        );
    }
}
