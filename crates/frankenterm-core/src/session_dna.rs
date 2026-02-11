//! Agent session behavioral DNA — fingerprinting, clustering, predictions.
//!
//! Computes compact behavioral fingerprints ("Session DNA") for each agent
//! session, enabling similarity clustering, anomaly detection, and duration
//! predictions.
//!
//! # Architecture
//!
//! ```text
//! Capture pipeline ──► SessionDnaBuilder ──► SessionDna
//!   (activity data)       (incremental)       ├── raw features (17-dim)
//! Pattern detections ─┘                       ├── embedding (8-dim PCA)
//!                                             └── similarity queries
//! ```
//!
//! # Cold Start
//!
//! Before `min_sessions_for_pca` sessions exist, similarity uses z-score
//! normalized raw features with L2 distance.  Once enough data accumulates,
//! PCA is fitted and all sessions are projected to 8-dimensional embeddings.

use serde::{Deserialize, Serialize};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the session DNA system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDnaConfig {
    /// PCA embedding dimensionality (default: 8).
    pub embedding_dim: usize,
    /// Cosine similarity threshold for clustering (default: 0.85).
    pub similarity_threshold: f64,
    /// K neighbors for KNN predictions (default: 10).
    pub k_neighbors: usize,
    /// Minimum sessions before PCA is fitted (default: 50).
    pub min_sessions_for_pca: usize,
}

impl Default for SessionDnaConfig {
    fn default() -> Self {
        Self {
            embedding_dim: 8,
            similarity_threshold: 0.85,
            k_neighbors: 10,
            min_sessions_for_pca: 50,
        }
    }
}

// =============================================================================
// Feature Vector (DNA)
// =============================================================================

/// The raw feature dimension count (before PCA).
pub const RAW_FEATURE_DIM: usize = 17;

/// Compact behavioral fingerprint for an agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDna {
    // ── Activity profile ────────────────────────────────────────
    /// Fraction of time with output (0.0–1.0).
    pub active_fraction: f32,
    /// Fraction with no output (0.0–1.0).
    pub idle_fraction: f32,
    /// Number of activity bursts.
    pub burst_count: u32,
    /// Mean burst duration in seconds.
    pub mean_burst_duration_s: f32,
    /// Mean idle gap duration in seconds.
    pub mean_idle_duration_s: f32,

    // ── Output characteristics ──────────────────────────────────
    /// Total lines of output.
    pub total_lines: u64,
    /// Shannon entropy of output.
    pub output_entropy: f32,
    /// Fraction of unique lines.
    pub unique_line_ratio: f32,
    /// Fraction of ANSI escape sequences.
    pub ansi_density: f32,
    /// Mean line length in characters.
    pub mean_line_length: f32,

    // ── Tool / event usage ──────────────────────────────────────
    /// Tool call detections.
    pub tool_call_count: u32,
    /// Error pattern detections.
    pub error_count: u32,
    /// Rate limit detections.
    pub rate_limit_count: u32,
    /// Context compaction events.
    pub compaction_count: u32,

    // ── Timing ──────────────────────────────────────────────────
    /// Session duration in hours.
    pub duration_hours: f32,
    /// Hours until first error (None if no errors).
    pub time_to_first_error: Option<f32>,
    /// Tokens per hour throughput.
    pub tokens_per_hour: f32,
}

impl SessionDna {
    /// Extract the raw feature vector (17 dimensions) for PCA / similarity.
    ///
    /// `time_to_first_error` is mapped to `duration_hours` if None (no error occurred).
    #[must_use]
    pub fn to_raw_features(&self) -> [f64; RAW_FEATURE_DIM] {
        [
            self.active_fraction as f64,
            self.idle_fraction as f64,
            self.burst_count as f64,
            self.mean_burst_duration_s as f64,
            self.mean_idle_duration_s as f64,
            self.total_lines as f64,
            self.output_entropy as f64,
            self.unique_line_ratio as f64,
            self.ansi_density as f64,
            self.mean_line_length as f64,
            self.tool_call_count as f64,
            self.error_count as f64,
            self.rate_limit_count as f64,
            self.compaction_count as f64,
            self.duration_hours as f64,
            self.time_to_first_error.unwrap_or(self.duration_hours) as f64,
            self.tokens_per_hour as f64,
        ]
    }
}

impl Default for SessionDna {
    fn default() -> Self {
        Self {
            active_fraction: 0.0,
            idle_fraction: 1.0,
            burst_count: 0,
            mean_burst_duration_s: 0.0,
            mean_idle_duration_s: 0.0,
            total_lines: 0,
            output_entropy: 0.0,
            unique_line_ratio: 0.0,
            ansi_density: 0.0,
            mean_line_length: 0.0,
            tool_call_count: 0,
            error_count: 0,
            rate_limit_count: 0,
            compaction_count: 0,
            duration_hours: 0.0,
            time_to_first_error: None,
            tokens_per_hour: 0.0,
        }
    }
}

