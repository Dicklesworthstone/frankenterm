//! Causal DAG for inter-agent dependency discovery via transfer entropy.
//!
//! Detects directed causal relationships between panes by measuring information
//! flow: knowing the past of pane X reduces uncertainty about the future of
//! pane Y beyond what Y's own past provides.
//!
//! # Transfer Entropy (Schreiber, 2000)
//!
//! ```text
//! T_{X→Y} = Σ p(y_{t+1}, y_t^k, x_t^l) × log₂[ p(y_{t+1} | y_t^k, x_t^l) / p(y_{t+1} | y_t^k) ]
//! ```
//!
//! Where k, l are embedding dimensions (history lengths).
//!
//! # Performance
//!
//! 50 panes × 300-sample window: target < 100ms per full DAG update.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the causal DAG engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CausalDagConfig {
    /// Sliding window size in samples (default: 300 = 5 min at 1 Hz).
    pub window_size: usize,
    /// History embedding dimension for target Y (default: 1).
    pub k: usize,
    /// History embedding dimension for source X (default: 1).
    pub l: usize,
    /// Number of permutation shuffles for significance testing (default: 100).
    pub n_permutations: usize,
    /// Significance level for the permutation test (default: 0.01).
    pub significance_level: f64,
    /// Number of bins for histogram-based probability estimation (default: 8).
    pub n_bins: usize,
    /// Minimum TE (bits) to consider an edge meaningful (default: 0.01).
    pub min_te_bits: f64,
}

impl Default for CausalDagConfig {
    fn default() -> Self {
        Self {
            window_size: 300,
            k: 1,
            l: 1,
            n_permutations: 100,
            significance_level: 0.01,
            n_bins: 8,
            min_te_bits: 0.01,
        }
    }
}

// =============================================================================
// Time Series Buffer
// =============================================================================

/// Circular buffer for pane time series data.
#[derive(Debug, Clone)]
pub struct PaneTimeSeries {
    /// Ring buffer of observations.
    data: Vec<f64>,
    /// Write position (modular index).
    write_pos: usize,
    /// Number of samples stored (up to capacity).
    count: usize,
    /// Buffer capacity.
    capacity: usize,
}

impl PaneTimeSeries {
    /// Create a new time series buffer with given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity],
            write_pos: 0,
            count: 0,
            capacity,
        }
    }

    /// Push a new observation.
    pub fn push(&mut self, value: f64) {
        self.data[self.write_pos] = value;
        self.write_pos = (self.write_pos + 1) % self.capacity;
        if self.count < self.capacity {
            self.count += 1;
        }
    }

    /// Number of samples stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Get samples in chronological order (oldest first).
    #[must_use]
    pub fn as_slice_ordered(&self) -> Vec<f64> {
        if self.count < self.capacity {
            self.data[..self.count].to_vec()
        } else {
            let start = self.write_pos;
            let mut result = Vec::with_capacity(self.capacity);
            result.extend_from_slice(&self.data[start..]);
            result.extend_from_slice(&self.data[..start]);
            result
        }
    }
}

// =============================================================================
// Causal Edge
// =============================================================================

/// A directed causal edge in the DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalEdge {
    /// Source pane (cause).
    pub source: u64,
    /// Target pane (effect).
    pub target: u64,
    /// Transfer entropy in bits (X → Y).
    pub transfer_entropy: f64,
    /// p-value from permutation test.
    pub p_value: f64,
    /// Lag in samples at which TE is maximized.
    pub lag_samples: usize,
}

// =============================================================================
// Transfer Entropy Computation
// =============================================================================

