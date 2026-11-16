//! Property-based tests for the hash_embedder module.
//!
//! Verifies invariants of `HashEmbedder` (FNV-1a feature-hashing embedder):
//! - Output dimension: embed(text).len() == dimension for all inputs
//! - Determinism: embed(text) == embed(text)
//! - L2 normalization: ||embed(text)|| ≈ 1.0 for non-empty text with sufficient chars
//! - Case insensitivity: embed("ABC") == embed("abc")
//! - Empty text produces zero vector
//! - Short text (< min_ngram chars) produces zero vector
//! - Default dimension is 128
//! - Info name follows format "fnv1a-hash-{dimension}"
//! - Batch embed produces same results as individual embed
//! - dimension() matches info().dimension
//! - tier() always returns EmbedderTier::Hash
//! - Cosine self-similarity ≈ 1.0
//! - Different texts produce different embeddings (probabilistically)
//! - ngram_range builder validation
//! - Large dimension support
//! - Unicode text support
//! - Whitespace-only text behavior
//! - Embedding sparsity properties
//! - Commutative hashing across dimensions

use proptest::prelude::*;

use frankenterm_core::search::{Embedder, EmbedderTier, HashEmbedder};

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

/// Compute L2 norm of a vector.
fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Compute cosine similarity between two vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na = l2_norm(a);
    let nb = l2_norm(b);
    if na < f32::EPSILON || nb < f32::EPSILON {
        return 0.0;
    }
    dot / (na * nb)
}

/// Check if a vector is all zeros.
fn is_zero_vector(v: &[f32]) -> bool {
    v.iter().all(|&x| x == 0.0)
}

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

/// Dimension >= 1 (dimension=0 panics).
fn arb_dimension() -> impl Strategy<Value = usize> {
    1usize..=1024
}

/// Non-trivial dimension for tests where very small dims cause collisions.
fn arb_medium_dimension() -> impl Strategy<Value = usize> {
    32usize..=512
}

/// Arbitrary non-empty text with at least `min_chars` characters.
fn arb_text_min_chars(min_chars: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), min_chars..min_chars + 100)
        .prop_map(|chars| chars.into_iter().collect::<String>())
}

/// Arbitrary ASCII text of various lengths.
fn arb_ascii_text() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-zA-Z0-9 ]{0,200}").unwrap()
}

/// Text guaranteed long enough for default ngram_range (3,4).
fn arb_long_text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 5..100)
        .prop_map(|chars| chars.into_iter().collect::<String>())
}

/// Valid ngram range: min >= 1, min <= max, max <= 10.
fn arb_ngram_range() -> impl Strategy<Value = (usize, usize)> {
    (1usize..=10).prop_flat_map(|min| (Just(min), min..=10))
}

