//! FNV-1a feature-hashing embedder — zero-dependency fallback.
use super::embedder::{EmbedError, Embedder, EmbedderInfo, EmbedderTier};

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x00000100000001B3;

#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dimension: usize,
    ngram_range: (usize, usize),
}

impl HashEmbedder {
    pub fn new(dimension: usize) -> Self {
        assert!(dimension > 0, "dimension must be > 0");
        Self {
            dimension,
            ngram_range: (3, 4),
        }
    }

    #[must_use]
    pub fn with_ngram_range(mut self, min: usize, max: usize) -> Self {
        assert!(min > 0 && min <= max);
        self.ngram_range = (min, max);
        self
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(128)
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn l2_normalize(v: &mut [f32]) -> f32 {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    norm
}

impl Embedder for HashEmbedder {
    fn info(&self) -> EmbedderInfo {
        EmbedderInfo {
            name: format!("fnv1a-hash-{}", self.dimension),
            dimension: self.dimension,
            tier: EmbedderTier::Hash,
        }
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut vector = vec![0.0f32; self.dimension];
        let lower = text.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        if chars.is_empty() {
            return Ok(vector);
        }
        for n in self.ngram_range.0..=self.ngram_range.1 {
            if n > chars.len() {
                continue;
            }
            for window in chars.windows(n) {
                let ngram: String = window.iter().collect();
                let h = fnv1a(ngram.as_bytes());
                let bucket = (h as usize) % self.dimension;
                let sign = if (h >> 32) & 1 == 0 { 1.0f32 } else { -1.0f32 };
                vector[bucket] += sign;
            }
        }
        l2_normalize(&mut vector);
        Ok(vector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_embedding() {
        let emb = HashEmbedder::new(64);
        let v = emb.embed("hello world").unwrap();
        assert_eq!(v.len(), 64);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01);
    }

    #[test]
    fn deterministic() {
        let emb = HashEmbedder::new(64);
        assert_eq!(emb.embed("test").unwrap(), emb.embed("test").unwrap());
    }

    #[test]
    fn different_inputs_differ() {
        let emb = HashEmbedder::new(128);
        let v1 = emb.embed("hello").unwrap();
        let v2 = emb.embed("goodbye").unwrap();
        let dot: f32 = v1.iter().zip(&v2).map(|(a, b)| a * b).sum();
        assert!(dot < 0.99);
    }

    #[test]
    fn empty_input() {
        let emb = HashEmbedder::new(32);
        let v = emb.embed("").unwrap();
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn case_insensitive() {
        let emb = HashEmbedder::new(64);
        assert_eq!(emb.embed("Hello").unwrap(), emb.embed("hello").unwrap());
    }

    #[test]
    fn batch_embed() {
        let emb = HashEmbedder::new(64);
        let results = emb.embed_batch(&["hello", "world"]).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn dimension_accessor() {
        let emb = HashEmbedder::new(256);
        assert_eq!(emb.dimension(), 256);
        assert_eq!(emb.tier(), EmbedderTier::Hash);
    }

    #[test]
    fn default_embedder() {
        let emb = HashEmbedder::default();
        assert_eq!(emb.dimension(), 128);
    }

    #[test]
    fn custom_ngram_range() {
        let emb = HashEmbedder::new(64).with_ngram_range(2, 5);
        let v = emb.embed("testing").unwrap();
        assert_eq!(v.len(), 64);
    }

    #[test]
    fn fnv1a_known_values() {
        assert_eq!(fnv1a(b""), FNV_OFFSET);
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
    }

    #[test]
    fn l2_normalize_unit() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 0.001);
        assert!((v[1] - 0.8).abs() < 0.001);
    }

    #[test]
    #[should_panic(expected = "dimension must be > 0")]
    fn zero_dimension_panics() {
        HashEmbedder::new(0);
    }

    #[test]
    fn similar_inputs_correlate() {
        let emb = HashEmbedder::new(256);
        let v1 = emb.embed("error in compilation step").unwrap();
        let v2 = emb.embed("compilation error detected").unwrap();
        let v3 = emb.embed("the quick brown fox").unwrap();
        let dot12: f32 = v1.iter().zip(&v2).map(|(a, b)| a * b).sum();
        let dot13: f32 = v1.iter().zip(&v3).map(|(a, b)| a * b).sum();
        assert!(
            dot12 > dot13,
            "similar={} should > dissimilar={}",
            dot12,
            dot13
        );
    }

    #[test]
    fn unicode_input() {
        let emb = HashEmbedder::new(64);
        let v = emb.embed("こんにちは世界").unwrap();
        assert_eq!(v.len(), 64);
    }

    #[test]
    fn single_char_input() {
        let emb = HashEmbedder::new(64);
        // single char is shorter than ngram_range.0=3, so all zeros
        let v = emb.embed("a").unwrap();
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn info_name_format() {
        let emb = HashEmbedder::new(512);
        let info = emb.info();
        assert_eq!(info.name, "fnv1a-hash-512");
        assert_eq!(info.tier, EmbedderTier::Hash);
    }

    // ====================================================================
    // fnv1a edge cases
    // ====================================================================

    #[test]
    fn fnv1a_empty_returns_offset_basis() {
        assert_eq!(fnv1a(b""), FNV_OFFSET);
    }

    #[test]
    fn fnv1a_single_byte() {
        let h = fnv1a(b"a");
        assert_ne!(h, FNV_OFFSET); // hashing a byte changes the result
        assert_ne!(h, 0);
    }

    #[test]
    fn fnv1a_different_bytes_differ() {
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
        assert_ne!(fnv1a(b"ab"), fnv1a(b"ba")); // order matters
    }

    #[test]
    fn fnv1a_deterministic() {
        assert_eq!(fnv1a(b"hello"), fnv1a(b"hello"));
        assert_eq!(fnv1a(b"test data"), fnv1a(b"test data"));
    }

    #[test]
    fn fnv1a_long_input() {
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let h = fnv1a(&data);
        assert_ne!(h, FNV_OFFSET);
        // Should be deterministic
        assert_eq!(h, fnv1a(&data));
    }

    #[test]
    fn fnv1a_null_byte() {
        let h = fnv1a(&[0]);
        assert_ne!(h, FNV_OFFSET);
    }

    #[test]
    fn fnv1a_all_same_bytes() {
        let all_a = fnv1a(&[b'a'; 10]);
        let all_b = fnv1a(&[b'b'; 10]);
        assert_ne!(all_a, all_b);
    }

    // ====================================================================
    // l2_normalize edge cases
    // ====================================================================

    #[test]
    fn l2_normalize_zero_vector_unchanged() {
        let mut v = vec![0.0f32; 4];
        let norm = l2_normalize(&mut v);
        assert!(norm <= f32::EPSILON);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn l2_normalize_already_unit() {
        let mut v = vec![1.0, 0.0, 0.0];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 0.001);
        assert!(v[1].abs() < 0.001);
        assert!(v[2].abs() < 0.001);
    }

    #[test]
    fn l2_normalize_returns_original_norm() {
        let mut v = vec![3.0, 4.0];
        let norm = l2_normalize(&mut v);
        assert!((norm - 5.0).abs() < 0.001);
    }

    #[test]
    fn l2_normalize_single_element() {
        let mut v = vec![5.0f32];
        l2_normalize(&mut v);
        assert!((v[0] - 1.0).abs() < 0.001);
    }

    #[test]
    fn l2_normalize_negative_values() {
        let mut v = vec![-3.0, 4.0];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.001);
        assert!(v[0] < 0.0); // sign preserved
    }

    #[test]
    fn l2_normalize_very_small_values() {
        // Values so small the norm is near epsilon
        let mut v = vec![f32::EPSILON * 0.1, 0.0];
        let norm = l2_normalize(&mut v);
        // Norm is below EPSILON, so vector should be unchanged
        assert!(norm <= f32::EPSILON);
    }

    #[test]
    fn l2_normalize_large_values() {
        let mut v = vec![1e10, 1e10, 1e10];
        l2_normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01);
    }

    // ====================================================================
    // HashEmbedder configuration tests
    // ====================================================================

    #[test]
    fn hash_embedder_debug() {
        let emb = HashEmbedder::new(64);
        let dbg = format!("{emb:?}");
        assert!(dbg.contains("HashEmbedder"));
        assert!(dbg.contains("64"));
    }

    #[test]
    fn hash_embedder_clone() {
        let emb = HashEmbedder::new(64).with_ngram_range(2, 5);
        let emb2 = emb.clone();
        // Both should produce the same embedding
        assert_eq!(
            emb.embed("test").unwrap(),
            emb2.embed("test").unwrap()
        );
    }

    #[test]
    fn hash_embedder_default_ngram_range() {
        let emb = HashEmbedder::new(64);
        // Default ngram range is (3, 4)
        // Input shorter than 3 chars should produce all zeros
        let v = emb.embed("ab").unwrap();
        assert!(v.iter().all(|&x| x == 0.0));
        // Input of exactly 3 chars should produce non-zero
        let v = emb.embed("abc").unwrap();
        assert!(v.iter().any(|&x| x != 0.0));
    }

    #[test]
    fn hash_embedder_custom_ngram_min_1() {
        let emb = HashEmbedder::new(64).with_ngram_range(1, 2);
        // Single char should now produce non-zero
        let v = emb.embed("a").unwrap();
        assert!(v.iter().any(|&x| x != 0.0));
    }

    #[test]
    #[should_panic]
    fn hash_embedder_ngram_min_zero_panics() {
        let _ = HashEmbedder::new(64).with_ngram_range(0, 2);
    }

    #[test]
    #[should_panic]
    fn hash_embedder_ngram_min_exceeds_max_panics() {
        let _ = HashEmbedder::new(64).with_ngram_range(5, 3);
    }

    // ====================================================================
    // Embedding property tests
    // ====================================================================

    #[test]
    fn embedding_dimension_matches_config() {
        for dim in [16, 32, 64, 128, 256, 512] {
            let emb = HashEmbedder::new(dim);
            let v = emb.embed("test input").unwrap();
            assert_eq!(v.len(), dim);
        }
    }

    #[test]
    fn embedding_unit_norm_for_nonempty() {
        let emb = HashEmbedder::new(128);
        for text in ["hello", "world", "a longer test string here", "abc"] {
            let v = emb.embed(text).unwrap();
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 0.01,
                "norm for '{}' was {}",
                text,
                norm
            );
        }
    }

    #[test]
    fn embedding_all_zeros_for_too_short_input() {
        let emb = HashEmbedder::new(64); // default ngram (3,4)
        // Inputs shorter than min ngram produce zero vectors
        for text in ["", "a", "ab"] {
            let v = emb.embed(text).unwrap();
            assert!(
                v.iter().all(|&x| x == 0.0),
                "expected all zeros for '{}'",
                text
            );
        }
    }

    #[test]
    fn embedding_whitespace_only() {
        let emb = HashEmbedder::new(64);
        let v = emb.embed("   ").unwrap();
        // 3 spaces = one trigram, so should produce non-zero
        assert!(v.iter().any(|&x| x != 0.0));
    }

    #[test]
    fn embedding_special_characters() {
        let emb = HashEmbedder::new(64);
        let v = emb.embed("!@#$%^&*()").unwrap();
        assert_eq!(v.len(), 64);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01);
    }

