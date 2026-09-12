//! Runtime-facing bridge for `frankensearch::TwoTierSearcher`.
//!
//! This module exposes an async API that can be called from FrankenTerm's
//! runtime surface while preserving frankensearch's progressive phase callbacks
//! and capability-context cancellation semantics.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::Poll;
use std::time::{Duration, Instant};

use crate::runtime_async::notify::Notify;
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
    registrations: Mutex<Vec<Weak<SearchCancellationRegistration>>>,
    #[cfg(test)]
    deadline_signals: Mutex<Vec<Weak<SearchTimeoutSignal>>>,
}

#[derive(Debug)]
struct SearchCancellationRegistration {
    cx: Mutex<Option<Cx>>,
}

impl SearchCancellationRegistration {
    fn cancel(&self) {
        let cx = self
            .cx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Claim exactly once, then publish without any bridge lock held:
        // cancellation may synchronously invoke an arbitrary registered waker.
        if let Some(cx) = cx {
            cx.set_cancel_requested(true);
        }
    }
}

/// Owns the token-to-search link. Drop revokes unclaimed cancellation; an
/// already-claimed cancellation can finish publishing its wake without
/// retaining a search future or phase callback, or blocking Drop on that wake.
struct SearchCancellationGuard {
    token: BridgeCancellationToken,
    registration: Arc<SearchCancellationRegistration>,
}

impl Drop for SearchCancellationGuard {
    fn drop(&mut self) {
        let cx = self
            .registration
            .cx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(cx);
        self.token
            .state
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| !entry.ptr_eq(&Arc::downgrade(&self.registration)));
    }
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
            let registrations: Vec<_> = self
                .state
                .registrations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
            for registration in registrations {
                registration.cancel();
            }
            self.state.notify.notify_waiters();
        }
    }

    fn register_search(&self, cx: Cx) -> SearchCancellationGuard {
        let registration = Arc::new(SearchCancellationRegistration {
            cx: Mutex::new(Some(cx)),
        });
        let mut registrations = self
            .state
            .registrations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The predicate and insertion share cancel's registry lock: a cancel
        // before registration is sticky, and a later one sees this entry.
        let cancelled = self.is_cancelled();
        if !cancelled {
            registrations.push(Arc::downgrade(&registration));
        }
        drop(registrations);
        if cancelled {
            registration.cancel();
        }
        SearchCancellationGuard {
            token: self.clone(),
            registration,
        }
    }

    /// Return whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Await cancellation and report whether the bridge or ambient Cx won.
    pub async fn cancelled(&self) -> BridgeWaitOutcome {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.cancelled_with_cx(&cx).await
    }

    /// Wait after one initial predicate observation while closing the
    /// check-to-registration race around edge-triggered `notify_waiters`.
    ///
    /// The hook is normally a zero-sized no-op. Keeping it explicit lets the
    /// unit test deterministically commit cancellation at the historical lost
    /// wake boundary without sleeps or scheduler timing assumptions.
    async fn cancelled_after_initial_check(&self, after_initial_check: impl FnOnce()) {
        if self.is_cancelled() {
            return;
        }
        let mut notified = std::pin::pin!(self.state.notify.notified());
        let mut after_initial_check = Some(after_initial_check);
        std::future::poll_fn(|task_cx| {
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            if let Some(after_initial_check) = after_initial_check.take() {
                after_initial_check();
            }

            let notified_result = std::future::Future::poll(notified.as_mut(), task_cx);
            if notified_result.is_ready() || self.is_cancelled() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`cancelled`].
    ///
    /// Returns `BridgeWaitOutcome::BridgeCancelled` when the bridge
    /// cancellation is observed, or `BridgeWaitOutcome::CxCancelled`
    /// when the caller's cx is cancelled first. This lets the
    /// caller short-circuit the bridge-cancellation wait when an
    /// outer cx is cancelled — useful for supervisor futures that
    /// race the bridge token against a shutdown scope.
    pub async fn cancelled_with_cx(&self, cx: &crate::cx::Cx) -> BridgeWaitOutcome {
        if self.is_cancelled() {
            return BridgeWaitOutcome::BridgeCancelled;
        }
        if cx.checkpoint().is_err() {
            return BridgeWaitOutcome::CxCancelled;
        }

        // Both branches register wakes; neither busy-polls nor spawns work.
        let bridge_cancelled = self.cancelled_after_initial_check(|| {});

        crate::runtime_async::select! {
            () = bridge_cancelled => BridgeWaitOutcome::BridgeCancelled,
            _ = crate::runtime_async::wait_for_cancellation(cx) => BridgeWaitOutcome::CxCancelled,
        }
    }
}

/// Outcome of [`BridgeCancellationToken::cancelled_with_cx`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeWaitOutcome {
    /// The bridge cancellation token was observed.
    BridgeCancelled,
    /// The caller's cx was cancelled before the bridge token.
    CxCancelled,
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
    /// br-ft-qfklb: request failed pre-flight validation. Surfaces
    /// before any side effect so an invalid request never reaches
    /// the underlying frankensearch surface.
    #[error("search bridge request validation failed: {0}")]
    ValidationError(String),
}

/// br-ft-qfklb: minimum acceptable `limit` on a `SearchBridgeRequest`.
/// `0` is structurally meaningless (asks for zero results, paying the
/// search cost for no gain) and is rejected at the validation gate.
pub const SEARCH_BRIDGE_MIN_LIMIT: usize = 1;

/// br-ft-qfklb: maximum acceptable `limit` on a `SearchBridgeRequest`.
/// Caps unbounded asks (e.g., `usize::MAX`) at a value matching the
/// MCP-layer cap on `wa.cass_search` (LIMIT_MAX = 1000) plus a 10x
/// headroom for non-MCP callers that may legitimately need larger
/// pages. Anything above this is rejected at the validation gate.
pub const SEARCH_BRIDGE_MAX_LIMIT: usize = 10_000;

/// br-ft-qfklb: minimum acceptable `timeout` on a `SearchBridgeRequest`.
/// `Duration::ZERO` is structurally meaningless (the timeout machinery
/// fires immediately, before any work, returning a confusing 'timeout'
/// on every call). Rejected at the validation gate.
pub const SEARCH_BRIDGE_MIN_TIMEOUT: Duration = Duration::from_millis(1);

/// br-ft-qfklb: maximum acceptable `timeout` on a `SearchBridgeRequest`.
/// Caps the upper end at 10 minutes — same value as the MCP-layer
/// `wa.cass_*` `timeout_secs` bound (CASS_TIMEOUT_SECS_MAX = 600,
/// shipped at ft-aylbh). Prevents misconfigured callers from pinning
/// the bridge on a slow query indefinitely.
pub const SEARCH_BRIDGE_MAX_TIMEOUT: Duration = Duration::from_secs(600);

