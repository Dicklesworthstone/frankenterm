//! Bayesian evidence ledger for pane state classification.
//!
//! Maintains full posterior distributions over pane states (Active, Thinking,
//! Idle, etc.) and evidence ledgers that explain exactly WHY each classification
//! decision was made.
//!
//! # Algorithm
//!
//! For each pane, maintains log-posterior:
//!
//! ```text
//! log P(S=s|evidence) = log P(S=s) + Σᵢ log P(evidenceᵢ|S=s)
//! ```
//!
//! Evidence sources contribute log-likelihood ratios that shift probability mass
//! between states. The evidence ledger records each contribution for transparency.
//!
//! # Bayes Factor Interpretation
//!
//! | BF        | Strength               |
//! |-----------|------------------------|
//! | < 3       | Barely worth mentioning |
//! | 3 – 10    | Substantial evidence   |
//! | 10 – 30   | Strong evidence        |
//! | 30 – 100  | Very strong evidence   |
//! | > 100     | Decisive               |

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the Bayesian classifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LedgerConfig {
    /// Minimum observations before classification starts.
    pub min_observations: usize,
    /// Bayes factor threshold for confident classification.
    pub bayes_factor_threshold: f64,
    /// Dirichlet concentration parameter for prior learning.
    pub dirichlet_alpha: f64,
    /// Maximum evidence entries to retain per pane ledger.
    pub max_ledger_entries: usize,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        Self {
            min_observations: 10,
            bayes_factor_threshold: 3.0,
            dirichlet_alpha: 1.0,
            max_ledger_entries: 100,
        }
    }
}

// =============================================================================
// Pane state
// =============================================================================

/// Possible states for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneState {
    Active,
    Thinking,
    Idle,
    RateLimited,
    Error,
    Stuck,
    Background,
}

impl PaneState {
    /// All possible states.
    pub const ALL: [PaneState; 7] = [
        PaneState::Active,
        PaneState::Thinking,
        PaneState::Idle,
        PaneState::RateLimited,
        PaneState::Error,
        PaneState::Stuck,
        PaneState::Background,
    ];

    /// Number of states.
    pub const COUNT: usize = 7;

    /// Index for array access.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            PaneState::Active => 0,
            PaneState::Thinking => 1,
            PaneState::Idle => 2,
            PaneState::RateLimited => 3,
            PaneState::Error => 4,
            PaneState::Stuck => 5,
            PaneState::Background => 6,
        }
    }

    /// State from index.
    #[must_use]
    pub fn from_index(i: usize) -> Option<PaneState> {
        PaneState::ALL.get(i).copied()
    }

    /// Display name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            PaneState::Active => "active",
            PaneState::Thinking => "thinking",
            PaneState::Idle => "idle",
            PaneState::RateLimited => "rate_limited",
            PaneState::Error => "error",
            PaneState::Stuck => "stuck",
            PaneState::Background => "background",
        }
    }
}

impl std::fmt::Display for PaneState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// =============================================================================
// Evidence sources
// =============================================================================

/// Types of evidence that inform pane classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", content = "value")]
pub enum Evidence {
    /// Output rate in lines per second.
    OutputRate(f64),
    /// Shannon entropy of recent output (0–8 bits).
    Entropy(f64),
    /// Pattern detection event (rule_id).
    PatternDetection(String),
    /// Seconds since last output.
    TimeSinceOutput(f64),
    /// Scrollback growth rate (bytes/sec).
    ScrollbackGrowth(f64),
}

impl Evidence {
    /// Compute log-likelihood ratios for each state given this evidence.
    ///
    /// Returns an array of log P(evidence|state) for each state. These are
    /// unnormalized — only relative values matter.
    fn log_likelihoods(&self) -> [f64; PaneState::COUNT] {
        match self {
            Evidence::OutputRate(rate) => output_rate_log_likelihoods(*rate),
            Evidence::Entropy(entropy) => entropy_log_likelihoods(*entropy),
            Evidence::PatternDetection(rule_id) => pattern_log_likelihoods(rule_id),
            Evidence::TimeSinceOutput(secs) => time_since_output_log_likelihoods(*secs),
            Evidence::ScrollbackGrowth(rate) => scrollback_growth_log_likelihoods(*rate),
        }
    }