// =============================================================================
// Incremental Builder
// =============================================================================

/// Incrementally builds a SessionDna from capture events.
#[derive(Debug, Clone)]
pub struct SessionDnaBuilder {
    dna: SessionDna,
    /// Total active time in seconds.
    active_time_s: f64,
    /// Total idle time in seconds.
    idle_time_s: f64,
    /// Total idle gap durations for mean computation.
    idle_duration_sum_s: f64,
    /// Total idle gaps count.
    idle_gap_count: u32,
    /// Running sum of line lengths for mean.
    line_length_sum: f64,
    /// Total line count.
    line_count: u64,
    /// Whether currently in an active burst.
    in_burst: bool,
    /// Session start time (seconds since epoch).
    start_time_s: Option<f64>,
    /// Time of first error (seconds since epoch).
    first_error_time_s: Option<f64>,
}

impl SessionDnaBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dna: SessionDna::default(),
            active_time_s: 0.0,
            idle_time_s: 0.0,
            idle_duration_sum_s: 0.0,
            idle_gap_count: 0,
            line_length_sum: 0.0,
            line_count: 0,
            in_burst: false,
            start_time_s: None,
            first_error_time_s: None,
        }
    }

    /// Record output from a capture cycle.
    ///
    /// - `lines`: Number of new lines captured.
    /// - `avg_line_length`: Average length of new lines.
    /// - `entropy`: Shannon entropy of the output.
    /// - `unique_ratio`: Fraction of unique lines.
    /// - `ansi_density`: Fraction of ANSI escape bytes.
    /// - `elapsed_s`: Time since last capture (seconds).
    /// - `timestamp_s`: Current time (seconds since epoch).
    pub fn record_output(
        &mut self,
        lines: u64,
        avg_line_length: f32,
        entropy: f32,
        unique_ratio: f32,
        ansi_density: f32,
        elapsed_s: f64,
        timestamp_s: f64,
    ) {
        if self.start_time_s.is_none() {
            self.start_time_s = Some(timestamp_s);
        }

        if lines > 0 {
            // Active period
            self.active_time_s += elapsed_s;
            self.line_count += lines;
            self.line_length_sum += avg_line_length as f64 * lines as f64;
            self.dna.total_lines += lines;

            // Update running entropy (weighted average)
            let total = self.dna.total_lines as f32;
            let prev = (total - lines as f32).max(0.0);
            if total > 0.0 {
                self.dna.output_entropy =
                    (self.dna.output_entropy * prev + entropy * lines as f32) / total;
                self.dna.unique_line_ratio =
                    (self.dna.unique_line_ratio * prev + unique_ratio * lines as f32) / total;
                self.dna.ansi_density =
                    (self.dna.ansi_density * prev + ansi_density * lines as f32) / total;
            }

            if !self.in_burst {
                // Start of new burst
                self.dna.burst_count += 1;
                self.in_burst = true;
            }
        } else {
            // Idle period
            self.idle_time_s += elapsed_s;

            if self.in_burst {
                // End of burst → start of idle gap
                self.in_burst = false;
                self.idle_gap_count += 1;
            }

            if self.idle_gap_count > 0 {
                self.idle_duration_sum_s += elapsed_s;
            }
        }

        // Update derived fields
        let total_time = self.active_time_s + self.idle_time_s;
        if total_time > 0.0 {
            self.dna.active_fraction = (self.active_time_s / total_time) as f32;
            self.dna.idle_fraction = (self.idle_time_s / total_time) as f32;
            self.dna.duration_hours = (total_time / 3600.0) as f32;
        }

        if self.dna.burst_count > 0 {
            self.dna.mean_burst_duration_s =
                (self.active_time_s / self.dna.burst_count as f64) as f32;
        }

        if self.idle_gap_count > 0 {
            self.dna.mean_idle_duration_s =
                (self.idle_duration_sum_s / self.idle_gap_count as f64) as f32;
        }

        if self.line_count > 0 {
            self.dna.mean_line_length = (self.line_length_sum / self.line_count as f64) as f32;
        }
    }

    /// Record a pattern detection event.
    pub fn record_detection(&mut self, detection_type: DetectionType, timestamp_s: f64) {
        match detection_type {
            DetectionType::ToolCall => self.dna.tool_call_count += 1,
            DetectionType::Error => {
                self.dna.error_count += 1;
                if self.first_error_time_s.is_none() {
                    self.first_error_time_s = Some(timestamp_s);
                }
            }
            DetectionType::RateLimit => self.dna.rate_limit_count += 1,
            DetectionType::Compaction => self.dna.compaction_count += 1,
        }

        // Update time_to_first_error
        if let (Some(start), Some(first_err)) = (self.start_time_s, self.first_error_time_s) {
            self.dna.time_to_first_error = Some(((first_err - start) / 3600.0) as f32);
        }
    }

    /// Set tokens per hour throughput.
    pub fn set_tokens_per_hour(&mut self, tph: f32) {
        self.dna.tokens_per_hour = tph;
    }

    /// Build the final DNA snapshot.
    #[must_use]
    pub fn build(&self) -> SessionDna {
        self.dna.clone()
    }

    /// Get a reference to the current DNA state.
    #[must_use]
    pub fn current(&self) -> &SessionDna {
        &self.dna
    }
}

