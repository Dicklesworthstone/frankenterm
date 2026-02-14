//! Property-based tests for the reranker module.
//!
//! Tests PassthroughReranker behavior, ScoredDoc invariants, RerankError display/debug,
//! query independence, and scale properties.

use frankenterm_core::search::{PassthroughReranker, RerankError, Reranker, ScoredDoc};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategy helpers
// ---------------------------------------------------------------------------

fn arb_scored_doc() -> impl Strategy<Value = ScoredDoc> {
    (0u64..1000, "[a-z]{0,50}", -100.0f32..100.0f32).prop_map(|(id, text, score)| ScoredDoc {
        id,
        text,
        score,
    })
}

fn arb_scored_docs(n: usize) -> impl Strategy<Value = Vec<ScoredDoc>> {
    proptest::collection::vec(arb_scored_doc(), n)
}

fn arb_query() -> impl Strategy<Value = String> {
    "[a-z ]{0,100}"
}

// ---------------------------------------------------------------------------
// Group 1: PassthroughReranker behavior
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // 1. Empty docs -> Err(EmptyInput)
    #[test]
    fn passthrough_empty_docs_returns_err(query in arb_query()) {
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, vec![]);
        prop_assert!(result.is_err(), "empty docs must return Err");
        match result.unwrap_err() {
            RerankError::EmptyInput => {} // expected
            other @ RerankError::ModelError(_) => prop_assert!(false, "expected EmptyInput, got {:?}", other),
        }
    }

    // 2. Non-empty docs -> Ok with same length
    #[test]
    fn passthrough_preserves_length(
        query in arb_query(),
        docs in proptest::collection::vec(arb_scored_doc(), 1..50)
    ) {
        let input_len = docs.len();
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, docs).unwrap();
        prop_assert_eq!(result.len(), input_len, "length mismatch: {} vs {}", result.len(), input_len);
    }

    // 3. Non-empty docs -> preserves all document IDs in same order
    #[test]
    fn passthrough_preserves_id_order(
        query in arb_query(),
        docs in proptest::collection::vec(arb_scored_doc(), 1..50)
    ) {
        let expected_ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, docs).unwrap();
        let result_ids: Vec<u64> = result.iter().map(|d| d.id).collect();
        prop_assert_eq!(&result_ids, &expected_ids, "IDs differ");
    }

    // 4. Non-empty docs -> preserves all scores exactly (f32 bit-exact)
    #[test]
    fn passthrough_preserves_scores_bitexact(
        query in arb_query(),
        docs in proptest::collection::vec(arb_scored_doc(), 1..50)
    ) {
        let expected_bits: Vec<u32> = docs.iter().map(|d| d.score.to_bits()).collect();
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, docs).unwrap();
        for (i, doc) in result.iter().enumerate() {
            prop_assert_eq!(
                doc.score.to_bits(),
                expected_bits[i],
                "score bits differ at index {}", i
            );
        }
    }

    // 5. Non-empty docs -> preserves all text exactly
    #[test]
    fn passthrough_preserves_text(
        query in arb_query(),
        docs in proptest::collection::vec(arb_scored_doc(), 1..50)
    ) {
        let expected_texts: Vec<String> = docs.iter().map(|d| d.text.clone()).collect();
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, docs).unwrap();
        for (i, doc) in result.iter().enumerate() {
            prop_assert_eq!(&doc.text, &expected_texts[i], "text differs at index {}", i);
        }
    }

    // 6. Single doc -> returns it unchanged
    #[test]
    fn passthrough_single_doc_unchanged(query in arb_query(), doc in arb_scored_doc()) {
        let id = doc.id;
        let text = doc.text.clone();
        let score_bits = doc.score.to_bits();
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, vec![doc]).unwrap();
        prop_assert_eq!(result.len(), 1, "expected single doc");
        prop_assert_eq!(result[0].id, id, "id mismatch");
        prop_assert_eq!(&result[0].text, &text, "text mismatch");
        prop_assert_eq!(result[0].score.to_bits(), score_bits, "score bits mismatch");
    }

    // 7. Result is deterministic (same input -> same output)
    #[test]
    fn passthrough_deterministic(
        query in arb_query(),
        docs in proptest::collection::vec(arb_scored_doc(), 1..20)
    ) {
        let reranker = PassthroughReranker;
        // Clone the docs for a second call
        let docs2: Vec<ScoredDoc> = docs.to_vec();
        let r1 = reranker.rerank(&query, docs).unwrap();
        let r2 = reranker.rerank(&query, docs2).unwrap();
        prop_assert_eq!(r1.len(), r2.len(), "lengths differ between runs");
        for (i, (a, b)) in r1.iter().zip(r2.iter()).enumerate() {
            prop_assert_eq!(a.id, b.id, "id differs at index {}", i);
            prop_assert_eq!(&a.text, &b.text, "text differs at index {}", i);
            prop_assert_eq!(a.score.to_bits(), b.score.to_bits(), "score differs at index {}", i);
        }
    }
}

// ---------------------------------------------------------------------------
// Group 2: ScoredDoc
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // 8. Clone preserves id
    #[test]
    fn scored_doc_clone_preserves_id(doc in arb_scored_doc()) {
        let cloned = doc.clone();
        prop_assert_eq!(cloned.id, doc.id, "clone id mismatch");
    }

    // 9. Clone preserves text
    #[test]
    fn scored_doc_clone_preserves_text(doc in arb_scored_doc()) {
        let cloned = doc.clone();
        prop_assert_eq!(&cloned.text, &doc.text, "clone text mismatch");
    }

    // 10. Clone preserves score (bit-exact)
    #[test]
    fn scored_doc_clone_preserves_score_bitexact(doc in arb_scored_doc()) {
        let cloned = doc.clone();
        prop_assert_eq!(
            cloned.score.to_bits(),
            doc.score.to_bits(),
            "clone score bits mismatch"
        );
    }

    // 11. Debug format is non-empty
    #[test]
    fn scored_doc_debug_non_empty(doc in arb_scored_doc()) {
        let debug = format!("{:?}", doc);
        prop_assert!(!debug.is_empty(), "debug output must not be empty");
    }
}