    /// Source name for ledger entries.
    fn source_name(&self) -> &'static str {
        match self {
            Evidence::OutputRate(_) => "output_rate",
            Evidence::Entropy(_) => "entropy",
            Evidence::PatternDetection(_) => "pattern_detection",
            Evidence::TimeSinceOutput(_) => "time_since_output",
            Evidence::ScrollbackGrowth(_) => "scrollback_growth",
        }
    }
}

// =============================================================================
// Evidence ledger
// =============================================================================

/// A single entry in the evidence ledger recording one evidence contribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Evidence source name.
    pub source: String,
    /// Evidence value description.
    pub value_description: String,
    /// Log-likelihood ratio for the winning state vs the runner-up.
    pub log_lr_top_vs_second: f64,
    /// Which state this evidence most supports.
    pub favors: PaneState,
    /// Qualitative strength of evidence.
    pub strength: EvidenceStrength,
}

/// Qualitative strength of a single evidence contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    Weak,
    Moderate,
    Strong,
    VeryStrong,
}

impl EvidenceStrength {
    /// Classify based on absolute log-likelihood ratio.
    #[must_use]
    fn from_log_lr(log_lr: f64) -> Self {
        let abs_lr = log_lr.abs();
        if abs_lr < 1.0 {
            EvidenceStrength::Weak
        } else if abs_lr < 2.0 {
            EvidenceStrength::Moderate
        } else if abs_lr < 3.5 {
            EvidenceStrength::Strong
        } else {
            EvidenceStrength::VeryStrong
        }
    }
}

// =============================================================================
// Classification result
// =============================================================================

/// Result of classifying a pane with full evidence ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationResult {
    /// Most likely state.
    pub classification: PaneState,
    /// Full posterior distribution (probabilities summing to 1).
    pub posterior: HashMap<String, f64>,
    /// Evidence ledger showing each contribution.
    pub ledger: Vec<LedgerEntry>,
    /// Bayes factor comparing top state vs second best.
    pub bayes_factor: f64,
    /// Whether the classification is confident (BF > threshold).
    pub confident: bool,
    /// Number of evidence updates processed.
    pub observation_count: u64,
}

// =============================================================================
// Per-pane classifier state
// =============================================================================

/// Per-pane Bayesian classification state.
struct PaneClassifier {
    /// Log-posterior over states (unnormalized).
    log_posterior: [f64; PaneState::COUNT],
    /// Total evidence updates received.
    observation_count: u64,
    /// Recent ledger entries.
    ledger_entries: Vec<LedgerEntry>,
}

impl PaneClassifier {
    fn new(log_prior: &[f64; PaneState::COUNT]) -> Self {
        Self {
            log_posterior: *log_prior,
            observation_count: 0,
            ledger_entries: Vec::new(),
        }
    }

