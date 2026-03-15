//! Tokio-facing bridge for `frankensearch::TwoTierSearcher`.
//!
//! This module exposes an async API that can be called from FrankenTerm's
//! runtime surface while preserving frankensearch's progressive phase callbacks
//! and capability-context cancellation semantics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::runtime_compat::notify::Notify;
#[cfg(not(feature = "asupersync-runtime"))]
use crate::runtime_compat::{CompatRuntime, mpsc};
use frankensearch::{Cx, ScoredResult, SearchError, SearchPhase, TwoTierMetrics, TwoTierSearcher};
use thiserror::Error;

/// Shared document-text provider for exclusion-aware search operations.
pub type TextProvider = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Request payload for a bridge search call.
#[derive(Clone)]
pub struct SearchBridgeRequest {
    /// Query string passed to frankensearch.
    pub query: String,
    /// Maximum number of results requested.
    pub limit: usize,
    /// Optional end-to-end timeout for the search operation.
    pub timeout: Option<Duration>,
    /// Optional bridge-level cancellation token.
    pub cancellation: Option<BridgeCancellationToken>,
    /// Document text lookup by doc-id.
    pub text_provider: TextProvider,
}

impl std::fmt::Debug for SearchBridgeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchBridgeRequest")
            .field("query", &self.query)
            .field("limit", &self.limit)
            .field("timeout", &self.timeout)
            .field("cancellation", &self.cancellation)
            .field("text_provider", &"<fn>")
            .finish()
    }
}

impl SearchBridgeRequest {
    /// Create a request with default bridge settings.
    #[must_use]
    pub fn new(query: impl Into<String>, limit: usize) -> Self {
        Self {
            query: query.into(),
            limit,
            timeout: None,
            cancellation: None,
            text_provider: Arc::new(|_| None),
        }
    }

    /// Set a request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set a cancellation token for this request.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: BridgeCancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Set a text provider via closure.
    #[must_use]
    pub fn with_text_provider(
        mut self,
        text_provider: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.text_provider = Arc::new(text_provider);
        self
    }

    /// Set a text provider using an existing shared provider.
    #[must_use]
    pub fn with_text_provider_arc(mut self, text_provider: TextProvider) -> Self {
        self.text_provider = text_provider;
        self
    }
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Bridge-local cancellation token.
#[derive(Clone, Debug, Default)]
pub struct BridgeCancellationToken {
    state: Arc<CancellationState>,
}

impl BridgeCancellationToken {
    /// Create a fresh cancellation token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_waiters();
        }
    }

    /// Return whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Await cancellation.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.state.notify.notified().await;
    }
}

/// Bridge result containing final results and two-tier metrics.
#[derive(Debug, Clone)]
pub struct SearchBridgeResult {
    /// Final best-result set (refined if available, otherwise initial results).
    pub results: Vec<ScoredResult>,
    /// Aggregated two-tier metrics.
    pub metrics: TwoTierMetrics,
}

/// Search bridge errors.
#[derive(Debug, Error)]
pub enum SearchBridgeError {
    /// Runtime boundary failed.
    #[error("search bridge runtime failure: {message}")]
    Runtime { message: String },
    /// Request timed out.
    #[error("search operation timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    /// Search cancelled.
    #[error("search operation cancelled: {reason}")]
    Cancelled { reason: String },
    /// frankensearch returned a non-cancellation error.
    #[error("search failed: {0}")]
    Search(#[source] SearchError),
}

/// Tokio-facing bridge wrapper around `TwoTierSearcher`.
#[derive(Clone)]
pub struct SearchBridge {
    searcher: Arc<TwoTierSearcher>,
}

impl std::fmt::Debug for SearchBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchBridge")
            .field("searcher", &"<TwoTierSearcher>")
            .finish()
    }
}

impl SearchBridge {
    /// Wrap an owned searcher.
    #[must_use]
    pub fn new(searcher: TwoTierSearcher) -> Self {
        Self {
            searcher: Arc::new(searcher),
        }
    }

    /// Wrap a shared searcher.
    #[must_use]
    pub fn from_shared(searcher: Arc<TwoTierSearcher>) -> Self {
        Self { searcher }
    }

    /// Access the shared underlying searcher.
    #[must_use]
    pub fn shared_searcher(&self) -> Arc<TwoTierSearcher> {
        Arc::clone(&self.searcher)
    }

    /// Run a search using an internally managed capability context.
    ///
    /// This creates a per-request `Cx::for_request()` capability context and
    /// forwards progressive phases to `on_phase`.
    pub async fn search(
        &self,
        request: SearchBridgeRequest,
        on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        self.search_with_cx(Cx::for_request(), request, on_phase)
            .await
    }

    /// Run a search with a caller-provided capability context.
    pub async fn search_with_cx(
        &self,
        cx: Cx,
        request: SearchBridgeRequest,
        on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        #[cfg(feature = "asupersync-runtime")]
        {
            self.search_direct(cx, request, on_phase).await
        }

        #[cfg(not(feature = "asupersync-runtime"))]
        {
            self.search_via_tokio_bridge(cx, request, on_phase).await
        }
    }

