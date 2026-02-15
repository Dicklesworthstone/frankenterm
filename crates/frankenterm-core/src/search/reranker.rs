//! Reranker trait and implementations.

use std::fmt;

#[derive(Debug)]
pub enum RerankError {
    ModelError(String),
    EmptyInput,
}

impl fmt::Display for RerankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelError(e) => write!(f, "rerank model error: {e}"),
            Self::EmptyInput => write!(f, "empty input to reranker"),
        }
    }
}

impl std::error::Error for RerankError {}

/// Scored document for reranking.
#[derive(Debug, Clone)]
pub struct ScoredDoc {
    pub id: u64,
    pub text: String,
    pub score: f32,
}

/// Trait for reranking a set of candidate documents against a query.
pub trait Reranker: Send + Sync {
    fn rerank(&self, query: &str, docs: Vec<ScoredDoc>) -> Result<Vec<ScoredDoc>, RerankError>;
}

/// Passthrough reranker that returns documents unchanged (for testing/fallback).
#[allow(dead_code)]
pub struct PassthroughReranker;

impl Reranker for PassthroughReranker {
    fn rerank(&self, _query: &str, docs: Vec<ScoredDoc>) -> Result<Vec<ScoredDoc>, RerankError> {
        if docs.is_empty() {
            return Err(RerankError::EmptyInput);
        }
        Ok(docs)
    }
}

/// Cross-encoder reranker (requires semantic-search feature).
#[cfg(feature = "semantic-search")]
pub struct CrossEncoderReranker {
    model_path: String,
}

#[cfg(feature = "semantic-search")]
impl CrossEncoderReranker {
    pub fn new(model_path: impl Into<String>) -> Self {
        Self {
            model_path: model_path.into(),
        }
    }

    pub fn model_path(&self) -> &str {
        &self.model_path
    }
}

#[cfg(feature = "semantic-search")]
impl Reranker for CrossEncoderReranker {
    fn rerank(&self, _query: &str, docs: Vec<ScoredDoc>) -> Result<Vec<ScoredDoc>, RerankError> {
        if docs.is_empty() {
            return Err(RerankError::EmptyInput);
        }
        // Stub: would invoke cross-encoder model inference here
        Ok(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_returns_unchanged() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 1,
                text: "hello".into(),
                score: 0.9,
            },
            ScoredDoc {
                id: 2,
                text: "world".into(),
                score: 0.8,
            },
        ];
        let result = reranker.rerank("query", docs).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, 1);
    }

    #[test]
    fn passthrough_empty_errors() {
        let reranker = PassthroughReranker;
        let result = reranker.rerank("query", vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn rerank_error_display() {
        let e = RerankError::ModelError("oom".into());
        assert!(e.to_string().contains("oom"));
        let e = RerankError::EmptyInput;
        assert!(e.to_string().contains("empty"));
    }

    #[test]
    fn scored_doc_clone() {
        let doc = ScoredDoc {
            id: 42,
            text: "test".into(),
            score: 0.5,
        };
        let doc2 = doc.clone();
        assert_eq!(doc2.id, 42);
        assert_eq!(doc2.text, "test");
    }

    #[test]
    fn passthrough_preserves_scores() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 1,
                text: "a".into(),
                score: 0.1,
            },
            ScoredDoc {
                id: 2,
                text: "b".into(),
                score: 0.9,
            },
        ];
        let result = reranker.rerank("q", docs).unwrap();
        assert!((result[0].score - 0.1).abs() < f32::EPSILON);
        assert!((result[1].score - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn rerank_error_is_error_trait() {
        let e = RerankError::EmptyInput;
        // Verify it implements std::error::Error
        let _: &dyn std::error::Error = &e;
    }

    // =====================================================================
    // RerankError tests
    // =====================================================================

    #[test]
    fn rerank_error_model_error_display() {
        let e = RerankError::ModelError("out of memory".into());
        let msg = e.to_string();
        assert!(msg.contains("rerank model error"));
        assert!(msg.contains("out of memory"));
    }

    #[test]
    fn rerank_error_empty_input_display() {
        let e = RerankError::EmptyInput;
        assert_eq!(e.to_string(), "empty input to reranker");
    }

    #[test]
    fn rerank_error_debug() {
        let e = RerankError::ModelError("fail".into());
        let dbg = format!("{e:?}");
        assert!(dbg.contains("ModelError"));
        assert!(dbg.contains("fail"));

        let e = RerankError::EmptyInput;
        let dbg = format!("{e:?}");
        assert!(dbg.contains("EmptyInput"));
    }

    // =====================================================================
    // ScoredDoc tests
    // =====================================================================

    #[test]
    fn scored_doc_debug() {
        let doc = ScoredDoc {
            id: 1,
            text: "hello".into(),
            score: 0.9,
        };
        let dbg = format!("{doc:?}");
        assert!(dbg.contains("ScoredDoc"));
        assert!(dbg.contains("hello"));
    }

    #[test]
    fn scored_doc_clone_preserves_all_fields() {
        let doc = ScoredDoc {
            id: 999,
            text: "long text content here".into(),
            score: 0.12345,
        };
        let doc2 = doc.clone();
        assert_eq!(doc2.id, 999);
        assert_eq!(doc2.text, "long text content here");
        assert!((doc2.score - 0.12345).abs() < f32::EPSILON);
    }

    // =====================================================================
    // PassthroughReranker additional tests
    // =====================================================================

    #[test]
    fn passthrough_single_doc() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "only".into(),
            score: 1.0,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 1);
    }

    #[test]
    fn passthrough_preserves_order() {
        let reranker = PassthroughReranker;
        let docs = vec![
            ScoredDoc {
                id: 3,
                text: "third".into(),
                score: 0.3,
            },
            ScoredDoc {
                id: 1,
                text: "first".into(),
                score: 0.1,
            },
            ScoredDoc {
                id: 2,
                text: "second".into(),
                score: 0.2,
            },
        ];
        let result = reranker.rerank("query", docs).unwrap();
        assert_eq!(result[0].id, 3);
        assert_eq!(result[1].id, 1);
        assert_eq!(result[2].id, 2);
    }

    #[test]
    fn passthrough_large_batch() {
        let reranker = PassthroughReranker;
        let docs: Vec<ScoredDoc> = (0..100)
            .map(|i| ScoredDoc {
                id: i,
                text: format!("doc-{i}"),
                score: i as f32 / 100.0,
            })
            .collect();
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(result[0].id, 0);
        assert_eq!(result[99].id, 99);
    }

    #[test]
    fn passthrough_preserves_text_content() {
        let reranker = PassthroughReranker;
        let docs = vec![ScoredDoc {
            id: 1,
            text: "special chars: !@#$%^&*()".into(),
            score: 0.5,
        }];
        let result = reranker.rerank("q", docs).unwrap();
        assert_eq!(result[0].text, "special chars: !@#$%^&*()");
    }
}