    /// Update posterior with new evidence.
    fn update(&mut self, evidence: &Evidence, max_entries: usize) {
        let log_liks = evidence.log_likelihoods();

        // Add log-likelihoods to log-posterior
        for (lp, ll) in self.log_posterior.iter_mut().zip(&log_liks) {
            *lp += ll;
        }

        self.observation_count += 1;

        // Record ledger entry
        let (top_idx, second_idx) = self.top_two_indices();
        let log_lr = log_liks[top_idx] - log_liks[second_idx];
        let favors_idx = if log_liks
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0)
            == top_idx
        {
            top_idx
        } else {
            log_liks
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0)
        };

        let entry = LedgerEntry {
            source: evidence.source_name().to_string(),
            value_description: format!("{evidence:?}"),
            log_lr_top_vs_second: log_lr,
            favors: PaneState::from_index(favors_idx).unwrap_or(PaneState::Active),
            strength: EvidenceStrength::from_log_lr(log_lr),
        };

        self.ledger_entries.push(entry);
        if self.ledger_entries.len() > max_entries {
            self.ledger_entries.remove(0);
        }
    }

    /// Get the normalized posterior distribution.
    fn posterior(&self) -> [f64; PaneState::COUNT] {
        log_softmax(&self.log_posterior)
    }

    /// Get classification result.
    fn classify(&self, config: &LedgerConfig) -> ClassificationResult {
        let posterior = self.posterior();
        let (top_idx, second_idx) = self.top_two_indices();

        let bayes_factor = (self.log_posterior[top_idx] - self.log_posterior[second_idx]).exp();

        let mut posterior_map = HashMap::new();
        for state in PaneState::ALL {
            posterior_map.insert(state.name().to_string(), posterior[state.index()]);
        }

        ClassificationResult {
            classification: PaneState::from_index(top_idx).unwrap_or(PaneState::Active),
            posterior: posterior_map,
            ledger: self.ledger_entries.clone(),
            bayes_factor,
            confident: bayes_factor >= config.bayes_factor_threshold
                && self.observation_count as usize >= config.min_observations,
            observation_count: self.observation_count,
        }
    }

    /// Indices of the top two states by log-posterior.
    fn top_two_indices(&self) -> (usize, usize) {
        let mut indices: Vec<usize> = (0..PaneState::COUNT).collect();
        indices.sort_by(|&a, &b| {
            self.log_posterior[b]
                .partial_cmp(&self.log_posterior[a])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        (indices[0], indices[1])
    }
}

// =============================================================================
// Bayesian classifier (multi-pane)
// =============================================================================

/// Multi-pane Bayesian classifier with evidence ledgers.
pub struct BayesianClassifier {
    config: LedgerConfig,
    /// Log-prior over states (learned from feedback).
    log_prior: [f64; PaneState::COUNT],
    /// Per-pane classifier state.
    panes: HashMap<u64, PaneClassifier>,
    /// Dirichlet counts for prior learning.
    dirichlet_counts: [f64; PaneState::COUNT],
    /// Total feedback observations received.
    feedback_count: u64,
}

impl BayesianClassifier {
    /// Create a new classifier with uniform prior.
    #[must_use]
    pub fn new(config: LedgerConfig) -> Self {
        // Uniform log-prior
        let uniform = -(PaneState::COUNT as f64).ln();
        let log_prior = [uniform; PaneState::COUNT];
        let alpha = config.dirichlet_alpha;
        let dirichlet_counts = [alpha; PaneState::COUNT];

        Self {
            config,
            log_prior,
            panes: HashMap::new(),
            dirichlet_counts,
            feedback_count: 0,
        }
    }

    /// Update a pane with new evidence.
    pub fn update(&mut self, pane_id: u64, evidence: Evidence) {
        let max_entries = self.config.max_ledger_entries;
        let pane = self
            .panes
            .entry(pane_id)
            .or_insert_with(|| PaneClassifier::new(&self.log_prior));
        pane.update(&evidence, max_entries);
    }

    /// Classify a pane. Returns None if the pane hasn't been observed.
    #[must_use]
    pub fn classify(&self, pane_id: u64) -> Option<ClassificationResult> {
        self.panes.get(&pane_id).map(|p| p.classify(&self.config))
    }

    /// Record user feedback (manual override) to update the prior.
    pub fn record_feedback(&mut self, _pane_id: u64, true_state: PaneState) {
        self.dirichlet_counts[true_state.index()] += 1.0;
        self.feedback_count += 1;

        // Recompute log-prior from Dirichlet counts
        let total: f64 = self.dirichlet_counts.iter().sum();
        for (i, &count) in self.dirichlet_counts.iter().enumerate() {
            self.log_prior[i] = (count / total).ln();
        }
    }

    /// Reset a pane's classifier (e.g., after manual override).
    pub fn reset_pane(&mut self, pane_id: u64) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            *pane = PaneClassifier::new(&self.log_prior);
        }
    }

    /// Remove a pane from tracking.
    pub fn remove_pane(&mut self, pane_id: u64) {
        self.panes.remove(&pane_id);
    }

    /// Number of tracked panes.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Total feedback observations received.
    #[must_use]
    pub fn feedback_count(&self) -> u64 {
        self.feedback_count
    }

    /// Current log-prior (for inspection/debugging).
    #[must_use]
    pub fn log_prior(&self) -> &[f64; PaneState::COUNT] {
        &self.log_prior
    }

    /// Get a serializable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ClassifierSnapshot {
        let prior_probs = log_softmax(&self.log_prior);
        let mut prior_map = HashMap::new();
        for state in PaneState::ALL {
            prior_map.insert(state.name().to_string(), prior_probs[state.index()]);
        }

        let pane_summaries: Vec<PaneClassificationSummary> = self
            .panes
            .iter()
            .map(|(&pane_id, p)| {
                let result = p.classify(&self.config);
                PaneClassificationSummary {
                    pane_id,
                    classification: result.classification,
                    bayes_factor: result.bayes_factor,
                    confident: result.confident,
                    observation_count: result.observation_count,
                }
            })
            .collect();

        ClassifierSnapshot {
            pane_count: self.panes.len() as u64,
            feedback_count: self.feedback_count,
            prior: prior_map,
            panes: pane_summaries,
            config: self.config.clone(),
        }
    }
}