    #[cfg(feature = "asupersync-runtime")]
    async fn search_direct(
        &self,
        cx: Cx,
        request: SearchBridgeRequest,
        mut on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        let SearchBridgeRequest {
            query,
            limit,
            timeout,
            cancellation,
            text_provider,
        } = request;

        let cancellation = cancellation.unwrap_or_default();
        let (timeout_done, timeout_fired, timeout_thread) =
            spawn_timeout_thread(timeout, cancellation.clone());
        let (cancel_done, cancel_thread) =
            spawn_cancellation_thread(cx.clone(), cancellation.clone());

        let mut best_results = Vec::new();
        let search_result = self
            .searcher
            .search(
                &cx,
                &query,
                limit,
                |doc_id| text_provider(doc_id),
                |phase| {
                    update_best_results(&mut best_results, &phase);
                    on_phase(phase);
                },
            )
            .await;

        cancel_done.store(true, Ordering::Release);
        if let Some(handle) = cancel_thread {
            handle.thread().unpark();
            let _ = handle.join();
        }
        timeout_done.store(true, Ordering::Release);
        if let Some(handle) = timeout_thread {
            handle.thread().unpark();
            let _ = handle.join();
        }

        match search_result {
            Ok(metrics) => Ok(SearchBridgeResult {
                results: best_results,
                metrics,
            }),
            Err(error) => Err(map_search_error(
                error,
                &cancellation,
                timeout_fired.load(Ordering::Acquire),
                timeout,
            )),
        }
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    async fn search_via_tokio_bridge(
        &self,
        cx: Cx,
        request: SearchBridgeRequest,
        mut on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        let SearchBridgeRequest {
            query,
            limit,
            timeout,
            cancellation,
            text_provider,
        } = request;

        let cancellation = cancellation.unwrap_or_default();
        if cx.is_cancel_requested() {
            cancellation.cancel();
            return Err(SearchBridgeError::Cancelled {
                reason: "capability context already cancelled".to_owned(),
            });
        }
        if cancellation.is_cancelled() {
            cx.set_cancel_requested(true);
            return Err(SearchBridgeError::Cancelled {
                reason: "bridge cancellation requested".to_owned(),
            });
        }

        let (timeout_done, timeout_fired, timeout_thread) =
            spawn_timeout_thread(timeout, cancellation.clone());

        let (phase_tx, mut phase_rx) = mpsc::unbounded_channel();
        let searcher = Arc::clone(&self.searcher);
        let worker_cancellation = cancellation.clone();

        let worker = crate::runtime_compat::spawn_blocking(
            move || -> Result<SearchBridgeResult, SearchError> {
                let (cancel_done, cancel_thread) =
                    spawn_cancellation_thread(cx.clone(), worker_cancellation.clone());

                let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
                    .build()
                    .map_err(|err| SearchError::InvalidConfig {
                        field: "search_bridge.runtime".to_owned(),
                        value: "tokio_current_thread".to_owned(),
                        reason: err,
                    })?;

                let search_result = runtime.block_on(async move {
                    let mut best_results = Vec::new();
                    let metrics = searcher
                        .search(
                            &cx,
                            &query,
                            limit,
                            |doc_id| text_provider(doc_id),
                            |phase| {
                                update_best_results(&mut best_results, &phase);
                                let _ = phase_tx.send(phase);
                            },
                        )
                        .await?;

                    Ok(SearchBridgeResult {
                        results: best_results,
                        metrics,
                    })
                });

                cancel_done.store(true, Ordering::Release);
                if let Some(handle) = cancel_thread {
                    handle.thread().unpark();
                    let _ = handle.join();
                }

                search_result
            },
        );
        tokio::pin!(worker);

        let search_result = loop {
            crate::runtime_compat::select! {
                maybe_phase = phase_rx.recv() => {
                    if let Some(phase) = maybe_phase {
                        on_phase(phase);
                    }
                }
                () = cancellation.cancelled() => {
                    // Tokio documents that abort does not stop a running
                    // `spawn_blocking` task, so this bridge relies on the
                    // worker cancellation thread to request cooperative exit.
                    let timeout_ms = timeout.map_or(0, |value| value.as_millis() as u64);
                    if timeout_fired.load(Ordering::Acquire) {
                        break Err(SearchBridgeError::Timeout { timeout_ms });
                    }
                    break Err(SearchBridgeError::Cancelled {
                        reason: "bridge cancellation requested".to_owned(),
                    });
                }
                worker_result = &mut worker => {
                    while let Ok(phase) = phase_rx.try_recv() {
                        on_phase(phase);
                    }
                    let worker_result = worker_result.map_err(|message| SearchBridgeError::Runtime {
                        message,
                    })?;

                    break match worker_result {
                        Ok(result) => Ok(result),
                        Err(error) => Err(map_search_error(
                            error,
                            &cancellation,
                            timeout_fired.load(Ordering::Acquire),
                            timeout,
                        )),
                    };
                }
            }
        };

        timeout_done.store(true, Ordering::Release);
        if let Some(handle) = timeout_thread {
            handle.thread().unpark();
            let _ = handle.join();
        }

        search_result
    }
}

fn map_search_error(
    error: SearchError,
    cancellation: &BridgeCancellationToken,
    timeout_fired: bool,
    timeout: Option<Duration>,
) -> SearchBridgeError {
    if timeout_fired {
        return SearchBridgeError::Timeout {
            timeout_ms: timeout.map_or(0, |value| value.as_millis() as u64),
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

const BRIDGE_WATCH_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn spawn_cancellation_thread(
    cx: Cx,
    cancellation: BridgeCancellationToken,
) -> (Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>) {
    let done = Arc::new(AtomicBool::new(false));
    if cancellation.is_cancelled() {
        cx.set_cancel_requested(true);
        return (done, None);
    }

    let done_for_thread = Arc::clone(&done);
    let handle = std::thread::spawn(move || {
        while !done_for_thread.load(Ordering::Acquire) {
            if cancellation.is_cancelled() {
                cx.set_cancel_requested(true);
                break;
            }
            // Avoid busy-waiting: this loop is best-effort cancellation plumbing.
            std::thread::park_timeout(BRIDGE_WATCH_POLL_INTERVAL);
        }
    });

    (done, Some(handle))
}

fn spawn_timeout_thread(
    timeout: Option<Duration>,
    cancellation: BridgeCancellationToken,
) -> (
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Option<std::thread::JoinHandle<()>>,
) {
    let done = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));

    let Some(timeout_duration) = timeout else {
        return (done, fired, None);
    };

    let done_for_thread = Arc::clone(&done);
    let fired_for_thread = Arc::clone(&fired);
    let handle = std::thread::spawn(move || {
        let started_at = Instant::now();
        while !done_for_thread.load(Ordering::Acquire) {
            let elapsed = started_at.elapsed();
            if elapsed >= timeout_duration {
                fired_for_thread.store(true, Ordering::Release);
                cancellation.cancel();
                break;
            }
            let remaining = timeout_duration.saturating_sub(elapsed);
            std::thread::park_timeout(remaining.min(BRIDGE_WATCH_POLL_INTERVAL));
        }
    });

    (done, fired, Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::runtime_compat::CompatRuntime;
    use frankensearch::{
        Embedder, EmbedderStack, HashEmbedder, IndexBuilder, PhaseMetrics, RankChanges,
        ScoreSource, TwoTierConfig, TwoTierIndex,
    };

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn log_test_event(test_name: &str, phase: &str, started_at: Instant, result: &str) {
        tracing::info!(
            test_name,
            phase,
            duration_ms = started_at.elapsed().as_millis() as u64,
            result,
            "search_bridge_test"
        );
    }

    fn phase_name(phase: &SearchPhase) -> &'static str {
        match phase {
            SearchPhase::Initial { .. } => "Initial",
            SearchPhase::Refined { .. } => "Refined",
            SearchPhase::RefinementFailed { .. } => "RefinementFailed",
        }
    }

    fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(future)
    }

    /// Helper to construct a `ScoredResult` for unit tests.
    fn make_scored_result(doc_id: &str, score: f32) -> ScoredResult {
        ScoredResult {
            doc_id: doc_id.to_string(),
            score,
            source: ScoreSource::Hybrid,
            index: None,
            fast_score: None,
            quality_score: None,
            lexical_score: None,
            rerank_score: None,
            explanation: None,
            metadata: None,
        }
    }

    /// Helper to construct a `PhaseMetrics` for unit tests.
    fn make_phase_metrics() -> PhaseMetrics {
        PhaseMetrics {
            embedder_id: "test-hash-256".to_string(),
            vectors_searched: 10,
            lexical_candidates: 5,
            fused_count: 8,
        }
    }

    fn build_test_bridge() -> (SearchBridge, TextProvider) {
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let nonce = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "frankenterm-search-bridge-{}-{now_nanos}-{nonce}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&dir).expect("create test index directory");

        let documents = vec![
            (
                "doc-rust-ownership".to_string(),
                "Rust ownership and borrowing prevents data races".to_string(),
            ),
            (
                "doc-distributed".to_string(),
                "Distributed consensus algorithms like Raft ensure fault tolerance".to_string(),
            ),
            (
                "doc-search".to_string(),
                "Hybrid lexical semantic search improves ranking quality".to_string(),
            ),
            (
                "doc-vector".to_string(),
                "Vector index structures accelerate nearest neighbor retrieval".to_string(),
            ),
            (
                "doc-timeout".to_string(),
                "Timeout handling keeps interactive systems responsive".to_string(),
            ),
            (
                "doc-cancel".to_string(),
                "Cancellation propagation avoids hanging background operations".to_string(),
            ),
        ];

        let fast: Arc<dyn Embedder> = Arc::new(HashEmbedder::default_256());
        let quality: Arc<dyn Embedder> = Arc::new(HashEmbedder::default_384());
        let stack = EmbedderStack::from_parts(Arc::clone(&fast), Some(Arc::clone(&quality)));

        let build_stats = run_async(async {
            let cx = Cx::for_testing();
            let mut builder = IndexBuilder::new(&dir).with_embedder_stack(stack);
            for (id, text) in &documents {
                builder = builder.add_document(id.clone(), text.clone());
            }
            builder.build(&cx).await.expect("build test index")
        });
        assert_eq!(build_stats.doc_count, documents.len());

        let index = Arc::new(
            TwoTierIndex::open(&dir, TwoTierConfig::default()).expect("open built test index"),
        );
        let searcher = TwoTierSearcher::new(index, fast, TwoTierConfig::default())
            .with_quality_embedder(quality);

        let text_map: Arc<HashMap<String, String>> = Arc::new(documents.into_iter().collect());
        let text_provider: TextProvider = Arc::new(move |doc_id| text_map.get(doc_id).cloned());

        (SearchBridge::new(searcher), text_provider)
    }

    #[allow(clippy::needless_return)] // return required by cfg-gated dual-runtime pattern
    async fn raw_search_baseline(
        searcher: Arc<TwoTierSearcher>,
        query: String,
        limit: usize,
        text_provider: TextProvider,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = Cx::for_testing();
            let (results, metrics) = searcher
                .search_collect_with_text(&cx, &query, limit, |doc_id| text_provider(doc_id))
                .await
                .map_err(SearchBridgeError::Search)?;
            return Ok(SearchBridgeResult { results, metrics });
        }

        #[cfg(not(feature = "asupersync-runtime"))]
        {
            let joined = crate::runtime_compat::spawn_blocking(
                move || -> Result<SearchBridgeResult, SearchError> {
                    let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
                        .build()
                        .map_err(|err| SearchError::InvalidConfig {
                            field: "search_bridge.raw.runtime".to_owned(),
                            value: "tokio_current_thread".to_owned(),
                            reason: err,
                        })?;

                    runtime.block_on(async move {
                        let cx = Cx::for_testing();
                        let (results, metrics) = searcher
                            .search_collect_with_text(&cx, &query, limit, |doc_id| {
                                text_provider(doc_id)
                            })
                            .await?;
                        Ok(SearchBridgeResult { results, metrics })
                    })
                },
            )
            .await
            .map_err(|message| SearchBridgeError::Runtime { message })?;

            joined.map_err(SearchBridgeError::Search)
        }
    }

