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
}