impl std::fmt::Debug for BayesianClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BayesianClassifier")
            .field("pane_count", &self.panes.len())
            .field("feedback_count", &self.feedback_count)
            .finish()
    }
}

// =============================================================================
// Snapshot types
// =============================================================================

/// Serializable classifier snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifierSnapshot {
    pub pane_count: u64,
    pub feedback_count: u64,
    pub prior: HashMap<String, f64>,
    pub panes: Vec<PaneClassificationSummary>,
    pub config: LedgerConfig,
}

/// Per-pane classification summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneClassificationSummary {
    pub pane_id: u64,
    pub classification: PaneState,
    pub bayes_factor: f64,
    pub confident: bool,
    pub observation_count: u64,
}

// =============================================================================
// Likelihood models
// =============================================================================

/// Log-likelihoods for each state given output rate.
fn output_rate_log_likelihoods(rate: f64) -> [f64; PaneState::COUNT] {
    // Gaussian log-likelihood around expected rate per state
    // Active: high rate (~10-50 lps), Thinking: low (~0.5-2), Idle: ~0,
    // RateLimited: ~0, Error: burst (~5-20), Stuck: very high (~50+), Background: ~0
    let params: [(f64, f64); PaneState::COUNT] = [
        (15.0, 10.0), // Active: mean=15, std=10
        (1.0, 1.5),   // Thinking: mean=1, std=1.5
        (0.1, 0.5),   // Idle: mean=0.1, std=0.5
        (0.0, 0.3),   // RateLimited: mean=0, std=0.3
        (10.0, 8.0),  // Error: mean=10, std=8
        (30.0, 15.0), // Stuck: mean=30 (repetitive output), std=15
        (0.0, 0.2),   // Background: mean=0, std=0.2
    ];
    gaussian_log_likelihoods(rate, &params)
}

/// Log-likelihoods for each state given entropy.
fn entropy_log_likelihoods(entropy: f64) -> [f64; PaneState::COUNT] {
    // Active: high entropy (~4-6), Stuck: low (~1-2), Error: moderate (~3-4)
    let params: [(f64, f64); PaneState::COUNT] = [
        (5.0, 1.0), // Active: diverse output
        (4.0, 1.0), // Thinking: moderate diversity
        (3.0, 2.0), // Idle: low/variable
        (2.0, 1.5), // RateLimited: repetitive messages
        (4.0, 1.5), // Error: moderate (error messages)
        (1.5, 0.8), // Stuck: low entropy (repetitive)
        (2.0, 2.0), // Background: variable
    ];
    gaussian_log_likelihoods(entropy, &params)
}