impl SearchBridgeRequest {
    /// br-ft-qfklb: validate the request against the bridge's
    /// invariants before any side effect. Returns
    /// `Err(SearchBridgeError::ValidationError(reason))` on any
    /// violation; `Ok(())` when the request is well-formed.
    ///
    /// Pre-fix the bridge accepted any usize / Duration without
    /// validation. Operators relying on per-tool MCP-layer caps
    /// (wa.cass_* via ft-aylbh) got protected, but anyone
    /// instantiating SearchBridgeRequest from a non-MCP path
    /// (e.g., embedded library use) had zero protection. This
    /// gate is now the single source of truth that every bridge
    /// dispatch path runs through.
    pub fn validate(&self) -> std::result::Result<(), SearchBridgeError> {
        if self.limit < SEARCH_BRIDGE_MIN_LIMIT {
            return Err(SearchBridgeError::ValidationError(format!(
                "br-ft-qfklb: limit must be >= {SEARCH_BRIDGE_MIN_LIMIT} (got {}); \
                 a zero limit asks for zero results which is structurally meaningless",
                self.limit
            )));
        }
        if self.limit > SEARCH_BRIDGE_MAX_LIMIT {
            return Err(SearchBridgeError::ValidationError(format!(
                "br-ft-qfklb: limit must be <= {SEARCH_BRIDGE_MAX_LIMIT} (got {}); \
                 prevents unbounded result allocation",
                self.limit
            )));
        }
        if let Some(timeout) = self.timeout {
            if timeout < SEARCH_BRIDGE_MIN_TIMEOUT {
                return Err(SearchBridgeError::ValidationError(format!(
                    "br-ft-qfklb: timeout must be >= {SEARCH_BRIDGE_MIN_TIMEOUT:?} when set \
                     (got {timeout:?}); zero timeout fires before any work and returns a \
                     confusing 'timeout' on every call"
                )));
            }
            if timeout > SEARCH_BRIDGE_MAX_TIMEOUT {
                return Err(SearchBridgeError::ValidationError(format!(
                    "br-ft-qfklb: timeout must be <= {SEARCH_BRIDGE_MAX_TIMEOUT:?} when set \
                     (got {timeout:?}); caps the upper end to match the MCP-layer wa.cass_* \
                     bound (ft-aylbh CASS_TIMEOUT_SECS_MAX=600)"
                )));
            }
        }
        Ok(())
    }
}