impl Default for SessionDnaBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Types of pattern detections that contribute to DNA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionType {
    ToolCall,
    Error,
    RateLimit,
    Compaction,
}

// =============================================================================
// Normalization (for cold-start similarity)
// =============================================================================

/// Running z-score normalizer for feature vectors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureNormalizer {
    /// Running mean per feature.
    mean: Vec<f64>,
    /// Running M2 (for Welford's variance).
    m2: Vec<f64>,
    /// Number of samples seen.
    count: u64,
}

impl FeatureNormalizer {
    /// Create a new normalizer for the given dimensionality.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            mean: vec![0.0; dim],
            m2: vec![0.0; dim],
            count: 0,
        }
    }

    /// Update running statistics with a new feature vector (Welford's algorithm).
    pub fn update(&mut self, features: &[f64]) {
        self.count += 1;
        let n = self.count as f64;
        for i in 0..self.mean.len().min(features.len()) {
            let delta = features[i] - self.mean[i];
            self.mean[i] += delta / n;
            let delta2 = features[i] - self.mean[i];
            self.m2[i] += delta * delta2;
        }
    }

    /// Normalize a feature vector to z-scores.
    #[must_use]
    pub fn normalize(&self, features: &[f64]) -> Vec<f64> {
        features
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                if i >= self.mean.len() || self.count < 2 {
                    return v;
                }
                let variance = self.m2[i] / (self.count as f64 - 1.0);
                let std_dev = variance.sqrt();
                if std_dev < 1e-10 {
                    0.0 // Constant feature
                } else {
                    (v - self.mean[i]) / std_dev
                }
            })
            .collect()
    }

    /// Number of samples processed.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }
}

// =============================================================================
// Similarity
// =============================================================================

/// Compute cosine similarity between two vectors.
///
/// Returns a value in [-1.0, 1.0]. Returns 0.0 if either vector is zero.
#[must_use]
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;

    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < 1e-15 {
        return 0.0;
    }

    (dot / denom).clamp(-1.0, 1.0)
}

/// Compute L2 (Euclidean) distance between two vectors.
#[must_use]
pub fn l2_distance(a: &[f64], b: &[f64]) -> f64 {
    let mut sum = 0.0;
    for i in 0..a.len().min(b.len()) {
        let d = a[i] - b[i];
        sum += d * d;
    }
    sum.sqrt()
}

// =============================================================================
// KNN Predictions
// =============================================================================

/// Result of a KNN prediction query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnnPrediction {
    /// Predicted duration in hours (median of K neighbors).
    pub predicted_duration_hours: f64,
    /// Duration IQR (interquartile range) in hours.
    pub duration_iqr_hours: f64,
    /// Number of neighbors used.
    pub k: usize,
    /// Similarities to the K neighbors.
    pub neighbor_similarities: Vec<f64>,
}

/// Find K nearest neighbors and predict duration.
///
/// Uses cosine similarity on embeddings (or normalized raw features in cold start).
pub fn knn_predict(
    query: &[f64],
    sessions: &[(Vec<f64>, f64)], // (embedding, duration_hours)
    k: usize,
) -> Option<KnnPrediction> {
    if sessions.is_empty() {
        return None;
    }

    let k = k.min(sessions.len());

    // Compute similarities
    let mut scored: Vec<(f64, f64)> = sessions
        .iter()
        .map(|(emb, dur)| (cosine_similarity(query, emb), *dur))
        .collect();

    // Sort by similarity descending
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let top_k: Vec<(f64, f64)> = scored.into_iter().take(k).collect();

    // Extract durations and compute median + IQR
    let mut durations: Vec<f64> = top_k.iter().map(|(_, d)| *d).collect();
    durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let median = if durations.len() % 2 == 0 {
        let mid = durations.len() / 2;
        (durations[mid - 1] + durations[mid]) / 2.0
    } else {
        durations[durations.len() / 2]
    };

    let q1_idx = durations.len() / 4;
    let q3_idx = (durations.len() * 3) / 4;
    let iqr = if durations.len() >= 4 {
        durations[q3_idx] - durations[q1_idx]
    } else {
        0.0
    };

    let neighbor_similarities = top_k.iter().map(|(s, _)| *s).collect();

    Some(KnnPrediction {
        predicted_duration_hours: median,
        duration_iqr_hours: iqr,
        k,
        neighbor_similarities,
    })
}

// =============================================================================
// Session Store (in-memory collection for clustering/prediction)
// =============================================================================

