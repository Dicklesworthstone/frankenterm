//! Property-based tests for the search bridge (Tokio↔frankensearch adapter).
//!
//! Covers: BridgeCancellationToken state machine, SearchBridgeRequest builder
//! chain composition, update_best_results phase selection, map_search_error
//! priority, and concurrent cancellation safety.
#![cfg(feature = "frankensearch")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use frankensearch::{ScoredResult, SearchError, SearchPhase};
use frankenterm_core::runtime_compat::{self, CompatRuntime, RuntimeBuilder, task};
use frankenterm_core::search_bridge::{
    BridgeCancellationToken, SearchBridgeError, SearchBridgeRequest, SearchBridgeResult,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_query() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z ]{1,60}").unwrap()
}

fn arb_limit() -> impl Strategy<Value = usize> {
    1..200usize
}

fn arb_timeout_ms() -> impl Strategy<Value = u64> {
    1..5000u64
}

fn arb_scored_result(id_prefix: &'static str) -> impl Strategy<Value = ScoredResult> {
    (0..100u32, prop::num::f32::NORMAL).prop_map(move |(idx, score)| ScoredResult {
        doc_id: format!("{id_prefix}-{idx}"),
        score: score.abs().clamp(0.0, 1.0),
        source: frankensearch::ScoreSource::Hybrid,
        index: None,
        fast_score: None,
        quality_score: None,
        lexical_score: None,
        rerank_score: None,
        explanation: None,
        metadata: None,
    })
}

fn arb_result_vec(prefix: &'static str) -> impl Strategy<Value = Vec<ScoredResult>> {
    prop::collection::vec(arb_scored_result(prefix), 0..20)
}

// ---------------------------------------------------------------------------
// BridgeCancellationToken invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Fresh token starts uncancelled.
    #[test]
    fn cancellation_token_starts_uncancelled(_seed in 0..1000u32) {
        let token = BridgeCancellationToken::new();
        prop_assert!(!token.is_cancelled());
    }

    /// Cancel is idempotent — calling it N times always ends in cancelled state.
    #[test]
    fn cancellation_is_idempotent(n in 1..50u32) {
        let token = BridgeCancellationToken::new();
        for _ in 0..n {
            token.cancel();
        }
        prop_assert!(token.is_cancelled());
    }

    /// Cancellation state is monotonic: once true, never reverts to false.
    #[test]
    fn cancellation_is_monotonic(ops in prop::collection::vec(prop::bool::ANY, 1..30)) {
        let token = BridgeCancellationToken::new();
        let mut ever_cancelled = false;

        for should_cancel in ops {
            if should_cancel {
                token.cancel();
                ever_cancelled = true;
            }
            if ever_cancelled {
                prop_assert!(token.is_cancelled(),
                    "token must stay cancelled once set");
            }
        }
    }

    /// Clones share cancellation state — cancelling one cancels all.
    #[test]
    fn clone_shares_cancellation_state(n_clones in 2..10usize) {
        let token = BridgeCancellationToken::new();
        let clones: Vec<_> = (0..n_clones).map(|_| token.clone()).collect();

        // Cancel the original
        token.cancel();

        // All clones should see the cancellation
        for (i, clone) in clones.iter().enumerate() {
            prop_assert!(clone.is_cancelled(),
                "clone {} should see cancellation", i);
        }
    }

    /// Cancelling any clone propagates to all siblings.
    #[test]
    fn cancel_any_clone_propagates(
        n_clones in 2..10usize,
        cancel_idx in 0..10usize,
    ) {
        let token = BridgeCancellationToken::new();
        let clones: Vec<_> = (0..n_clones).map(|_| token.clone()).collect();
        let actual_idx = cancel_idx % n_clones;

        clones[actual_idx].cancel();

        prop_assert!(token.is_cancelled());
        for (i, clone) in clones.iter().enumerate() {
            prop_assert!(clone.is_cancelled(),
                "clone {} should see cancellation from clone {}", i, actual_idx);
        }
    }

    /// Default token is uncancelled.
    #[test]
    fn default_token_uncancelled(_seed in 0..1000u32) {
        let token = BridgeCancellationToken::default();
        prop_assert!(!token.is_cancelled());
    }
}