    #[test]
    fn embedding_newlines_and_tabs() {
        let emb = HashEmbedder::new(64);
        let v = emb.embed("line1\nline2\ttab").unwrap();
        assert_eq!(v.len(), 64);
    }

    #[test]
    fn embedding_repeated_text() {
        let emb = HashEmbedder::new(128);
        let v1 = emb.embed("aaa").unwrap();
        let v2 = emb.embed("aaaaaa").unwrap();
        // Different lengths should produce different embeddings
        assert_ne!(v1, v2);
    }

    #[test]
    fn small_dimension_still_works() {
        let emb = HashEmbedder::new(1);
        let v = emb.embed("test input").unwrap();
        assert_eq!(v.len(), 1);
        // All ngrams hash to bucket 0, so should be normalized
        assert!((v[0].abs() - 1.0).abs() < 0.01);
    }

    #[test]
    fn info_dimension_matches() {
        let emb = HashEmbedder::new(256);
        assert_eq!(emb.info().dimension, 256);
        assert_eq!(emb.dimension(), 256);
    }

    #[test]
    fn batch_embed_consistency() {
        let emb = HashEmbedder::new(64);
        let texts = ["hello", "world", "test"];
        let batch = emb.embed_batch(&texts).unwrap();
        for (i, text) in texts.iter().enumerate() {
            let single = emb.embed(text).unwrap();
            assert_eq!(batch[i], single, "batch vs single mismatch for '{}'", text);
        }
    }

    #[test]
    fn batch_embed_empty_list() {
        let emb = HashEmbedder::new(64);
        let batch = emb.embed_batch(&[] as &[&str]).unwrap();
        assert!(batch.is_empty());
    }
}