/// In-memory store of completed session DNAs for similarity queries.
#[derive(Debug, Clone)]
pub struct SessionStore {
    config: SessionDnaConfig,
    sessions: Vec<StoredSession>,
    normalizer: FeatureNormalizer,
}

/// A stored session with its DNA and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub session_id: String,
    pub dna: SessionDna,
    /// Normalized feature vector (or PCA embedding when available).
    pub embedding: Vec<f64>,
    pub duration_hours: f64,
    pub successful: bool,
}

impl SessionStore {
    /// Create a new session store.
    #[must_use]
    pub fn new(config: SessionDnaConfig) -> Self {
        Self {
            config,
            sessions: Vec::new(),
            normalizer: FeatureNormalizer::new(RAW_FEATURE_DIM),
        }
    }

    /// Add a completed session.
    pub fn add_session(&mut self, session_id: String, dna: SessionDna, successful: bool) {
        let raw = dna.to_raw_features();
        let raw_vec: Vec<f64> = raw.to_vec();

        // Update normalizer
        self.normalizer.update(&raw_vec);

        // Compute embedding (normalized raw features in cold-start mode)
        let embedding = self.normalizer.normalize(&raw_vec);

        let duration_hours = dna.duration_hours as f64;

        self.sessions.push(StoredSession {
            session_id,
            dna,
            embedding,
            duration_hours,
            successful,
        });

        // Re-normalize all existing sessions when normalizer has enough data
        // (every 10 sessions to avoid O(n²) cost)
        if self.sessions.len() % 10 == 0 {
            self.renormalize_all();
        }
    }

    /// Find K nearest neighbors and predict duration for a query DNA.
    #[must_use]
    pub fn predict(&self, query_dna: &SessionDna) -> Option<KnnPrediction> {
        let raw = query_dna.to_raw_features();
        let embedding = self.normalizer.normalize(&raw.to_vec());

        let session_data: Vec<(Vec<f64>, f64)> = self
            .sessions
            .iter()
            .map(|s| (s.embedding.clone(), s.duration_hours))
            .collect();

        knn_predict(&embedding, &session_data, self.config.k_neighbors)
    }

    /// Find sessions similar to the given DNA (above similarity threshold).
    #[must_use]
    pub fn find_similar(&self, query_dna: &SessionDna) -> Vec<(&StoredSession, f64)> {
        let raw = query_dna.to_raw_features();
        let embedding = self.normalizer.normalize(&raw.to_vec());

        let mut results: Vec<(&StoredSession, f64)> = self
            .sessions
            .iter()
            .map(|s| {
                let sim = cosine_similarity(&embedding, &s.embedding);
                (s, sim)
            })
            .filter(|(_, sim)| *sim >= self.config.similarity_threshold)
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    /// Number of stored sessions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Re-normalize all session embeddings with current normalizer state.
    fn renormalize_all(&mut self) {
        for session in &mut self.sessions {
            let raw = session.dna.to_raw_features();
            session.embedding = self.normalizer.normalize(&raw.to_vec());
        }
    }
}

// =============================================================================
// PCA Model (power iteration for top-k eigenvectors)
// =============================================================================

/// Fitted PCA model for dimensionality reduction.
///
/// Projects the 17-dimensional raw feature vector down to `embedding_dim`
/// dimensions using the top-k eigenvectors of the covariance matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PcaModel {
    /// Projection matrix: each row is an eigenvector (embedding_dim × RAW_FEATURE_DIM).
    pub components: Vec<Vec<f64>>,
    /// Explained variance per component.
    pub explained_variance: Vec<f64>,
    /// Feature means (for centering before projection).
    pub feature_means: Vec<f64>,
    /// Number of sessions used to fit.
    pub fit_count: usize,
}

impl PcaModel {
    /// Fit PCA on a collection of raw feature vectors.
    ///
    /// Uses power iteration with deflation to find the top `embedding_dim`
    /// eigenvectors. Returns `None` if insufficient data.
    pub fn fit(data: &[[f64; RAW_FEATURE_DIM]], embedding_dim: usize) -> Option<Self> {
        let n = data.len();
        if n < 2 || embedding_dim == 0 {
            return None;
        }
        let k = embedding_dim.min(RAW_FEATURE_DIM);

        // Compute means.
        let mut means = vec![0.0f64; RAW_FEATURE_DIM];
        for row in data {
            for (i, &v) in row.iter().enumerate() {
                means[i] += v;
            }
        }
        for m in means.iter_mut() {
            *m /= n as f64;
        }

        // Compute covariance matrix (D × D).
        let mut cov = vec![vec![0.0f64; RAW_FEATURE_DIM]; RAW_FEATURE_DIM];
        for row in data {
            for i in 0..RAW_FEATURE_DIM {
                let ci = row[i] - means[i];
                for j in i..RAW_FEATURE_DIM {
                    cov[i][j] += ci * (row[j] - means[j]);
                }
            }
        }
        let denom = (n - 1) as f64;
        for i in 0..RAW_FEATURE_DIM {
            for j in i..RAW_FEATURE_DIM {
                cov[i][j] /= denom;
                cov[j][i] = cov[i][j];
            }
        }

        // Power iteration for top-k with deflation.
        let mut components = Vec::with_capacity(k);
        let mut explained = Vec::with_capacity(k);

        for _ in 0..k {
            let (eigenvalue, eigenvector) = pca_power_iteration(&cov, 200);
            if eigenvalue.abs() < 1e-12 {
                break;
            }
            // Deflate.
            for i in 0..RAW_FEATURE_DIM {
                for j in 0..RAW_FEATURE_DIM {
                    cov[i][j] -= eigenvalue * eigenvector[i] * eigenvector[j];
                }
            }
            components.push(eigenvector.to_vec());
            explained.push(eigenvalue);
        }

        if components.is_empty() {
            return None;
        }

        Some(Self {
            components,
            explained_variance: explained,
            feature_means: means,
            fit_count: n,
        })
    }