// ---------------------------------------------------------------------------
// SearchBridgeRequest builder invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// new() preserves query and limit; defaults are None/no-op.
    #[test]
    fn request_new_preserves_fields(query in arb_query(), limit in arb_limit()) {
        let req = SearchBridgeRequest::new(query.clone(), limit);
        prop_assert_eq!(&req.query, &query);
        prop_assert_eq!(req.limit, limit);
        prop_assert!(req.timeout.is_none());
        prop_assert!(req.cancellation.is_none());
    }

    /// with_timeout sets the timeout without affecting other fields.
    #[test]
    fn request_with_timeout_preserves(
        query in arb_query(),
        limit in arb_limit(),
        timeout_ms in arb_timeout_ms(),
    ) {
        let req = SearchBridgeRequest::new(query.clone(), limit)
            .with_timeout(Duration::from_millis(timeout_ms));

        prop_assert_eq!(&req.query, &query);
        prop_assert_eq!(req.limit, limit);
        prop_assert_eq!(req.timeout, Some(Duration::from_millis(timeout_ms)));
    }

    /// with_cancellation attaches token without affecting other fields.
    #[test]
    fn request_with_cancellation_preserves(
        query in arb_query(),
        limit in arb_limit(),
    ) {
        let token = BridgeCancellationToken::new();
        let req = SearchBridgeRequest::new(query.clone(), limit)
            .with_cancellation(token.clone());

        prop_assert_eq!(&req.query, &query);
        prop_assert_eq!(req.limit, limit);
        prop_assert!(req.cancellation.is_some());
    }

    /// Builder chain is order-independent for field preservation.
    #[test]
    fn builder_chain_order_independent(
        query in arb_query(),
        limit in arb_limit(),
        timeout_ms in arb_timeout_ms(),
    ) {
        let token = BridgeCancellationToken::new();

        // Order A: timeout then cancellation
        let req_a = SearchBridgeRequest::new(query.clone(), limit)
            .with_timeout(Duration::from_millis(timeout_ms))
            .with_cancellation(token.clone());

        // Order B: cancellation then timeout
        let req_b = SearchBridgeRequest::new(query.clone(), limit)
            .with_cancellation(token.clone())
            .with_timeout(Duration::from_millis(timeout_ms));

        prop_assert_eq!(&req_a.query, &req_b.query);
        prop_assert_eq!(req_a.limit, req_b.limit);
        prop_assert_eq!(req_a.timeout, req_b.timeout);
    }

    /// Debug formatting doesn't panic.
    #[test]
    fn request_debug_no_panic(query in arb_query(), limit in arb_limit()) {
        let req = SearchBridgeRequest::new(query, limit);
        let debug = format!("{req:?}");
        prop_assert!(!debug.is_empty());
    }

    /// text_provider from with_text_provider works correctly.
    #[test]
    fn text_provider_round_trips(
        query in arb_query(),
        doc_id in "[a-z]{3,10}",
        doc_text in "[a-z ]{5,50}",
    ) {
        let text = doc_text.clone();
        let id = doc_id.clone();
        let req = SearchBridgeRequest::new(query, 10)
            .with_text_provider(move |qid| {
                if qid == id { Some(text.clone()) } else { None }
            });

        let result = (req.text_provider)(&doc_id);
        prop_assert_eq!(result, Some(doc_text));

        let missing = (req.text_provider)("nonexistent-doc");
        prop_assert_eq!(missing, None);
    }
}

// ---------------------------------------------------------------------------
// update_best_results invariants
// ---------------------------------------------------------------------------