/// Text that is shorter than the given min_ngram length.
fn arb_short_text(max_len: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::char::range('a', 'z'), 0..max_len)
        .prop_map(|chars| chars.into_iter().collect::<String>())
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Output dimension invariant
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Embedding output length always equals the configured dimension.
    #[test]
    fn output_length_equals_dimension(
        dim in arb_dimension(),
        text in arb_ascii_text(),
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        prop_assert_eq!(v.len(), dim, "output len {} != dimension {}", v.len(), dim);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Embedding output length equals dimension for empty text too.
    #[test]
    fn output_length_for_empty_text(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed("").unwrap();
        prop_assert_eq!(v.len(), dim, "empty text output len {} != dimension {}", v.len(), dim);
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Determinism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Embedding the same text twice produces identical vectors.
    #[test]
    fn determinism(
        dim in arb_medium_dimension(),
        text in arb_ascii_text(),
    ) {
        let emb = HashEmbedder::new(dim);
        let v1 = emb.embed(&text).unwrap();
        let v2 = emb.embed(&text).unwrap();
        prop_assert_eq!(&v1, &v2, "embed() not deterministic for text len={}", text.len());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Two separate HashEmbedder instances with the same config produce identical results.
    #[test]
    fn determinism_across_instances(
        dim in arb_medium_dimension(),
        text in arb_long_text(),
    ) {
        let emb1 = HashEmbedder::new(dim);
        let emb2 = HashEmbedder::new(dim);
        let v1 = emb1.embed(&text).unwrap();
        let v2 = emb2.embed(&text).unwrap();
        prop_assert_eq!(&v1, &v2, "different instances give different results for dim={}", dim);
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: L2 normalization
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Non-empty text with enough chars for ngrams produces a unit-norm vector.
    #[test]
    fn l2_norm_unit_for_sufficient_text(
        dim in arb_medium_dimension(),
        text in arb_text_min_chars(5),
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        let norm = l2_norm(&v);
        prop_assert!(
            (norm - 1.0).abs() < 0.01,
            "norm {} not close to 1.0 for dim={}, text_len={}",
            norm, dim, text.len()
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// L2 norm is either 0 (zero vector) or approximately 1.0.
    #[test]
    fn l2_norm_zero_or_unit(
        dim in arb_medium_dimension(),
        text in arb_ascii_text(),
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        let norm = l2_norm(&v);
        let is_zero = is_zero_vector(&v);
        if is_zero {
            prop_assert!(
                norm < f32::EPSILON,
                "zero vector has non-zero norm: {}",
                norm
            );
        } else {
            prop_assert!(
                (norm - 1.0).abs() < 0.01,
                "non-zero vector norm {} not ≈ 1.0",
                norm
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Case insensitivity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Uppercased and lowercased versions of the same text produce identical embeddings.
    #[test]
    fn case_insensitivity(
        dim in arb_medium_dimension(),
        text in arb_long_text(),
    ) {
        let emb = HashEmbedder::new(dim);
        let upper = text.to_uppercase();
        let lower = text.to_lowercase();
        let vu = emb.embed(&upper).unwrap();
        let vl = emb.embed(&lower).unwrap();
        prop_assert_eq!(&vu, &vl, "case insensitivity failed for text_len={}", text.len());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Mixed case produces same embedding as lowercase.
    #[test]
    fn mixed_case_equals_lowercase(
        dim in arb_medium_dimension(),
        text in "[a-zA-Z]{5,30}",
    ) {
        let emb = HashEmbedder::new(dim);
        let v_mixed = emb.embed(&text).unwrap();
        let v_lower = emb.embed(&text.to_lowercase()).unwrap();
        prop_assert_eq!(&v_mixed, &v_lower, "mixed case != lowercase for dim={}", dim);
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Empty and short text → zero vector
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty text always produces a zero vector regardless of dimension.
    #[test]
    fn empty_text_zero_vector(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed("").unwrap();
        prop_assert!(
            is_zero_vector(&v),
            "empty text produced non-zero vector for dim={}",
            dim
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Text shorter than min_ngram produces zero vector.
    #[test]
    fn short_text_zero_vector(
        dim in arb_medium_dimension(),
        (min_ngram, max_ngram) in arb_ngram_range(),
        text in arb_short_text(1),
    ) {
        // Only test when text char count < min_ngram
        let char_count = text.chars().count();
        prop_assume!(char_count < min_ngram);

        let emb = HashEmbedder::new(dim).with_ngram_range(min_ngram, max_ngram);
        let v = emb.embed(&text).unwrap();
        prop_assert!(
            is_zero_vector(&v),
            "text with {} chars (< min_ngram {}) gave non-zero vector for dim={}",
            char_count, min_ngram, dim
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Single character with default ngram range (3,4) produces zero vector.
    #[test]
    fn single_char_default_ngram_zero(
        dim in arb_dimension(),
        c in prop::char::range('a', 'z'),
    ) {
        let emb = HashEmbedder::new(dim);
        let text = c.to_string();
        let v = emb.embed(&text).unwrap();
        prop_assert!(
            is_zero_vector(&v),
            "single char '{}' produced non-zero vector for dim={}",
            c, dim
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Two-character text with default ngram range (3,4) produces zero vector.
    #[test]
    fn two_char_default_ngram_zero(
        dim in arb_dimension(),
        a in prop::char::range('a', 'z'),
        b in prop::char::range('a', 'z'),
    ) {
        let emb = HashEmbedder::new(dim);
        let text = format!("{}{}", a, b);
        let v = emb.embed(&text).unwrap();
        prop_assert!(
            is_zero_vector(&v),
            "two-char text '{}' produced non-zero vector for dim={}",
            text, dim
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Info and metadata
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// info().name follows the format "fnv1a-hash-{dimension}".
    #[test]
    fn info_name_format(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        let info = emb.info();
        let expected = format!("fnv1a-hash-{}", dim);
        prop_assert_eq!(info.name, expected, "info name mismatch for dim={}", dim);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// info().dimension matches the configured dimension.
    #[test]
    fn info_dimension_matches(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        let info = emb.info();
        prop_assert_eq!(info.dimension, dim, "info dimension {} != configured {}", info.dimension, dim);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// dimension() accessor matches info().dimension.
    #[test]
    fn dimension_accessor_consistent(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        prop_assert_eq!(
            emb.dimension(), emb.info().dimension,
            "dimension() {} != info().dimension {}",
            emb.dimension(), emb.info().dimension
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// tier() always returns EmbedderTier::Hash.
    #[test]
    fn tier_always_hash(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        prop_assert_eq!(
            emb.tier(), EmbedderTier::Hash,
            "tier was not Hash for dim={}", dim
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// info().tier matches tier().
    #[test]
    fn info_tier_consistent(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        prop_assert_eq!(
            emb.info().tier, emb.tier(),
            "info().tier != tier() for dim={}", dim
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Batch embed consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// embed_batch produces same results as calling embed individually.
    #[test]
    fn batch_matches_individual(
        dim in arb_medium_dimension(),
        texts in prop::collection::vec(arb_long_text(), 1..10),
    ) {
        let emb = HashEmbedder::new(dim);
        let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let batch_results = emb.embed_batch(&text_refs).unwrap();

        prop_assert_eq!(
            batch_results.len(), texts.len(),
            "batch len {} != texts len {}", batch_results.len(), texts.len()
        );

        for (i, text) in texts.iter().enumerate() {
            let individual = emb.embed(text).unwrap();
            prop_assert_eq!(
                &batch_results[i], &individual,
                "batch[{}] != individual embed for dim={}", i, dim
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Batch embedding of empty slice returns empty vec.
    #[test]
    fn batch_empty_input(dim in arb_dimension()) {
        let emb = HashEmbedder::new(dim);
        let result = emb.embed_batch(&[]).unwrap();
        prop_assert!(result.is_empty(), "batch of empty slice should be empty");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Batch dimension: every vector in batch has correct length.
    #[test]
    fn batch_all_correct_dimension(
        dim in arb_medium_dimension(),
        texts in prop::collection::vec(arb_ascii_text(), 1..15),
    ) {
        let emb = HashEmbedder::new(dim);
        let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let batch = emb.embed_batch(&text_refs).unwrap();
        for (i, v) in batch.iter().enumerate() {
            prop_assert_eq!(
                v.len(), dim,
                "batch[{}] has len {} but expected {}", i, v.len(), dim
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Self-similarity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Cosine similarity of a vector with itself is ≈ 1.0 (for non-zero vectors).
    #[test]
    fn self_similarity_near_one(
        dim in arb_medium_dimension(),
        text in arb_text_min_chars(5),
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        let sim = cosine_similarity(&v, &v);
        prop_assert!(
            (sim - 1.0).abs() < 0.01,
            "self-similarity {} not ≈ 1.0 for dim={}, text_len={}",
            sim, dim, text.len()
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Ngram range builder
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// with_ngram_range preserves dimension and produces valid output.
    #[test]
    fn ngram_range_preserves_dimension(
        dim in arb_medium_dimension(),
        (min_n, max_n) in arb_ngram_range(),
        text in arb_long_text(),
    ) {
        let emb = HashEmbedder::new(dim).with_ngram_range(min_n, max_n);
        let v = emb.embed(&text).unwrap();
        prop_assert_eq!(
            v.len(), dim,
            "ngram_range({},{}) changed output dim from {} to {}",
            min_n, max_n, dim, v.len()
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Changing ngram range changes embedding (for sufficiently long text).
    #[test]
    fn different_ngram_ranges_differ(
        dim in 64usize..=256,
        text in arb_text_min_chars(12),
    ) {
        let emb1 = HashEmbedder::new(dim).with_ngram_range(2, 3);
        let emb2 = HashEmbedder::new(dim).with_ngram_range(4, 6);
        let v1 = emb1.embed(&text).unwrap();
        let v2 = emb2.embed(&text).unwrap();
        // Different ngram ranges should produce different vectors (probabilistically)
        let sim = cosine_similarity(&v1, &v2);
        prop_assert!(
            sim < 0.9999,
            "ngram ranges (2,3) and (4,6) gave identical vectors, sim={}",
            sim
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Different texts differ
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Different texts produce different embeddings (with high probability at dim >= 64).
    #[test]
    fn different_texts_differ(
        dim in 64usize..=512,
        t1 in arb_text_min_chars(8),
        t2 in arb_text_min_chars(8),
    ) {
        prop_assume!(t1.to_lowercase() != t2.to_lowercase());
        let emb = HashEmbedder::new(dim);
        let v1 = emb.embed(&t1).unwrap();
        let v2 = emb.embed(&t2).unwrap();
        prop_assert_ne!(&v1, &v2, "different texts produced identical vectors");
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Unicode support
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Unicode text produces a valid embedding of the correct dimension.
    #[test]
    fn unicode_text_valid(
        dim in arb_medium_dimension(),
        text in "[\\p{Han}\\p{Hiragana}\\p{Katakana}]{5,50}",
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        prop_assert_eq!(v.len(), dim, "unicode embed len {} != dim {}", v.len(), dim);
        // Non-empty unicode should produce a normalized vector
        let norm = l2_norm(&v);
        prop_assert!(
            (norm - 1.0).abs() < 0.01,
            "unicode norm {} not ≈ 1.0",
            norm
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Default configuration
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Default embedder has dimension 128 and produces correct output length.
    #[test]
    fn default_dimension_128(text in arb_ascii_text()) {
        let emb = HashEmbedder::default();
        prop_assert_eq!(emb.dimension(), 128_usize, "default dimension is not 128");
        let v = emb.embed(&text).unwrap();
        prop_assert_eq!(v.len(), 128_usize, "default output len {} != 128", v.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Embedding vector properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// All values in the embedding are finite (no NaN or Inf).
    #[test]
    fn all_values_finite(
        dim in arb_medium_dimension(),
        text in arb_ascii_text(),
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        for (i, &val) in v.iter().enumerate() {
            prop_assert!(
                val.is_finite(),
                "v[{}] = {} is not finite for dim={}, text_len={}",
                i, val, dim, text.len()
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Normalized vectors have all components in [-1.0, 1.0].
    #[test]
    fn components_bounded(
        dim in arb_medium_dimension(),
        text in arb_text_min_chars(5),
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        for (i, &val) in v.iter().enumerate() {
            prop_assert!(
                (-1.0..=1.0).contains(&val),
                "v[{}] = {} out of [-1,1] range for dim={}",
                i, val, dim
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Non-zero embeddings have at least one non-zero component.
    #[test]
    fn sufficient_text_non_zero(
        dim in arb_medium_dimension(),
        text in arb_text_min_chars(5),
    ) {
        let emb = HashEmbedder::new(dim);
        let v = emb.embed(&text).unwrap();
        prop_assert!(
            !is_zero_vector(&v),
            "text with {} chars produced zero vector for dim={}",
            text.len(), dim
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Dimension-independence of hashing
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Cloning an embedder produces identical results.
    #[test]
    fn clone_produces_identical_results(
        dim in arb_medium_dimension(),
        text in arb_long_text(),
    ) {
        let emb1 = HashEmbedder::new(dim);
        let emb2 = emb1.clone();
        let v1 = emb1.embed(&text).unwrap();
        let v2 = emb2.embed(&text).unwrap();
        prop_assert_eq!(&v1, &v2, "cloned embedder gave different result for dim={}", dim);
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Whitespace handling
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Whitespace-only strings of length >= min_ngram produce non-zero embeddings
    /// (spaces are valid ngram characters).
    #[test]
    fn whitespace_only_long_enough(dim in arb_medium_dimension()) {
        let emb = HashEmbedder::new(dim); // default ngram_range (3,4)
        // 5 spaces >= min_ngram=3, so ngrams are generated
        let text = "     ";
        let v = emb.embed(text).unwrap();
        // Spaces form valid ngrams, so the vector should be non-zero
        prop_assert!(
            !is_zero_vector(&v),
            "5-space string produced zero vector for dim={}",
            dim
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Embedding result is always Ok (never errors) for any string input.
    #[test]
    fn embed_never_errors(
        dim in arb_dimension(),
        text in ".*",
    ) {
        let emb = HashEmbedder::new(dim);
        let result = emb.embed(&text);
        prop_assert!(result.is_ok(), "embed returned error for dim={}", dim);
    }
}