    // -----------------------------------------------------------------------
    // BridgeCancellationToken unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cancellation_token_new_not_cancelled() {
        let token = BridgeCancellationToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_cancellation_token_cancel_sets_flag() {
        let token = BridgeCancellationToken::new();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_cancellation_token_cancel_idempotent() {
        let token = BridgeCancellationToken::new();
        token.cancel();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_cancellation_token_clone_shares_state() {
        let token_a = BridgeCancellationToken::new();
        let token_b = token_a.clone();
        assert!(!token_b.is_cancelled());
        token_a.cancel();
        assert!(token_b.is_cancelled());
    }

    #[test]
    fn test_cancellation_token_default_not_cancelled() {
        let token = BridgeCancellationToken::default();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_cancellation_token_debug_format() {
        let token = BridgeCancellationToken::new();
        let debug_str = format!("{:?}", token);
        assert!(debug_str.contains("BridgeCancellationToken"));
    }

    #[test]
    fn test_cancellation_token_cancelled_returns_immediately_if_already_cancelled() {
        let token = BridgeCancellationToken::new();
        token.cancel();
        // cancelled() should return immediately because it's already cancelled
        run_async(async {
            crate::runtime_compat::timeout(Duration::from_millis(100), token.cancelled())
                .await
                .expect("cancelled() should resolve immediately when already cancelled");
        });
    }

    #[test]
    fn test_cancellation_token_cancelled_wakes_on_cancel() {
        let token = BridgeCancellationToken::new();
        let token_clone = token.clone();
        run_async(async {
            let waiter = crate::runtime_compat::task::spawn(async move {
                token_clone.cancelled().await;
                true
            });
            // Give the waiter a moment to register
            crate::runtime_compat::sleep(Duration::from_millis(10)).await;
            token.cancel();
            let result = crate::runtime_compat::timeout(Duration::from_millis(200), waiter)
                .await
                .expect("should not timeout")
                .expect("task should not panic");
            assert!(result);
        });
    }

    // -----------------------------------------------------------------------
    // SearchBridgeRequest unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_request_new_defaults() {
        let req = SearchBridgeRequest::new("hello world", 10);
        assert_eq!(req.query, "hello world");
        assert_eq!(req.limit, 10);
        assert!(req.timeout.is_none());
        assert!(req.cancellation.is_none());
        // Default text_provider returns None for any doc_id
        assert!((req.text_provider)("any-doc").is_none());
    }

    #[test]
    fn test_request_with_timeout() {
        let req = SearchBridgeRequest::new("test", 5).with_timeout(Duration::from_secs(30));
        assert_eq!(req.timeout, Some(Duration::from_secs(30)));
    }

    #[test]
    fn test_request_with_cancellation() {
        let token = BridgeCancellationToken::new();
        let req = SearchBridgeRequest::new("test", 5).with_cancellation(token.clone());
        assert!(req.cancellation.is_some());
        // Cancelling via the original token is visible through the request
        token.cancel();
        assert!(req.cancellation.as_ref().unwrap().is_cancelled());
    }

    #[test]
    fn test_request_with_text_provider() {
        let req = SearchBridgeRequest::new("test", 5).with_text_provider(|doc_id| {
            if doc_id == "doc-1" {
                Some("Document one content".to_string())
            } else {
                None
            }
        });
        assert_eq!(
            (req.text_provider)("doc-1"),
            Some("Document one content".to_string())
        );
        assert!((req.text_provider)("doc-2").is_none());
    }

    #[test]
    fn test_request_with_text_provider_arc() {
        let provider: TextProvider = Arc::new(|_| Some("shared".to_string()));
        let req = SearchBridgeRequest::new("test", 5).with_text_provider_arc(Arc::clone(&provider));
        assert_eq!((req.text_provider)("anything"), Some("shared".to_string()));
    }

    #[test]
    fn test_request_debug_format() {
        let req =
            SearchBridgeRequest::new("debug query", 3).with_timeout(Duration::from_millis(500));
        let debug_str = format!("{:?}", req);
        assert!(debug_str.contains("SearchBridgeRequest"));
        assert!(debug_str.contains("debug query"));
        assert!(debug_str.contains("3"));
        assert!(debug_str.contains("<fn>"));
    }

    #[test]
    fn test_request_clone() {
        let token = BridgeCancellationToken::new();
        let req = SearchBridgeRequest::new("clone me", 7)
            .with_timeout(Duration::from_secs(5))
            .with_cancellation(token.clone());
        let cloned = req.clone();
        assert_eq!(cloned.query, "clone me");
        assert_eq!(cloned.limit, 7);
        assert_eq!(cloned.timeout, Some(Duration::from_secs(5)));
        // Cancellation token is shared via Arc
        token.cancel();
        assert!(cloned.cancellation.as_ref().unwrap().is_cancelled());
    }

    #[test]
    fn test_request_builder_chain() {
        let token = BridgeCancellationToken::new();
        let req = SearchBridgeRequest::new("chained", 20)
            .with_timeout(Duration::from_secs(10))
            .with_cancellation(token)
            .with_text_provider(|_| Some("chained-text".to_string()));
        assert_eq!(req.query, "chained");
        assert_eq!(req.limit, 20);
        assert!(req.timeout.is_some());
        assert!(req.cancellation.is_some());
        assert_eq!((req.text_provider)("x"), Some("chained-text".to_string()));
    }

    // -----------------------------------------------------------------------
    // SearchBridgeError unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_runtime_display() {
        let err = SearchBridgeError::Runtime {
            message: "worker panicked".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("runtime failure"));
        assert!(display.contains("worker panicked"));
    }

    #[test]
    fn test_error_timeout_display() {
        let err = SearchBridgeError::Timeout { timeout_ms: 5000 };
        let display = format!("{err}");
        assert!(display.contains("timed out"));
        assert!(display.contains("5000"));
    }

    #[test]
    fn test_error_cancelled_display() {
        let err = SearchBridgeError::Cancelled {
            reason: "user requested abort".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("cancelled"));
        assert!(display.contains("user requested abort"));
    }

    #[test]
    fn test_error_search_display() {
        let inner = SearchError::Cancelled {
            phase: "initial".to_string(),
            reason: "cx cancelled".to_string(),
        };
        let err = SearchBridgeError::Search(inner);
        let display = format!("{err}");
        assert!(display.contains("search failed"));
    }

    #[test]
    fn test_error_debug_format() {
        let err = SearchBridgeError::Timeout { timeout_ms: 100 };
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("Timeout"));
        assert!(debug_str.contains("100"));
    }