/// Log-likelihoods from pattern detections.
fn pattern_log_likelihoods(rule_id: &str) -> [f64; PaneState::COUNT] {
    // Strong evidence from pattern matches
    let mut lls = [0.0f64; PaneState::COUNT];

    if rule_id.contains("rate_limit") || rule_id.contains("usage") {
        lls[PaneState::RateLimited.index()] = 4.0;
        lls[PaneState::Active.index()] = -2.0;
    } else if rule_id.contains("error") || rule_id.contains("fail") {
        lls[PaneState::Error.index()] = 4.0;
        lls[PaneState::Active.index()] = -1.0;
    } else if rule_id.contains("tool_use") || rule_id.contains("compaction") {
        lls[PaneState::Active.index()] = 3.0;
        lls[PaneState::Thinking.index()] = 1.0;
    } else if rule_id.contains("thinking") || rule_id.contains("processing") {
        lls[PaneState::Thinking.index()] = 3.0;
    } else if rule_id.contains("approval") || rule_id.contains("waiting") {
        lls[PaneState::Idle.index()] = 3.0;
    } else if rule_id.contains("banner") || rule_id.contains("start") {
        lls[PaneState::Active.index()] = 2.0;
    } else if rule_id.contains("session.end") || rule_id.contains("cost_summary") {
        lls[PaneState::Background.index()] = 3.0;
        lls[PaneState::Idle.index()] = 1.0;
    }

    lls
}

/// Log-likelihoods for time since last output.
fn time_since_output_log_likelihoods(secs: f64) -> [f64; PaneState::COUNT] {
    // Exponential decay model: P(time|state) ∝ λ * exp(-λ * time)
    // Higher λ = more frequent output expected
    let rates: [f64; PaneState::COUNT] = [
        2.0,   // Active: output every ~0.5s
        0.2,   // Thinking: output every ~5s
        0.02,  // Idle: output every ~50s
        0.01,  // RateLimited: very infrequent
        0.5,   // Error: moderate frequency
        0.5,   // Stuck: moderate (repetitive)
        0.005, // Background: very infrequent
    ];

    let mut lls = [0.0f64; PaneState::COUNT];
    for (i, &lambda) in rates.iter().enumerate() {
        // Log of exponential PDF: log(λ) - λ*t
        lls[i] = lambda.mul_add(-secs, lambda.ln());
    }
    lls
}

/// Log-likelihoods for scrollback growth rate.
fn scrollback_growth_log_likelihoods(rate: f64) -> [f64; PaneState::COUNT] {
    let params: [(f64, f64); PaneState::COUNT] = [
        (500.0, 300.0),  // Active: high growth
        (50.0, 50.0),    // Thinking: moderate
        (0.0, 10.0),     // Idle: minimal
        (0.0, 5.0),      // RateLimited: minimal
        (200.0, 150.0),  // Error: high (error output)
        (1000.0, 500.0), // Stuck: very high (loop output)
        (0.0, 5.0),      // Background: minimal
    ];
    gaussian_log_likelihoods(rate, &params)
}

/// Compute Gaussian log-likelihoods for each state.
fn gaussian_log_likelihoods(
    x: f64,
    params: &[(f64, f64); PaneState::COUNT],
) -> [f64; PaneState::COUNT] {
    let mut lls = [0.0f64; PaneState::COUNT];
    for (i, &(mean, std)) in params.iter().enumerate() {
        let std = std.max(0.01); // avoid division by zero
        let z = (x - mean) / std;
        // Log of Gaussian PDF (dropping constant term since it's shared)
        lls[i] = (-0.5 * z).mul_add(z, -std.ln());
    }
    lls
}

// =============================================================================
// Math helpers
// =============================================================================

