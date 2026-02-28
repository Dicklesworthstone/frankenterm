//! Cross-Pane Correlation Engine
//!
//! Detects simultaneous or causally-related events across multiple panes using
//! chi-squared co-occurrence testing. This complements the pattern engine (which
//! detects events within a single pane) by finding statistical associations
//! between events across different panes.
//!
//! # Algorithm
//!
//! 1. Maintain a sliding time-window of recent events per (pane, event_type) pair
//! 2. Build a co-occurrence matrix: count how often event types co-occur across
//!    panes within a configurable time window
//! 3. Apply Pearson's chi-squared test for independence to detect significant
//!    co-occurrence patterns
//! 4. Report correlated event pairs with p-values
//!
//! # Use Cases
//!
//! - Rate limit hits across multiple agents → shared API key exhaustion
//! - Error cascades: one agent failure triggers others
//! - Build coordination: multiple agents hitting same compilation
//! - Pattern: "whenever pane A errors, pane B errors within 30s"

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Telemetry
// =============================================================================

/// Operational telemetry counters for the correlation engine.
///
/// All counters are plain `u64` because `CorrelationEngine` uses `&mut self`.
#[derive(Debug, Clone, Default)]
pub struct CorrelationEngineTelemetry {
    /// Total events ingested via `ingest()` or `ingest_batch()`.
    events_ingested: u64,
    /// Total `scan()` calls.
    scans: u64,
    /// Total correlations found across all scans.
    correlations_found: u64,
    /// Total `prune()` calls (including those triggered by scan).
    prunes: u64,
    /// Total events removed by pruning.
    events_pruned: u64,
}

impl CorrelationEngineTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> CorrelationEngineTelemetrySnapshot {
        CorrelationEngineTelemetrySnapshot {
            events_ingested: self.events_ingested,
            scans: self.scans,
            correlations_found: self.correlations_found,
            prunes: self.prunes,
            events_pruned: self.events_pruned,
        }
    }
}

/// Serializable snapshot of correlation engine telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelationEngineTelemetrySnapshot {
    /// Total events ingested.
    pub events_ingested: u64,
    /// Total scan() calls.
    pub scans: u64,
    /// Total correlations found.
    pub correlations_found: u64,
    /// Total prune() calls.
    pub prunes: u64,
    /// Total events removed by pruning.
    pub events_pruned: u64,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the cross-pane correlation engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrelationConfig {
    /// Time window for co-occurrence detection (milliseconds).
    pub window_ms: u64,
    /// Minimum number of co-occurrence observations before testing significance.
    pub min_observations: usize,
    /// P-value threshold for chi-squared test.
    pub p_value_threshold: f64,
    /// Maximum number of distinct event types to track per pane.
    pub max_event_types: usize,
    /// Maximum age of events to retain (milliseconds).
    pub retention_ms: u64,
    /// Maximum number of panes to track.
    pub max_panes: usize,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            window_ms: 30_000,
            min_observations: 5,
            p_value_threshold: 0.01,
            max_event_types: 50,
            retention_ms: 300_000,
            max_panes: 250,
        }
    }
}

// =============================================================================
// Event Record
// =============================================================================

/// A timestamped event observation from a pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// Pane that produced the event.
    pub pane_id: u64,
    /// Event type identifier (e.g., rule_id from pattern engine).
    pub event_type: String,
    /// Timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
}

// =============================================================================
// Co-occurrence Matrix
// =============================================================================

/// Tracks co-occurrence counts between pairs of event types across panes.
#[derive(Debug, Clone)]
pub struct CoOccurrenceMatrix {
    pair_counts: HashMap<(String, String), u64>,
    marginal_counts: HashMap<String, u64>,
    total_windows: u64,
}