    /// Project a raw feature vector to the embedding space.
    pub fn project(&self, features: &[f64; RAW_FEATURE_DIM]) -> Vec<f64> {
        self.components
            .iter()
            .map(|component| {
                let mut dot = 0.0;
                for i in 0..RAW_FEATURE_DIM {
                    dot += (features[i] - self.feature_means[i]) * component[i];
                }
                dot
            })
            .collect()
    }

    /// Reconstruct a feature vector from its embedding (approximate).
    pub fn reconstruct(&self, embedding: &[f64]) -> [f64; RAW_FEATURE_DIM] {
        let mut result = [0.0; RAW_FEATURE_DIM];
        for (i, &m) in self.feature_means.iter().enumerate().take(RAW_FEATURE_DIM) {
            result[i] = m;
        }
        for (k, &coeff) in embedding.iter().enumerate() {
            if k < self.components.len() {
                for i in 0..RAW_FEATURE_DIM {
                    result[i] += coeff * self.components[k][i];
                }
            }
        }
        result
    }

    /// Number of components.
    pub fn embedding_dim(&self) -> usize {
        self.components.len()
    }

    /// Total explained variance.
    pub fn total_explained_variance(&self) -> f64 {
        self.explained_variance.iter().sum()
    }
}

/// Power iteration to find the dominant eigenvector of a symmetric matrix.
fn pca_power_iteration(matrix: &[Vec<f64>], max_iter: usize) -> (f64, [f64; RAW_FEATURE_DIM]) {
    let d = RAW_FEATURE_DIM;
    let mut v = [0.0f64; RAW_FEATURE_DIM];
    v[0] = 1.0;

    let mut eigenvalue = 0.0;

    for _ in 0..max_iter {
        let mut w = [0.0f64; RAW_FEATURE_DIM];
        for i in 0..d {
            for j in 0..d {
                w[i] += matrix[i][j] * v[j];
            }
        }

        let norm: f64 = w.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm < 1e-15 {
            return (0.0, v);
        }
        eigenvalue = norm;

        for i in 0..d {
            v[i] = w[i] / norm;
        }
    }

    (eigenvalue, v)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dna() -> SessionDna {
        SessionDna {
            active_fraction: 0.7,
            idle_fraction: 0.3,
            burst_count: 5,
            mean_burst_duration_s: 120.0,
            mean_idle_duration_s: 30.0,
            total_lines: 5000,
            output_entropy: 4.5,
            unique_line_ratio: 0.8,
            ansi_density: 0.05,
            mean_line_length: 60.0,
            tool_call_count: 50,
            error_count: 3,
            rate_limit_count: 1,
            compaction_count: 2,
            duration_hours: 2.5,
            time_to_first_error: Some(0.5),
            tokens_per_hour: 1000.0,
        }
    }

    // ── SessionDna ──────────────────────────────────────────────────────

    #[test]
    fn dna_default() {
        let dna = SessionDna::default();
        assert_eq!(dna.total_lines, 0);
        assert!((dna.idle_fraction - 1.0).abs() < f64::EPSILON as f32);
    }

    #[test]
    fn dna_raw_features_dim() {
        let dna = sample_dna();
        let features = dna.to_raw_features();
        assert_eq!(features.len(), RAW_FEATURE_DIM);
    }

    #[test]
    fn dna_raw_features_no_error_maps_duration() {
        let mut dna = sample_dna();
        dna.time_to_first_error = None;
        dna.duration_hours = 3.0;
        let features = dna.to_raw_features();
        // time_to_first_error (index 15) should equal duration_hours
        assert!((features[15] - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dna_serde_roundtrip() {
        let dna = sample_dna();
        let json = serde_json::to_string(&dna).unwrap();
        let parsed: SessionDna = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_lines, 5000);
        assert_eq!(parsed.burst_count, 5);
        assert!((parsed.active_fraction - 0.7).abs() < 0.001);
    }

    // ── Builder ─────────────────────────────────────────────────────────

    #[test]
    fn builder_default_state() {
        let builder = SessionDnaBuilder::new();
        let dna = builder.build();
        assert_eq!(dna.total_lines, 0);
        assert_eq!(dna.burst_count, 0);
    }

    #[test]
    fn builder_records_output() {
        let mut builder = SessionDnaBuilder::new();
        builder.record_output(100, 50.0, 4.0, 0.8, 0.05, 1.0, 1000.0);
        let dna = builder.build();
        assert_eq!(dna.total_lines, 100);
        assert!(dna.active_fraction > 0.0);
        assert_eq!(dna.burst_count, 1);
    }

    #[test]
    fn builder_burst_detection() {
        let mut builder = SessionDnaBuilder::new();

        // Burst 1: active
        builder.record_output(10, 40.0, 3.0, 0.7, 0.02, 1.0, 100.0);
        builder.record_output(20, 50.0, 4.0, 0.8, 0.03, 1.0, 101.0);

        // Idle gap
        builder.record_output(0, 0.0, 0.0, 0.0, 0.0, 5.0, 106.0);

        // Burst 2: active again
        builder.record_output(15, 45.0, 3.5, 0.75, 0.04, 1.0, 107.0);

        let dna = builder.build();
        assert_eq!(dna.burst_count, 2);
        assert!(dna.mean_idle_duration_s > 0.0);
    }

    #[test]
    fn builder_detection_types() {
        let mut builder = SessionDnaBuilder::new();
        builder.record_output(10, 40.0, 3.0, 0.7, 0.02, 1.0, 100.0);

        builder.record_detection(DetectionType::ToolCall, 101.0);
        builder.record_detection(DetectionType::ToolCall, 102.0);
        builder.record_detection(DetectionType::Error, 103.0);
        builder.record_detection(DetectionType::RateLimit, 104.0);
        builder.record_detection(DetectionType::Compaction, 105.0);

        let dna = builder.build();
        assert_eq!(dna.tool_call_count, 2);
        assert_eq!(dna.error_count, 1);
        assert_eq!(dna.rate_limit_count, 1);
        assert_eq!(dna.compaction_count, 1);
    }

    #[test]
    fn builder_time_to_first_error() {
        let mut builder = SessionDnaBuilder::new();
        builder.record_output(10, 40.0, 3.0, 0.7, 0.02, 1.0, 1000.0);
        // Error at 4600s → 1 hour after start
        builder.record_detection(DetectionType::Error, 4600.0);

        let dna = builder.build();
        assert!(dna.time_to_first_error.is_some());
        assert!((dna.time_to_first_error.unwrap() - 1.0).abs() < 0.01);
    }

    #[test]
    fn builder_tokens_per_hour() {
        let mut builder = SessionDnaBuilder::new();
        builder.set_tokens_per_hour(1500.0);
        let dna = builder.build();
        assert!((dna.tokens_per_hour - 1500.0).abs() < f64::EPSILON as f32);
    }

    // ── Normalizer ──────────────────────────────────────────────────────

    #[test]
    fn normalizer_single_sample() {
        let mut norm = FeatureNormalizer::new(3);
        norm.update(&[1.0, 2.0, 3.0]);
        // With one sample, can't compute std dev → return raw values
        let result = norm.normalize(&[1.0, 2.0, 3.0]);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn normalizer_z_scores() {
        let mut norm = FeatureNormalizer::new(2);
        // Feed 100 samples: feature 0 ~ 10.0, feature 1 ~ 100.0
        for _ in 0..100 {
            norm.update(&[10.0, 100.0]);
        }

        // Normalize the mean → should give ~0.0
        let result = norm.normalize(&[10.0, 100.0]);
        assert!(result[0].abs() < 0.01, "mean should normalize to ~0");
        assert!(result[1].abs() < 0.01, "mean should normalize to ~0");
    }

    #[test]
    fn normalizer_serde() {
        let mut norm = FeatureNormalizer::new(3);
        norm.update(&[1.0, 2.0, 3.0]);
        let json = serde_json::to_string(&norm).unwrap();
        let parsed: FeatureNormalizer = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.count(), 1);
    }

    // ── Similarity ──────────────────────────────────────────────────────

    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-10);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-10);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0, 2.0];
        let b = vec![-1.0, -2.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-10);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = vec![1.0, 2.0];
        let b = vec![0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cosine_similarity_bounded() {
        let a = vec![3.0, -4.0, 5.0, -1.0];
        let b = vec![-2.0, 7.0, 1.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((-1.0..=1.0).contains(&sim), "sim {sim} out of [-1,1]");
    }

    #[test]
    fn l2_distance_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let dist = l2_distance(&a, &a);
        assert!(dist < 1e-10);
    }

    #[test]
    fn l2_distance_known() {
        let a = vec![0.0, 0.0];
        let b = vec![3.0, 4.0];
        let dist = l2_distance(&a, &b);
        assert!((dist - 5.0).abs() < 1e-10);
    }

    // ── KNN ─────────────────────────────────────────────────────────────

    #[test]
    fn knn_empty_sessions() {
        let query = vec![1.0, 2.0, 3.0];
        let result = knn_predict(&query, &[], 5);
        assert!(result.is_none());
    }

    #[test]
    fn knn_single_session() {
        let query = vec![1.0, 2.0, 3.0];
        let sessions = vec![(vec![1.0, 2.0, 3.0], 2.5)];
        let result = knn_predict(&query, &sessions, 5).unwrap();
        assert_eq!(result.k, 1);
        assert!((result.predicted_duration_hours - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn knn_multiple_sessions() {
        let query = vec![1.0, 0.0];
        let sessions = vec![
            (vec![1.0, 0.1], 2.0),
            (vec![0.0, 1.0], 8.0),
            (vec![1.0, -0.1], 3.0),
            (vec![-1.0, 0.0], 10.0),
        ];
        let result = knn_predict(&query, &sessions, 2).unwrap();
        assert_eq!(result.k, 2);
        // Top-2 should be the first and third (most similar to [1,0])
        assert!(result.predicted_duration_hours >= 2.0 && result.predicted_duration_hours <= 3.0);
    }

    #[test]
    fn knn_prediction_serializes() {
        let pred = KnnPrediction {
            predicted_duration_hours: 3.5,
            duration_iqr_hours: 1.2,
            k: 5,
            neighbor_similarities: vec![0.95, 0.90, 0.88, 0.85, 0.82],
        };
        let json = serde_json::to_string(&pred).unwrap();
        let parsed: KnnPrediction = serde_json::from_str(&json).unwrap();
        assert!((parsed.predicted_duration_hours - 3.5).abs() < f64::EPSILON);
    }

    // ── SessionStore ────────────────────────────────────────────────────

    #[test]
    fn store_add_and_query() {
        let mut store = SessionStore::new(SessionDnaConfig::default());
        let dna = sample_dna();
        store.add_session("s1".to_string(), dna.clone(), true);
        assert_eq!(store.len(), 1);

        let prediction = store.predict(&dna);
        assert!(prediction.is_some());
    }

    #[test]
    fn store_find_similar() {
        let mut store = SessionStore::new(SessionDnaConfig {
            similarity_threshold: 0.5, // Lower threshold for testing
            ..Default::default()
        });

        let dna1 = sample_dna();
        store.add_session("s1".to_string(), dna1.clone(), true);

        // Identical DNA should be found
        let similar = store.find_similar(&dna1);
        assert!(!similar.is_empty(), "should find self as similar");
    }

    #[test]
    fn store_empty() {
        let store = SessionStore::new(SessionDnaConfig::default());
        assert!(store.is_empty());
        let dna = sample_dna();
        assert!(store.predict(&dna).is_none());
    }

    #[test]
    fn store_multiple_sessions() {
        let mut store = SessionStore::new(SessionDnaConfig::default());

        for i in 0..20 {
            let mut dna = sample_dna();
            dna.duration_hours = 1.0 + (i as f32) * 0.5;
            dna.total_lines = 1000 + i * 500;
            store.add_session(format!("s{i}"), dna, i % 3 != 0);
        }

        assert_eq!(store.len(), 20);
        let pred = store.predict(&sample_dna()).unwrap();
        assert!(pred.k <= 10);
    }

    // ── Config ──────────────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = SessionDnaConfig::default();
        assert_eq!(c.embedding_dim, 8);
        assert!((c.similarity_threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(c.k_neighbors, 10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = SessionDnaConfig {
            embedding_dim: 4,
            similarity_threshold: 0.90,
            k_neighbors: 5,
            min_sessions_for_pca: 100,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: SessionDnaConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.embedding_dim, 4);
        assert_eq!(parsed.min_sessions_for_pca, 100);
    }

    // ── PCA ─────────────────────────────────────────────────────────────

    #[test]
    fn pca_fit_and_project() {
        let mut data: Vec<[f64; RAW_FEATURE_DIM]> = Vec::new();
        for i in 0..20 {
            let mut row = [0.0; RAW_FEATURE_DIM];
            row[0] = i as f64;
            row[1] = 0.1 * i as f64;
            data.push(row);
        }

        let model = PcaModel::fit(&data, 4).expect("PCA should fit");
        assert!(model.embedding_dim() <= 4);
        assert!(model.explained_variance[0] > 0.0);

        let projected = model.project(&data[5]);
        assert_eq!(projected.len(), model.embedding_dim());
        for &v in &projected {
            assert!(v.is_finite(), "projection should be finite");
        }
    }

    #[test]
    fn pca_insufficient_data() {
        let data: Vec<[f64; RAW_FEATURE_DIM]> = vec![[1.0; RAW_FEATURE_DIM]];
        assert!(PcaModel::fit(&data, 4).is_none());
    }

    #[test]
    fn pca_reconstruction_error() {
        let mut data: Vec<[f64; RAW_FEATURE_DIM]> = Vec::new();
        for i in 0..50 {
            let mut row = [0.0; RAW_FEATURE_DIM];
            row[0] = i as f64;
            row[1] = 0.5 * i as f64;
            row[2] = 0.1;
            data.push(row);
        }

        let model = PcaModel::fit(&data, 8).unwrap();
        let original = &data[10];
        let embedding = model.project(original);
        let reconstructed = model.reconstruct(&embedding);

        let error: f64 = original
            .iter()
            .zip(reconstructed.iter())
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f64>()
            .sqrt();
        assert!(error < 5.0, "reconstruction error={error} should be small");
    }

    #[test]
    fn pca_serde_roundtrip() {
        let mut data: Vec<[f64; RAW_FEATURE_DIM]> = Vec::new();
        for i in 0..10 {
            let mut row = [0.0; RAW_FEATURE_DIM];
            row[0] = i as f64;
            data.push(row);
        }
        let model = PcaModel::fit(&data, 2).unwrap();
        let json = serde_json::to_string(&model).unwrap();
        let parsed: PcaModel = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.fit_count, 10);
        assert_eq!(parsed.embedding_dim(), model.embedding_dim());
    }

    #[test]
    fn pca_total_explained_variance() {
        let mut data: Vec<[f64; RAW_FEATURE_DIM]> = Vec::new();
        for i in 0..30 {
            let mut row = [0.0; RAW_FEATURE_DIM];
            row[0] = i as f64;
            row[1] = 0.5 * i as f64;
            data.push(row);
        }
        let model = PcaModel::fit(&data, 4).unwrap();
        assert!(model.total_explained_variance() > 0.0);
    }

    // ── Proptest ────────────────────────────────────────────────────────

    mod proptest_dna {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Cosine similarity is always in [-1, 1].
            #[test]
            fn similarity_bounds(
                a in proptest::collection::vec(-100.0f64..100.0, 8),
                b in proptest::collection::vec(-100.0f64..100.0, 8),
            ) {
                let sim = cosine_similarity(&a, &b);
                prop_assert!(sim >= -1.0 - 1e-10 && sim <= 1.0 + 1e-10,
                    "cosine sim {sim} out of bounds");
            }

            /// Self-similarity is 1.0 for non-zero vectors.
            #[test]
            fn self_similarity_is_one(
                v in proptest::collection::vec(0.1f64..100.0, 8),
            ) {
                let sim = cosine_similarity(&v, &v);
                prop_assert!((sim - 1.0).abs() < 1e-10,
                    "self-similarity should be 1.0, got {sim}");
            }

            /// L2 distance is symmetric and non-negative.
            #[test]
            fn l2_symmetric_nonnegative(
                a in proptest::collection::vec(-100.0f64..100.0, 8),
                b in proptest::collection::vec(-100.0f64..100.0, 8),
            ) {
                let d_ab = l2_distance(&a, &b);
                let d_ba = l2_distance(&b, &a);
                prop_assert!(d_ab >= 0.0);
                prop_assert!((d_ab - d_ba).abs() < 1e-10, "L2 should be symmetric");
            }

            /// PCA projection produces finite values.
            #[test]
            fn pca_projection_finite(
                values in proptest::collection::vec(-1e3f64..1e3, RAW_FEATURE_DIM),
            ) {
                let mut data = Vec::new();
                for i in 0..10 {
                    let mut row = [0.0; RAW_FEATURE_DIM];
                    for j in 0..RAW_FEATURE_DIM {
                        row[j] = (i * RAW_FEATURE_DIM + j) as f64;
                    }
                    data.push(row);
                }
                if let Some(model) = PcaModel::fit(&data, 8) {
                    let mut features = [0.0; RAW_FEATURE_DIM];
                    for (i, &v) in values.iter().enumerate().take(RAW_FEATURE_DIM) {
                        features[i] = v;
                    }
                    let embedding = model.project(&features);
                    for (i, &v) in embedding.iter().enumerate() {
                        prop_assert!(v.is_finite(), "embedding[{i}]={v} not finite");
                    }
                }
            }

            /// Feature normalizer converges to target mean.
            #[test]
            fn normalizer_converges(
                target in proptest::collection::vec(-10.0f64..10.0, RAW_FEATURE_DIM),
            ) {
                let mut norm = FeatureNormalizer::new(RAW_FEATURE_DIM);
                for _ in 0..100 {
                    norm.update(&target);
                }
                // All identical observations → z-score should be ~0.
                let result = norm.normalize(&target);
                for (i, &v) in result.iter().enumerate() {
                    prop_assert!(v.abs() < 0.01,
                        "z-score of constant feature[{i}] should be ~0, got {v}");
                }
            }
        }
    }
}
