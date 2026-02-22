//! Semantic Anomaly Detection for Terminal Streams.
//!
//! Detects sudden shifts in the semantic meaning of terminal output (e.g., an agent
//! suddenly encountering a massive Java stack trace after cleanly compiling Rust code,
//! or a server responding with an HTML 404 page instead of JSON).
//!
//! # Mathematics
//!
//! Computes the Exponentially Weighted Moving Average (EWMA) of embedding vectors
//! to maintain a "Semantic Centroid" on the unit sphere. It tracks the EWMA of
//! cosine distances to measure the expected semantic variance of the session. 
//! A "Shock" occurs when a new segment's distance from the centroid exceeds the 
//! expected variance by a tunable Z-score threshold.
//!
//! By operating natively in the embedding space (e.g., `all-MiniLM-L6-v2`), this
//! provides a zero-shot, LLM-free guardrail against context collapse and catastrophic
//! hallucination loops without requiring brittle regex rules for every possible error.

use serde::{Deserialize, Serialize};

/// Configuration for the Semantic Anomaly Detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticAnomalyConfig {
    /// The EWMA alpha for the semantic centroid (e.g., 0.1 ~ 10-step half-life).
    pub centroid_alpha: f32,
    /// The EWMA alpha for the variance tracking.
    pub variance_alpha: f32,
    /// Minimum number of samples required before shocks can be triggered.
    pub min_samples: usize,
    /// Z-score threshold for triggering a semantic shock (e.g., 3.0 standard deviations).
    pub shock_threshold_z: f32,
}

impl Default for SemanticAnomalyConfig {
    fn default() -> Self {
        Self {
            centroid_alpha: 0.15,
            variance_alpha: 0.10,
            min_samples: 5,
            shock_threshold_z: 3.5,
        }
    }
}

/// A detected semantic shock event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticShock {
    /// The distance from the semantic centroid.
    pub distance: f32,
    /// The current expected mean distance.
    pub expected_distance: f32,
    /// The current standard deviation of the distance.
    pub std_dev: f32,
    /// The calculated Z-score of this event.
    pub z_score: f32,
}

/// Tracks the semantic trajectory of a terminal pane.
#[derive(Debug, Clone)]
pub struct SemanticAnomalyDetector {
    config: SemanticAnomalyConfig,
    /// The running semantic centroid (normalized).
    centroid: Vec<f32>,
    /// Exponentially weighted moving average of the cosine distance to the centroid.
    mean_distance: f32,
    /// Exponentially weighted moving variance of the cosine distance.
    variance: f32,
    /// Number of observations processed.
    samples: usize,
}

impl SemanticAnomalyDetector {
    /// Create a new detector with the given configuration.
    #[must_use]
    pub fn new(config: SemanticAnomalyConfig) -> Self {
        Self {
            config,
            centroid: Vec::new(),
            mean_distance: 0.0,
            variance: 0.0,
            samples: 0,
        }
    }

    /// Process a new semantic embedding vector representing terminal output.
    ///
    /// The input vector is expected to be a valid embedding (typically 384d or 768d).
    /// Returns a `SemanticShock` if the new text represents a radical departure from
    /// the established semantic context of the pane.
    pub fn observe(&mut self, embedding: &[f32]) -> Option<SemanticShock> {
        if embedding.is_empty() {
            return None;
        }

        // Initialize centroid on first observation
        if self.samples == 0 {
            self.centroid = normalize(embedding);
            self.samples += 1;
            return None;
        }

        // Embedding dimensionality can change across model/provider switches.
        // Reset to the new basis instead of panicking on mismatched indexing.
        if self.centroid.len() != embedding.len() {
            self.centroid = normalize(embedding);
            self.mean_distance = 0.0;
            self.variance = 0.0;
            self.samples = 1;
            return None;
        }

        let normalized_emb = normalize(embedding);
        
        // Calculate Cosine Distance (1.0 - Cosine Similarity)
        // Since vectors are normalized, dot product is cosine similarity.
        let similarity = dot_product(&self.centroid, &normalized_emb);
        let distance = (1.0 - similarity).max(0.0);

        let mut shock = None;

        if self.samples >= self.config.min_samples {
            let std_dev = self.variance.sqrt();
            
            // Avoid division by zero for identical repeated inputs
            let safe_std_dev = std_dev.max(1e-5);
            
            let z_score = (distance - self.mean_distance) / safe_std_dev;

            if z_score >= self.config.shock_threshold_z {
                shock = Some(SemanticShock {
                    distance,
                    expected_distance: self.mean_distance,
                    std_dev,
                    z_score,
                });
            }
        }

        // Update statistics using Welford's online algorithm adapted for EWMA
        let diff = distance - self.mean_distance;
        self.mean_distance += self.config.variance_alpha * diff;
        
        // Variance EWMA update: (1-alpha)*(var + alpha*diff^2)
        // See: https://fanf2.user.srcf.net/hermes/doc/antiforgery/stats.pdf
        self.variance = (1.0 - self.config.variance_alpha) 
            * (self.variance + self.config.variance_alpha * diff * diff);

        // Update the semantic centroid via spherical linear interpolation (SLERP approximation)
        // We use simple vector addition and re-normalization for efficiency.
        for (i, val) in self.centroid.iter_mut().enumerate() {
            *val = (1.0 - self.config.centroid_alpha) * (*val) 
                 + self.config.centroid_alpha * normalized_emb[i];
        }
        self.centroid = normalize(&self.centroid);

        self.samples += 1;
        shock
    }