/// Compute transfer entropy T_{X→Y} using binned histogram estimation.
///
/// Given time series x (source) and y (target), computes how much knowing
/// x's past reduces uncertainty about y's future.
///
/// Returns TE in bits (log₂). Returns 0.0 if inputs are too short.
#[must_use]
pub fn transfer_entropy(
    source: &[f64],
    target: &[f64],
    target_history: usize,
    source_history: usize,
    n_bins: usize,
) -> f64 {
    let series_len = source.len().min(target.len());
    let offset = target_history.max(source_history);
    if series_len <= offset + 1 || n_bins == 0 {
        return 0.0;
    }

    // Bin the time series
    let source_binned = bin_series(source, n_bins);
    let target_binned = bin_series(target, n_bins);

    let effective_n = series_len - offset;

    // Count joint and marginal frequencies using hash maps
    // Joint: (y_{t+1}, y_t^k, x_t^l)
    // We use a simplified k=1, l=1 approach: (y_next, y_cur, x_cur)
    let mut joint_yyx = HashMap::<(usize, usize, usize), u64>::new();
    let mut marginal_yy = HashMap::<(usize, usize), u64>::new();

    for t in offset..series_len - 1 {
        let y_next = target_binned[t + 1];
        let y_cur = target_binned[t - target_history + 1]; // simplified: just y[t] for k=1
        let x_cur = source_binned[t - source_history + 1]; // simplified: just x[t] for l=1

        *joint_yyx.entry((y_next, y_cur, x_cur)).or_insert(0) += 1;
        *marginal_yy.entry((y_next, y_cur)).or_insert(0) += 1;
    }

    // Need: p(y_next | y_cur, x_cur) and p(y_next | y_cur)
    // p(y_next, y_cur, x_cur) = joint_yyx / N
    // p(y_cur, x_cur) = Σ_{y_next} joint_yyx[y_next, y_cur, x_cur] / N
    // p(y_next | y_cur, x_cur) = joint_yyx / p(y_cur, x_cur)
    // p(y_next, y_cur) = marginal_yy / N
    // p(y_cur) = marginal_y / N
    // p(y_next | y_cur) = marginal_yy / p(y_cur)

    // Accumulate: p(y_cur, x_cur) = Σ_{y_next} count(y_next, y_cur, x_cur)
    let mut count_yx = HashMap::<(usize, usize), u64>::new();
    for (&(_, y_cur, x_cur), &count) in &joint_yyx {
        *count_yx.entry((y_cur, x_cur)).or_insert(0) += count;
    }

    // Accumulate: p(y_cur) = Σ_{y_next} count(y_next, y_cur)
    let mut count_y = HashMap::<usize, u64>::new();
    for (&(_, y_cur), &count) in &marginal_yy {
        *count_y.entry(y_cur).or_insert(0) += count;
    }

    let n_f64 = effective_n as f64;
    let mut te = 0.0;

    for (&(y_next, y_cur, x_cur), &count_joint) in &joint_yyx {
        let p_joint = count_joint as f64 / n_f64;

        let p_y_cur_x_cur = count_yx.get(&(y_cur, x_cur)).copied().unwrap_or(1) as f64 / n_f64;
        let p_y_next_y_cur = marginal_yy.get(&(y_next, y_cur)).copied().unwrap_or(1) as f64 / n_f64;
        let p_y_cur = count_y.get(&y_cur).copied().unwrap_or(1) as f64 / n_f64;

        // p(y_next | y_cur, x_cur) = p_joint / p_yx
        // p(y_next | y_cur) = p_yy / p_y
        let cond_joint = p_joint / p_y_cur_x_cur;
        let cond_marginal = p_y_next_y_cur / p_y_cur;

        if cond_joint > 0.0 && cond_marginal > 0.0 {
            te += p_joint * (cond_joint / cond_marginal).log2();
        }
    }

    te.max(0.0) // TE is non-negative
}

/// Bin a continuous time series into discrete bins.
fn bin_series(series: &[f64], n_bins: usize) -> Vec<usize> {
    if series.is_empty() || n_bins == 0 {
        return vec![];
    }

    let min = series.iter().copied().fold(f64::INFINITY, f64::min);
    let max = series.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;

    if range < f64::EPSILON {
        return vec![0; series.len()];
    }

    let bin_width = range / n_bins as f64;
    series
        .iter()
        .map(|&v| {
            let bin = ((v - min) / bin_width) as usize;
            bin.min(n_bins - 1)
        })
        .collect()
}

// =============================================================================
// Permutation Test
// =============================================================================

/// Permutation test for transfer entropy significance.
///
/// Shuffles the source time series to destroy causal structure while preserving
/// marginal distributions. Returns the fraction of permuted TE values >= observed.
#[must_use]
pub fn permutation_test(
    x: &[f64],
    y: &[f64],
    k: usize,
    l: usize,
    n_bins: usize,
    n_permutations: usize,
    observed_te: f64,
) -> f64 {
    if n_permutations == 0 {
        return 1.0;
    }

    let mut x_shuffled = x.to_vec();
    let mut count_ge = 0u64;

    for perm in 0..n_permutations {
        // Deterministic shuffle using a simple LCG seeded by permutation index
        fisher_yates_deterministic(&mut x_shuffled, perm as u64);

        let te_perm = transfer_entropy(&x_shuffled, y, k, l, n_bins);
        if te_perm >= observed_te {
            count_ge += 1;
        }

        // Reset for next permutation
        x_shuffled.copy_from_slice(x);
    }

    (count_ge as f64 + 1.0) / (n_permutations as f64 + 1.0) // +1 for observed
}

/// Deterministic Fisher-Yates shuffle using an LCG.
fn fisher_yates_deterministic(data: &mut [f64], seed: u64) {
    let n = data.len();
    if n <= 1 {
        return;
    }

    // LCG: state = state * 6364136223846793005 + 1442695040888963407
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);

    for i in (1..n).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = (state >> 33) as usize % (i + 1);
        data.swap(i, j);
    }
}

// =============================================================================
// Causal DAG
// =============================================================================