impl CoOccurrenceMatrix {
    /// Create a new empty co-occurrence matrix.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pair_counts: HashMap::new(),
            marginal_counts: HashMap::new(),
            total_windows: 0,
        }
    }

    /// Record a set of event types that co-occurred in a single time window.
    pub fn record_window(&mut self, event_types: &[String]) {
        if event_types.is_empty() {
            self.total_windows += 1;
            return;
        }
        let mut unique: Vec<&String> = event_types.iter().collect();
        unique.sort();
        unique.dedup();

        for et in &unique {
            *self.marginal_counts.entry((*et).clone()).or_insert(0) += 1;
        }
        for i in 0..unique.len() {
            for j in (i + 1)..unique.len() {
                let key = ordered_pair(unique[i].clone(), unique[j].clone());
                *self.pair_counts.entry(key).or_insert(0) += 1;
            }
        }
        self.total_windows += 1;
    }

    /// Get the co-occurrence count for a pair of event types.
    #[must_use]
    pub fn pair_count(&self, a: &str, b: &str) -> u64 {
        let key = ordered_pair(a.to_string(), b.to_string());
        self.pair_counts.get(&key).copied().unwrap_or(0)
    }

    /// Get the marginal count for an event type.
    #[must_use]
    pub fn marginal(&self, event_type: &str) -> u64 {
        self.marginal_counts.get(event_type).copied().unwrap_or(0)
    }

    /// Total observed windows.
    #[must_use]
    pub fn total_windows(&self) -> u64 {
        self.total_windows
    }

    /// Number of distinct event types tracked.
    #[must_use]
    pub fn event_type_count(&self) -> usize {
        self.marginal_counts.len()
    }

    /// Number of distinct pairs with nonzero co-occurrence.
    #[must_use]
    pub fn pair_count_nonzero(&self) -> usize {
        self.pair_counts.values().filter(|&&c| c > 0).count()
    }

    /// Reset all counts.
    pub fn reset(&mut self) {
        self.pair_counts.clear();
        self.marginal_counts.clear();
        self.total_windows = 0;
    }

    /// Iterate over all pairs with their counts.
    pub fn pairs(&self) -> impl Iterator<Item = (&(String, String), &u64)> {
        self.pair_counts.iter()
    }

    /// Get all event type keys.
    fn marginal_counts_keys(&self) -> Vec<String> {
        self.marginal_counts.keys().cloned().collect()
    }
}

impl Default for CoOccurrenceMatrix {
    fn default() -> Self {
        Self::new()
    }
}

fn ordered_pair(a: String, b: String) -> (String, String) {
    if a <= b { (a, b) } else { (b, a) }
}

// =============================================================================
// Chi-Squared Test
// =============================================================================

/// Result of a chi-squared independence test for an event pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChiSquaredResult {
    /// First event type.
    pub event_a: String,
    /// Second event type.
    pub event_b: String,
    /// Chi-squared statistic.
    pub chi_squared: f64,
    /// Approximate p-value (1 degree of freedom).
    pub p_value: f64,
    /// Observed co-occurrence count.
    pub observed: u64,
    /// Expected co-occurrence count under independence.
    pub expected: f64,
    /// Whether the association is positive (more co-occurrence than expected).
    pub positive_association: bool,
}

/// Perform a chi-squared test of independence for a single pair.
#[must_use]
pub fn chi_squared_test(
    matrix: &CoOccurrenceMatrix,
    event_a: &str,
    event_b: &str,
) -> Option<ChiSquaredResult> {
    let total_windows = matrix.total_windows() as f64;
    if total_windows < 1.0 {
        return None;
    }

    let left_marginal = matrix.marginal(event_a) as f64;
    let right_marginal = matrix.marginal(event_b) as f64;
    let cooccurrence_count = matrix.pair_count(event_a, event_b) as f64;

    let expected = left_marginal * right_marginal / total_windows;
    if expected < 1.0 {
        return None;
    }

    let o11 = cooccurrence_count;
    let o12 = left_marginal - cooccurrence_count;
    let o21 = right_marginal - cooccurrence_count;
    let o22 = total_windows - left_marginal - right_marginal + cooccurrence_count;

    if o12 < 0.0 || o21 < 0.0 || o22 < 0.0 {
        return None;
    }

    let e11 = expected;
    let e12 = left_marginal * (total_windows - right_marginal) / total_windows;
    let e21 = (total_windows - left_marginal) * right_marginal / total_windows;
    let e22 = (total_windows - left_marginal) * (total_windows - right_marginal) / total_windows;

    if e11 <= 0.0 || e12 <= 0.0 || e21 <= 0.0 || e22 <= 0.0 {
        return None;
    }

    let chi_sq = (o11 - e11).powi(2) / e11
        + (o12 - e12).powi(2) / e12
        + (o21 - e21).powi(2) / e21
        + (o22 - e22).powi(2) / e22;

    let p_value = chi_squared_survival(chi_sq, 1.0);

    Some(ChiSquaredResult {
        event_a: event_a.to_string(),
        event_b: event_b.to_string(),
        chi_squared: chi_sq,
        p_value,
        observed: cooccurrence_count as u64,
        expected,
        positive_association: cooccurrence_count > expected,
    })
}

/// Survival function (1 - CDF) of the chi-squared distribution.
fn chi_squared_survival(x: f64, dof: f64) -> f64 {
    if x <= 0.0 {
        return 1.0;
    }
    if (dof - 1.0).abs() < 0.01 {
        erfc((x / 2.0).sqrt())
    } else {
        let z = ((x / dof).cbrt() - (1.0 - 2.0 / (9.0 * dof))) / (2.0 / (9.0 * dof)).sqrt();
        0.5 * erfc(z / std::f64::consts::SQRT_2)
    }
}

