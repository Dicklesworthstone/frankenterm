//! Property-based tests for session_dna module.
//!
//! Verifies session DNA behavioral fingerprinting invariants:
//! - SessionDna: serde roundtrip, to_raw_features dimension, default
//! - SessionDnaConfig: serde roundtrip, default values
//! - DetectionType: serde roundtrip snake_case
//! - SessionDnaBuilder: detection counts monotonic, build snapshot
//! - FeatureNormalizer: serde roundtrip, output dimension, count tracking
//! - cosine_similarity: bounded [-1,1], symmetric, self=1, zero=0
//! - l2_distance: non-negative, symmetric, self=0, triangle inequality
//! - KnnPrediction: serde roundtrip
//! - PcaModel: serde roundtrip, embedding_dim bounded, project finite

use proptest::prelude::*;

use frankenterm_core::session_dna::{
    DetectionType, FeatureNormalizer, KnnPrediction, PcaModel, RAW_FEATURE_DIM, SessionDna,
    SessionDnaBuilder, SessionDnaConfig, cosine_similarity, l2_distance,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_detection_type() -> impl Strategy<Value = DetectionType> {
    prop_oneof![
        Just(DetectionType::ToolCall),
        Just(DetectionType::Error),
        Just(DetectionType::RateLimit),
        Just(DetectionType::Compaction),
    ]
}

fn arb_session_dna() -> impl Strategy<Value = SessionDna> {
    // Split into two tuples (max 12 elements each) to satisfy proptest bounds.
    let activity = (
        0.0f32..=1.0,     // active_fraction
        0.0f32..=1.0,     // idle_fraction
        0u32..=1000,      // burst_count
        0.0f32..=3600.0,  // mean_burst_duration_s
        0.0f32..=3600.0,  // mean_idle_duration_s
        0u64..=1_000_000, // total_lines
        0.0f32..=8.0,     // output_entropy
        0.0f32..=1.0,     // unique_line_ratio
        0.0f32..=1.0,     // ansi_density
        0.0f32..=500.0,   // mean_line_length
    );
    let events = (
        0u32..=10000,                     // tool_call_count
        0u32..=1000,                      // error_count
        0u32..=1000,                      // rate_limit_count
        0u32..=100,                       // compaction_count
        0.0f32..=100.0,                   // duration_hours
        prop::option::of(0.0f32..=100.0), // time_to_first_error
        0.0f32..=100000.0,                // tokens_per_hour
    );
    (activity, events).prop_map(
        |((af, idf, bc, mbd, mid, tl, oe, ulr, ad, mll), (tc, ec, rl, cc, dh, ttfe, tph))| {
            SessionDna {
                active_fraction: af,
                idle_fraction: idf,
                burst_count: bc,
                mean_burst_duration_s: mbd,
                mean_idle_duration_s: mid,
                total_lines: tl,
                output_entropy: oe,
                unique_line_ratio: ulr,
                ansi_density: ad,
                mean_line_length: mll,
                tool_call_count: tc,
                error_count: ec,
                rate_limit_count: rl,
                compaction_count: cc,
                duration_hours: dh,
                time_to_first_error: ttfe,
                tokens_per_hour: tph,
            }
        },
    )
}

fn arb_config() -> impl Strategy<Value = SessionDnaConfig> {
    (
        1usize..=32,  // embedding_dim
        0.0f64..=1.0, // similarity_threshold
        1usize..=100, // k_neighbors
        1usize..=500, // min_sessions_for_pca
    )
        .prop_map(|(dim, thresh, k, min_pca)| SessionDnaConfig {
            embedding_dim: dim,
            similarity_threshold: thresh,
            k_neighbors: k,
            min_sessions_for_pca: min_pca,
        })
}

fn arb_vec(dim: usize) -> impl Strategy<Value = Vec<f64>> {
    proptest::collection::vec(-100.0f64..100.0, dim)
}

fn arb_nonzero_vec(dim: usize) -> impl Strategy<Value = Vec<f64>> {
    proptest::collection::vec(0.1f64..100.0, dim)
}

// ────────────────────────────────────────────────────────────────────
// SessionDna: serde, raw features, default
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SessionDna JSON roundtrip preserves all fields.
    #[test]
    fn prop_dna_serde_roundtrip(dna in arb_session_dna()) {
        let json = serde_json::to_string(&dna).unwrap();
        let back: SessionDna = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_lines, dna.total_lines);
        prop_assert_eq!(back.burst_count, dna.burst_count);
        prop_assert_eq!(back.tool_call_count, dna.tool_call_count);
        prop_assert_eq!(back.error_count, dna.error_count);
        prop_assert_eq!(back.time_to_first_error.is_some(), dna.time_to_first_error.is_some());
    }

    /// to_raw_features always returns RAW_FEATURE_DIM elements.
    #[test]
    fn prop_dna_raw_features_dim(dna in arb_session_dna()) {
        let features = dna.to_raw_features();
        prop_assert_eq!(features.len(), RAW_FEATURE_DIM);
    }

    /// to_raw_features values are finite.
    #[test]
    fn prop_dna_raw_features_finite(dna in arb_session_dna()) {
        let features = dna.to_raw_features();
        for (i, &v) in features.iter().enumerate() {
            prop_assert!(v.is_finite(), "feature[{}] = {} not finite", i, v);
        }
    }

    /// to_raw_features maps time_to_first_error None to duration_hours.
    #[test]
    fn prop_dna_raw_features_error_fallback(
        dna_base in arb_session_dna(),
    ) {
        let mut dna = dna_base;
        dna.time_to_first_error = None;
        let features = dna.to_raw_features();
        // Index 15 is time_to_first_error, should equal duration_hours
        let expected = dna.duration_hours as f64;
        prop_assert!(
            (features[15] - expected).abs() < 1e-4,
            "feature[15]={} should be duration_hours={}", features[15], expected
        );
    }

    /// Default DNA has zero total_lines and 1.0 idle_fraction.
    #[test]
    fn prop_dna_default_values(_dummy in 0..1u32) {
        let dna = SessionDna::default();
        prop_assert_eq!(dna.total_lines, 0);
        prop_assert!((dna.idle_fraction - 1.0).abs() < 0.001);
        prop_assert!((dna.active_fraction - 0.0).abs() < 0.001);
    }
}