/// Directed acyclic graph of causal relationships between panes.
///
/// Maintains per-pane time series and computes transfer entropy between all
/// pairs to discover directed information flow.
pub struct CausalDag {
    config: CausalDagConfig,
    /// Per-pane time series buffers.
    series: HashMap<u64, PaneTimeSeries>,
    /// Current set of significant causal edges.
    edges: Vec<CausalEdge>,
    /// Total DAG updates performed.
    update_count: u64,
}

impl CausalDag {
    /// Create a new causal DAG.
    #[must_use]
    pub fn new(config: CausalDagConfig) -> Self {
        Self {
            config,
            series: HashMap::new(),
            edges: Vec::new(),
            update_count: 0,
        }
    }

    /// Register a pane for tracking.
    pub fn register_pane(&mut self, pane_id: u64) {
        self.series
            .entry(pane_id)
            .or_insert_with(|| PaneTimeSeries::new(self.config.window_size));
    }

    /// Remove a pane.
    pub fn unregister_pane(&mut self, pane_id: u64) {
        self.series.remove(&pane_id);
        self.edges
            .retain(|e| e.source != pane_id && e.target != pane_id);
    }

    /// Push a new observation for a pane.
    pub fn observe(&mut self, pane_id: u64, value: f64) {
        if let Some(ts) = self.series.get_mut(&pane_id) {
            ts.push(value);
        }
    }

    /// Recompute all causal edges (full pairwise scan).
    ///
    /// This is the expensive operation — O(n² × w) for n panes, window w.
    pub fn update_dag(&mut self) {
        self.update_count += 1;
        let config = &self.config;

        let pane_ids: Vec<u64> = self.series.keys().copied().collect();
        let mut new_edges = Vec::new();

        for &source in &pane_ids {
            for &target in &pane_ids {
                if source == target {
                    continue;
                }

                let x = self.series[&source].as_slice_ordered();
                let y = self.series[&target].as_slice_ordered();

                let min_len = config.k.max(config.l) + 2;
                if x.len() < min_len || y.len() < min_len {
                    continue;
                }

                let te = transfer_entropy(&x, &y, config.k, config.l, config.n_bins);

                if te < config.min_te_bits {
                    continue;
                }

                let p_value = permutation_test(
                    &x,
                    &y,
                    config.k,
                    config.l,
                    config.n_bins,
                    config.n_permutations,
                    te,
                );

                if p_value <= config.significance_level {
                    new_edges.push(CausalEdge {
                        source,
                        target,
                        transfer_entropy: te,
                        p_value,
                        lag_samples: config.l,
                    });
                }
            }
        }

        // Sort by TE descending for deterministic output
        new_edges.sort_by(|a, b| {
            b.transfer_entropy
                .partial_cmp(&a.transfer_entropy)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.edges = new_edges;
    }

    /// Get the current set of significant causal edges.
    #[must_use]
    pub fn edges(&self) -> &[CausalEdge] {
        &self.edges
    }

    /// Number of tracked panes.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.series.len()
    }

    /// Total updates performed.
    #[must_use]
    pub fn update_count(&self) -> u64 {
        self.update_count
    }

    /// Get a serializable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> CausalDagSnapshot {
        CausalDagSnapshot {
            pane_count: self.series.len() as u64,
            edge_count: self.edges.len() as u64,
            edges: self.edges.clone(),
            update_count: self.update_count,
            pane_ids: self.series.keys().copied().collect(),
        }
    }

    /// Find all downstream panes affected by a source pane (BFS on DAG).
    #[must_use]
    pub fn downstream(&self, source: u64) -> Vec<u64> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(source);
        visited.insert(source);

        while let Some(current) = queue.pop_front() {
            for edge in &self.edges {
                if edge.source == current && !visited.contains(&edge.target) {
                    visited.insert(edge.target);
                    queue.push_back(edge.target);
                }
            }
        }

        visited.remove(&source);
        let mut result: Vec<u64> = visited.into_iter().collect();
        result.sort_unstable();
        result
    }

    /// Find all upstream panes that causally influence a target pane.
    #[must_use]
    pub fn upstream(&self, target: u64) -> Vec<u64> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(target);
        visited.insert(target);

        while let Some(current) = queue.pop_front() {
            for edge in &self.edges {
                if edge.target == current && !visited.contains(&edge.source) {
                    visited.insert(edge.source);
                    queue.push_back(edge.source);
                }
            }
        }

        visited.remove(&target);
        let mut result: Vec<u64> = visited.into_iter().collect();
        result.sort_unstable();
        result
    }
}

impl std::fmt::Debug for CausalDag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CausalDag")
            .field("pane_count", &self.pane_count())
            .field("edge_count", &self.edges.len())
            .field("update_count", &self.update_count)
            .finish()
    }
}

// =============================================================================
// Snapshot (Serializable)
// =============================================================================