    /// Retrieve the current stable semantic centroid of this session.
    #[must_use]
    pub fn current_centroid(&self) -> &[f32] {
        &self.centroid
    }
    
    /// Reset the detector's state (e.g., after an intentional context switch like ssh/su).
    pub fn reset(&mut self) {
        self.centroid.clear();
        self.mean_distance = 0.0;
        self.variance = 0.0;
        self.samples = 0;
    }
}

// ─── Math Helpers ────────────────────────────────────────────────────────────

#[inline]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[inline]
fn normalize(v: &[f32]) -> Vec<f32> {
    let mag_sq: f32 = v.iter().map(|x| x * x).sum();
    if mag_sq > 0.0 {
        let inv_mag = 1.0 / mag_sq.sqrt();
        v.iter().map(|x| x * inv_mag).collect()
    } else {
        v.to_vec()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_vector(val: f32, dim: usize) -> Vec<f32> {
        vec![val; dim]
    }

    #[test]
    fn test_semantic_anomaly_initialization() {
        let mut detector = SemanticAnomalyDetector::new(SemanticAnomalyConfig::default());
        assert!(detector.observe(&dummy_vector(1.0, 10)).is_none());
        assert_eq!(detector.samples, 1);
    }

    #[test]
    fn test_stable_stream_no_shocks() {
        let mut detector = SemanticAnomalyDetector::new(SemanticAnomalyConfig::default());
        let vec_a = vec![1.0, 0.0, 0.0];
        let vec_b = vec![0.9, 0.1, 0.0]; // very similar

        for _ in 0..10 {
            assert!(detector.observe(&vec_a).is_none());
            assert!(detector.observe(&vec_b).is_none());
        }
    }

    #[test]
    fn test_semantic_shock_detection() {
        let config = SemanticAnomalyConfig {
            min_samples: 3,
            shock_threshold_z: 2.0,
            ..SemanticAnomalyConfig::default()
        };
        let mut detector = SemanticAnomalyDetector::new(config);

        // Context A: (1, 0, 0)
        let context_a = vec![1.0, 0.0, 0.0];
        for _ in 0..10 {
            assert!(detector.observe(&context_a).is_none());
        }

        // Sudden Shift to Context B: (0, 1, 0) - Orthogonal!
        let context_b = vec![0.0, 1.0, 0.0];
        let shock = detector.observe(&context_b);
        
        assert!(shock.is_some(), "Expected a semantic shock upon orthogonal shift");
        let s = shock.unwrap();
        assert!(s.z_score > 2.0, "Z-score {} should exceed threshold", s.z_score);
        assert!(s.distance > 0.5, "Distance should be large (orthogonal)");
    }

    #[test]
    fn test_dimension_change_resets_detector_state() {
        let mut detector = SemanticAnomalyDetector::new(SemanticAnomalyConfig::default());

        assert!(detector.observe(&[1.0, 0.0, 0.0]).is_none());
        assert_eq!(detector.current_centroid().len(), 3);
        assert_eq!(detector.samples, 1);

        // Different embedding dimension should reset instead of panicking.
        assert!(detector.observe(&[0.0, 1.0]).is_none());
        assert_eq!(detector.current_centroid().len(), 2);
        assert_eq!(detector.samples, 1);
    }
}