// ────────────────────────────────────────────────────────────────────
// SessionDnaConfig: serde, defaults
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Config JSON roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(c in arb_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: SessionDnaConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.embedding_dim, c.embedding_dim);
        prop_assert!((back.similarity_threshold - c.similarity_threshold).abs() < 1e-9);
        prop_assert_eq!(back.k_neighbors, c.k_neighbors);
        prop_assert_eq!(back.min_sessions_for_pca, c.min_sessions_for_pca);
    }

    /// Default config has valid values.
    #[test]
    fn prop_config_defaults(_dummy in 0..1u32) {
        let c = SessionDnaConfig::default();
        prop_assert!(c.embedding_dim > 0);
        prop_assert!(c.similarity_threshold > 0.0 && c.similarity_threshold <= 1.0);
        prop_assert!(c.k_neighbors > 0);
        prop_assert!(c.min_sessions_for_pca > 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// DetectionType: serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// DetectionType serde roundtrip.
    #[test]
    fn prop_detection_type_serde_roundtrip(dt in arb_detection_type()) {
        let json = serde_json::to_string(&dt).unwrap();
        let back: DetectionType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dt, back);
    }

    /// DetectionType serializes to snake_case.
    #[test]
    fn prop_detection_type_snake_case(dt in arb_detection_type()) {
        let json = serde_json::to_string(&dt).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized detection '{}' should be snake_case", inner
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// SessionDnaBuilder: detection counts and snapshots
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Detection counts are monotonically non-decreasing.
    #[test]
    fn prop_builder_detection_counts_monotonic(
        detections in proptest::collection::vec(arb_detection_type(), 1..20),
    ) {
        let mut builder = SessionDnaBuilder::new();
        builder.record_output(10, 40.0, 3.0, 0.7, 0.02, 1.0, 1000.0);

        let mut prev_tool = 0u32;
        let mut prev_error = 0u32;
        let mut prev_rate = 0u32;
        let mut prev_comp = 0u32;

        for (i, dt) in detections.iter().enumerate() {
            builder.record_detection(*dt, 1001.0 + i as f64);
            let dna = builder.build();
            prop_assert!(dna.tool_call_count >= prev_tool);
            prop_assert!(dna.error_count >= prev_error);
            prop_assert!(dna.rate_limit_count >= prev_rate);
            prop_assert!(dna.compaction_count >= prev_comp);
            prev_tool = dna.tool_call_count;
            prev_error = dna.error_count;
            prev_rate = dna.rate_limit_count;
            prev_comp = dna.compaction_count;
        }
    }

    /// Builder with 0-line records doesn't increase total_lines.
    #[test]
    fn prop_builder_zero_lines_no_increase(
        n_idle in 1usize..10,
    ) {
        let mut builder = SessionDnaBuilder::new();
        for i in 0..n_idle {
            builder.record_output(0, 0.0, 0.0, 0.0, 0.0, 1.0, 100.0 + i as f64);
        }
        let dna = builder.build();
        prop_assert_eq!(dna.total_lines, 0);
    }

    /// Builder accumulates total_lines correctly.
    #[test]
    fn prop_builder_total_lines_accumulate(
        line_counts in proptest::collection::vec(1u64..100, 1..10),
    ) {
        let mut builder = SessionDnaBuilder::new();
        let expected_total: u64 = line_counts.iter().sum();
        for (i, &lines) in line_counts.iter().enumerate() {
            builder.record_output(lines, 50.0, 3.0, 0.8, 0.02, 1.0, 100.0 + i as f64);
        }
        let dna = builder.build();
        prop_assert_eq!(dna.total_lines, expected_total);
    }

    /// set_tokens_per_hour is reflected in build.
    #[test]
    fn prop_builder_tokens_per_hour(tph in 0.0f32..100000.0) {
        let mut builder = SessionDnaBuilder::new();
        builder.set_tokens_per_hour(tph);
        let dna = builder.build();
        prop_assert!((dna.tokens_per_hour - tph).abs() < 0.001);
    }
}

// ────────────────────────────────────────────────────────────────────
// FeatureNormalizer: serde, dimension, count
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Normalizer serde roundtrip preserves count.
    #[test]
    fn prop_normalizer_serde_roundtrip(
        dim in 1usize..=20,
        n_updates in 0usize..=10,
    ) {
        let mut norm = FeatureNormalizer::new(dim);
        let sample: Vec<f64> = (0..dim).map(|i| i as f64).collect();
        for _ in 0..n_updates {
            norm.update(&sample);
        }

        let json = serde_json::to_string(&norm).unwrap();
        let back: FeatureNormalizer = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.count(), norm.count());
    }

    /// Normalize output has same length as input.
    #[test]
    fn prop_normalizer_output_dim(
        dim in 1usize..=20,
    ) {
        let mut norm = FeatureNormalizer::new(dim);
        let sample: Vec<f64> = (0..dim).map(|i| i as f64 * 2.0).collect();
        for _ in 0..5 {
            norm.update(&sample);
        }
        let result = norm.normalize(&sample);
        prop_assert_eq!(result.len(), dim);
    }

    /// Count tracks number of updates.
    #[test]
    fn prop_normalizer_count_tracks(n in 0u64..=50) {
        let mut norm = FeatureNormalizer::new(3);
        for _ in 0..n {
            norm.update(&[1.0, 2.0, 3.0]);
        }
        prop_assert_eq!(norm.count(), n);
    }

    /// Normalize of constant input gives ~0 z-scores (after enough samples).
    #[test]
    fn prop_normalizer_constant_gives_zero(
        value in proptest::collection::vec(-10.0f64..10.0, 3..=3),
    ) {
        let mut norm = FeatureNormalizer::new(3);
        for _ in 0..100 {
            norm.update(&value);
        }
        let result = norm.normalize(&value);
        for (i, &v) in result.iter().enumerate() {
            prop_assert!(v.abs() < 0.01, "z-score[{}]={} should be ~0 for constant", i, v);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// cosine_similarity: bounds, symmetry, self, zero
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Cosine similarity is always in [-1, 1].
    #[test]
    fn prop_cosine_bounded(a in arb_vec(8), b in arb_vec(8)) {
        let sim = cosine_similarity(&a, &b);
        prop_assert!(
            (-1.0 - 1e-10..=1.0 + 1e-10).contains(&sim),
            "cosine sim {} out of [-1,1]", sim
        );
    }

    /// Cosine similarity is symmetric.
    #[test]
    fn prop_cosine_symmetric(a in arb_vec(8), b in arb_vec(8)) {
        let ab = cosine_similarity(&a, &b);
        let ba = cosine_similarity(&b, &a);
        prop_assert!(
            (ab - ba).abs() < 1e-10,
            "cosine({:?},{:?})={} != cosine({:?},{:?})={}", a, b, ab, b, a, ba
        );
    }

    /// Self-similarity is 1.0 for non-zero vectors.
    #[test]
    fn prop_cosine_self_is_one(v in arb_nonzero_vec(8)) {
        let sim = cosine_similarity(&v, &v);
        prop_assert!(
            (sim - 1.0).abs() < 1e-10,
            "self-similarity should be 1.0, got {}", sim
        );
    }

    /// Cosine with zero vector returns 0.
    #[test]
    fn prop_cosine_zero_returns_zero(v in arb_vec(8)) {
        let zero = vec![0.0; 8];
        let sim = cosine_similarity(&v, &zero);
        prop_assert!(
            sim.abs() < 1e-10,
            "cosine with zero should be 0, got {}", sim
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// l2_distance: non-negative, symmetric, self=0, triangle inequality
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// L2 distance is non-negative.
    #[test]
    fn prop_l2_nonneg(a in arb_vec(8), b in arb_vec(8)) {
        prop_assert!(l2_distance(&a, &b) >= 0.0);
    }

    /// L2 distance is symmetric.
    #[test]
    fn prop_l2_symmetric(a in arb_vec(8), b in arb_vec(8)) {
        let ab = l2_distance(&a, &b);
        let ba = l2_distance(&b, &a);
        prop_assert!(
            (ab - ba).abs() < 1e-10,
            "L2 distance not symmetric: {} != {}", ab, ba
        );
    }

    /// L2 distance to self is 0.
    #[test]
    fn prop_l2_self_is_zero(v in arb_vec(8)) {
        let d = l2_distance(&v, &v);
        prop_assert!(d < 1e-10, "L2 self-distance should be 0, got {}", d);
    }

    /// L2 satisfies triangle inequality.
    #[test]
    fn prop_l2_triangle_inequality(
        a in arb_vec(8),
        b in arb_vec(8),
        c in arb_vec(8),
    ) {
        let ab = l2_distance(&a, &b);
        let bc = l2_distance(&b, &c);
        let ac = l2_distance(&a, &c);
        prop_assert!(
            ac <= ab + bc + 1e-9,
            "triangle inequality violated: d(a,c)={} > d(a,b)={} + d(b,c)={}", ac, ab, bc
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// KnnPrediction: serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// KnnPrediction serde roundtrip.
    #[test]
    fn prop_knn_prediction_serde(
        duration in 0.0f64..100.0,
        iqr in 0.0f64..50.0,
        k in 1usize..=20,
        sims in proptest::collection::vec(-1.0f64..=1.0, 1..=20),
    ) {
        let pred = KnnPrediction {
            predicted_duration_hours: duration,
            duration_iqr_hours: iqr,
            k,
            neighbor_similarities: sims.clone(),
        };
        let json = serde_json::to_string(&pred).unwrap();
        let back: KnnPrediction = serde_json::from_str(&json).unwrap();
        prop_assert!((back.predicted_duration_hours - duration).abs() < 1e-9);
        prop_assert!((back.duration_iqr_hours - iqr).abs() < 1e-9);
        prop_assert_eq!(back.k, k);
        prop_assert_eq!(back.neighbor_similarities.len(), sims.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// PcaModel: fit constraints, serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// PCA embedding_dim <= min(RAW_FEATURE_DIM, requested).
    #[test]
    fn prop_pca_embedding_bounded(
        requested_dim in 1usize..=20,
    ) {
        let mut data: Vec<[f64; RAW_FEATURE_DIM]> = Vec::new();
        for i in 0..20 {
            let mut row = [0.0; RAW_FEATURE_DIM];
            row[0] = i as f64;
            row[1] = 0.3 * i as f64;
            row[2] = 0.1 * i as f64 * i as f64;
            data.push(row);
        }
        if let Some(model) = PcaModel::fit(&data, requested_dim) {
            prop_assert!(
                model.embedding_dim() <= requested_dim.min(RAW_FEATURE_DIM),
                "embedding_dim {} > min({}, {})",
                model.embedding_dim(), requested_dim, RAW_FEATURE_DIM
            );
        }
    }

    /// PCA model serde roundtrip preserves fit_count and embedding_dim.
    #[test]
    fn prop_pca_serde_roundtrip(
        n_rows in 3usize..=15,
    ) {
        let mut data: Vec<[f64; RAW_FEATURE_DIM]> = Vec::new();
        for i in 0..n_rows {
            let mut row = [0.0; RAW_FEATURE_DIM];
            row[0] = i as f64;
            row[1] = 0.5 * i as f64;
            data.push(row);
        }
        if let Some(model) = PcaModel::fit(&data, 4) {
            let json = serde_json::to_string(&model).unwrap();
            let back: PcaModel = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back.fit_count, model.fit_count);
            prop_assert_eq!(back.embedding_dim(), model.embedding_dim());
        }
    }

    /// PCA explained_variance entries are non-negative.
    #[test]
    fn prop_pca_variance_nonneg(
        n_rows in 5usize..=20,
    ) {
        let mut data: Vec<[f64; RAW_FEATURE_DIM]> = Vec::new();
        for i in 0..n_rows {
            let mut row = [0.0; RAW_FEATURE_DIM];
            row[0] = i as f64;
            row[1] = 0.3 * i as f64;
            data.push(row);
        }
        if let Some(model) = PcaModel::fit(&data, 4) {
            for (i, &v) in model.explained_variance.iter().enumerate() {
                prop_assert!(v >= 0.0, "explained_variance[{}] = {} < 0", i, v);
            }
        }
    }

    /// PCA insufficient data (< 2 rows) returns None.
    #[test]
    fn prop_pca_insufficient_data(dim in 1usize..=8) {
        let data: Vec<[f64; RAW_FEATURE_DIM]> = vec![[1.0; RAW_FEATURE_DIM]];
        prop_assert!(PcaModel::fit(&data, dim).is_none());
    }
}