/// Complementary error function (Abramowitz & Stegun 7.1.26, max error 1.5e-7).
fn erfc(x: f64) -> f64 {
    if x < 0.0 {
        return 2.0 - erfc(-x);
    }
    let t = 1.0 / 0.327_591_1f64.mul_add(x, 1.0);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    poly * (-x * x).exp()
}

// =============================================================================
// Correlation Result
// =============================================================================

/// A detected correlation between event types across panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Correlation {
    /// First event type in the correlated pair.
    pub event_a: String,
    /// Second event type in the correlated pair.
    pub event_b: String,
    /// Chi-squared statistic.
    pub chi_squared: f64,
    /// P-value (lower = more significant).
    pub p_value: f64,
    /// Number of observed co-occurrences.
    pub co_occurrence_count: u64,
    /// Expected count under independence.
    pub expected_count: f64,
    /// Whether the correlation is positive (co-occur more than expected).
    pub positive: bool,
    /// Pane IDs that participated in this correlation.
    pub participating_panes: Vec<u64>,
}

// =============================================================================
// Correlation Engine
// =============================================================================

/// The cross-pane correlation engine.
pub struct CorrelationEngine {
    config: CorrelationConfig,
    events: Vec<EventRecord>,
    matrix: CoOccurrenceMatrix,
    pane_participation: HashMap<String, Vec<u64>>,
    last_scan_ms: u64,
    telemetry: CorrelationEngineTelemetry,
}

impl CorrelationEngine {
    /// Create a new correlation engine with the given configuration.
    #[must_use]
    pub fn new(config: CorrelationConfig) -> Self {
        Self {
            config,
            events: Vec::new(),
            matrix: CoOccurrenceMatrix::new(),
            pane_participation: HashMap::new(),
            last_scan_ms: 0,
            telemetry: CorrelationEngineTelemetry::new(),
        }
    }

    /// Ingest a new event.
    pub fn ingest(&mut self, record: EventRecord) {
        self.events.push(record);
        self.telemetry.events_ingested += 1;
    }

    /// Ingest a batch of events.
    pub fn ingest_batch(&mut self, records: impl IntoIterator<Item = EventRecord>) {
        let before = self.events.len();
        self.events.extend(records);
        self.telemetry.events_ingested += (self.events.len() - before) as u64;
    }

    /// Prune events older than the retention window relative to `now_ms`.
    pub fn prune(&mut self, now_ms: u64) {
        let before = self.events.len();
        let cutoff = now_ms.saturating_sub(self.config.retention_ms);
        self.events.retain(|e| e.timestamp_ms >= cutoff);
        self.telemetry.prunes += 1;
        self.telemetry.events_pruned += (before - self.events.len()) as u64;
    }