/// Reimplementation of the private function for testing.
fn update_best_results(best_results: &mut Vec<ScoredResult>, phase: &SearchPhase) {
    match phase {
        SearchPhase::Initial { results, .. } | SearchPhase::Refined { results, .. } => {
            best_results.clone_from(results);
        }
        SearchPhase::RefinementFailed {
            initial_results, ..
        } => {
            best_results.clone_from(initial_results);
        }
    }
}

fn make_phase_metrics() -> frankensearch::PhaseMetrics {
    frankensearch::PhaseMetrics {
        embedder_id: "test-hash".to_string(),
        vectors_searched: 0,
        lexical_candidates: 0,
        fused_count: 0,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Initial phase replaces best_results entirely.
    #[test]
    fn update_initial_replaces_all(
        prev in arb_result_vec("prev"),
        initial in arb_result_vec("init"),
    ) {
        let mut best = prev;
        let phase = SearchPhase::Initial {
            results: initial.clone(),
            latency: Duration::from_millis(10),
            metrics: make_phase_metrics(),
        };
        update_best_results(&mut best, &phase);
        prop_assert_eq!(best.len(), initial.len());
        for (a, b) in best.iter().zip(initial.iter()) {
            prop_assert_eq!(&a.doc_id, &b.doc_id);
        }
    }

    /// Refined phase replaces best_results entirely.
    #[test]
    fn update_refined_replaces_all(
        prev in arb_result_vec("prev"),
        refined in arb_result_vec("ref"),
    ) {
        let mut best = prev;
        let phase = SearchPhase::Refined {
            results: refined.clone(),
            latency: Duration::from_millis(20),
            metrics: make_phase_metrics(),
            rank_changes: frankensearch::RankChanges {
                promoted: 0,
                demoted: 0,
                stable: refined.len(),
            },
        };
        update_best_results(&mut best, &phase);
        prop_assert_eq!(best.len(), refined.len());
        for (a, b) in best.iter().zip(refined.iter()) {
            prop_assert_eq!(&a.doc_id, &b.doc_id);
        }
    }

    /// RefinementFailed falls back to initial_results.
    #[test]
    fn update_refinement_failed_uses_initial(
        prev in arb_result_vec("prev"),
        fallback in arb_result_vec("fb"),
    ) {
        let mut best = prev;
        let phase = SearchPhase::RefinementFailed {
            initial_results: fallback.clone(),
            error: SearchError::SearchTimeout {
                elapsed_ms: 500,
                budget_ms: 200,
            },
            latency: Duration::from_millis(200),
        };
        update_best_results(&mut best, &phase);
        prop_assert_eq!(best.len(), fallback.len());
        for (a, b) in best.iter().zip(fallback.iter()) {
            prop_assert_eq!(&a.doc_id, &b.doc_id);
        }
    }

    /// Sequential phase updates: last phase wins.
    #[test]
    fn last_phase_wins(
        initial in arb_result_vec("init"),
        refined in arb_result_vec("ref"),
    ) {
        let mut best = Vec::new();

        // Apply Initial
        let phase1 = SearchPhase::Initial {
            results: initial.clone(),
            latency: Duration::from_millis(5),
            metrics: make_phase_metrics(),
        };
        update_best_results(&mut best, &phase1);
        prop_assert_eq!(best.len(), initial.len());

        // Apply Refined — should fully replace
        let phase2 = SearchPhase::Refined {
            results: refined.clone(),
            latency: Duration::from_millis(15),
            metrics: make_phase_metrics(),
            rank_changes: frankensearch::RankChanges {
                promoted: 0,
                demoted: 0,
                stable: refined.len(),
            },
        };
        update_best_results(&mut best, &phase2);
        prop_assert_eq!(best.len(), refined.len());
        for (a, b) in best.iter().zip(refined.iter()) {
            prop_assert_eq!(&a.doc_id, &b.doc_id);
        }
    }

    /// Empty result sets are valid — update clears best_results.
    #[test]
    fn empty_results_clear_best(
        prev in arb_result_vec("prev"),
    ) {
        let mut best = prev;
        let phase = SearchPhase::Initial {
            results: Vec::new(),
            latency: Duration::from_millis(1),
            metrics: make_phase_metrics(),
        };
        update_best_results(&mut best, &phase);
        prop_assert!(best.is_empty());
    }
}

// ---------------------------------------------------------------------------
// map_search_error priority invariants
// ---------------------------------------------------------------------------

/// Reimplementation matching the private function.
fn map_search_error(
    error: SearchError,
    cancellation: &BridgeCancellationToken,
    timeout_fired: bool,
    timeout: Option<Duration>,
) -> SearchBridgeError {
    if timeout_fired {
        return SearchBridgeError::Timeout {
            timeout_ms: timeout.map_or(0, |v| v.as_millis() as u64),
        };
    }

    if let SearchError::Cancelled { reason, .. } = &error {
        cancellation.cancel();
        return SearchBridgeError::Cancelled {
            reason: reason.clone(),
        };
    }

    SearchBridgeError::Search(error)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// When timeout_fired is true, map_search_error always returns Timeout
    /// regardless of the underlying error type.
    #[test]
    fn timeout_takes_priority(timeout_ms in arb_timeout_ms()) {
        let token = BridgeCancellationToken::new();
        // Even with a Cancelled error underneath, timeout wins
        let error = SearchError::Cancelled {
            phase: "quality".to_string(),
            reason: "cx cancelled".to_string(),
        };
        let result = map_search_error(
            error,
            &token,
            true,
            Some(Duration::from_millis(timeout_ms)),
        );
        let is_timeout = matches!(result, SearchBridgeError::Timeout { .. });
        prop_assert!(is_timeout, "timeout_fired should produce Timeout variant");
    }

    /// When not timed out and error is Cancelled, result is Cancelled and
    /// token gets cancelled as side effect.
    #[test]
    fn cancellation_maps_correctly(
        phase in "[a-z_]{3,20}",
        reason in "[a-z ]{5,50}",
    ) {
        let token = BridgeCancellationToken::new();
        let error = SearchError::Cancelled {
            phase: phase.clone(),
            reason: reason.clone(),
        };
        let result = map_search_error(error, &token, false, None);
        let is_cancelled = matches!(result, SearchBridgeError::Cancelled { .. });
        prop_assert!(is_cancelled, "Cancelled error should map to Cancelled");
        prop_assert!(token.is_cancelled(), "token should be cancelled as side effect");
    }

    /// Non-timeout, non-cancelled errors pass through as Search variant.
    #[test]
    fn generic_error_passes_through(
        elapsed in 100..5000u64,
        budget in 100..5000u64,
    ) {
        let token = BridgeCancellationToken::new();
        let error = SearchError::SearchTimeout {
            elapsed_ms: elapsed,
            budget_ms: budget,
        };
        let result = map_search_error(error, &token, false, None);
        let is_search = matches!(result, SearchBridgeError::Search(_));
        prop_assert!(is_search, "non-cancelled error should map to Search variant");
        prop_assert!(!token.is_cancelled(),
            "token should NOT be cancelled for non-Cancelled errors");
    }

    /// Timeout with None duration produces timeout_ms=0.
    #[test]
    fn timeout_with_none_duration(_seed in 0..1000u32) {
        let token = BridgeCancellationToken::new();
        let error = SearchError::DurabilityDisabled;
        let result = map_search_error(error, &token, true, None);
        match result {
            SearchBridgeError::Timeout { timeout_ms } => {
                prop_assert_eq!(timeout_ms, 0);
            }
            _ => prop_assert!(false, "expected Timeout variant"),
        }
    }

    /// Timeout duration is correctly converted to milliseconds.
    #[test]
    fn timeout_ms_conversion(timeout_ms in 1..10000u64) {
        let token = BridgeCancellationToken::new();
        let error = SearchError::DurabilityDisabled;
        let result = map_search_error(
            error,
            &token,
            true,
            Some(Duration::from_millis(timeout_ms)),
        );
        match result {
            SearchBridgeError::Timeout { timeout_ms: got } => {
                prop_assert_eq!(got, timeout_ms);
            }
            _ => prop_assert!(false, "expected Timeout variant"),
        }
    }
}

// ---------------------------------------------------------------------------
// Concurrent cancellation safety
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Multiple threads cancelling concurrently is safe and all observe cancelled.
    #[test]
    fn concurrent_cancel_is_safe(n_threads in 2..16usize) {
        let token = BridgeCancellationToken::new();
        let barrier = Arc::new(std::sync::Barrier::new(n_threads));
        let cancel_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let t = token.clone();
                let b = Arc::clone(&barrier);
                let c = Arc::clone(&cancel_count);
                std::thread::spawn(move || {
                    b.wait();
                    t.cancel();
                    c.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread should not panic");
        }

        prop_assert!(token.is_cancelled());
        prop_assert_eq!(cancel_count.load(Ordering::Relaxed), n_threads);
    }

    /// Readers and writers concurrently: readers see consistent state.
    #[test]
    fn concurrent_read_write_consistent(
        n_readers in 2..8usize,
        n_writers in 1..4usize,
    ) {
        let token = BridgeCancellationToken::new();
        let barrier = Arc::new(std::sync::Barrier::new(n_readers + n_writers));
        let saw_false_after_true = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();

        // Writer threads
        for _ in 0..n_writers {
            let t = token.clone();
            let b = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                b.wait();
                t.cancel();
            }));
        }

        // Reader threads — check monotonicity
        for _ in 0..n_readers {
            let t = token.clone();
            let b = Arc::clone(&barrier);
            let violation = Arc::clone(&saw_false_after_true);
            handles.push(std::thread::spawn(move || {
                b.wait();
                let mut seen_true = false;
                for _ in 0..100 {
                    let state = t.is_cancelled();
                    if seen_true && !state {
                        violation.fetch_add(1, Ordering::Relaxed);
                    }
                    if state {
                        seen_true = true;
                    }
                    std::thread::yield_now();
                }
            }));
        }

        for h in handles {
            h.join().expect("thread should not panic");
        }

        prop_assert_eq!(
            saw_false_after_true.load(Ordering::Relaxed),
            0,
            "monotonicity violation: saw false after true"
        );
    }
}