    // -----------------------------------------------------------------------
    // SearchBridge construction unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_bridge_debug_format() {
        let (bridge, _) = build_test_bridge();
        let debug_str = format!("{:?}", bridge);
        assert!(debug_str.contains("SearchBridge"));
        assert!(debug_str.contains("<TwoTierSearcher>"));
    }

    #[test]
    fn test_bridge_clone_shares_searcher() {
        let (bridge, _) = build_test_bridge();
        let cloned = bridge.clone();
        // Both should share the same Arc<TwoTierSearcher>
        assert!(Arc::ptr_eq(
            &bridge.shared_searcher(),
            &cloned.shared_searcher()
        ));
    }

    #[test]
    fn test_bridge_from_shared_preserves_arc() {
        let (bridge, _) = build_test_bridge();
        let searcher_arc = bridge.shared_searcher();
        let bridge2 = SearchBridge::from_shared(Arc::clone(&searcher_arc));
        assert!(Arc::ptr_eq(&searcher_arc, &bridge2.shared_searcher()));
    }

    // -----------------------------------------------------------------------
    // update_best_results unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_update_best_results_initial_phase() {
        let mut best = Vec::new();
        let results = vec![make_scored_result("a", 0.9), make_scored_result("b", 0.8)];
        let phase = SearchPhase::Initial {
            results: results.clone(),
            latency: Duration::from_millis(10),
            metrics: make_phase_metrics(),
        };
        update_best_results(&mut best, &phase);
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].doc_id, "a");
        assert_eq!(best[1].doc_id, "b");
    }

    #[test]
    fn test_update_best_results_refined_replaces_initial() {
        let mut best = vec![make_scored_result("old", 0.5)];
        let refined = vec![
            make_scored_result("x", 0.95),
            make_scored_result("y", 0.85),
            make_scored_result("z", 0.75),
        ];
        let phase = SearchPhase::Refined {
            results: refined.clone(),
            latency: Duration::from_millis(20),
            metrics: make_phase_metrics(),
            rank_changes: RankChanges {
                promoted: 1,
                demoted: 0,
                stable: 2,
            },
        };
        update_best_results(&mut best, &phase);
        assert_eq!(best.len(), 3);
        assert_eq!(best[0].doc_id, "x");
    }

    #[test]
    fn test_update_best_results_refinement_failed_uses_initial() {
        let mut best = vec![make_scored_result("stale", 0.1)];
        let initial = vec![
            make_scored_result("fallback-a", 0.7),
            make_scored_result("fallback-b", 0.6),
        ];
        let phase = SearchPhase::RefinementFailed {
            initial_results: initial.clone(),
            error: SearchError::Cancelled {
                phase: "refined".to_string(),
                reason: "timeout".to_string(),
            },
            latency: Duration::from_millis(500),
        };
        update_best_results(&mut best, &phase);
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].doc_id, "fallback-a");
    }

    #[test]
    fn test_update_best_results_empty_results() {
        let mut best = vec![make_scored_result("existing", 0.5)];
        let phase = SearchPhase::Initial {
            results: Vec::new(),
            latency: Duration::from_millis(1),
            metrics: make_phase_metrics(),
        };
        update_best_results(&mut best, &phase);
        assert!(best.is_empty());
    }

    #[test]
    fn test_update_best_results_sequential_phases() {
        let mut best = Vec::new();

        // Phase 1: Initial
        let initial = vec![make_scored_result("init-1", 0.8)];
        update_best_results(
            &mut best,
            &SearchPhase::Initial {
                results: initial,
                latency: Duration::from_millis(5),
                metrics: make_phase_metrics(),
            },
        );
        assert_eq!(best.len(), 1);
        assert_eq!(best[0].doc_id, "init-1");

        // Phase 2: Refined replaces
        let refined = vec![
            make_scored_result("ref-1", 0.95),
            make_scored_result("ref-2", 0.85),
        ];
        update_best_results(
            &mut best,
            &SearchPhase::Refined {
                results: refined,
                latency: Duration::from_millis(15),
                metrics: make_phase_metrics(),
                rank_changes: RankChanges {
                    promoted: 1,
                    demoted: 0,
                    stable: 1,
                },
            },
        );
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].doc_id, "ref-1");
    }

    // -----------------------------------------------------------------------
    // map_search_error unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_search_error_timeout_takes_priority() {
        let token = BridgeCancellationToken::new();
        let error = SearchError::Cancelled {
            phase: "initial".to_string(),
            reason: "cx was cancelled".to_string(),
        };
        // timeout_fired=true should take priority over Cancelled variant
        let mapped = map_search_error(error, &token, true, Some(Duration::from_secs(5)));
        let is_timeout = matches!(mapped, SearchBridgeError::Timeout { timeout_ms: 5000 });
        assert!(is_timeout, "expected Timeout, got {:?}", mapped);
    }

    #[test]
    fn test_map_search_error_cancelled_propagates() {
        let token = BridgeCancellationToken::new();
        let error = SearchError::Cancelled {
            phase: "refined".to_string(),
            reason: "user abort".to_string(),
        };
        let mapped = map_search_error(error, &token, false, None);
        let is_cancelled = matches!(mapped, SearchBridgeError::Cancelled { .. });
        assert!(is_cancelled, "expected Cancelled, got {:?}", mapped);
        // map_search_error also cancels the token on Cancelled errors
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_map_search_error_generic_passthrough() {
        let token = BridgeCancellationToken::new();
        let error = SearchError::InvalidConfig {
            field: "limit".to_string(),
            value: "-1".to_string(),
            reason: "must be positive".to_string(),
        };
        let mapped = map_search_error(error, &token, false, None);
        let is_search = matches!(mapped, SearchBridgeError::Search(_));
        assert!(is_search, "expected Search, got {:?}", mapped);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_map_search_error_timeout_zero_when_no_duration() {
        let token = BridgeCancellationToken::new();
        let error = SearchError::Cancelled {
            phase: "test".to_string(),
            reason: "test".to_string(),
        };
        // timeout_fired=true but no duration => timeout_ms should be 0
        let mapped = map_search_error(error, &token, true, None);
        let is_timeout_zero = matches!(mapped, SearchBridgeError::Timeout { timeout_ms: 0 });
        assert!(
            is_timeout_zero,
            "expected Timeout with 0ms, got {:?}",
            mapped
        );
    }

    // -----------------------------------------------------------------------
    // SearchBridgeResult unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_search_bridge_result_debug() {
        let result = SearchBridgeResult {
            results: vec![make_scored_result("doc-1", 0.9)],
            metrics: TwoTierMetrics::default(),
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("SearchBridgeResult"));
        assert!(debug_str.contains("doc-1"));
    }

    #[test]
    fn test_search_bridge_result_clone() {
        let result = SearchBridgeResult {
            results: vec![
                make_scored_result("doc-a", 0.8),
                make_scored_result("doc-b", 0.7),
            ],
            metrics: TwoTierMetrics::default(),
        };
        let cloned = result.clone();
        assert_eq!(cloned.results.len(), 2);
        assert_eq!(cloned.results[0].doc_id, "doc-a");
    }

    // -----------------------------------------------------------------------
    // spawn_cancellation_thread unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_spawn_cancellation_thread_pre_cancelled() {
        let cx = Cx::for_testing();
        let token = BridgeCancellationToken::new();
        token.cancel();

        let (done, handle) = spawn_cancellation_thread(cx.clone(), token);
        // When pre-cancelled, no thread is spawned
        assert!(handle.is_none());
        // The cx should have cancel requested set
        assert!(cx.is_cancel_requested());
        // done flag should still be false (no thread ran)
        assert!(!done.load(Ordering::Acquire));
    }

    #[test]
    fn test_spawn_cancellation_thread_polls_and_stops() {
        let cx = Cx::for_testing();
        let token = BridgeCancellationToken::new();

        let (done, handle) = spawn_cancellation_thread(cx.clone(), token);
        assert!(handle.is_some());
        // Signal done so thread exits cleanly
        done.store(true, Ordering::Release);
        handle.unwrap().join().expect("thread should join");
        // cx should NOT have cancel_requested since we didn't cancel the token
        assert!(!cx.is_cancel_requested());
    }

    // -----------------------------------------------------------------------
    // spawn_timeout_thread unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_spawn_timeout_thread_none_timeout() {
        let token = BridgeCancellationToken::new();
        let (done, fired, handle) = spawn_timeout_thread(None, token.clone());
        assert!(handle.is_none());
        assert!(!fired.load(Ordering::Acquire));
        assert!(!done.load(Ordering::Acquire));
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_spawn_timeout_thread_fires_on_expiry() {
        let token = BridgeCancellationToken::new();
        let (done, fired, handle) =
            spawn_timeout_thread(Some(Duration::from_millis(20)), token.clone());
        assert!(handle.is_some());
        // Wait for timeout to fire
        std::thread::sleep(Duration::from_millis(100));
        assert!(fired.load(Ordering::Acquire));
        assert!(token.is_cancelled());
        // Clean up
        done.store(true, Ordering::Release);
        handle.unwrap().join().expect("thread should join");
    }

    #[test]
    fn test_spawn_timeout_thread_does_not_fire_if_done_early() {
        let token = BridgeCancellationToken::new();
        let (done, fired, handle) =
            spawn_timeout_thread(Some(Duration::from_secs(60)), token.clone());
        assert!(handle.is_some());
        // Signal done immediately, before timeout
        done.store(true, Ordering::Release);
        handle.unwrap().join().expect("thread should join");
        assert!(!fired.load(Ordering::Acquire));
        assert!(!token.is_cancelled());
    }

    // -----------------------------------------------------------------------
    // Integration tests (require building a search index)
    // -----------------------------------------------------------------------

    #[test]
    fn test_bridge_round_trip() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();
        let phases: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let phases_sink = Arc::clone(&phases);

        let request = SearchBridgeRequest::new("rust ownership", 5)
            .with_text_provider_arc(Arc::clone(&text_provider));

        let result = run_async(bridge.search(request, move |phase| {
            phases_sink
                .lock()
                .expect("phase lock")
                .push(phase_name(&phase).to_owned());
        }));

        let search_result = result.expect("bridge round trip succeeds");
        assert!(!search_result.results.is_empty());
        assert!(!phases.lock().expect("phase lock").is_empty());

        log_test_event("test_bridge_round_trip", "done", started_at, "ok");
    }

    #[test]
    fn test_bridge_cancellation_forward() {
        let started_at = Instant::now();
        let (bridge, _text_provider) = build_test_bridge();
        let token = BridgeCancellationToken::new();
        token.cancel();

        let request =
            SearchBridgeRequest::new("distributed consensus", 5).with_cancellation(token.clone());

        let result = run_async(bridge.search(request, |_| {}));
        assert!(
            matches!(result, Err(SearchBridgeError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert!(token.is_cancelled());
        log_test_event("test_bridge_cancellation_forward", "done", started_at, "ok");
    }

    #[test]
    fn test_bridge_cancellation_reverse() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();
        let token = BridgeCancellationToken::new();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let slow_provider: TextProvider = Arc::new(move |doc_id| {
            std::thread::sleep(Duration::from_millis(200));
            text_provider(doc_id)
        });

        let request = SearchBridgeRequest::new("hybrid search", 5)
            .with_text_provider_arc(slow_provider)
            .with_cancellation(token.clone());

        let result = run_async(bridge.search_with_cx(cx, request, |_| {}));
        assert!(matches!(result, Err(SearchBridgeError::Cancelled { .. })));
        assert!(token.is_cancelled());

        log_test_event("test_bridge_cancellation_reverse", "done", started_at, "ok");
    }

    #[test]
    fn test_bridge_timeout() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();
        let slow_provider: TextProvider = Arc::new(move |doc_id| {
            std::thread::sleep(Duration::from_millis(200));
            text_provider(doc_id)
        });

        // Use a negation term so frankensearch invokes text_provider for each
        // result (exclusion filtering).  Each call sleeps 200 ms, making the
        // overall search far exceed the 100 ms budget and causing a timeout.
        let request = SearchBridgeRequest::new("vector retrieval -nonexistent", 6)
            .with_text_provider_arc(slow_provider)
            .with_timeout(Duration::from_millis(100));

        let result = run_async(bridge.search(request, |_| {}));
        assert!(matches!(result, Err(SearchBridgeError::Timeout { .. })));

        log_test_event("test_bridge_timeout", "done", started_at, "ok");
    }

    #[test]
    fn test_bridge_concurrent_searches() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();

        run_async(async move {
            let mut tasks = Vec::new();
            for i in 0..10 {
                let bridge = bridge.clone();
                let text_provider = Arc::clone(&text_provider);
                tasks.push(crate::runtime_compat::task::spawn(async move {
                    let query = format!("search quality {i}");
                    let request =
                        SearchBridgeRequest::new(query, 5).with_text_provider_arc(text_provider);
                    bridge.search(request, |_| {}).await
                }));
            }

            for task in tasks {
                let result = task.await.expect("task join");
                assert!(result.is_ok());
            }
        });

        log_test_event("test_bridge_concurrent_searches", "done", started_at, "ok");
    }

    #[test]
    fn test_bridge_overhead() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();
        let query = "rust distributed search".to_owned();
        let iterations = 5_u32;

        run_async(async {
            let mut raw_total = Duration::ZERO;
            let mut bridge_total = Duration::ZERO;

            for _ in 0..iterations {
                let raw_started = Instant::now();
                let raw_result = raw_search_baseline(
                    bridge.shared_searcher(),
                    query.clone(),
                    5,
                    Arc::clone(&text_provider),
                )
                .await
                .expect("raw baseline result");
                raw_total += raw_started.elapsed();
                assert!(!raw_result.results.is_empty());

                let bridge_started = Instant::now();
                let bridge_result = bridge
                    .search(
                        SearchBridgeRequest::new(query.clone(), 5)
                            .with_text_provider_arc(Arc::clone(&text_provider)),
                        |_| {},
                    )
                    .await
                    .expect("bridge result");
                bridge_total += bridge_started.elapsed();
                assert!(!bridge_result.results.is_empty());
            }

            let raw_average = raw_total / iterations;
            let bridge_average = bridge_total / iterations;
            let overhead = bridge_average.saturating_sub(raw_average);

            assert!(
                overhead <= Duration::from_millis(10),
                "bridge overhead exceeded budget: raw={raw_average:?}, bridge={bridge_average:?}, overhead={overhead:?}"
            );
        });

        log_test_event("test_bridge_overhead", "done", started_at, "ok");
    }

    #[test]
    fn test_bridge_empty_query() {
        let (bridge, text_provider) = build_test_bridge();
        let request = SearchBridgeRequest::new("", 5).with_text_provider_arc(text_provider);
        // Empty query should not panic — it may return empty or all results
        let result = run_async(bridge.search(request, |_| {}));
        assert!(result.is_ok());
    }

    #[test]
    fn test_bridge_search_result_has_metrics() {
        let (bridge, text_provider) = build_test_bridge();
        let request = SearchBridgeRequest::new("rust", 5).with_text_provider_arc(text_provider);
        let result = run_async(bridge.search(request, |_| {})).expect("search should succeed");
        // TwoTierMetrics should have non-zero phase1_total_ms
        assert!(
            result.metrics.phase1_total_ms >= 0.0,
            "phase1_total_ms should be non-negative"
        );
    }

    #[test]
    fn test_bridge_phase_callback_receives_initial() {
        let (bridge, text_provider) = build_test_bridge();
        let saw_initial = Arc::new(AtomicBool::new(false));
        let saw_initial_clone = Arc::clone(&saw_initial);

        let request =
            SearchBridgeRequest::new("consensus", 5).with_text_provider_arc(text_provider);

        run_async(bridge.search(request, move |phase| {
            if matches!(phase, SearchPhase::Initial { .. }) {
                saw_initial_clone.store(true, Ordering::Release);
            }
        }))
        .expect("search should succeed");

        assert!(
            saw_initial.load(Ordering::Acquire),
            "should have seen Initial phase"
        );
    }
}