/// Runtime-facing bridge wrapper around `TwoTierSearcher`.
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
    /// This creates a per-request context. When called within a runtime task,
    /// caller cancellation propagates into the search, while request timeout
    /// and token cancellation leave the surrounding task's context intact.
    pub async fn search(
        &self,
        request: SearchBridgeRequest,
        on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        let cx = Cx::current().unwrap_or_else(Cx::for_request);
        self.search_with_cx(cx, request, on_phase).await
    }

    /// Run a search governed by a caller-provided capability context.
    /// Request cancellation is isolated from the caller's surrounding scope.
    pub async fn search_with_cx(
        &self,
        cx: Cx,
        request: SearchBridgeRequest,
        on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        self.search_with_asupersync_cx(&cx, request, on_phase).await
    }

    /// Run a search against a caller-provided asupersync capability context
    /// (ft-xbnl0.2.2 Cx-first entry point).
    ///
    /// The caller's `asupersync::Cx` governs cooperative cancellation for
    /// the outer bridge: if the Cx is already cancelled on entry this
    /// method short-circuits with `SearchBridgeError::Cancelled` without
    /// invoking the underlying `TwoTierSearcher`. The search and the caller's
    /// cancellation wait are owned by this future; dropping it drops both.
    ///
    /// Both surfaces use the same Cx type. A fresh search context preserves
    /// caller isolation: a request timeout or bridge token must not cancel
    /// the caller's surrounding scope.
    pub async fn search_with_asupersync_cx(
        &self,
        cx: &crate::cx::Cx,
        request: SearchBridgeRequest,
        on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        // Check before the select: an immediately-ready search could otherwise
        // win before its cancellation branch checks an already-expired budget.
        if cx.checkpoint().is_err() {
            return Err(SearchBridgeError::Cancelled {
                reason: "capability context cancelled or exhausted".to_owned(),
            });
        }

        // Ensure the request carries a bridge cancellation token so we can
        // wire the caller's asupersync Cx to the token (and from there
        // into the frankensearch-side cancellation).
        let mut request = request;
        let bridge_token = request.cancellation.clone().unwrap_or_default();
        request.cancellation = Some(bridge_token.clone());

        // Although both APIs use the same Cx type, a clone would share cancel
        // state and allow a request timeout to cancel the caller's scope.
        // Preserve the one-way link by owning a fresh search context.
        let search_cx = Cx::for_request();
        let phase_cx = cx.clone();
        let phase_token = bridge_token.clone();
        let mut on_phase = on_phase;
        let forward_phase = move |phase| {
            // A synchronous provider can occupy the search poll while the
            // caller is cancelled. Observe caller authority at publication,
            // rather than waiting for the outer select to regain control.
            // Cancellation racing an already-started callback remains
            // cooperative; this check is the publication boundary.
            if phase_cx.checkpoint().is_err() {
                phase_token.cancel();
                return;
            }
            on_phase(phase);
        };
        crate::runtime_async::select! {
            result = self.search_direct(search_cx, request, forward_phase) => {
                if cx.checkpoint().is_err() {
                    bridge_token.cancel();
                    Err(SearchBridgeError::Cancelled {
                        reason: "capability context cancelled or exhausted".to_owned(),
                    })
                } else {
                    result
                }
            },
            _ = crate::runtime_async::wait_for_cancellation(cx) => {
                bridge_token.cancel();
                Err(SearchBridgeError::Cancelled {
                    reason: "capability context cancelled".to_owned(),
                })
            },
        }
    }

    async fn search_direct(
        &self,
        cx: Cx,
        request: SearchBridgeRequest,
        mut on_phase: impl FnMut(SearchPhase) + Send + 'static,
    ) -> Result<SearchBridgeResult, SearchBridgeError> {
        // br-ft-qfklb: pre-flight request validation before any side
        // effect. Catches limit=0/usize::MAX and timeout=ZERO/MAX
        // misconfigurations before deadline creation, cancellation registration,
        // or the underlying searcher.
        request.validate()?;

        let SearchBridgeRequest {
            query,
            limit,
            timeout,
            cancellation,
            text_provider,
        } = request;

        // Link tokens with possible writers: the caller publication gate,
        // explicit cancellation, or a deadline. No timeout needs no OS thread;
        // the searcher observes linked cancellation through its private Cx.
        let needs_cancellation_link = cancellation.is_some() || timeout.is_some();
        let cancellation = cancellation.unwrap_or_default();
        if cx.checkpoint().is_err() {
            cancellation.cancel();
            return Err(SearchBridgeError::Cancelled {
                reason: "capability context cancelled or exhausted".to_owned(),
            });
        }
        if cancellation.is_cancelled() {
            cx.set_cancel_requested(true);
            return Err(SearchBridgeError::Cancelled {
                reason: "bridge cancellation requested".to_owned(),
            });
        }

        let timeout_guard = SearchTimeoutGuard::start(timeout, cancellation.clone())?;
        let cancellation_link =
            needs_cancellation_link.then(|| cancellation.register_search(cx.clone()));

        let mut best_results = Vec::new();
        let search_result = self
            .searcher
            .search(
                &cx,
                &query,
                limit,
                |doc_id| text_provider(doc_id),
                |phase| {
                    if cancellation.is_cancelled() || cx.is_cancel_requested() {
                        return;
                    }
                    update_best_results(&mut best_results, &phase);
                    on_phase(phase);
                },
            )
            .await;

        // Stop callbacks before interpreting the result. Drop runs these same
        // ownership boundaries if the search future is abandoned or unwinds.
        timeout_guard.stop();
        drop(cancellation_link);

        if timeout_guard.fired() {
            return Err(SearchBridgeError::Timeout {
                timeout_ms: timeout.map_or(0, |value| value.as_millis() as u64),
            });
        }
        if cancellation.is_cancelled() || cx.is_cancel_requested() {
            cancellation.cancel();
            return Err(SearchBridgeError::Cancelled {
                reason: "search cancellation requested".to_owned(),
            });
        }

        match search_result {
            Ok(metrics) => Ok(SearchBridgeResult {
                results: best_results,
                metrics,
            }),
            Err(error) => Err(map_search_error(
                error,
                &cancellation,
                timeout_guard.fired(),
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
        SearchPhase::Initial { results, .. }
        | SearchPhase::Refined { results, .. }
        | SearchPhase::Reranked { results, .. } => best_results.clone_from(results),
        SearchPhase::RefinementFailed {
            initial_results, ..
        } => {
            best_results.clone_from(initial_results);
        }
    }
}

#[derive(Debug, Default)]
struct SearchTimeoutState {
    stopped: bool,
    fired: bool,
    settled: bool,
}

#[derive(Debug)]
struct SearchTimeoutSignal {
    state: Mutex<SearchTimeoutState>,
    wake: Condvar,
    started: Instant,
    duration: Duration,
    cancellation: BridgeCancellationToken,
}

/// A deadline must still run while a synchronous embedder/provider occupies a
/// poll. Its thread owns only this small signal and token, never the search or
/// callback. Drop revokes an unclaimed deadline and wakes it; it does not join
/// an OS thread on the async executor. A deadline already claimed before drop
/// may finish publishing cancellation, with no bridge locks held. Otherwise
/// the detached thread needs only one wake to settle, regardless of deadline.
struct SearchTimeoutGuard {
    signal: Option<Arc<SearchTimeoutSignal>>,
}

impl SearchTimeoutGuard {
    fn start(
        timeout: Option<Duration>,
        cancellation: BridgeCancellationToken,
    ) -> Result<Self, SearchBridgeError> {
        Self::start_with_spawn(timeout, cancellation, |work| {
            std::thread::Builder::new()
                .name("ft-search-bridge-timeout".to_owned())
                .spawn(work)
                .map(drop)
        })
    }

    fn start_with_spawn(
        timeout: Option<Duration>,
        cancellation: BridgeCancellationToken,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    ) -> Result<Self, SearchBridgeError> {
        let Some(duration) = timeout else {
            return Ok(Self { signal: None });
        };
        let signal = Arc::new(SearchTimeoutSignal {
            state: Mutex::new(SearchTimeoutState::default()),
            wake: Condvar::new(),
            started: Instant::now(),
            duration,
            cancellation,
        });
        let thread_signal = Arc::clone(&signal);
        spawn(Box::new(move || {
            let mut state = thread_signal
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !state.stopped {
                let remaining = thread_signal
                    .duration
                    .saturating_sub(thread_signal.started.elapsed());
                if remaining.is_zero() {
                    state.fired = true;
                    // Claim expiry under the lock, but never hold it while
                    // waking tasks: a waker may drop this search reentrantly.
                    drop(state);
                    thread_signal.cancellation.cancel();
                    state = thread_signal
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    break;
                }
                (state, _) = thread_signal
                    .wake
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            state.settled = true;
            thread_signal.wake.notify_all();
        }))
        .map_err(|error| {
            tracing::warn!(kind = ?error.kind(), "Search deadline worker admission failed");
            SearchBridgeError::Runtime {
                message: "deadline worker admission failed".to_owned(),
            }
        })?;
        #[cfg(test)]
        signal
            .cancellation
            .state
            .deadline_signals
            .lock()
            .unwrap()
            .push(Arc::downgrade(&signal));
        Ok(Self {
            signal: Some(signal),
        })
    }

    fn stop(&self) {
        self.stop_at(Instant::now());
    }

    fn stop_at(&self, now: Instant) {
        if let Some(signal) = &self.signal {
            let mut state = signal
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Completion checks the actual clock as well as the worker's
            // receipt: an unscheduled deadline thread cannot authorize a late
            // success. Stop-before-expiry remains final on repeated calls.
            let claim_expiry = !state.stopped
                && !state.fired
                && now.saturating_duration_since(signal.started) >= signal.duration;
            state.fired |= claim_expiry;
            state.stopped = true;
            drop(state);
            signal.wake.notify_all();
            if claim_expiry {
                signal.cancellation.cancel();
            }
        }
    }

    fn fired(&self) -> bool {
        self.signal.as_ref().is_some_and(|signal| {
            signal
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fired
        })
    }
}

impl Drop for SearchTimeoutGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::runtime_async::CompatRuntime;
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
            SearchPhase::Reranked { .. } => "Reranked",
        }
    }

    fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(future)
    }

    /// Helper to construct a `ScoredResult` for unit tests.
    fn make_scored_result(doc_id: &str, score: f32) -> ScoredResult {
        ScoredResult {
            doc_id: doc_id.into(),
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
            skip_reason: None,
            hash_control_candidates: 0,
        }
    }

    fn build_test_bridge() -> (SearchBridge, TextProvider) {
        build_test_bridge_with_embedder(None)
    }

    fn build_test_bridge_with_embedder(
        search_embedder: Option<Arc<dyn Embedder>>,
    ) -> (SearchBridge, TextProvider) {
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
        let searcher = TwoTierSearcher::new(
            index,
            search_embedder.unwrap_or(fast),
            TwoTierConfig::default(),
        )
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
        {
            let cx = Cx::for_testing();
            let (results, metrics) = searcher
                .search_collect_with_text(&cx, &query, limit, |doc_id| text_provider(doc_id))
                .await
                .map_err(SearchBridgeError::Search)?;
            return Ok(SearchBridgeResult { results, metrics });
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
            crate::runtime_async::timeout(Duration::from_millis(100), token.cancelled())
                .await
                .expect("cancelled() should resolve immediately when already cancelled");
        });
    }

    #[test]
    fn test_cancellation_token_cancelled_wakes_on_cancel() {
        let token = BridgeCancellationToken::new();
        let token_clone = token.clone();
        run_async(async {
            let waiter = crate::runtime_async::task::spawn(async move {
                token_clone.cancelled().await;
                true
            });
            token.cancel();
            let result = crate::runtime_async::timeout(Duration::from_millis(200), waiter)
                .await
                .expect("should not timeout")
                .expect("task should not panic");
            assert!(result);
        });
    }

    #[test]
    fn test_cancellation_token_closes_check_to_registration_race() {
        let token = BridgeCancellationToken::new();
        run_async(async {
            token.cancelled_after_initial_check(|| token.cancel()).await;
        });
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancellation_wait_registers_wakes_without_self_polling() {
        use std::future::Future;

        struct WakeCount(AtomicU64);
        impl std::task::Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        for cancel_bridge in [false, true] {
            let token = BridgeCancellationToken::new();
            let cx = Cx::for_testing();
            let notifications = Arc::new(WakeCount(AtomicU64::new(0)));
            let waker = std::task::Waker::from(Arc::clone(&notifications));
            let mut task_cx = std::task::Context::from_waker(&waker);
            let mut waiter = Box::pin(token.cancelled_with_cx(&cx));
            assert!(waiter.as_mut().poll(&mut task_cx).is_pending());
            assert_eq!(
                notifications.0.load(Ordering::Relaxed),
                0,
                "no busy-loop wake"
            );
            if cancel_bridge {
                token.cancel();
            } else {
                cx.set_cancel_requested(true);
            }
            assert!(notifications.0.load(Ordering::Relaxed) > 0);
            assert_eq!(
                waiter.as_mut().poll(&mut task_cx),
                Poll::Ready(if cancel_bridge {
                    BridgeWaitOutcome::BridgeCancelled
                } else {
                    BridgeWaitOutcome::CxCancelled
                })
            );
        }
    }

    #[test]
    fn test_cancellation_token_wakes_multiple_racing_waiters() {
        let token = BridgeCancellationToken::new();
        let first_token = token.clone();
        let second_token = token.clone();
        run_async(async {
            let first = crate::runtime_async::task::spawn(async move {
                first_token.cancelled().await;
                1_u8
            });
            let second = crate::runtime_async::task::spawn(async move {
                second_token.cancelled().await;
                2_u8
            });

            token.cancel();
            let results = crate::runtime_async::timeout(Duration::from_millis(200), async {
                let first = first.await.expect("first waiter must not panic");
                let second = second.await.expect("second waiter must not panic");
                (first, second)
            })
            .await
            .expect("all racing waiters must observe sticky cancellation");
            assert_eq!(results, (1, 2));
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
    // Cancellation registration ownership
    // -----------------------------------------------------------------------

    #[test]
    fn search_registration_observes_cancellation_before_registration() {
        let cx = Cx::for_testing();
        let token = BridgeCancellationToken::new();
        token.cancel();

        let _registration = token.register_search(cx.clone());
        assert!(cx.is_cancel_requested());
    }

    #[test]
    fn search_registration_observes_cancel_and_revokes_dropped_links() {
        let cx = Cx::for_testing();
        let dropped_cx = Cx::for_testing();
        let token = BridgeCancellationToken::new();

        let registration = token.register_search(cx.clone());
        let dropped_registration = token.register_search(dropped_cx.clone());
        drop(dropped_registration);
        token.cancel();
        assert!(cx.is_cancel_requested());
        assert!(!dropped_cx.is_cancel_requested());
        drop(registration);
        assert!(token.state.registrations.lock().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Deadline ownership
    // -----------------------------------------------------------------------

    #[test]
    fn search_timeout_without_deadline_creates_no_signal() {
        let token = BridgeCancellationToken::new();
        let guard = SearchTimeoutGuard::start(None, token.clone()).unwrap();
        assert!(guard.signal.is_none());
        assert!(!guard.fired());
        assert!(!token.is_cancelled());
    }

    #[test]
    fn deadline_admission_failure_is_runtime_error_without_cancellation() {
        let token = BridgeCancellationToken::new();
        let error = SearchTimeoutGuard::start_with_spawn(
            Some(Duration::from_secs(60)),
            token.clone(),
            |_| Err(std::io::ErrorKind::WouldBlock.into()),
        )
        .err()
        .expect("refused thread admission must fail");
        assert!(matches!(
            error,
            SearchBridgeError::Runtime { message }
                if message == "deadline worker admission failed"
        ));
        assert!(!token.is_cancelled());
        assert!(token.state.deadline_signals.lock().unwrap().is_empty());
        SearchTimeoutGuard::start_with_spawn(None, token, |_| {
            panic!("ordinary search must not attempt worker admission")
        })
        .unwrap();
    }

    #[test]
    fn completion_checks_deadline_before_an_unscheduled_worker_runs() {
        for expired in [false, true] {
            let token = BridgeCancellationToken::new();
            let mut pending_work = None;
            let guard = SearchTimeoutGuard::start_with_spawn(
                Some(Duration::from_secs(60)),
                token.clone(),
                |work| {
                    pending_work = Some(work);
                    Ok(())
                },
            )
            .unwrap();
            let signal = Arc::clone(guard.signal.as_ref().unwrap());
            let deadline = signal.started + signal.duration;
            guard.stop_at(if expired { deadline } else { signal.started });
            assert_eq!(guard.fired(), expired);
            assert_eq!(token.is_cancelled(), expired);
            // Repeated stop/drop cannot retroactively expire a search that
            // completed on time, nor unclaim an expired deadline.
            guard.stop_at(deadline + Duration::from_nanos(1));
            assert_eq!(guard.fired(), expired);
            pending_work.take().expect("worker was admitted")();
            assert!(signal.state.lock().unwrap().settled);
            assert_eq!(token.is_cancelled(), expired);
        }
    }

    fn assert_deadline_thread_settled(signal: &SearchTimeoutSignal) {
        let state = signal.state.lock().unwrap();
        let (state, _) = signal
            .wake
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.settled)
            .unwrap();
        assert!(state.settled, "deadline thread must acknowledge shutdown");
    }

    #[test]
    fn search_timeout_fires_on_expiry() {
        let token = BridgeCancellationToken::new();
        let guard =
            SearchTimeoutGuard::start(Some(Duration::from_millis(20)), token.clone()).unwrap();
        assert_deadline_thread_settled(guard.signal.as_ref().unwrap());
        assert!(guard.fired());
        assert!(token.is_cancelled());
    }

    #[test]
    fn search_timeout_drop_wakes_long_deadline_without_cancelling_token() {
        let token = BridgeCancellationToken::new();
        let guard =
            SearchTimeoutGuard::start(Some(Duration::from_secs(60)), token.clone()).unwrap();
        let signal = Arc::clone(guard.signal.as_ref().unwrap());
        drop(guard);
        assert_deadline_thread_settled(&signal);
        let state = signal.state.lock().unwrap();
        assert!(state.stopped);
        assert!(!state.fired);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancellation_wakers_can_drop_search_owners_reentrantly() {
        use std::future::Future;

        struct DropOwnersOnWake {
            owners: Mutex<Option<(SearchCancellationGuard, SearchTimeoutGuard)>>,
            called: AtomicBool,
        }

        impl DropOwnersOnWake {
            fn release(&self) {
                let owners = self.owners.lock().unwrap().take();
                drop(owners);
                self.called.store(true, Ordering::Release);
            }
        }

        impl std::task::Wake for DropOwnersOnWake {
            fn wake(self: Arc<Self>) {
                self.release();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.release();
            }
        }

        for deadline_fires in [false, true] {
            let token = BridgeCancellationToken::new();
            let cx = Cx::for_testing();
            let registration = token.register_search(cx.clone());
            let owner = Arc::new(DropOwnersOnWake {
                owners: Mutex::new(None),
                called: AtomicBool::new(false),
            });
            let waker = std::task::Waker::from(Arc::clone(&owner));
            let mut task_cx = std::task::Context::from_waker(&waker);
            let mut cancellation = Box::pin(crate::runtime_async::wait_for_cancellation(&cx));
            assert!(cancellation.as_mut().poll(&mut task_cx).is_pending());

            // Hold the receiver lock while arming: even an immediately-fired
            // timer's waker must observe the installed owners before dropping.
            let mut owners = owner.owners.lock().unwrap();
            let duration = if deadline_fires {
                Duration::from_millis(1)
            } else {
                Duration::from_secs(60)
            };
            let deadline = SearchTimeoutGuard::start(Some(duration), token.clone()).unwrap();
            let signal = Arc::clone(deadline.signal.as_ref().unwrap());
            *owners = Some((registration, deadline));
            drop(owners);

            if deadline_fires {
                assert_deadline_thread_settled(&signal);
                assert!(signal.state.lock().unwrap().fired);
            } else {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let publisher = std::thread::spawn(move || {
                    token.cancel();
                    done_tx.send(()).unwrap();
                });
                done_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("reentrant cancellation must not deadlock");
                publisher.join().unwrap();
            }
            assert!(owner.called.load(Ordering::Acquire));
            assert!(owner.owners.lock().unwrap().is_none());
            assert_deadline_thread_settled(&signal);
            assert!(cancellation.as_mut().poll(&mut task_cx).is_ready());
        }
    }

    struct PendingSearchEmbedder {
        inner: HashEmbedder,
        entered: Arc<Mutex<Option<Cx>>>,
        dropped: Arc<AtomicBool>,
        token: BridgeCancellationToken,
        deadline: Arc<Mutex<Option<Arc<SearchTimeoutSignal>>>>,
        panic_on_poll: bool,
    }

    impl Embedder for PendingSearchEmbedder {
        fn identity(
            &self,
        ) -> frankensearch::SearchResult<&frankensearch::core::EmbeddingIdentityBundleV1> {
            // This test wrapper delays or panics before yielding any vector.
            // Retain the underlying producer identity so the existing index
            // admission succeeds and the cancellation path is actually polled.
            self.inner.identity()
        }

        fn embed<'a>(
            &'a self,
            cx: &'a Cx,
            _text: &'a str,
        ) -> frankensearch::SearchFuture<'a, Vec<f32>> {
            Box::pin(async move {
                struct DropReceipt(Arc<AtomicBool>);
                impl Drop for DropReceipt {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _receipt = DropReceipt(Arc::clone(&self.dropped));
                *self.entered.lock().unwrap() = Some(cx.clone());
                *self.deadline.lock().unwrap() =
                    self.token.state.deadline_signals.lock().unwrap()[0].upgrade();
                assert!(!self.panic_on_poll, "injected pending-search panic");
                std::future::pending().await
            })
        }

        fn dimension(&self) -> usize {
            self.inner.dimension()
        }

        fn id(&self) -> &str {
            self.inner.id()
        }

        fn model_name(&self) -> &str {
            self.inner.model_name()
        }

        fn is_semantic(&self) -> bool {
            self.inner.is_semantic()
        }

        fn category(&self) -> frankensearch::ModelCategory {
            // Select frankensearch's genuinely async await path. Hash-category
            // embedders are deliberately polled once on rayon and reject Pending.
            frankensearch::ModelCategory::ApiEmbedder
        }
    }

    #[test]
    fn dropped_and_panicking_search_futures_revoke_all_cancellation_owners() {
        use std::future::Future;

        for entrypoint in 0..3 {
            for panic_on_poll in [false, true] {
                let entered = Arc::new(Mutex::new(None));
                let dropped = Arc::new(AtomicBool::new(false));
                let token = BridgeCancellationToken::new();
                let deadline = Arc::new(Mutex::new(None));
                let embedder = Arc::new(PendingSearchEmbedder {
                    inner: HashEmbedder::default_256(),
                    entered: Arc::clone(&entered),
                    dropped: Arc::clone(&dropped),
                    token: token.clone(),
                    deadline: Arc::clone(&deadline),
                    panic_on_poll,
                });
                let (bridge, _) = build_test_bridge_with_embedder(Some(embedder));
                let caller_cx = Cx::for_testing();
                let request = SearchBridgeRequest::new("rust ownership", 5)
                    .with_cancellation(token.clone())
                    .with_timeout(Duration::from_secs(60));
                let phases = Arc::new(AtomicU64::new(0));
                let phase_receipt = Arc::clone(&phases);
                let callback = move |_| {
                    phase_receipt.fetch_add(1, Ordering::Relaxed);
                };
                let mut search = Box::pin(async {
                    match entrypoint {
                        0 => bridge.search(request, callback).await,
                        1 => {
                            bridge
                                .search_with_cx(caller_cx.clone(), request, callback)
                                .await
                        }
                        _ => {
                            bridge
                                .search_with_asupersync_cx(&caller_cx, request, callback)
                                .await
                        }
                    }
                });
                let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
                let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    search.as_mut().poll(&mut task_cx)
                }));
                if panic_on_poll {
                    assert!(polled.is_err(), "the injected embedder must run");
                } else {
                    assert!(polled.unwrap().is_pending(), "search must actually suspend");
                }
                let search_cx = entered.lock().unwrap().clone().expect("embedder entered");
                let signal = deadline.lock().unwrap().clone().expect("deadline observed");
                drop(search);
                assert!(
                    dropped.load(Ordering::Acquire),
                    "pending embedder was dropped"
                );
                assert!(token.state.registrations.lock().unwrap().is_empty());
                assert_deadline_thread_settled(&signal);
                assert!(!signal.state.lock().unwrap().fired);
                assert!(!token.is_cancelled());
                token.cancel();
                assert!(
                    !search_cx.is_cancel_requested(),
                    "dropped search link was revoked"
                );
                assert!(!caller_cx.is_cancel_requested());
                assert_eq!(phases.load(Ordering::Relaxed), 0);
            }
        }
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

    /// ft-xbnl0.2.3 Cx-first: `cancelled_with_cx` returns
    /// `BridgeCancelled` when the bridge token is cancelled
    /// first (fast path via is_cancelled() check).
    #[test]
    fn cancelled_with_cx_observes_bridge_cancel_fast_path() {
        let token = BridgeCancellationToken::new();
        token.cancel();
        let cx = crate::cx::for_testing();

        let outcome = run_async(token.cancelled_with_cx(&cx));
        assert_eq!(outcome, BridgeWaitOutcome::BridgeCancelled);
    }

    /// ft-xbnl0.2.3 Cx-first: `cancelled_with_cx` returns
    /// `CxCancelled` when the cx is pre-cancelled and the
    /// bridge is NOT cancelled.
    #[test]
    fn cancelled_with_cx_observes_cx_cancel_when_bridge_live() {
        let token = BridgeCancellationToken::new();
        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("pre-cancel cx for bridge wait"),
        );

        let outcome = run_async(token.cancelled_with_cx(&cx));
        assert_eq!(outcome, BridgeWaitOutcome::CxCancelled);
        assert!(
            !token.is_cancelled(),
            "bridge token should still be live — cx-cancel must not signal the bridge"
        );
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
        assert!(
            !token.is_cancelled(),
            "pre-cancelled caller is rejected before linking the request token"
        );

        log_test_event("test_bridge_cancellation_reverse", "done", started_at, "ok");
    }

    #[test]
    fn expired_caller_budgets_fail_before_search_or_deadline_admission() {
        let (bridge, _) = build_test_bridge();
        for budget in [
            crate::cx::Budget::new().with_poll_quota(0),
            crate::cx::Budget::new().with_deadline(asupersync::types::Time::ZERO),
        ] {
            let cx = Cx::for_testing_with_budget(budget);
            assert!(!cx.is_cancel_requested(), "budget has not been checked yet");
            let token = BridgeCancellationToken::new();
            let calls = Arc::new(AtomicU64::new(0));
            let provider_calls = Arc::clone(&calls);
            let phase_calls = Arc::clone(&calls);
            let request = SearchBridgeRequest::new("rust -missing", 5)
                .with_timeout(Duration::from_secs(60))
                .with_cancellation(token.clone())
                .with_text_provider(move |_| {
                    provider_calls.fetch_add(1, Ordering::Relaxed);
                    None
                });
            let result = run_async(bridge.search_with_asupersync_cx(&cx, request, move |_| {
                phase_calls.fetch_add(1, Ordering::Relaxed);
            }));
            assert!(matches!(result, Err(SearchBridgeError::Cancelled { .. })));
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert!(token.state.registrations.lock().unwrap().is_empty());
            assert!(token.state.deadline_signals.lock().unwrap().is_empty());
            assert!(
                !token.is_cancelled(),
                "rejected caller never linked the token"
            );
        }
    }

    #[test]
    fn test_bridge_timeout() {
        let started_at = Instant::now();
        let (bridge, text_provider) = build_test_bridge();
        let token = BridgeCancellationToken::new();
        let provider_token = token.clone();
        let slow_provider: TextProvider = Arc::new(move |doc_id| {
            // A synchronous provider occupies the search poll until the real
            // deadline fires. Async-only timeout plumbing cannot pass this.
            let signal = provider_token.state.deadline_signals.lock().unwrap()[0]
                .upgrade()
                .expect("active deadline");
            assert_deadline_thread_settled(&signal);
            text_provider(doc_id)
        });

        // Use a negation term so frankensearch invokes text_provider for each
        // result (exclusion filtering), holding the poll across the deadline.
        let request = SearchBridgeRequest::new("vector retrieval -nonexistent", 6)
            .with_text_provider_arc(slow_provider)
            .with_cancellation(token)
            .with_timeout(Duration::from_millis(100));

        let result = run_async(bridge.search(request, |_| {}));
        assert!(
            matches!(result, Err(SearchBridgeError::Timeout { timeout_ms: 100 })),
            "deadline must win over late provider completion: {result:?}"
        );

        log_test_event("test_bridge_timeout", "done", started_at, "ok");
    }

    #[test]
    fn ambient_search_timeout_and_token_cancellation_preserve_native_task_context() {
        let (bridge, text_provider) = build_test_bridge();
        for cancel_token in [false, true] {
            let bridge = bridge.clone();
            let text_provider = Arc::clone(&text_provider);
            run_async(async move {
                let task = crate::runtime_async::task::spawn(async move {
                    let caller = Cx::current().expect("native task installs current Cx");
                    caller.checkpoint().expect("caller starts live");
                    let token = BridgeCancellationToken::new();
                    if cancel_token {
                        token.cancel();
                    }
                    let provider_token = token.clone();
                    let request = SearchBridgeRequest::new("vector retrieval -nonexistent", 6)
                        .with_cancellation(token)
                        .with_timeout(Duration::from_millis(100))
                        .with_text_provider(move |doc_id| {
                            let signal = provider_token.state.deadline_signals.lock().unwrap()[0]
                                .upgrade()
                                .expect("active request deadline");
                            assert_deadline_thread_settled(&signal);
                            text_provider(doc_id)
                        });
                    let result = bridge.search(request, |_| {}).await;
                    if cancel_token {
                        assert!(matches!(result, Err(SearchBridgeError::Cancelled { .. })));
                    } else {
                        assert!(matches!(
                            result,
                            Err(SearchBridgeError::Timeout { timeout_ms: 100 })
                        ));
                    }
                    assert!(
                        !caller.is_cancel_requested(),
                        "request must not cancel task"
                    );
                    caller.checkpoint().expect("surrounding task can continue");
                    42
                });
                assert_eq!(task.await.expect("native task finishes normally"), 42);
            });
        }
    }

    #[test]
    fn synchronous_provider_caller_cancellation_prevents_later_phase_publication() {
        let (bridge, text_provider) = build_test_bridge();
        for cancel_caller in [false, true] {
            let caller = Cx::for_testing();
            let provider_caller = caller.clone();
            let text_provider = Arc::clone(&text_provider);
            let entered = Arc::new(AtomicBool::new(false));
            let provider_entered = Arc::clone(&entered);
            let phases = Arc::new(AtomicU64::new(0));
            let phase_count = Arc::clone(&phases);
            let request = SearchBridgeRequest::new("vector retrieval -nonexistent", 6)
                .with_text_provider(move |doc_id| {
                    provider_entered.store(true, Ordering::Release);
                    if cancel_caller {
                        provider_caller.set_cancel_requested(true);
                    }
                    text_provider(doc_id)
                });
            let result = run_async(
                bridge.search_with_asupersync_cx(&caller, request, move |_| {
                    phase_count.fetch_add(1, Ordering::Relaxed);
                }),
            );
            assert!(entered.load(Ordering::Acquire), "synchronous provider ran");
            if cancel_caller {
                assert!(matches!(result, Err(SearchBridgeError::Cancelled { .. })));
                assert_eq!(phases.load(Ordering::Relaxed), 0);
            } else {
                assert!(result.is_ok(), "uncancelled control succeeds: {result:?}");
                assert!(phases.load(Ordering::Relaxed) > 0);
                assert!(!caller.is_cancel_requested());
            }
        }
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
                tasks.push(crate::runtime_async::task::spawn(async move {
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

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for the Cx-first entry point
    // (ft-xbnl0.2.2 / search_bridge slice)
    //
    // These tests pin pre-cancellation and one-way caller authority under
    // deterministic scheduling. Suspended real-search ownership is covered
    // separately by the pending embedder control above.
    // -------------------------------------------------------------------------

    mod labruntime_search_bridge {
        use super::*;

        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime got stuck; termination: {:?}",
                report.termination,
            );
        }

        fn cancelled_request_cx(msg: &'static str) -> crate::cx::Cx {
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(crate::outcome::CancelKind::User, Some(msg));
            cx
        }

        /// 1. `search_with_asupersync_cx` short-circuits a pre-cancelled
        ///    asupersync Cx with `SearchBridgeError::Cancelled` before
        ///    touching the underlying searcher. Mirrors pool.rs and
        ///    caut.rs Cx-first contracts.
        #[test]
        fn search_with_asupersync_cx_precancelled_short_circuits_under_labruntime() {
            run_lab(4001, || async move {
                let (bridge, text_provider) = build_test_bridge();
                let request =
                    SearchBridgeRequest::new("qwen", 3).with_text_provider_arc(text_provider);
                let cx = cancelled_request_cx("wa-search-bridge cancel");
                let err = bridge
                    .search_with_asupersync_cx(&cx, request, |_phase| {})
                    .await
                    .expect_err("precancelled Cx must short-circuit");
                assert!(
                    matches!(err, SearchBridgeError::Cancelled { .. }),
                    "expected Cancelled, got {err:?}"
                );
            });
        }

        #[test]
        fn wait_observes_precancelled_cx_without_mutating_token_under_labruntime() {
            run_lab(4002, || async move {
                let cx = cancelled_request_cx("pre-cancel watcher");
                let token = BridgeCancellationToken::new();
                assert!(!token.is_cancelled());
                assert_eq!(
                    token.cancelled_with_cx(&cx).await,
                    BridgeWaitOutcome::CxCancelled
                );
                assert!(!token.is_cancelled());
            });
        }

        #[test]
        fn precancelled_request_does_not_cancel_caller_under_labruntime() {
            run_lab(4003, || async move {
                let cx = crate::cx::Cx::for_testing_with_budget(crate::cx::Budget::new());
                let token = BridgeCancellationToken::new();
                token.cancel();
                let (bridge, _) = build_test_bridge();
                let result = bridge
                    .search_with_asupersync_cx(
                        &cx,
                        SearchBridgeRequest::new("rust", 5).with_cancellation(token),
                        |_| {},
                    )
                    .await;
                assert!(
                    matches!(result, Err(SearchBridgeError::Cancelled { .. })),
                    "pre-cancelled request must not search"
                );
                assert!(
                    !cx.is_cancel_requested(),
                    "token cancellation must not retroactively cancel the asupersync Cx"
                );
            });
        }

        /// 4. `BridgeCancellationToken` defaults are not cancelled and
        ///    `cancel()` flips the flag atomically.
        #[test]
        fn bridge_cancellation_token_defaults_under_labruntime() {
            run_lab(4004, || async move {
                let token = BridgeCancellationToken::new();
                assert!(!token.is_cancelled());
                token.cancel();
                assert!(token.is_cancelled());
                // Idempotent.
                token.cancel();
                assert!(token.is_cancelled());
            });
        }

        /// 5. `SearchBridgeRequest` default field values are stable.
        #[test]
        fn search_bridge_request_defaults_under_labruntime() {
            run_lab(4005, || async move {
                let req = SearchBridgeRequest::new("query", 10);
                assert_eq!(req.query, "query");
                assert_eq!(req.limit, 10);
                assert!(req.timeout.is_none());
                assert!(req.cancellation.is_none());
            });
        }

        /// 6. `SearchBridgeError::Cancelled` carries its reason through
        ///    Display so operator-visible logs stay informative.
        #[test]
        fn search_bridge_error_display_under_labruntime() {
            run_lab(4006, || async move {
                let err = SearchBridgeError::Cancelled {
                    reason: "wa-search-bridge display".into(),
                };
                let text = format!("{err}");
                assert!(
                    text.contains("wa-search-bridge display"),
                    "reason should surface in Display: {text}"
                );
            });
        }
    }

    // ── br-ft-qfklb: SearchBridgeRequest validation tests ─────────────

    /// br-ft-qfklb: limit=0 must reject with a structured
    /// ValidationError. Pre-fix limit=0 silently produced an empty-
    /// result search, paying the search cost for no gain.
    #[test]
    fn validate_rejects_zero_limit_ft_qfklb() {
        let req = SearchBridgeRequest::new("query", 0);
        let err = req.validate().expect_err("limit=0 must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-qfklb") && msg.contains("limit must be >="),
            "ft-qfklb: error must reference the bead + the lower-bound rule; got {msg}"
        );
        assert!(
            msg.contains("(got 0)"),
            "ft-qfklb: error must cite the rejected value; got {msg}"
        );
    }

    /// br-ft-qfklb: limit > SEARCH_BRIDGE_MAX_LIMIT must reject.
    /// Pre-fix limit=usize::MAX would pass through to frankensearch
    /// and trigger unbounded allocation downstream.
    #[test]
    fn validate_rejects_above_max_limit_ft_qfklb() {
        let req = SearchBridgeRequest::new("query", SEARCH_BRIDGE_MAX_LIMIT + 1);
        let err = req.validate().expect_err("limit > MAX must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-qfklb") && msg.contains("limit must be <="),
            "ft-qfklb: error must reference the bead + the upper-bound rule; got {msg}"
        );
    }

    /// br-ft-qfklb: limit=usize::MAX is the practical worst case for
    /// the upper-bound check. Pin it explicitly so a future cap
    /// change doesn't accidentally let MAX through.
    #[test]
    fn validate_rejects_usize_max_limit_ft_qfklb() {
        let req = SearchBridgeRequest::new("query", usize::MAX);
        let err = req.validate().expect_err("limit=usize::MAX must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-qfklb"),
            "ft-qfklb: error must reference the bead; got {msg}"
        );
    }

    /// br-ft-qfklb: timeout=Duration::ZERO must reject when set.
    /// Pre-fix this would fire the timeout machinery immediately,
    /// returning a confusing 'timeout' on every call.
    #[test]
    fn validate_rejects_zero_timeout_ft_qfklb() {
        let req = SearchBridgeRequest::new("query", 10).with_timeout(Duration::ZERO);
        let err = req.validate().expect_err("timeout=ZERO must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-qfklb") && msg.contains("timeout must be >="),
            "ft-qfklb: error must reference the bead + the lower-bound rule; got {msg}"
        );
    }

    /// br-ft-qfklb: timeout > SEARCH_BRIDGE_MAX_TIMEOUT must reject.
    /// Pre-fix Duration::from_secs(u64::MAX) was accepted, allowing
    /// a misconfigured caller to pin the bridge for billions of years.
    #[test]
    fn validate_rejects_above_max_timeout_ft_qfklb() {
        let req = SearchBridgeRequest::new("query", 10)
            .with_timeout(SEARCH_BRIDGE_MAX_TIMEOUT + Duration::from_secs(1));
        let err = req.validate().expect_err("timeout > MAX must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("br-ft-qfklb") && msg.contains("timeout must be <="),
            "ft-qfklb: error must reference the bead + the upper-bound rule; got {msg}"
        );
    }

    /// br-ft-qfklb: a well-formed request (limit in range, no
    /// timeout) must validate cleanly. Vacuous-regression guard
    /// against the new gate false-positiving on the legitimate
    /// happy path.
    #[test]
    fn validate_accepts_well_formed_request_ft_qfklb() {
        let req = SearchBridgeRequest::new("query", 10);
        req.validate()
            .expect("well-formed request without timeout must validate");

        let req_with_timeout =
            SearchBridgeRequest::new("query", 10).with_timeout(Duration::from_secs(30));
        req_with_timeout
            .validate()
            .expect("well-formed request with valid timeout must validate");

        // Boundary values: MIN and MAX inclusive must pass.
        let min_req = SearchBridgeRequest::new("query", SEARCH_BRIDGE_MIN_LIMIT)
            .with_timeout(SEARCH_BRIDGE_MIN_TIMEOUT);
        min_req.validate().expect("MIN-boundary values must pass");

        let max_req = SearchBridgeRequest::new("query", SEARCH_BRIDGE_MAX_LIMIT)
            .with_timeout(SEARCH_BRIDGE_MAX_TIMEOUT);
        max_req.validate().expect("MAX-boundary values must pass");
    }

    /// br-ft-qfklb: property-style sweep across the four
    /// equivalence classes:
    ///   - in-range limit + no timeout → Ok
    ///   - in-range limit + in-range timeout → Ok
    ///   - out-of-range limit (low or high) → Err
    ///   - in-range limit + out-of-range timeout (low or high) → Err
    ///
    /// Pins the cross-product invariant against drift in either
    /// constant.
    #[test]
    fn validate_property_sweep_ft_qfklb() {
        // In-range limits — no timeout
        for &limit in &[
            SEARCH_BRIDGE_MIN_LIMIT,
            10,
            100,
            1000,
            SEARCH_BRIDGE_MAX_LIMIT,
        ] {
            let req = SearchBridgeRequest::new("q", limit);
            assert!(
                req.validate().is_ok(),
                "ft-qfklb: in-range limit={limit} must pass"
            );
        }

        // Out-of-range limits
        for &limit in &[0_usize, SEARCH_BRIDGE_MAX_LIMIT + 1, usize::MAX] {
            let req = SearchBridgeRequest::new("q", limit);
            assert!(
                req.validate().is_err(),
                "ft-qfklb: out-of-range limit={limit} must reject"
            );
        }

        // In-range timeouts (with valid limit)
        for &timeout in &[
            SEARCH_BRIDGE_MIN_TIMEOUT,
            Duration::from_millis(100),
            Duration::from_secs(30),
            SEARCH_BRIDGE_MAX_TIMEOUT,
        ] {
            let req = SearchBridgeRequest::new("q", 10).with_timeout(timeout);
            assert!(
                req.validate().is_ok(),
                "ft-qfklb: in-range timeout={timeout:?} must pass"
            );
        }

        // Out-of-range timeouts (with valid limit)
        for &timeout in &[
            Duration::ZERO,
            SEARCH_BRIDGE_MAX_TIMEOUT + Duration::from_secs(1),
            Duration::from_secs(u64::MAX / 2), // effectively unbounded
        ] {
            let req = SearchBridgeRequest::new("q", 10).with_timeout(timeout);
            assert!(
                req.validate().is_err(),
                "ft-qfklb: out-of-range timeout={timeout:?} must reject"
            );
        }
    }
}