// ---------------------------------------------------------------------------
// Group 3: RerankError
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // 12. Display for ModelError contains the message
    #[test]
    fn rerank_error_model_display_contains_message(msg in "[a-z]{1,50}") {
        let err = RerankError::ModelError(msg.clone());
        let display = format!("{}", err);
        prop_assert!(
            display.contains(&msg),
            "display '{}' must contain '{}'", display, msg
        );
    }

    // 13. Display for EmptyInput contains "empty"
    #[test]
    fn rerank_error_empty_input_display_contains_empty(_dummy in 0..1u8) {
        let err = RerankError::EmptyInput;
        let display = format!("{}", err);
        prop_assert!(
            display.contains("empty"),
            "display '{}' must contain 'empty'", display
        );
    }

    // 14. ModelError is Debug-formatted non-empty
    #[test]
    fn rerank_error_model_debug_non_empty(msg in "[a-z]{1,50}") {
        let err = RerankError::ModelError(msg);
        let debug = format!("{:?}", err);
        prop_assert!(!debug.is_empty(), "debug output must not be empty");
    }

    // 15. EmptyInput is Debug-formatted non-empty
    #[test]
    fn rerank_error_empty_input_debug_non_empty(_dummy in 0..1u8) {
        let err = RerankError::EmptyInput;
        let debug = format!("{:?}", err);
        prop_assert!(!debug.is_empty(), "debug output must not be empty");
    }

    // 16. RerankError implements std::error::Error
    #[test]
    fn rerank_error_implements_error_trait(msg in "[a-z]{0,50}") {
        let err_model: &dyn std::error::Error = &RerankError::ModelError(msg);
        let err_empty: &dyn std::error::Error = &RerankError::EmptyInput;
        // Verify we can call Error methods
        let _ = err_model.to_string();
        let _ = err_empty.to_string();
        prop_assert!(err_model.source().is_none(), "ModelError should have no source");
        prop_assert!(err_empty.source().is_none(), "EmptyInput should have no source");
    }
}

// ---------------------------------------------------------------------------
// Group 4: Query independence
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // 17. Different query strings don't affect passthrough output
    #[test]
    fn passthrough_query_independent(
        query_a in arb_query(),
        query_b in arb_query(),
        docs in proptest::collection::vec(arb_scored_doc(), 1..20)
    ) {
        let reranker = PassthroughReranker;
        let docs_a: Vec<ScoredDoc> = docs.to_vec();
        let docs_b: Vec<ScoredDoc> = docs.to_vec();
        let r_a = reranker.rerank(&query_a, docs_a).unwrap();
        let r_b = reranker.rerank(&query_b, docs_b).unwrap();
        prop_assert_eq!(r_a.len(), r_b.len(), "lengths differ for different queries");
        for (i, (a, b)) in r_a.iter().zip(r_b.iter()).enumerate() {
            prop_assert_eq!(a.id, b.id, "id differs at index {} for different queries", i);
            prop_assert_eq!(&a.text, &b.text, "text differs at index {} for different queries", i);
            prop_assert_eq!(
                a.score.to_bits(),
                b.score.to_bits(),
                "score differs at index {} for different queries", i
            );
        }
    }

    // 18. Empty query string works fine
    #[test]
    fn passthrough_empty_query_works(docs in proptest::collection::vec(arb_scored_doc(), 1..20)) {
        let input_len = docs.len();
        let reranker = PassthroughReranker;
        let result = reranker.rerank("", docs).unwrap();
        prop_assert_eq!(result.len(), input_len, "empty query should not affect doc count");
    }
}

// ---------------------------------------------------------------------------
// Group 5: Scale
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 19. Large doc vector (100 items) preserves all
    #[test]
    fn passthrough_large_vector_preserves_all(
        query in arb_query(),
        docs in arb_scored_docs(100)
    ) {
        let expected_ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
        let expected_texts: Vec<String> = docs.iter().map(|d| d.text.clone()).collect();
        let expected_scores: Vec<u32> = docs.iter().map(|d| d.score.to_bits()).collect();
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, docs).unwrap();
        prop_assert_eq!(result.len(), 100, "expected 100 docs");
        for (i, doc) in result.iter().enumerate() {
            prop_assert_eq!(doc.id, expected_ids[i], "id mismatch at index {}", i);
            prop_assert_eq!(&doc.text, &expected_texts[i], "text mismatch at index {}", i);
            prop_assert_eq!(
                doc.score.to_bits(),
                expected_scores[i],
                "score mismatch at index {}", i
            );
        }
    }

    // 20. Doc with empty text is preserved
    #[test]
    fn passthrough_preserves_empty_text_doc(query in arb_query(), id in 0u64..1000, score in -100.0f32..100.0f32) {
        let doc = ScoredDoc {
            id,
            text: String::new(),
            score,
        };
        let reranker = PassthroughReranker;
        let result = reranker.rerank(&query, vec![doc]).unwrap();
        prop_assert_eq!(result.len(), 1, "expected single doc");
        prop_assert_eq!(result[0].id, id, "id mismatch for empty-text doc");
        prop_assert!(result[0].text.is_empty(), "text should be empty");
        prop_assert_eq!(result[0].score.to_bits(), score.to_bits(), "score bits mismatch for empty-text doc");
    }
}