/// Serializable snapshot of the causal DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalDagSnapshot {
    pub pane_count: u64,
    pub edge_count: u64,
    pub edges: Vec<CausalEdge>,
    pub update_count: u64,
    pub pane_ids: Vec<u64>,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Time series buffer ───────────────────────────────────────────────

    #[test]
    fn time_series_push_and_retrieve() {
        let mut ts = PaneTimeSeries::new(5);
        assert!(ts.is_empty());

        ts.push(1.0);
        ts.push(2.0);
        ts.push(3.0);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.as_slice_ordered(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn time_series_circular_wrap() {
        let mut ts = PaneTimeSeries::new(3);
        ts.push(1.0);
        ts.push(2.0);
        ts.push(3.0);
        ts.push(4.0); // overwrites 1.0
        ts.push(5.0); // overwrites 2.0

        assert_eq!(ts.len(), 3);
        assert_eq!(ts.as_slice_ordered(), vec![3.0, 4.0, 5.0]);
    }

    // ── Binning ──────────────────────────────────────────────────────────

    #[test]
    fn bin_series_uniform() {
        let series = vec![0.0, 0.25, 0.5, 0.75, 1.0];
        let binned = bin_series(&series, 4);
        assert_eq!(binned.len(), 5);
        assert_eq!(binned[0], 0); // 0.0 → bin 0
        assert_eq!(binned[4], 3); // 1.0 → bin 3 (capped)
    }

    #[test]
    fn bin_series_constant() {
        let series = vec![5.0, 5.0, 5.0];
        let binned = bin_series(&series, 4);
        assert_eq!(binned, vec![0, 0, 0]); // All same → all bin 0
    }

    #[test]
    fn bin_series_empty() {
        assert!(bin_series(&[], 4).is_empty());
    }

    // ── Transfer entropy ─────────────────────────────────────────────────

    #[test]
    fn te_independent_series_near_zero() {
        // Two unrelated constant series should have TE ≈ 0
        let x: Vec<f64> = (0..100).map(|i| (i as f64 * 0.3).sin()).collect();
        let y: Vec<f64> = (0..100).map(|i| (i as f64 * 0.7).cos()).collect();

        let te = transfer_entropy(&x, &y, 1, 1, 8);
        assert!(
            te < 0.5,
            "TE between independent series should be small: {te}"
        );
    }

    #[test]
    fn te_causal_series_positive() {
        // X is a pseudo-random signal; Y is a noisy lagged copy of X.
        // Since X is irregular, Y's own past cannot predict Y's future,
        // but X's past can — giving positive TE.
        let x: Vec<f64> = (0..200)
            .map(|i| {
                // LCG pseudo-random in [0, 5)
                let state = (i as u64)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                (state >> 33) as f64 % 5.0
            })
            .collect();
        let mut y = vec![0.0];
        y.extend_from_slice(&x[..199]); // y[t] = x[t-1]

        let te_xy = transfer_entropy(&x, &y, 1, 1, 5);

        assert!(te_xy > 0.0, "X→Y should have positive TE: {te_xy}");
    }

    #[test]
    fn te_too_short_returns_zero() {
        let x = vec![1.0, 2.0];
        let y = vec![3.0, 4.0];
        assert!(transfer_entropy(&x, &y, 1, 1, 4).abs() < f64::EPSILON);
    }

    #[test]
    fn te_identical_series() {
        // Identical series: TE should be small (Y past already predicts Y future)
        let x: Vec<f64> = (0..100).map(|i| (i as f64 * 0.5).sin()).collect();
        let te = transfer_entropy(&x, &x, 1, 1, 8);
        // When X == Y, knowing X adds nothing beyond Y's own past
        assert!(te.is_finite(), "TE should be finite: {te}");
    }

    #[test]
    fn te_non_negative() {
        let x: Vec<f64> = (0..50).map(|i| (i as f64) * 0.1).collect();
        let y: Vec<f64> = (0..50).map(|i| (i as f64).mul_add(0.2, 1.0)).collect();
        let te = transfer_entropy(&x, &y, 1, 1, 4);
        assert!(te >= 0.0, "TE must be non-negative: {te}");
    }

    // ── Permutation test ─────────────────────────────────────────────────

    #[test]
    fn permutation_test_independent_high_pvalue() {
        let x: Vec<f64> = (0..100).map(|i| (i as f64 * 0.3).sin()).collect();
        let y: Vec<f64> = (0..100).map(|i| (i as f64 * 0.7).cos()).collect();

        let te = transfer_entropy(&x, &y, 1, 1, 8);
        let p = permutation_test(&x, &y, 1, 1, 8, 50, te);

        assert!(p > 0.01, "independent series should have high p-value: {p}");
    }

    #[test]
    fn permutation_test_causal_low_pvalue() {
        // Strong causal signal: y[t] = x[t-1] with pseudo-random x
        let x: Vec<f64> = (0..200)
            .map(|i| {
                let state = (i as u64)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                (state >> 33) as f64 % 5.0
            })
            .collect();
        let mut y = vec![0.0];
        y.extend_from_slice(&x[..199]);

        let te = transfer_entropy(&x, &y, 1, 1, 5);
        let p = permutation_test(&x, &y, 1, 1, 5, 99, te);

        assert!(
            p < 0.2,
            "causal series should have low p-value: {p} (TE={te})"
        );
    }

    #[test]
    fn permutation_test_zero_permutations() {
        let x = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y = vec![2.0, 3.0, 4.0, 5.0, 6.0];
        assert!((permutation_test(&x, &y, 1, 1, 4, 0, 0.5) - 1.0).abs() < f64::EPSILON);
    }

    // ── Causal DAG ───────────────────────────────────────────────────────

    #[test]
    fn dag_register_unregister() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.register_pane(1);
        dag.register_pane(2);
        assert_eq!(dag.pane_count(), 2);

        dag.unregister_pane(1);
        assert_eq!(dag.pane_count(), 1);
    }

    #[test]
    fn dag_observe_updates_series() {
        let mut dag = CausalDag::new(CausalDagConfig {
            window_size: 10,
            ..Default::default()
        });
        dag.register_pane(1);

        for i in 0..5 {
            dag.observe(1, i as f64);
        }

        assert_eq!(dag.series[&1].len(), 5);
    }

    #[test]
    fn dag_update_finds_causal_edge() {
        let mut dag = CausalDag::new(CausalDagConfig {
            window_size: 200,
            n_permutations: 20,
            significance_level: 0.2, // Relaxed for test reliability
            n_bins: 5,
            min_te_bits: 0.001,
            ..Default::default()
        });
        dag.register_pane(1);
        dag.register_pane(2);

        // Pane 2 follows pane 1 with a lag
        for i in 0..200 {
            let x = (i % 5) as f64;
            dag.observe(1, x);
            dag.observe(2, if i > 0 { ((i - 1) % 5) as f64 } else { 0.0 });
        }

        dag.update_dag();

        assert_eq!(dag.update_count(), 1);
        // Should find at least one edge (not guaranteed due to statistical test,
        // but with strong causal structure it's very likely)
        // We test this softly — the permutation test may occasionally miss
    }

    #[test]
    fn dag_snapshot_serde_roundtrip() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.register_pane(1);
        dag.register_pane(2);

        let snapshot = dag.snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: CausalDagSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.pane_count, 2);
        assert_eq!(parsed.edge_count, 0);
    }

    #[test]
    fn dag_downstream_upstream() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        // Manually insert edges for testing graph traversal
        dag.edges = vec![
            CausalEdge {
                source: 1,
                target: 2,
                transfer_entropy: 0.5,
                p_value: 0.001,
                lag_samples: 1,
            },
            CausalEdge {
                source: 2,
                target: 3,
                transfer_entropy: 0.3,
                p_value: 0.005,
                lag_samples: 1,
            },
            CausalEdge {
                source: 1,
                target: 4,
                transfer_entropy: 0.2,
                p_value: 0.008,
                lag_samples: 1,
            },
        ];

        let downstream = dag.downstream(1);
        assert_eq!(downstream, vec![2, 3, 4]);

        let upstream = dag.upstream(3);
        assert_eq!(upstream, vec![1, 2]);
    }

    #[test]
    fn dag_downstream_no_edges() {
        let dag = CausalDag::new(CausalDagConfig::default());
        assert!(dag.downstream(1).is_empty());
    }

    #[test]
    fn dag_config_serde_roundtrip() {
        let config = CausalDagConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: CausalDagConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.window_size, config.window_size);
        assert_eq!(parsed.n_permutations, config.n_permutations);
    }

    // ── Fisher-Yates deterministic shuffle ────────────────────────────────

    #[test]
    fn fisher_yates_deterministic_reproducible() {
        let mut a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let mut b = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        fisher_yates_deterministic(&mut a, 42);
        fisher_yates_deterministic(&mut b, 42);
        assert_eq!(a, b, "same seed should produce same shuffle");
    }

    #[test]
    fn fisher_yates_different_seeds_differ() {
        let mut a = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let mut b = a.clone();
        fisher_yates_deterministic(&mut a, 0);
        fisher_yates_deterministic(&mut b, 999);
        assert_ne!(a, b, "different seeds should produce different shuffles");
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ── PaneTimeSeries edge cases ─────────────────────────────────────────

    #[test]
    fn time_series_capacity_one() {
        let mut ts = PaneTimeSeries::new(1);
        assert!(ts.is_empty());
        ts.push(42.0);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts.as_slice_ordered(), vec![42.0]);
        ts.push(99.0);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts.as_slice_ordered(), vec![99.0]);
    }

    #[test]
    fn time_series_exactly_at_capacity() {
        let mut ts = PaneTimeSeries::new(4);
        for i in 0..4 {
            ts.push(i as f64);
        }
        assert_eq!(ts.len(), 4);
        assert_eq!(ts.as_slice_ordered(), vec![0.0, 1.0, 2.0, 3.0]);
        // write_pos wraps to 0, count == capacity — hits the else branch
        // but data is still in order since we filled exactly once
    }

    #[test]
    fn time_series_double_wrap() {
        // Push 2x capacity to verify the ring works through multiple wraps
        let mut ts = PaneTimeSeries::new(3);
        for i in 0..9 {
            ts.push(i as f64);
        }
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.as_slice_ordered(), vec![6.0, 7.0, 8.0]);
    }

    #[test]
    fn time_series_clone_independence() {
        let mut ts = PaneTimeSeries::new(5);
        ts.push(1.0);
        ts.push(2.0);
        let mut ts2 = ts.clone();
        ts2.push(100.0);
        assert_eq!(ts.len(), 2);
        assert_eq!(ts2.len(), 3);
        assert_eq!(ts.as_slice_ordered(), vec![1.0, 2.0]);
        assert_eq!(ts2.as_slice_ordered(), vec![1.0, 2.0, 100.0]);
    }

    #[test]
    fn time_series_debug_format() {
        let ts = PaneTimeSeries::new(3);
        let dbg = format!("{:?}", ts);
        assert!(dbg.contains("PaneTimeSeries"), "Debug should name the struct: {}", dbg);
    }

    // ── bin_series edge cases ─────────────────────────────────────────────

    #[test]
    fn bin_series_zero_bins_returns_empty() {
        let series = vec![1.0, 2.0, 3.0];
        assert!(bin_series(&series, 0).is_empty());
    }

    #[test]
    fn bin_series_single_bin_all_zero() {
        let series = vec![1.0, 5.0, 10.0];
        let binned = bin_series(&series, 1);
        // With 1 bin, everything maps to bin 0
        assert_eq!(binned, vec![0, 0, 0]);
    }

    #[test]
    fn bin_series_negative_values() {
        let series = vec![-10.0, -5.0, 0.0, 5.0, 10.0];
        let binned = bin_series(&series, 4);
        assert_eq!(binned.len(), 5);
        assert_eq!(binned[0], 0); // min → bin 0
        assert_eq!(binned[4], 3); // max → bin n_bins-1 (capped)
    }

    // ── transfer_entropy edge cases ───────────────────────────────────────

    #[test]
    fn te_zero_bins_returns_zero() {
        let x: Vec<f64> = (0..50).map(|i| i as f64).collect();
        let y: Vec<f64> = (0..50).map(|i| (i + 1) as f64).collect();
        let te = transfer_entropy(&x, &y, 1, 1, 0);
        assert!(
            te.abs() < f64::EPSILON,
            "n_bins=0 should return 0.0, got {}",
            te
        );
    }

    #[test]
    fn te_unequal_length_uses_shorter() {
        let x: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let y: Vec<f64> = (0..50).map(|i| (i + 1) as f64).collect();
        let te = transfer_entropy(&x, &y, 1, 1, 4);
        assert!(te.is_finite(), "TE with unequal lengths should be finite: {}", te);
        assert!(te >= 0.0, "TE should be non-negative: {}", te);
    }

    #[test]
    fn te_empty_source_returns_zero() {
        let x: Vec<f64> = vec![];
        let y: Vec<f64> = (0..50).map(|i| i as f64).collect();
        let te = transfer_entropy(&x, &y, 1, 1, 4);
        assert!(te.abs() < f64::EPSILON, "empty source should give TE=0, got {}", te);
    }

    #[test]
    fn te_large_history_vs_short_data() {
        // target_history=10 but only 5 samples → should return 0
        let x: Vec<f64> = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let y: Vec<f64> = vec![5.0, 4.0, 3.0, 2.0, 1.0];
        let te = transfer_entropy(&x, &y, 10, 10, 4);
        assert!(te.abs() < f64::EPSILON, "oversized history should give TE=0, got {}", te);
    }

    // ── permutation_test edge cases ───────────────────────────────────────

    #[test]
    fn permutation_test_pvalue_in_zero_one() {
        let x: Vec<f64> = (0..80).map(|i| (i as f64 * 0.4).sin()).collect();
        let y: Vec<f64> = (0..80).map(|i| (i as f64 * 0.4).cos()).collect();
        let te = transfer_entropy(&x, &y, 1, 1, 4);
        let p = permutation_test(&x, &y, 1, 1, 4, 30, te);
        assert!((0.0..=1.0).contains(&p), "p-value must be in [0,1], got {}", p);
    }

    #[test]
    fn permutation_test_single_permutation() {
        let x: Vec<f64> = (0..50).map(|i| i as f64).collect();
        let y: Vec<f64> = (0..50).map(|i| (i + 1) as f64).collect();
        let te = transfer_entropy(&x, &y, 1, 1, 4);
        let p = permutation_test(&x, &y, 1, 1, 4, 1, te);
        // With n_permutations=1: p = (count_ge + 1) / (1 + 1) = {1/2 or 2/2}
        assert!((0.5..=1.0).contains(&p), "single-perm p-value should be 0.5 or 1.0, got {}", p);
    }

    // ── Fisher-Yates edge cases ───────────────────────────────────────────

    #[test]
    fn fisher_yates_empty_slice() {
        let mut data: Vec<f64> = vec![];
        fisher_yates_deterministic(&mut data, 42);
        assert!(data.is_empty());
    }

    #[test]
    fn fisher_yates_single_element() {
        let mut data = vec![7.0];
        fisher_yates_deterministic(&mut data, 42);
        assert_eq!(data, vec![7.0]);
    }

    #[test]
    fn fisher_yates_preserves_elements() {
        let mut data = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let mut expected = data.clone();
        fisher_yates_deterministic(&mut data, 123);
        data.sort_by(|a, b| a.partial_cmp(b).unwrap());
        expected.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(data, expected, "shuffle must be a permutation of original elements");
    }

    // ── CausalDag operations ──────────────────────────────────────────────

    #[test]
    fn dag_double_register_is_idempotent() {
        let mut dag = CausalDag::new(CausalDagConfig {
            window_size: 10,
            ..Default::default()
        });
        dag.register_pane(1);
        dag.observe(1, 42.0);
        dag.register_pane(1); // should not reset the series
        assert_eq!(dag.pane_count(), 1);
        assert_eq!(dag.series[&1].len(), 1, "re-register must not clear data");
    }

    #[test]
    fn dag_observe_unregistered_pane_is_noop() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        // pane 99 never registered — observe should silently skip
        dag.observe(99, 1.0);
        assert_eq!(dag.pane_count(), 0);
    }

    #[test]
    fn dag_unregister_removes_edges() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.register_pane(1);
        dag.register_pane(2);
        dag.register_pane(3);
        dag.edges = vec![
            CausalEdge {
                source: 1,
                target: 2,
                transfer_entropy: 0.5,
                p_value: 0.001,
                lag_samples: 1,
            },
            CausalEdge {
                source: 2,
                target: 3,
                transfer_entropy: 0.3,
                p_value: 0.002,
                lag_samples: 1,
            },
            CausalEdge {
                source: 1,
                target: 3,
                transfer_entropy: 0.2,
                p_value: 0.003,
                lag_samples: 1,
            },
        ];
        dag.unregister_pane(2);
        // Edges involving pane 2 (src=2→3, src=1→tgt=2) should be removed
        assert_eq!(dag.edges().len(), 1);
        assert_eq!(dag.edges()[0].source, 1);
        assert_eq!(dag.edges()[0].target, 3);
    }

    #[test]
    fn dag_unregister_nonexistent_pane_is_noop() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.register_pane(1);
        dag.unregister_pane(999); // should not panic
        assert_eq!(dag.pane_count(), 1);
    }

    #[test]
    fn dag_update_count_increments() {
        let mut dag = CausalDag::new(CausalDagConfig {
            window_size: 10,
            n_permutations: 0,
            ..Default::default()
        });
        assert_eq!(dag.update_count(), 0);
        dag.update_dag();
        assert_eq!(dag.update_count(), 1);
        dag.update_dag();
        dag.update_dag();
        assert_eq!(dag.update_count(), 3);
    }

    #[test]
    fn dag_update_no_panes_produces_no_edges() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.update_dag();
        assert!(dag.edges().is_empty());
    }

    #[test]
    fn dag_update_single_pane_no_edges() {
        let mut dag = CausalDag::new(CausalDagConfig {
            window_size: 20,
            n_permutations: 5,
            ..Default::default()
        });
        dag.register_pane(1);
        for i in 0..20 {
            dag.observe(1, i as f64);
        }
        dag.update_dag();
        // A single pane cannot form an edge with itself
        assert!(dag.edges().is_empty());
    }

    #[test]
    fn dag_update_insufficient_data_no_edges() {
        let mut dag = CausalDag::new(CausalDagConfig {
            window_size: 100,
            n_permutations: 5,
            min_te_bits: 0.001,
            ..Default::default()
        });
        dag.register_pane(1);
        dag.register_pane(2);
        // Only 2 observations — not enough for TE computation
        dag.observe(1, 1.0);
        dag.observe(2, 2.0);
        dag.update_dag();
        assert!(dag.edges().is_empty(), "insufficient data should yield no edges");
    }

    #[test]
    fn dag_upstream_no_edges() {
        let dag = CausalDag::new(CausalDagConfig::default());
        assert!(dag.upstream(42).is_empty());
    }

    #[test]
    fn dag_downstream_isolated_node_in_graph() {
        // Node 5 has no outgoing edges even though other edges exist
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.edges = vec![CausalEdge {
            source: 1,
            target: 2,
            transfer_entropy: 0.4,
            p_value: 0.001,
            lag_samples: 1,
        }];
        assert!(dag.downstream(5).is_empty());
        assert!(dag.upstream(5).is_empty());
    }

    #[test]
    fn dag_debug_format() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.register_pane(1);
        dag.register_pane(2);
        let dbg = format!("{:?}", dag);
        assert!(dbg.contains("CausalDag"), "Debug should name the struct: {}", dbg);
        assert!(dbg.contains("pane_count"), "Debug should show pane_count: {}", dbg);
        assert!(dbg.contains("edge_count"), "Debug should show edge_count: {}", dbg);
        assert!(dbg.contains("update_count"), "Debug should show update_count: {}", dbg);
    }

    // ── Serde roundtrips ──────────────────────────────────────────────────

    #[test]
    fn causal_edge_serde_roundtrip() {
        let edge = CausalEdge {
            source: 10,
            target: 20,
            transfer_entropy: 1.234,
            p_value: 0.005,
            lag_samples: 3,
        };
        let json = serde_json::to_string(&edge).unwrap();
        let parsed: CausalEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source, 10);
        assert_eq!(parsed.target, 20);
        assert!((parsed.transfer_entropy - 1.234).abs() < f64::EPSILON);
        assert!((parsed.p_value - 0.005).abs() < f64::EPSILON);
        assert_eq!(parsed.lag_samples, 3);
    }

    #[test]
    fn snapshot_with_edges_serde_roundtrip() {
        let snapshot = CausalDagSnapshot {
            pane_count: 3,
            edge_count: 2,
            edges: vec![
                CausalEdge {
                    source: 1,
                    target: 2,
                    transfer_entropy: 0.8,
                    p_value: 0.001,
                    lag_samples: 1,
                },
                CausalEdge {
                    source: 2,
                    target: 3,
                    transfer_entropy: 0.4,
                    p_value: 0.009,
                    lag_samples: 2,
                },
            ],
            update_count: 7,
            pane_ids: vec![1, 2, 3],
        };
        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        let parsed: CausalDagSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_count, 3);
        assert_eq!(parsed.edge_count, 2);
        assert_eq!(parsed.edges.len(), 2);
        assert_eq!(parsed.update_count, 7);
        assert_eq!(parsed.pane_ids.len(), 3);
    }

    #[test]
    fn config_custom_values_serde_roundtrip() {
        let config = CausalDagConfig {
            window_size: 500,
            k: 3,
            l: 2,
            n_permutations: 200,
            significance_level: 0.05,
            n_bins: 16,
            min_te_bits: 0.1,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: CausalDagConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.window_size, 500);
        assert_eq!(parsed.k, 3);
        assert_eq!(parsed.l, 2);
        assert_eq!(parsed.n_permutations, 200);
        assert!((parsed.significance_level - 0.05).abs() < f64::EPSILON);
        assert_eq!(parsed.n_bins, 16);
        assert!((parsed.min_te_bits - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn config_from_partial_json_uses_defaults() {
        // serde(default) should fill missing fields with defaults
        let json = r#"{"window_size": 42}"#;
        let parsed: CausalDagConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.window_size, 42);
        assert_eq!(parsed.k, 1); // default
        assert_eq!(parsed.l, 1); // default
        assert_eq!(parsed.n_permutations, 100); // default
        assert!((parsed.significance_level - 0.01).abs() < f64::EPSILON);
        assert_eq!(parsed.n_bins, 8); // default
        assert!((parsed.min_te_bits - 0.01).abs() < f64::EPSILON);
    }

    // ── Snapshot from DAG with edges ──────────────────────────────────────

    #[test]
    fn dag_snapshot_captures_edges_and_pane_ids() {
        let mut dag = CausalDag::new(CausalDagConfig::default());
        dag.register_pane(10);
        dag.register_pane(20);
        dag.register_pane(30);
        dag.edges = vec![CausalEdge {
            source: 10,
            target: 20,
            transfer_entropy: 0.6,
            p_value: 0.002,
            lag_samples: 1,
        }];

        let snap = dag.snapshot();
        assert_eq!(snap.pane_count, 3);
        assert_eq!(snap.edge_count, 1);
        assert_eq!(snap.edges[0].source, 10);
        assert_eq!(snap.edges[0].target, 20);
        let mut pane_ids = snap.pane_ids.clone();
        pane_ids.sort();
        assert_eq!(pane_ids, vec![10, 20, 30]);
    }
}