/// Numerically stable log-softmax: convert log-unnormalized to probabilities.
fn log_softmax(log_values: &[f64; PaneState::COUNT]) -> [f64; PaneState::COUNT] {
    let max = log_values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    let mut probs = [0.0f64; PaneState::COUNT];
    let sum: f64 = log_values.iter().map(|&lp| (lp - max).exp()).sum();
    let log_sum = max + sum.ln();

    for (i, &lp) in log_values.iter().enumerate() {
        probs[i] = (lp - log_sum).exp();
    }
    probs
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Config ---------------------------------------------------------------

    #[test]
    fn config_defaults() {
        let config = LedgerConfig::default();
        assert_eq!(config.min_observations, 10);
        assert!((config.bayes_factor_threshold - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = LedgerConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: LedgerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.min_observations, 10);
        assert_eq!(back.max_ledger_entries, 100);
    }

    // -- PaneState ------------------------------------------------------------

    #[test]
    fn state_index_roundtrip() {
        for state in PaneState::ALL {
            let idx = state.index();
            assert_eq!(PaneState::from_index(idx), Some(state));
        }
    }

    #[test]
    fn state_display() {
        assert_eq!(PaneState::Active.to_string(), "active");
        assert_eq!(PaneState::RateLimited.to_string(), "rate_limited");
        assert_eq!(PaneState::Stuck.to_string(), "stuck");
    }

    #[test]
    fn state_serde_roundtrip() {
        let state = PaneState::Error;
        let json = serde_json::to_string(&state).unwrap();
        let back: PaneState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, PaneState::Error);
    }

    #[test]
    fn all_states_have_unique_indices() {
        let mut seen = std::collections::HashSet::new();
        for state in PaneState::ALL {
            assert!(seen.insert(state.index()), "duplicate index for {state:?}");
        }
    }

    // -- Evidence -------------------------------------------------------------

    #[test]
    fn evidence_serde_roundtrip() {
        let ev = Evidence::OutputRate(12.5);
        let json = serde_json::to_string(&ev).unwrap();
        let back: Evidence = serde_json::from_str(&json).unwrap();
        if let Evidence::OutputRate(r) = back {
            assert!((r - 12.5).abs() < f64::EPSILON);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pattern_evidence_serde() {
        let ev = Evidence::PatternDetection("core.codex:rate_limited".to_string());
        let json = serde_json::to_string(&ev).unwrap();
        let back: Evidence = serde_json::from_str(&json).unwrap();
        if let Evidence::PatternDetection(s) = back {
            assert_eq!(s, "core.codex:rate_limited");
        } else {
            panic!("wrong variant");
        }
    }

    // -- Log-softmax ----------------------------------------------------------

    #[test]
    fn log_softmax_sums_to_one() {
        let log_vals = [1.0, 2.0, 3.0, 0.5, -1.0, 0.0, -0.5];
        let probs = log_softmax(&log_vals);
        let sum: f64 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-10, "sum={sum}, expected ~1.0");
    }

    #[test]
    fn log_softmax_preserves_order() {
        let log_vals = [5.0, 3.0, 1.0, 0.0, -1.0, -3.0, -5.0];
        let probs = log_softmax(&log_vals);
        for i in 0..6 {
            assert!(
                probs[i] >= probs[i + 1],
                "order violated at {i}: {} < {}",
                probs[i],
                probs[i + 1]
            );
        }
    }

    #[test]
    fn log_softmax_equal_inputs_uniform() {
        let log_vals = [0.0; PaneState::COUNT];
        let probs = log_softmax(&log_vals);
        let expected = 1.0 / PaneState::COUNT as f64;
        for &p in &probs {
            assert!((p - expected).abs() < 1e-10, "p={p}, expected {expected}");
        }
    }

    // -- Likelihood models ----------------------------------------------------

    #[test]
    fn output_rate_high_favors_active() {
        let lls = output_rate_log_likelihoods(15.0);
        assert!(
            lls[PaneState::Active.index()] > lls[PaneState::Idle.index()],
            "rate=15 should favor Active over Idle"
        );
    }

    #[test]
    fn output_rate_zero_favors_idle() {
        let lls = output_rate_log_likelihoods(0.0);
        assert!(
            lls[PaneState::Idle.index()] > lls[PaneState::Active.index()],
            "rate=0 should favor Idle over Active"
        );
    }

    #[test]
    fn entropy_low_favors_stuck() {
        let lls = entropy_log_likelihoods(1.0);
        assert!(
            lls[PaneState::Stuck.index()] > lls[PaneState::Active.index()],
            "entropy=1 should favor Stuck over Active"
        );
    }

    #[test]
    fn pattern_rate_limit_favors_rate_limited() {
        let lls = pattern_log_likelihoods("core.codex:rate_limited");
        assert!(
            lls[PaneState::RateLimited.index()] > lls[PaneState::Active.index()],
            "rate_limit pattern should favor RateLimited"
        );
    }

    #[test]
    fn pattern_error_favors_error() {
        let lls = pattern_log_likelihoods("core.codex:error");
        assert!(
            lls[PaneState::Error.index()] > lls[PaneState::Active.index()],
            "error pattern should favor Error"
        );
    }

    #[test]
    fn time_since_output_long_favors_idle() {
        let lls = time_since_output_log_likelihoods(120.0);
        assert!(
            lls[PaneState::Idle.index()] > lls[PaneState::Active.index()],
            "120s silence should favor Idle over Active"
        );
    }

    // -- BayesianClassifier ---------------------------------------------------

    #[test]
    fn classifier_creation() {
        let clf = BayesianClassifier::new(LedgerConfig::default());
        assert_eq!(clf.pane_count(), 0);
        assert_eq!(clf.feedback_count(), 0);
    }

    #[test]
    fn classifier_auto_registers_pane() {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        clf.update(1, Evidence::OutputRate(10.0));
        assert_eq!(clf.pane_count(), 1);
    }

    #[test]
    fn classifier_active_pane() {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            min_observations: 1,
            ..Default::default()
        });

        // Feed strong Active evidence
        for _ in 0..5 {
            clf.update(1, Evidence::OutputRate(15.0));
            clf.update(1, Evidence::Entropy(5.0));
            clf.update(1, Evidence::PatternDetection("tool_use".to_string()));
        }

        let result = clf.classify(1).unwrap();
        assert_eq!(result.classification, PaneState::Active);
        assert!(result.bayes_factor > 1.0);
    }

    #[test]
    fn classifier_rate_limited_pane() {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            min_observations: 1,
            ..Default::default()
        });

        for _ in 0..5 {
            clf.update(1, Evidence::OutputRate(0.0));
            clf.update(1, Evidence::PatternDetection("rate_limited".to_string()));
            clf.update(1, Evidence::TimeSinceOutput(60.0));
        }

        let result = clf.classify(1).unwrap();
        assert_eq!(result.classification, PaneState::RateLimited);
    }

    #[test]
    fn classifier_stuck_pane() {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            min_observations: 1,
            ..Default::default()
        });

        for _ in 0..5 {
            clf.update(1, Evidence::OutputRate(40.0));
            clf.update(1, Evidence::Entropy(1.0));
            clf.update(1, Evidence::ScrollbackGrowth(1000.0));
        }

        let result = clf.classify(1).unwrap();
        assert_eq!(result.classification, PaneState::Stuck);
    }

    #[test]
    fn classifier_unknown_pane_returns_none() {
        let clf = BayesianClassifier::new(LedgerConfig::default());
        assert!(clf.classify(999).is_none());
    }

    #[test]
    fn classifier_posterior_sums_to_one() {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for _ in 0..10 {
            clf.update(1, Evidence::OutputRate(5.0));
        }

        let result = clf.classify(1).unwrap();
        let sum: f64 = result.posterior.values().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "posterior sum={sum}, expected ~1.0"
        );
    }

    #[test]
    fn classifier_feedback_updates_prior() {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());

        // Feed lots of "Error" feedback
        for _ in 0..20 {
            clf.record_feedback(1, PaneState::Error);
        }

        // Prior should now favor Error
        let probs = log_softmax(clf.log_prior());
        assert!(
            probs[PaneState::Error.index()] > probs[PaneState::Active.index()],
            "error prior={}, active prior={}",
            probs[PaneState::Error.index()],
            probs[PaneState::Active.index()]
        );
    }

    #[test]
    fn classifier_remove_pane() {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        clf.update(1, Evidence::OutputRate(10.0));
        assert_eq!(clf.pane_count(), 1);
        clf.remove_pane(1);
        assert_eq!(clf.pane_count(), 0);
    }

    #[test]
    fn classifier_reset_pane() {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for _ in 0..10 {
            clf.update(1, Evidence::OutputRate(50.0));
        }

        clf.reset_pane(1);
        let result = clf.classify(1).unwrap();
        assert_eq!(result.observation_count, 0);
    }

    // -- Evidence ledger entries -----------------------------------------------

    #[test]
    fn ledger_entries_recorded() {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        clf.update(1, Evidence::OutputRate(10.0));
        clf.update(1, Evidence::Entropy(4.0));

        let result = clf.classify(1).unwrap();
        assert_eq!(result.ledger.len(), 2);
        assert_eq!(result.ledger[0].source, "output_rate");
        assert_eq!(result.ledger[1].source, "entropy");
    }

    #[test]
    fn ledger_max_entries_enforced() {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            max_ledger_entries: 5,
            ..Default::default()
        });

        for _ in 0..10 {
            clf.update(1, Evidence::OutputRate(10.0));
        }

        let result = clf.classify(1).unwrap();
        assert!(result.ledger.len() <= 5);
    }

    // -- Evidence strength ----------------------------------------------------

    #[test]
    fn evidence_strength_classification() {
        assert_eq!(EvidenceStrength::from_log_lr(0.5), EvidenceStrength::Weak);
        assert_eq!(
            EvidenceStrength::from_log_lr(1.5),
            EvidenceStrength::Moderate
        );
        assert_eq!(EvidenceStrength::from_log_lr(2.5), EvidenceStrength::Strong);
        assert_eq!(
            EvidenceStrength::from_log_lr(4.0),
            EvidenceStrength::VeryStrong
        );
    }

    // -- Snapshot -------------------------------------------------------------

    #[test]
    fn snapshot_captures_state() {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        clf.update(1, Evidence::OutputRate(10.0));
        clf.update(2, Evidence::OutputRate(0.0));

        let snap = clf.snapshot();
        assert_eq!(snap.pane_count, 2);
        assert_eq!(snap.feedback_count, 0);
        assert_eq!(snap.panes.len(), 2);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = ClassifierSnapshot {
            pane_count: 2,
            feedback_count: 5,
            prior: HashMap::from([("active".to_string(), 0.3), ("idle".to_string(), 0.7)]),
            panes: vec![PaneClassificationSummary {
                pane_id: 1,
                classification: PaneState::Active,
                bayes_factor: 5.2,
                confident: true,
                observation_count: 20,
            }],
            config: LedgerConfig::default(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ClassifierSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_count, 2);
        assert_eq!(back.panes[0].classification, PaneState::Active);
    }

    // -- Classification result ------------------------------------------------

    #[test]
    fn classification_result_serde() {
        let mut posterior = HashMap::new();
        posterior.insert("active".to_string(), 0.8);
        posterior.insert("idle".to_string(), 0.2);

        let result = ClassificationResult {
            classification: PaneState::Active,
            posterior,
            ledger: vec![],
            bayes_factor: 4.0,
            confident: true,
            observation_count: 15,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ClassificationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.classification, PaneState::Active);
        assert!((back.bayes_factor - 4.0).abs() < f64::EPSILON);
    }

    // -- Monotone evidence property -------------------------------------------

    #[test]
    fn adding_evidence_increases_favored_state() {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            min_observations: 1,
            ..Default::default()
        });

        // Start with some neutral evidence
        clf.update(1, Evidence::OutputRate(5.0));
        let before = clf.classify(1).unwrap();
        let p_active_before = before.posterior.get("active").copied().unwrap_or(0.0);

        // Add strong Active evidence
        clf.update(1, Evidence::PatternDetection("tool_use".to_string()));
        let after = clf.classify(1).unwrap();
        let p_active_after = after.posterior.get("active").copied().unwrap_or(0.0);

        assert!(
            p_active_after >= p_active_before,
            "active evidence should increase P(Active): {p_active_before} → {p_active_after}"
        );
    }

    // -- Debug impl -----------------------------------------------------------

    #[test]
    fn debug_impl() {
        let clf = BayesianClassifier::new(LedgerConfig::default());
        let debug = format!("{clf:?}");
        assert!(debug.contains("BayesianClassifier"));
    }
}