    /// Rebuild the co-occurrence matrix and scan for significant correlations.
    pub fn scan(&mut self, now_ms: u64) -> Vec<Correlation> {
        self.prune(now_ms);
        self.last_scan_ms = now_ms;
        self.matrix.reset();
        self.pane_participation.clear();
        self.telemetry.scans += 1;

        if self.events.is_empty() {
            return Vec::new();
        }

        self.events.sort_by_key(|e| e.timestamp_ms);

        for ev in &self.events {
            self.pane_participation
                .entry(ev.event_type.clone())
                .or_default()
                .push(ev.pane_id);
        }
        for panes in self.pane_participation.values_mut() {
            panes.sort();
            panes.dedup();
        }

        let window_ms = self.config.window_ms;
        let min_ts = self.events.first().map(|e| e.timestamp_ms).unwrap_or(0);
        let max_ts = self.events.last().map(|e| e.timestamp_ms).unwrap_or(0);

        let mut window_start = min_ts;
        while window_start <= max_ts {
            let window_end = window_start + window_ms;
            let mut types_in_window: Vec<String> = self
                .events
                .iter()
                .filter(|e| e.timestamp_ms >= window_start && e.timestamp_ms < window_end)
                .map(|e| e.event_type.clone())
                .collect();
            types_in_window.sort();
            types_in_window.dedup();
            self.matrix.record_window(&types_in_window);
            window_start = window_end;
        }

        let mut results = Vec::new();
        let event_types: Vec<String> = self.matrix.marginal_counts_keys();

        for i in 0..event_types.len() {
            for j in (i + 1)..event_types.len() {
                let count = self.matrix.pair_count(&event_types[i], &event_types[j]);
                if (count as usize) < self.config.min_observations {
                    continue;
                }
                if let Some(test) = chi_squared_test(&self.matrix, &event_types[i], &event_types[j])
                {
                    if test.p_value < self.config.p_value_threshold && test.positive_association {
                        let mut panes = Vec::new();
                        if let Some(p) = self.pane_participation.get(&event_types[i]) {
                            panes.extend(p);
                        }
                        if let Some(p) = self.pane_participation.get(&event_types[j]) {
                            panes.extend(p);
                        }
                        panes.sort();
                        panes.dedup();

                        results.push(Correlation {
                            event_a: test.event_a,
                            event_b: test.event_b,
                            chi_squared: test.chi_squared,
                            p_value: test.p_value,
                            co_occurrence_count: test.observed,
                            expected_count: test.expected,
                            positive: test.positive_association,
                            participating_panes: panes,
                        });
                    }
                }
            }
        }

        results.sort_by(|a, b| {
            a.p_value
                .partial_cmp(&b.p_value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.telemetry.correlations_found += results.len() as u64;

        results
    }

    /// Number of events in the sliding window.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Access the co-occurrence matrix (valid after last `scan()`).
    #[must_use]
    pub fn matrix(&self) -> &CoOccurrenceMatrix {
        &self.matrix
    }

    /// Access the configuration.
    #[must_use]
    pub fn config(&self) -> &CorrelationConfig {
        &self.config
    }

    /// Last scan timestamp.
    #[must_use]
    pub fn last_scan_ms(&self) -> u64 {
        self.last_scan_ms
    }

    /// Get the telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &CorrelationEngineTelemetry {
        &self.telemetry
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_matrix() {
        let m = CoOccurrenceMatrix::new();
        assert_eq!(m.total_windows(), 0);
        assert_eq!(m.event_type_count(), 0);
        assert_eq!(m.pair_count("a", "b"), 0);
        assert_eq!(m.marginal("a"), 0);
    }

    #[test]
    fn single_window_single_event() {
        let mut m = CoOccurrenceMatrix::new();
        m.record_window(&["rate_limit".to_string()]);
        assert_eq!(m.total_windows(), 1);
        assert_eq!(m.marginal("rate_limit"), 1);
        assert_eq!(m.pair_count_nonzero(), 0);
    }

    #[test]
    fn co_occurrence_pair_symmetric() {
        let mut m = CoOccurrenceMatrix::new();
        m.record_window(&["error".to_string(), "rate_limit".to_string()]);
        assert_eq!(m.pair_count("error", "rate_limit"), 1);
        assert_eq!(m.pair_count("rate_limit", "error"), 1);
    }

    #[test]
    fn multiple_windows_accumulate() {
        let mut m = CoOccurrenceMatrix::new();
        for _ in 0..10 {
            m.record_window(&["a".to_string(), "b".to_string()]);
        }
        for _ in 0..5 {
            m.record_window(&["a".to_string()]);
        }
        for _ in 0..3 {
            m.record_window(&["b".to_string()]);
        }
        assert_eq!(m.total_windows(), 18);
        assert_eq!(m.marginal("a"), 15);
        assert_eq!(m.marginal("b"), 13);
        assert_eq!(m.pair_count("a", "b"), 10);
    }

    #[test]
    fn deduplicates_within_window() {
        let mut m = CoOccurrenceMatrix::new();
        m.record_window(&["x".to_string(), "x".to_string(), "y".to_string()]);
        assert_eq!(m.marginal("x"), 1);
        assert_eq!(m.pair_count("x", "y"), 1);
    }

    #[test]
    fn empty_window_increments_total() {
        let mut m = CoOccurrenceMatrix::new();
        m.record_window(&[]);
        assert_eq!(m.total_windows(), 1);
        assert_eq!(m.event_type_count(), 0);
    }

    #[test]
    fn three_event_types_all_pairs() {
        let mut m = CoOccurrenceMatrix::new();
        m.record_window(&["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(m.pair_count("a", "b"), 1);
        assert_eq!(m.pair_count("a", "c"), 1);
        assert_eq!(m.pair_count("b", "c"), 1);
        assert_eq!(m.pair_count_nonzero(), 3);
    }

    #[test]
    fn reset_clears_all() {
        let mut m = CoOccurrenceMatrix::new();
        m.record_window(&["a".to_string(), "b".to_string()]);
        m.reset();
        assert_eq!(m.total_windows(), 0);
        assert_eq!(m.pair_count("a", "b"), 0);
    }

    #[test]
    fn chi_squared_perfect_correlation() {
        let mut m = CoOccurrenceMatrix::new();
        for _ in 0..100 {
            m.record_window(&["error".to_string(), "rate_limit".to_string()]);
        }
        for _ in 0..100 {
            m.record_window(&[]);
        }
        let result = chi_squared_test(&m, "error", "rate_limit").unwrap();
        assert!(
            result.p_value < 0.001,
            "p={} should be very small",
            result.p_value
        );
        assert!(result.positive_association);
        assert_eq!(result.observed, 100);
    }

    #[test]
    fn chi_squared_independent_events() {
        let mut m = CoOccurrenceMatrix::new();
        for i in 0..200 {
            if i % 3 == 0 {
                m.record_window(&["a".to_string(), "b".to_string()]);
            } else if i % 3 == 1 {
                m.record_window(&["a".to_string()]);
            } else {
                m.record_window(&["b".to_string()]);
            }
        }
        let result = chi_squared_test(&m, "a", "b").unwrap();
        assert!(!result.positive_association);
    }

    #[test]
    fn chi_squared_insufficient_data() {
        let m = CoOccurrenceMatrix::new();
        assert!(chi_squared_test(&m, "a", "b").is_none());
    }

    #[test]
    fn erfc_known_values() {
        assert!((erfc(0.0) - 1.0).abs() < 1e-6);
        assert!(erfc(5.0) < 1e-6);
        assert!((erfc(-5.0) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn chi_squared_survival_known() {
        assert!((chi_squared_survival(0.0, 1.0) - 1.0).abs() < 1e-6);
        let p = chi_squared_survival(3.841, 1.0);
        assert!((p - 0.05).abs() < 0.005, "p={p}, expected ~0.05");
        let p = chi_squared_survival(6.635, 1.0);
        assert!((p - 0.01).abs() < 0.005, "p={p}, expected ~0.01");
    }

    fn make_event(pane_id: u64, event_type: &str, ts_ms: u64) -> EventRecord {
        EventRecord {
            pane_id,
            event_type: event_type.to_string(),
            timestamp_ms: ts_ms,
        }
    }

    #[test]
    fn engine_empty_scan() {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        let results = engine.scan(1000);
        assert!(results.is_empty());
    }

    #[test]
    fn engine_detects_strong_correlation() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 10_000,
            min_observations: 3,
            p_value_threshold: 0.05,
            retention_ms: 600_000,
            ..Default::default()
        });

        for i in 0..20u64 {
            let base_ts = i * 15_000;
            engine.ingest(make_event(1, "rate_limit", base_ts));
            engine.ingest(make_event(2, "error", base_ts + 1000));
        }

        let now_ms = 20 * 15_000;
        let results = engine.scan(now_ms);

        let found = results.iter().any(|c| {
            (c.event_a == "error" && c.event_b == "rate_limit")
                || (c.event_a == "rate_limit" && c.event_b == "error")
        });
        assert!(
            found,
            "expected correlation between rate_limit and error; results={results:?}"
        );
    }

    #[test]
    fn engine_no_false_positive_independent() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 10_000,
            min_observations: 3,
            p_value_threshold: 0.01,
            retention_ms: 600_000,
            ..Default::default()
        });

        for i in 0..40u64 {
            let base_ts = i * 15_000;
            if i % 2 == 0 {
                engine.ingest(make_event(1, "event_a", base_ts));
            } else {
                engine.ingest(make_event(2, "event_b", base_ts));
            }
        }

        let now_ms = 40 * 15_000;
        let results = engine.scan(now_ms);
        let found = results.iter().any(|c| c.positive);
        assert!(
            !found,
            "should not detect false positive correlation; results={results:?}"
        );
    }

    #[test]
    fn engine_prune_old_events() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms: 10_000,
            ..Default::default()
        });
        engine.ingest(make_event(1, "old_event", 1000));
        engine.ingest(make_event(2, "recent_event", 50_000));
        engine.prune(55_000);
        assert_eq!(engine.event_count(), 1);
    }

    #[test]
    fn engine_respects_min_observations() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 10_000,
            min_observations: 10,
            p_value_threshold: 0.05,
            retention_ms: 600_000,
            ..Default::default()
        });

        for i in 0..3u64 {
            let ts = i * 15_000;
            engine.ingest(make_event(1, "a", ts));
            engine.ingest(make_event(2, "b", ts + 1000));
        }

        let results = engine.scan(3 * 15_000);
        assert!(results.is_empty(), "should require min_observations");
    }

    #[test]
    fn engine_multi_event_correlation() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 10_000,
            min_observations: 3,
            p_value_threshold: 0.05,
            retention_ms: 600_000,
            ..Default::default()
        });

        for i in 0..30u64 {
            let ts = i * 15_000;
            engine.ingest(make_event(1, "a", ts));
            engine.ingest(make_event(2, "b", ts + 500));
            if i % 5 == 0 {
                engine.ingest(make_event(3, "c", ts + 2000));
            }
        }

        let results = engine.scan(30 * 15_000);
        let ab_corr = results.iter().any(|c| {
            (c.event_a == "a" && c.event_b == "b") || (c.event_a == "b" && c.event_b == "a")
        });
        assert!(ab_corr, "should detect A-B correlation");
    }

    #[test]
    fn engine_participating_panes_tracked() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 10_000,
            min_observations: 3,
            p_value_threshold: 0.05,
            retention_ms: 600_000,
            ..Default::default()
        });

        for i in 0..20u64 {
            let ts = i * 15_000;
            engine.ingest(make_event(10, "x", ts));
            engine.ingest(make_event(20, "y", ts + 1000));
        }

        let results = engine.scan(20 * 15_000);
        if let Some(corr) = results.first() {
            assert!(corr.participating_panes.contains(&10));
            assert!(corr.participating_panes.contains(&20));
        }
    }

    #[test]
    fn correlation_config_serde() {
        let config = CorrelationConfig {
            window_ms: 60_000,
            min_observations: 10,
            p_value_threshold: 0.001,
            max_event_types: 100,
            retention_ms: 600_000,
            max_panes: 500,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: CorrelationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.window_ms, 60_000);
        assert!((deserialized.p_value_threshold - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn event_record_serde() {
        let record = EventRecord {
            pane_id: 42,
            event_type: "codex.usage_reached".to_string(),
            timestamp_ms: 1_000_000,
        };
        let json = serde_json::to_string(&record).unwrap();
        let deserialized: EventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.pane_id, 42);
        assert_eq!(deserialized.event_type, "codex.usage_reached");
    }

    #[test]
    fn correlation_serde() {
        let corr = Correlation {
            event_a: "error".to_string(),
            event_b: "rate_limit".to_string(),
            chi_squared: 15.5,
            p_value: 0.001,
            co_occurrence_count: 20,
            expected_count: 5.0,
            positive: true,
            participating_panes: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&corr).unwrap();
        let deserialized: Correlation = serde_json::from_str(&json).unwrap();
        assert!((deserialized.p_value - 0.001).abs() < f64::EPSILON);
        assert_eq!(deserialized.participating_panes.len(), 3);
    }

    // --- Default & Clone ---

    #[test]
    fn correlation_config_default_values() {
        let cfg = CorrelationConfig::default();
        assert_eq!(cfg.window_ms, 30_000);
        assert_eq!(cfg.min_observations, 5);
        assert!((cfg.p_value_threshold - 0.01).abs() < f64::EPSILON);
        assert_eq!(cfg.max_event_types, 50);
        assert_eq!(cfg.retention_ms, 300_000);
        assert_eq!(cfg.max_panes, 250);
    }

    #[test]
    fn correlation_config_clone() {
        let cfg = CorrelationConfig::default();
        let cfg2 = cfg.clone();
        assert_eq!(cfg2.window_ms, cfg.window_ms);
        assert_eq!(cfg2.max_panes, cfg.max_panes);
    }

    #[test]
    fn event_record_clone() {
        let r = EventRecord {
            pane_id: 1,
            event_type: "test".to_string(),
            timestamp_ms: 5000,
        };
        let r2 = r.clone();
        assert_eq!(r2.pane_id, 1);
        assert_eq!(r2.event_type, "test");
    }

    #[test]
    fn chi_squared_result_clone() {
        let r = ChiSquaredResult {
            event_a: "a".to_string(),
            event_b: "b".to_string(),
            chi_squared: 10.0,
            p_value: 0.005,
            observed: 20,
            expected: 10.0,
            positive_association: true,
        };
        let r2 = r.clone();
        assert_eq!(r2.event_a, "a");
        assert!((r2.chi_squared - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn correlation_clone() {
        let c = Correlation {
            event_a: "x".to_string(),
            event_b: "y".to_string(),
            chi_squared: 5.0,
            p_value: 0.05,
            co_occurrence_count: 10,
            expected_count: 3.0,
            positive: false,
            participating_panes: vec![1, 2],
        };
        let c2 = c.clone();
        assert_eq!(c2.participating_panes.len(), 2);
        assert!(!c2.positive);
    }

    // --- Debug ---

    #[test]
    fn correlation_config_debug() {
        let cfg = CorrelationConfig::default();
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("CorrelationConfig"));
    }

    #[test]
    fn event_record_debug() {
        let r = EventRecord {
            pane_id: 42,
            event_type: "test".to_string(),
            timestamp_ms: 1000,
        };
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("EventRecord"));
    }

    #[test]
    fn chi_squared_result_debug() {
        let r = ChiSquaredResult {
            event_a: "a".to_string(),
            event_b: "b".to_string(),
            chi_squared: 1.0,
            p_value: 0.5,
            observed: 5,
            expected: 5.0,
            positive_association: false,
        };
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("ChiSquaredResult"));
    }

    #[test]
    fn co_occurrence_matrix_debug() {
        let m = CoOccurrenceMatrix::new();
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("CoOccurrenceMatrix"));
    }

    // --- CoOccurrenceMatrix edge cases ---

    #[test]
    fn marginal_unknown_event_returns_zero() {
        let m = CoOccurrenceMatrix::new();
        assert_eq!(m.marginal("nonexistent"), 0);
    }

    #[test]
    fn pair_count_unknown_events_returns_zero() {
        let m = CoOccurrenceMatrix::new();
        assert_eq!(m.pair_count("a", "b"), 0);
    }

    #[test]
    fn pair_count_nonzero_empty() {
        let m = CoOccurrenceMatrix::new();
        assert_eq!(m.pair_count_nonzero(), 0);
    }

    #[test]
    fn event_type_count_empty() {
        let m = CoOccurrenceMatrix::new();
        assert_eq!(m.event_type_count(), 0);
    }

    #[test]
    fn pairs_iterator_empty_matrix() {
        let m = CoOccurrenceMatrix::new();
        assert_eq!(m.pairs().count(), 0);
    }

    #[test]
    fn single_event_no_pairs() {
        let mut m = CoOccurrenceMatrix::new();
        m.record_window(&["error".to_string()]);
        assert_eq!(m.pair_count_nonzero(), 0);
        assert_eq!(m.marginal("error"), 1);
        assert_eq!(m.event_type_count(), 1);
    }

    // --- erfc math edge cases ---

    #[test]
    fn erfc_at_zero() {
        let val = erfc(0.0);
        assert!(
            (val - 1.0).abs() < 0.01,
            "erfc(0) should be ~1.0, got {val}"
        );
    }

    #[test]
    fn erfc_large_positive() {
        let val = erfc(5.0);
        assert!(val < 1e-6, "erfc(5) should be very small, got {val}");
    }

    #[test]
    fn chi_squared_survival_at_zero() {
        let val = chi_squared_survival(0.0, 1.0);
        assert!(
            (val - 1.0).abs() < 0.01,
            "survival(0, 1) should be ~1.0, got {val}"
        );
    }

    #[test]
    fn chi_squared_survival_large_x() {
        let val = chi_squared_survival(50.0, 1.0);
        assert!(val < 1e-10, "survival(50,1) should be near zero, got {val}");
    }

    // --- CorrelationEngine extras ---

    #[test]
    fn engine_config_accessor() {
        let cfg = CorrelationConfig {
            window_ms: 5000,
            ..CorrelationConfig::default()
        };
        let engine = CorrelationEngine::new(cfg);
        assert_eq!(engine.config().window_ms, 5000);
    }

    #[test]
    fn engine_last_scan_ms_initial() {
        let engine = CorrelationEngine::new(CorrelationConfig::default());
        assert_eq!(engine.last_scan_ms(), 0);
    }

    #[test]
    fn engine_last_scan_ms_updates() {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        let _ = engine.scan(42000);
        assert_eq!(engine.last_scan_ms(), 42000);
    }

    #[test]
    fn engine_event_count_empty() {
        let engine = CorrelationEngine::new(CorrelationConfig::default());
        assert_eq!(engine.event_count(), 0);
    }

    #[test]
    fn engine_event_count_after_ingest() {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        engine.ingest(EventRecord {
            pane_id: 1,
            event_type: "test".to_string(),
            timestamp_ms: 1000,
        });
        assert_eq!(engine.event_count(), 1);
    }

    #[test]
    fn engine_ingest_batch_empty() {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        engine.ingest_batch(std::iter::empty());
        assert_eq!(engine.event_count(), 0);
    }

    #[test]
    fn engine_prune_future_clears_all() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms: 1000,
            ..CorrelationConfig::default()
        });
        engine.ingest(EventRecord {
            pane_id: 1,
            event_type: "a".to_string(),
            timestamp_ms: 100,
        });
        engine.prune(1_000_000);
        assert_eq!(engine.event_count(), 0);
    }

    #[test]
    fn engine_prune_keeps_recent() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms: 10_000,
            ..CorrelationConfig::default()
        });
        engine.ingest(EventRecord {
            pane_id: 1,
            event_type: "a".to_string(),
            timestamp_ms: 5000,
        });
        engine.prune(6000);
        assert_eq!(engine.event_count(), 1);
    }

    #[test]
    fn engine_matrix_empty_before_scan() {
        let engine = CorrelationEngine::new(CorrelationConfig::default());
        assert_eq!(engine.matrix().total_windows(), 0);
    }

    // --- Correlation serde edge cases ---

    #[test]
    fn correlation_empty_panes_serde() {
        let c = Correlation {
            event_a: "a".to_string(),
            event_b: "b".to_string(),
            chi_squared: 1.0,
            p_value: 0.5,
            co_occurrence_count: 1,
            expected_count: 1.0,
            positive: true,
            participating_panes: vec![],
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: Correlation = serde_json::from_str(&json).unwrap();
        assert!(parsed.participating_panes.is_empty());
    }

    #[test]
    fn event_record_large_pane_id() {
        let r = EventRecord {
            pane_id: u64::MAX,
            event_type: "test".to_string(),
            timestamp_ms: 0,
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: EventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, u64::MAX);
    }

    // ── Telemetry tests ──────────────────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let engine = CorrelationEngine::new(CorrelationConfig::default());
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.events_ingested, 0);
        assert_eq!(snap.scans, 0);
        assert_eq!(snap.correlations_found, 0);
        assert_eq!(snap.prunes, 0);
        assert_eq!(snap.events_pruned, 0);
    }

    #[test]
    fn telemetry_ingest_counted() {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        engine.ingest(EventRecord {
            pane_id: 1,
            event_type: "rate_limit".into(),
            timestamp_ms: 1000,
        });
        engine.ingest(EventRecord {
            pane_id: 2,
            event_type: "error".into(),
            timestamp_ms: 1001,
        });
        assert_eq!(engine.telemetry().snapshot().events_ingested, 2);
    }

    #[test]
    fn telemetry_ingest_batch_counted() {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        let records = vec![
            EventRecord { pane_id: 1, event_type: "a".into(), timestamp_ms: 100 },
            EventRecord { pane_id: 2, event_type: "b".into(), timestamp_ms: 200 },
            EventRecord { pane_id: 3, event_type: "c".into(), timestamp_ms: 300 },
        ];
        engine.ingest_batch(records);
        assert_eq!(engine.telemetry().snapshot().events_ingested, 3);
    }

    #[test]
    fn telemetry_prune_counted() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms: 1000,
            ..CorrelationConfig::default()
        });
        engine.ingest(EventRecord {
            pane_id: 1,
            event_type: "old".into(),
            timestamp_ms: 100,
        });
        engine.ingest(EventRecord {
            pane_id: 2,
            event_type: "new".into(),
            timestamp_ms: 5000,
        });
        engine.prune(5500);
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.prunes, 1);
        assert_eq!(snap.events_pruned, 1);
    }

    #[test]
    fn telemetry_scan_counted() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            min_observations: 1,
            ..CorrelationConfig::default()
        });
        engine.ingest(EventRecord {
            pane_id: 1,
            event_type: "a".into(),
            timestamp_ms: 1000,
        });
        engine.scan(2000);
        engine.scan(3000);
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.scans, 2);
        // scan() calls prune() internally
        assert_eq!(snap.prunes, 2);
    }

    #[test]
    fn telemetry_correlations_found_counted() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 1000,
            min_observations: 2,
            p_value_threshold: 1.0, // accept all so we get correlations
            retention_ms: 100_000,
            ..CorrelationConfig::default()
        });

        // Produce co-occurring events across multiple windows
        for w in 0..10 {
            let base = w * 1000;
            engine.ingest(EventRecord {
                pane_id: 1,
                event_type: "error".into(),
                timestamp_ms: base,
            });
            engine.ingest(EventRecord {
                pane_id: 2,
                event_type: "rate_limit".into(),
                timestamp_ms: base + 100,
            });
        }

        let results = engine.scan(15_000);
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.scans, 1);
        assert_eq!(snap.correlations_found, results.len() as u64);
    }

    #[test]
    fn telemetry_survives_multiple_operations() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms: 5000,
            ..CorrelationConfig::default()
        });

        // Ingest
        for i in 0..5 {
            engine.ingest(EventRecord {
                pane_id: i,
                event_type: format!("type_{i}"),
                timestamp_ms: 1000 + i * 100,
            });
        }

        // Scan (triggers prune internally)
        engine.scan(2000);

        // Batch ingest
        engine.ingest_batch(vec![
            EventRecord { pane_id: 10, event_type: "x".into(), timestamp_ms: 3000 },
            EventRecord { pane_id: 11, event_type: "y".into(), timestamp_ms: 3100 },
        ]);

        // Another scan
        engine.scan(4000);

        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.events_ingested, 7); // 5 + 2
        assert_eq!(snap.scans, 2);
        assert!(snap.prunes >= 2); // at least 2 from scans
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let snap = CorrelationEngineTelemetrySnapshot {
            events_ingested: 1000,
            scans: 50,
            correlations_found: 25,
            prunes: 100,
            events_pruned: 500,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: CorrelationEngineTelemetrySnapshot =
            serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn telemetry_prune_no_removal_still_counted() {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms: 100_000,
            ..CorrelationConfig::default()
        });
        engine.ingest(EventRecord {
            pane_id: 1,
            event_type: "recent".into(),
            timestamp_ms: 50_000,
        });
        engine.prune(50_100);
        let snap = engine.telemetry().snapshot();
        assert_eq!(snap.prunes, 1);
        assert_eq!(snap.events_pruned, 0);
    }
}