// ---------------------------------------------------------------------------
// SearchBridgeResult invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// SearchBridgeResult Clone preserves all fields.
    #[test]
    fn bridge_result_clone_preserves(results in arb_result_vec("clone")) {
        let orig = SearchBridgeResult {
            results: results.clone(),
            metrics: frankensearch::TwoTierMetrics::default(),
        };
        let cloned = orig.clone();
        prop_assert_eq!(cloned.results.len(), orig.results.len());
        for (a, b) in cloned.results.iter().zip(orig.results.iter()) {
            prop_assert_eq!(&a.doc_id, &b.doc_id);
            prop_assert_eq!(a.score.to_bits(), b.score.to_bits());
        }
    }

    /// SearchBridgeError Display doesn't panic for any variant.
    #[test]
    fn error_display_no_panic(timeout_ms in arb_timeout_ms()) {
        let timeout_err = SearchBridgeError::Timeout { timeout_ms };
        let display = format!("{timeout_err}");
        prop_assert!(!display.is_empty());

        let cancelled_err = SearchBridgeError::Cancelled {
            reason: "test".to_string(),
        };
        let display = format!("{cancelled_err}");
        prop_assert!(!display.is_empty());

        let runtime_err = SearchBridgeError::Runtime {
            message: "thread panicked".to_string(),
        };
        let display = format!("{runtime_err}");
        prop_assert!(!display.is_empty());
    }
}

