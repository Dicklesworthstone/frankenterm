//! Tokio-facing bridge for `frankensearch::TwoTierSearcher`.
//!
//! This module exposes an async API that can be called from FrankenTerm's
//! runtime surface while preserving frankensearch's progressive phase callbacks
//! and capability-context cancellation semantics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use frankensearch::{Cx, ScoredResult, SearchError, SearchPhase, TwoTierMetrics, TwoTierSearcher};
use thiserror::Error;
use tokio::sync::Notify;
#[cfg(not(feature = "asupersync-runtime"))]
use tokio::sync::mpsc;

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
    /// This creates a per-request `Cx::for_testing()` capability context and
    /// forwards progressive phases to `on_phase`.
    pub async fn search(
        &self,
        request: SearchBridgeRequest,
        on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        self.search_with_cx(Cx::for_testing(), request, on_phase)
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
            let _ = handle.join();
        }
        timeout_done.store(true, Ordering::Release);
        if let Some(handle) = timeout_thread {
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
        let (timeout_done, timeout_fired, timeout_thread) =
            spawn_timeout_thread(timeout, cancellation.clone());

        let (phase_tx, mut phase_rx) = mpsc::unbounded_channel();
        let searcher = Arc::clone(&self.searcher);
        let worker_cancellation = cancellation.clone();

        let worker = crate::runtime_compat::spawn_blocking(
            move || -> Result<SearchBridgeResult, SearchError> {
                let (cancel_done, cancel_thread) =
                    spawn_cancellation_thread(cx.clone(), worker_cancellation.clone());

                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| SearchError::InvalidConfig {
                        field: "search_bridge.runtime".to_owned(),
                        value: "tokio_current_thread".to_owned(),
                        reason: err.to_string(),
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
                    let _ = handle.join();
                }

                search_result
            },
        );

        let mut worker = Box::pin(worker);
        let joined = loop {
            tokio::select! {
                maybe_phase = phase_rx.recv() => {
                    if let Some(phase) = maybe_phase {
                        on_phase(phase);
                    }
                }
                worker_result = &mut worker => {
                    while let Ok(phase) = phase_rx.try_recv() {
                        on_phase(phase);
                    }
                    break worker_result;
                }
            }
        };

        timeout_done.store(true, Ordering::Release);
        if let Some(handle) = timeout_thread {
            let _ = handle.join();
        }

        let worker_result = joined.map_err(|message| SearchBridgeError::Runtime { message })?;

        match worker_result {
            Ok(result) => Ok(result),
            Err(error) => Err(map_search_error(
                error,
                &cancellation,
                timeout_fired.load(Ordering::Acquire),
                timeout,
            )),
        }
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
            std::thread::sleep(Duration::from_millis(1));
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
            if started_at.elapsed() >= timeout_duration {
                fired_for_thread.store(true, Ordering::Release);
                cancellation.cancel();
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    (done, fired, Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::runtime_compat::CompatRuntime;
    use frankensearch::{
        Embedder, EmbedderStack, HashEmbedder, IndexBuilder, TwoTierConfig, TwoTierIndex,
    };

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

    fn build_test_bridge() -> (SearchBridge, TextProvider) {
        let now_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "frankenterm-search-bridge-{}-{now_nanos}",
            std::process::id()
        ));

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
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| SearchError::InvalidConfig {
                            field: "search_bridge.raw.runtime".to_owned(),
                            value: "tokio_current_thread".to_owned(),
                            reason: err.to_string(),
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
    #[ignore = "searcher does not yet check cancellation; bridge plumbing ready but needs search-engine cooperation"]
    fn test_bridge_cancellation_forward() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();
        let token = BridgeCancellationToken::new();
        let token_for_thread = token.clone();

        let slow_provider: TextProvider = Arc::new(move |doc_id| {
            std::thread::sleep(Duration::from_millis(200));
            text_provider(doc_id)
        });

        let request = SearchBridgeRequest::new("distributed consensus", 5)
            .with_text_provider_arc(slow_provider)
            .with_cancellation(token.clone());

        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            token_for_thread.cancel();
        });

        let result = run_async(bridge.search(request, |_| {}));
        let _ = cancel_thread.join();

        assert!(matches!(result, Err(SearchBridgeError::Cancelled { .. })));
        log_test_event("test_bridge_cancellation_forward", "done", started_at, "ok");
    }

    #[test]
    #[ignore = "searcher does not yet check Cx cancellation; bridge plumbing ready but needs search-engine cooperation"]
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
    #[ignore = "searcher does not yet check timeout/cancellation; bridge plumbing ready but needs search-engine cooperation"]
    fn test_bridge_timeout() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();
        let slow_provider: TextProvider = Arc::new(move |doc_id| {
            std::thread::sleep(Duration::from_millis(200));
            text_provider(doc_id)
        });

        let request = SearchBridgeRequest::new("vector retrieval", 6)
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
                tasks.push(tokio::spawn(async move {
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
}