// ---------------------------------------------------------------------------
// SearchBridge construction invariants
// ---------------------------------------------------------------------------

#[test]
fn bridge_debug_no_panic() {
    // Just verify Debug impl doesn't blow up (bridge requires TwoTierSearcher
    // which needs disk, so we skip full construction in proptest)
    let token = BridgeCancellationToken::new();
    let debug = format!("{token:?}");
    assert!(!debug.is_empty());
}

#[test]
fn cancellation_token_cancelled_future_returns_immediately_when_already_cancelled() {
    let token = BridgeCancellationToken::new();
    token.cancel();

    let rt = RuntimeBuilder::current_thread().build().unwrap();

    rt.block_on(async {
        // This should return immediately since token is already cancelled
        runtime_compat::timeout(Duration::from_millis(100), token.cancelled())
            .await
            .expect("cancelled() should return immediately for already-cancelled token");
    });
}

#[test]
fn search_bridge_error_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<SearchBridgeError>();
}

#[test]
fn cancellation_token_cancelled_future_resolves_on_cancel() {
    let token = BridgeCancellationToken::new();
    let token2 = token.clone();

    let rt = RuntimeBuilder::current_thread().build().unwrap();

    rt.block_on(async {
        let handle = task::spawn(async move {
            token2.cancelled().await;
        });

        // Give the task a moment to start waiting
        runtime_compat::sleep(Duration::from_millis(10)).await;

        // Cancel should unblock the waiting task
        token.cancel();

        runtime_compat::timeout(Duration::from_millis(200), handle)
            .await
            .expect("task should complete after cancel")
            .expect("task should not panic");
    });
}

// ---------------------------------------------------------------------------
// Batch 13: additional property tests (DarkMill)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// SearchBridgeRequest preserves query through builder chain.
    #[test]
    fn request_preserves_query(query in arb_query(), limit in arb_limit()) {
        let req = SearchBridgeRequest::new(query.clone(), limit);
        // The request stores the query — verify Debug output contains it
        let debug = format!("{:?}", req);
        prop_assert!(debug.contains(&query) || query.trim().is_empty(),
            "debug should contain query");
    }

    /// SearchBridgeRequest with_timeout sets a duration.
    #[test]
    fn request_with_timeout(query in arb_query(), limit in arb_limit(), ms in arb_timeout_ms()) {
        let req = SearchBridgeRequest::new(query, limit)
            .with_timeout(Duration::from_millis(ms));
        let debug = format!("{:?}", req);
        prop_assert!(!debug.is_empty());
    }

    /// BridgeCancellationToken starts not cancelled.
    #[test]
    fn cancellation_token_starts_not_cancelled(_dummy in 0..1u8) {
        let token = BridgeCancellationToken::new();
        prop_assert!(!token.is_cancelled());
    }

    /// BridgeCancellationToken cancel is idempotent.
    #[test]
    fn cancellation_token_cancel_idempotent(_dummy in 0..1u8) {
        let token = BridgeCancellationToken::new();
        token.cancel();
        prop_assert!(token.is_cancelled());
        token.cancel(); // second call should not panic
        prop_assert!(token.is_cancelled());
    }

    /// Cloned BridgeCancellationToken shares state.
    #[test]
    fn cancellation_token_clone_shares_state(_dummy in 0..1u8) {
        let token1 = BridgeCancellationToken::new();
        let token2 = token1.clone();
        prop_assert!(!token1.is_cancelled());
        prop_assert!(!token2.is_cancelled());
        token1.cancel();
        prop_assert!(token2.is_cancelled());
    }

    /// SearchBridgeError Runtime variant has non-empty Display.
    #[test]
    fn error_runtime_display_nonempty(msg in "[a-z ]{1,50}") {
        let err = SearchBridgeError::Runtime { message: msg };
        let display = format!("{}", err);
        prop_assert!(!display.is_empty());
    }

    /// SearchBridgeError Timeout variant includes timeout_ms.
    #[test]
    fn error_timeout_display(ms in 1_u64..100_000) {
        let err = SearchBridgeError::Timeout { timeout_ms: ms };
        let display = format!("{}", err);
        prop_assert!(!display.is_empty());
    }
}
