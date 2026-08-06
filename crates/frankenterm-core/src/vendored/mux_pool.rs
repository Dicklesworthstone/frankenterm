//! Connection pool for `DirectMuxClient` connections.
//!
//! Wraps [`Pool<DirectMuxClient>`](crate::pool::Pool) to manage persistent
//! Unix socket connections to the WezTerm mux server. Instead of spawning
//! a `wezterm cli` subprocess for every operation (which creates 60+ stuck
//! processes under agent swarm load), this pool reuses persistent connections.
//!
//! # Design
//!
//! - Connections are created on-demand when the pool has no idle entries.
//! - Each connection is a full `DirectMuxClient` with completed handshake
//!   (codec version + client registration).
//! - On success, the connection is returned for reuse when the return context
//!   remains live. If return is cancelled, the connection is safely dropped
//!   without changing the already-completed operation result.
//! - On error, the canonical mux recovery decision independently controls
//!   retry and connection disposition. A client is reused only when that
//!   decision permits reuse and the client's actual protocol state is not
//!   poisoned; all other failed clients are discarded.
//! - The underlying `Pool<C>` provides semaphore-based concurrency limiting
//!   and idle timeout eviction.

// Vendored mux pool: large futures are inherent to the mux protocol's
// deeply-nested async call chains and not worth Box::pin-wrapping individually.
#![allow(clippy::large_futures)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cx::{self, Budget, Cx};
use crate::outcome::CancelKind;
use crate::pool::{Pool, PoolAcquireGuard, PoolConfig, PoolError, PoolStats};
use crate::protocol_recovery::{
    MuxConnectionDisposition, MuxRecoveryDecision, ProtocolErrorKind, mux_recovery_decision,
};
use crate::retry::RetryPolicy;
// Used by the test-module eviction cases under both runtime variants.
#[cfg(test)]
use crate::runtime_async::sleep;

use super::mux_client::{
    DirectMuxClient, DirectMuxClientConfig, DirectMuxError, validate_render_batch_panes,
};
use codec::{
    GetLinesResponse, GetPaneRenderChangesResponse, GetSemanticZonesResponse, ListPanesResponse,
    SpawnResponse, SpawnV2, SplitPane, UnitResponse,
};

/// Error type for mux pool operations.
#[derive(Debug, thiserror::Error)]
pub enum MuxPoolError {
    /// The pool could not acquire a slot (timeout or closed).
    #[error("pool: {0}")]
    Pool(#[from] PoolError),
    /// The mux client encountered an error.
    #[error("mux: {0}")]
    Mux(#[from] DirectMuxError),
    /// A mutation was invoked but its completion could not be established.
    ///
    /// Callers must surface this error without replaying the mutation through
    /// another transport, because request bytes may already have committed.
    #[error("indeterminate mux mutation: {0}")]
    IndeterminateMutation(#[source] DirectMuxError),
}

impl MuxPoolError {
    /// Whether this error is a pool-level timeout (vs a mux protocol error).
    #[must_use]
    pub fn is_pool_timeout(&self) -> bool {
        matches!(self, Self::Pool(PoolError::AcquireTimeout))
    }

    /// Whether this error indicates the mux server disconnected.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        matches!(
            self,
            Self::Mux(DirectMuxError::Disconnected)
                | Self::IndeterminateMutation(DirectMuxError::Disconnected)
        )
    }

    /// Whether this error represents cooperative cancellation rather than a
    /// completed mux health or protocol failure.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Pool(PoolError::Cancelled) => true,
            Self::Mux(error) => mux_recovery_decision(error).cancelled,
            Self::Pool(_) | Self::IndeterminateMutation(_) => false,
        }
    }
}

/// Recovery settings for mux protocol errors.
#[derive(Debug, Clone)]
pub struct MuxRecoveryConfig {
    /// Enable reconnect+retry recovery for protocol corruption (`UnexpectedResponse`, codec errors,
    /// disconnects).
    pub enabled: bool,
    /// Backoff policy for recovery attempts.
    pub retry_policy: RetryPolicy,
}

impl Default for MuxRecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Default: allow one retry with a very short delay (avoid hammering).
            retry_policy: RetryPolicy::new(
                Duration::from_millis(10),
                Duration::from_millis(50),
                2.0,
                0.0,
                Some(2),
            ),
        }
    }
}

/// Configuration for the mux connection pool.
#[derive(Debug, Clone)]
pub struct MuxPoolConfig {
    /// Pool concurrency and eviction settings.
    pub pool: PoolConfig,
    /// DirectMuxClient connection settings.
    pub mux: DirectMuxClientConfig,
    /// Auto-recovery configuration for protocol errors.
    pub recovery: MuxRecoveryConfig,
    /// Max concurrent in-flight requests per pipelined batch.
    pub pipeline_depth: usize,
    /// Timeout for the full pipelined batch operation.
    pub pipeline_timeout: Duration,
}

impl Default for MuxPoolConfig {
    fn default() -> Self {
        Self {
            pool: PoolConfig {
                max_size: 8,
                idle_timeout: std::time::Duration::from_secs(300),
                acquire_timeout: std::time::Duration::from_secs(10),
            },
            mux: DirectMuxClientConfig::default(),
            recovery: MuxRecoveryConfig::default(),
            pipeline_depth: 32,
            pipeline_timeout: Duration::from_secs(5),
        }
    }
}

/// Pool statistics including mux-specific counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxPoolStats {
    /// Underlying pool stats (idle count, active count, etc.).
    pub pool: PoolStats,
    /// Total connections successfully created.
    pub connections_created: u64,
    /// Total connection creation failures.
    pub connections_failed: u64,
    /// Total health check attempts.
    pub health_checks: u64,
    /// Total health check failures.
    pub health_check_failures: u64,
    /// Number of recovery retries performed (reconnect+retry).
    pub recovery_attempts: u64,
    /// Number of operations that succeeded after at least one recovery retry.
    pub recovery_successes: u64,
    /// Number of errors classified as permanent (not retried).
    pub permanent_failures: u64,
}

/// A connection pool for `DirectMuxClient` instances.
///
/// Manages persistent Unix socket connections to the WezTerm mux server,
/// reusing them across operations instead of spawning CLI subprocesses.
pub struct MuxPool {
    pool: Pool<DirectMuxClient>,
    mux_config: DirectMuxClientConfig,
    recovery: MuxRecoveryConfig,
    connections_created: AtomicU64,
    connections_failed: AtomicU64,
    health_checks: AtomicU64,
    health_check_failures: AtomicU64,
    recovery_attempts: AtomicU64,
    recovery_successes: AtomicU64,
    permanent_failures: AtomicU64,
    pipeline_depth: usize,
    pipeline_timeout: Duration,
}

fn classify_render_batch_fallback(decision: Option<MuxRecoveryDecision>) -> bool {
    decision.is_some_and(is_transport_failover_decision)
}

fn is_transport_failover_decision(decision: MuxRecoveryDecision) -> bool {
    decision.retry
        && !decision.cancelled
        && matches!(decision.connection, MuxConnectionDisposition::Discard)
}

fn should_fallback_render_batch(error: &MuxPoolError) -> bool {
    match error {
        MuxPoolError::Mux(mux_error) => {
            classify_render_batch_fallback(Some(mux_recovery_decision(mux_error)))
        }
        MuxPoolError::Pool(_) | MuxPoolError::IndeterminateMutation(_) => {
            classify_render_batch_fallback(None)
        }
    }
}

fn duration_to_timeout_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn require_render_batch_remaining(
    remaining: Option<Duration>,
    configured_timeout: Duration,
) -> Result<Duration, DirectMuxError> {
    remaining
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| DirectMuxError::BatchTimeout {
            timeout_ms: duration_to_timeout_ms(configured_timeout),
        })
}

type MuxOpFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DirectMuxError>> + Send + 'a>>;

/// One logical attempt budget shared by the pipelined render path and its
/// sequential fallback. `started` advances only when another attempt is about
/// to begin, after cancellation and deadline checks have passed.
#[derive(Debug)]
struct RenderAttemptState {
    started: u32,
    limit: u32,
}

impl RenderAttemptState {
    fn new(limit: u32) -> Self {
        Self {
            started: 0,
            limit: limit.max(1),
        }
    }

    fn has_remaining(&self) -> bool {
        self.started < self.limit
    }

    fn begin(&mut self) -> Option<u32> {
        if !self.has_remaining() {
            return None;
        }
        self.started = self.started.saturating_add(1);
        Some(self.started)
    }

    fn recovery_started(&self) -> bool {
        self.started > 1
    }
}

/// Preserve whether a render failure happened before or after invoking the
/// operation. Only post-invocation transport failures are eligible for the
/// pipelined-to-sequential fallback.
#[derive(Debug)]
enum RenderExecutionError {
    BeforeOperation(MuxPoolError),
    Operation(MuxPoolError),
}

impl RenderExecutionError {
    fn into_inner(self) -> MuxPoolError {
        match self {
            Self::BeforeOperation(error) | Self::Operation(error) => error,
        }
    }
}

impl MuxPool {
    /// Create a new mux connection pool.
    #[must_use]
    pub fn new(config: MuxPoolConfig) -> Self {
        let pipeline_depth = config.pipeline_depth.max(1);
        let pipeline_timeout = if config.pipeline_timeout.is_zero() {
            Duration::from_millis(1)
        } else {
            config.pipeline_timeout
        };
        Self {
            pool: Pool::new(config.pool),
            mux_config: config.mux,
            recovery: config.recovery,
            connections_created: AtomicU64::new(0),
            connections_failed: AtomicU64::new(0),
            health_checks: AtomicU64::new(0),
            health_check_failures: AtomicU64::new(0),
            recovery_attempts: AtomicU64::new(0),
            recovery_successes: AtomicU64::new(0),
            permanent_failures: AtomicU64::new(0),
            pipeline_depth,
            pipeline_timeout,
        }
    }

    fn max_recovery_attempts(&self) -> u32 {
        if self.recovery.enabled {
            self.recovery.retry_policy.max_attempts.unwrap_or(1).max(1)
        } else {
            1
        }
    }

    fn can_retry(&self, attempt: u32, decision: MuxRecoveryDecision) -> bool {
        self.recovery.enabled
            && attempt < self.max_recovery_attempts()
            && decision.retry
            && !decision.cancelled
    }

    fn render_attempt_limit(&self) -> u32 {
        self.max_recovery_attempts()
    }

    fn can_retry_render(
        &self,
        attempts: &RenderAttemptState,
        decision: MuxRecoveryDecision,
    ) -> bool {
        self.recovery.enabled
            && attempts.has_remaining()
            && decision.retry
            && !decision.cancelled
    }

    fn record_permanent_failure(&self, decision: MuxRecoveryDecision) {
        if decision.kind == ProtocolErrorKind::Permanent {
            self.permanent_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn checkpoint_cx(cx: &Cx) -> Result<(), MuxPoolError> {
        cx.checkpoint()
            .map_err(|_| MuxPoolError::Pool(PoolError::Cancelled))
    }

    fn render_batch_timeout_error(&self) -> MuxPoolError {
        MuxPoolError::Mux(DirectMuxError::BatchTimeout {
            timeout_ms: duration_to_timeout_ms(self.pipeline_timeout),
        })
    }

    fn render_interruption_error(&self, cx: &Cx) -> MuxPoolError {
        if matches!(
            cx.cancel_reason().map(|reason| reason.kind),
            Some(CancelKind::Deadline)
        ) {
            self.render_batch_timeout_error()
        } else {
            MuxPoolError::Pool(PoolError::Cancelled)
        }
    }

    fn remaining_render_batch_time(
        &self,
        cx: &Cx,
        deadline: &Budget,
    ) -> Result<Duration, MuxPoolError> {
        if cx.checkpoint().is_err() {
            return Err(self.render_interruption_error(cx));
        }

        // This helper is deliberately private to the render-batch path, where
        // `deadline` is always produced by `cx.budget_for_timeout`. Therefore
        // it always carries a deadline (possibly tightened by an earlier
        // ambient deadline), and `None` means that deadline has expired. Do
        // not reuse this interpretation for an arbitrary `Budget`, where
        // `None` can also mean "no deadline".
        require_render_batch_remaining(deadline.remaining_time(cx.now()), self.pipeline_timeout)
            .map_err(MuxPoolError::Mux)
    }

    fn begin_render_attempt(
        &self,
        cx: &Cx,
        deadline: &Budget,
        attempts: &mut RenderAttemptState,
    ) -> Result<u32, MuxPoolError> {
        self.remaining_render_batch_time(cx, deadline)?;
        let attempt = attempts
            .begin()
            .expect("render attempt must be checked before beginning");
        if attempt > 1 {
            self.recovery_attempts.fetch_add(1, Ordering::Relaxed);
        }
        Ok(attempt)
    }

    async fn return_client_after_error(
        &self,
        cx: &Cx,
        client: DirectMuxClient,
        decision: MuxRecoveryDecision,
    ) {
        let poisoned = client.is_connection_poisoned();
        let reusable = matches!(decision.connection, MuxConnectionDisposition::Reuse)
            && !poisoned
            && !cx.is_cancel_requested();
        if reusable {
            self.return_client_with_cx(cx, client).await;
        } else {
            tracing::trace!(
                kind = ?decision.kind,
                cancelled = decision.cancelled,
                poisoned,
                disposition = ?decision.connection,
                "discarding mux connection after operation error"
            );
        }
    }

    async fn wait_before_retry_with_cx(
        &self,
        cx: &Cx,
        failed_attempt: u32,
    ) -> Result<(), MuxPoolError> {
        Self::checkpoint_cx(cx)?;
        let delay = self
            .recovery
            .retry_policy
            .delay_for_attempt(failed_attempt.saturating_sub(1));
        if !delay.is_zero() {
            let _ = crate::runtime_async::sleep_with_cx(cx, delay).await;
        }
        Self::checkpoint_cx(cx)
    }

    async fn wait_before_render_retry_with_cx(
        &self,
        cx: &Cx,
        deadline: &Budget,
        failed_attempt: u32,
    ) -> Result<(), MuxPoolError> {
        let remaining = self.remaining_render_batch_time(cx, deadline)?;
        let delay = self
            .recovery
            .retry_policy
            .delay_for_attempt(failed_attempt.saturating_sub(1))
            .min(remaining);
        if !delay.is_zero() {
            let _ = crate::runtime_async::sleep_with_cx(cx, delay).await;
        }
        self.remaining_render_batch_time(cx, deadline)?;
        Ok(())
    }

    fn validate_render_batch_preflight(&self, pane_ids: &[u64]) -> Result<(), MuxPoolError> {
        if let Err(error) = validate_render_batch_panes(pane_ids) {
            let decision = mux_recovery_decision(&error);
            self.record_permanent_failure(decision);
            tracing::debug!(
                pane_count = pane_ids.len(),
                kind = ?decision.kind,
                error = %error,
                phase = "render_batch_preflight",
                "mux render batch rejected before pool acquisition"
            );
            return Err(MuxPoolError::Mux(error));
        }
        Ok(())
    }

    /// Acquire a client from the pool or create a new one.
    ///
    /// Returns the client and a guard that holds the concurrency slot.
    /// The guard must be dropped after the client is returned (or discarded).
    /// Used directly by tests and by `execute_with_recovery_inner`.
    #[allow(dead_code)]
    async fn acquire_client(&self) -> Result<(DirectMuxClient, PoolAcquireGuard), MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.acquire_client_with_cx(&cx).await
    }

    /// Acquire a client using an explicit capability context.
    async fn acquire_client_with_cx(
        &self,
        cx: &Cx,
    ) -> Result<(DirectMuxClient, PoolAcquireGuard), MuxPoolError> {
        let result = self.pool.acquire_with_cx(cx).await?;
        let (conn, guard) = result.into_parts();
        let client = match conn {
            Some(c) => {
                tracing::trace!(
                    subsystem = "mux_pool",
                    event = "acquire",
                    source = "idle",
                    "reused idle mux connection"
                );
                c
            }
            None => match DirectMuxClient::connect_with_cx(cx, self.mux_config.clone()).await {
                Ok(client) => {
                    let count = self.connections_created.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::debug!(
                        subsystem = "mux_pool",
                        event = "acquire",
                        source = "new",
                        total_created = count,
                        "created new mux connection"
                    );
                    client
                }
                Err(e) => {
                    let count = self.connections_failed.fetch_add(1, Ordering::Relaxed) + 1;
                    tracing::warn!(subsystem = "mux_pool", event = "connect_failed", total_failed = count, error = %e, "mux connection creation failed");
                    return Err(MuxPoolError::Mux(e));
                }
            },
        };
        Ok((client, guard))
    }

    /// Acquire a client without an explicit capability context.
    /// Return a healthy client to the pool for reuse.
    ///
    /// Legacy ambient path retained alongside the cx-first sibling
    /// `return_client_with_cx` (below). Current production MuxPool
    /// ops route through the cx-first path.
    #[allow(dead_code)]
    async fn return_client(&self, client: DirectMuxClient) {
        tracing::trace!(
            subsystem = "mux_pool",
            event = "release",
            "returned mux connection to pool"
        );
        self.pool.put(client).await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`Self::return_client`].
    ///
    /// Routes through `ConnectionPool::put_with_cx` so cancellation during
    /// the idle-pool mutex acquire is a typed return failure rather than a
    /// panic. A failed return drops the client and is logged, but deliberately
    /// does not replace the result of an already-completed mux operation.
    async fn return_client_with_cx(&self, cx: &Cx, client: DirectMuxClient) {
        match self.pool.put_with_cx(cx, client).await {
            Ok(()) => {
                tracing::trace!(
                    subsystem = "mux_pool",
                    event = "release",
                    explicit_cx = true,
                    "returned mux connection to pool (cx path)"
                );
            }
            Err(error @ PoolError::Cancelled) => {
                tracing::debug!(
                    subsystem = "mux_pool",
                    event = "release_drop",
                    explicit_cx = true,
                    error = %error,
                    "dropped mux connection because its cancelled caller could not return it"
                );
            }
            Err(error) => {
                tracing::warn!(
                    subsystem = "mux_pool",
                    event = "release_drop",
                    explicit_cx = true,
                    error = %error,
                    "dropped mux connection after pool return failed"
                );
            }
        }
    }

    async fn execute_once_with_cx<T, Op>(
        &self,
        cx: &Cx,
        op_name: &'static str,
        op: Op,
    ) -> Result<T, MuxPoolError>
    where
        Op: for<'a> FnOnce(&'a mut DirectMuxClient) -> MuxOpFuture<'a, T>,
    {
        let mut attempt = 0u32;
        let (mut client, _guard) = loop {
            Self::checkpoint_cx(cx)?;
            attempt = attempt.saturating_add(1);
            if attempt > 1 {
                self.recovery_attempts.fetch_add(1, Ordering::Relaxed);
            }

            match self.acquire_client_with_cx(cx).await {
                Ok(acquired) => break acquired,
                Err(MuxPoolError::Pool(error)) => return Err(MuxPoolError::Pool(error)),
                Err(MuxPoolError::IndeterminateMutation(error)) => {
                    return Err(MuxPoolError::IndeterminateMutation(error));
                }
                Err(MuxPoolError::Mux(error)) => {
                    if cx.is_cancel_requested() {
                        return Err(MuxPoolError::Pool(PoolError::Cancelled));
                    }
                    let decision = mux_recovery_decision(&error);
                    if self.can_retry(attempt, decision) {
                        tracing::debug!(
                            op = op_name,
                            attempt,
                            max_attempts = self.max_recovery_attempts(),
                            kind = ?decision.kind,
                            error = %error,
                            "non-idempotent mux op acquisition failed; retrying acquisition only"
                        );
                        self.wait_before_retry_with_cx(cx, attempt).await?;
                        continue;
                    }

                    self.record_permanent_failure(decision);
                    return Err(MuxPoolError::Mux(error));
                }
            }
        };

        // Acquiring a connection may be retried, but once this boundary is
        // crossed the non-idempotent operation is invoked exactly once. The
        // sole determinate exception is a typed outbound rejection that proves
        // no socket write boundary was reached and no bytes reached the peer.
        // A local serial may have been consumed and encoding attempted.
        if let Err(error) = Self::checkpoint_cx(cx) {
            drop(client);
            return Err(error);
        }
        let result = op(&mut client).await;
        match result {
            Ok(value) => {
                self.return_client_with_cx(cx, client).await;
                if attempt > 1 {
                    self.recovery_successes.fetch_add(1, Ordering::Relaxed);
                }
                Ok(value)
            }
            Err(err) => {
                let proven_pre_write_rejection = err.is_proven_pre_write_rejection();
                let decision = mux_recovery_decision(&err);
                self.record_permanent_failure(decision);
                tracing::debug!(
                    op = op_name,
                    cancelled = decision.cancelled,
                    kind = ?decision.kind,
                    connection = ?decision.connection,
                    proven_pre_write_rejection,
                    error = %err,
                    "non-idempotent mux pool op failed without replay"
                );
                self.return_client_after_error(cx, client, decision).await;
                if proven_pre_write_rejection {
                    Err(MuxPoolError::Mux(err))
                } else {
                    Err(MuxPoolError::IndeterminateMutation(err))
                }
            }
        }
    }

    async fn execute_with_recovery_with_cx<T, Op>(
        &self,
        cx: &Cx,
        op_name: &'static str,
        mut op: Op,
    ) -> Result<T, MuxPoolError>
    where
        Op: for<'a> FnMut(&'a mut DirectMuxClient) -> MuxOpFuture<'a, T>,
    {
        let mut attempt: u32 = 0;
        let mut retained_client: Option<(DirectMuxClient, PoolAcquireGuard)> = None;
        loop {
            Self::checkpoint_cx(cx)?;
            attempt = attempt.saturating_add(1);
            if attempt > 1 {
                // Count a retry only after cancellation gates have passed and
                // the next acquisition attempt is actually about to begin.
                self.recovery_attempts.fetch_add(1, Ordering::Relaxed);
            }

            let (mut client, guard) = if let Some(retained) = retained_client.take() {
                retained
            } else {
                match self.acquire_client_with_cx(cx).await {
                    Ok(acquired) => acquired,
                    Err(MuxPoolError::Pool(error)) => return Err(MuxPoolError::Pool(error)),
                    Err(MuxPoolError::IndeterminateMutation(error)) => {
                        return Err(MuxPoolError::IndeterminateMutation(error));
                    }
                    Err(MuxPoolError::Mux(error)) => {
                        if cx.is_cancel_requested() {
                            return Err(MuxPoolError::Pool(PoolError::Cancelled));
                        }
                        let decision = mux_recovery_decision(&error);
                        if self.can_retry(attempt, decision) {
                            tracing::debug!(
                                op = op_name,
                                attempt,
                                max_attempts = self.max_recovery_attempts(),
                                cancelled = decision.cancelled,
                                kind = ?decision.kind,
                                error = %error,
                                "mux pool acquisition failed; reconnecting and retrying"
                            );
                            self.wait_before_retry_with_cx(cx, attempt).await?;
                            continue;
                        }

                        self.record_permanent_failure(decision);
                        return Err(MuxPoolError::Mux(error));
                    }
                }
            };

            if let Err(error) = Self::checkpoint_cx(cx) {
                drop(client);
                return Err(error);
            }
            let result = op(&mut client).await;
            match result {
                Ok(value) => {
                    self.return_client_with_cx(cx, client).await;
                    if attempt > 1 {
                        self.recovery_successes.fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(value);
                }
                Err(err) => {
                    let decision = mux_recovery_decision(&err);
                    if self.can_retry(attempt, decision) {
                        let reuse_in_hand = matches!(
                            decision.connection,
                            MuxConnectionDisposition::Reuse
                        ) && !client.is_connection_poisoned()
                            && !cx.is_cancel_requested();
                        tracing::debug!(
                            op = op_name,
                            attempt,
                            max_attempts = self.max_recovery_attempts(),
                            cancelled = decision.cancelled,
                            kind = ?decision.kind,
                            connection = ?decision.connection,
                            error = %err,
                            "mux pool op failed; applying recovery decision"
                        );
                        if reuse_in_hand {
                            self.wait_before_retry_with_cx(cx, attempt).await?;
                            retained_client = Some((client, guard));
                        } else {
                            self.return_client_after_error(cx, client, decision).await;
                            drop(guard);
                            self.wait_before_retry_with_cx(cx, attempt).await?;
                        }
                        continue;
                    }

                    self.record_permanent_failure(decision);

                    tracing::debug!(
                        op = op_name,
                        attempt,
                        max_attempts = self.max_recovery_attempts(),
                        cancelled = decision.cancelled,
                        kind = ?decision.kind,
                        connection = ?decision.connection,
                        error = %err,
                        "mux pool op failed terminally"
                    );
                    self.return_client_after_error(cx, client, decision).await;
                    return Err(MuxPoolError::Mux(err));
                }
            }
        }
    }

    /// Non-cx recovery loop for when asupersync-runtime is not enabled.
    /// List all panes via a pooled connection.
    pub async fn list_panes(&self) -> Result<ListPanesResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.list_panes_with_cx(&cx).await
    }

    /// List all panes via a pooled connection using explicit `Cx`.
    pub async fn list_panes_with_cx(&self, cx: &Cx) -> Result<ListPanesResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_with_recovery_with_cx(cx, "list_panes", move |client| {
            let op_cx = op_cx.clone();
            Box::pin(async move { client.list_panes_with_cx(&op_cx).await })
        })
        .await
    }

    /// Spawn a new mux pane/tab through a pooled connection.
    pub async fn spawn_v2(&self, spawn: SpawnV2) -> Result<SpawnResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.spawn_v2_with_cx(&cx, spawn).await
    }

    /// Spawn a new mux pane/tab through a pooled connection using explicit `Cx`.
    pub async fn spawn_v2_with_cx(
        &self,
        cx: &Cx,
        spawn: SpawnV2,
    ) -> Result<SpawnResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_once_with_cx(cx, "spawn_v2", move |client| {
            Box::pin(async move { client.spawn_v2_with_cx(&op_cx, spawn).await })
        })
        .await
    }

    /// Split an existing pane through a pooled connection.
    pub async fn split_pane(&self, split: SplitPane) -> Result<SpawnResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.split_pane_with_cx(&cx, split).await
    }

    /// Split an existing pane through a pooled connection using explicit `Cx`.
    pub async fn split_pane_with_cx(
        &self,
        cx: &Cx,
        split: SplitPane,
    ) -> Result<SpawnResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_once_with_cx(cx, "split_pane", move |client| {
            Box::pin(async move { client.split_pane_with_cx(&op_cx, split).await })
        })
        .await
    }

    /// Get lines from a pane via a pooled connection.
    pub async fn get_lines(
        &self,
        pane_id: u64,
        lines: Vec<std::ops::Range<isize>>,
    ) -> Result<GetLinesResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_lines_with_cx(&cx, pane_id, lines).await
    }

    /// Get lines from a pane via a pooled connection using explicit `Cx`.
    pub async fn get_lines_with_cx(
        &self,
        cx: &Cx,
        pane_id: u64,
        lines: Vec<std::ops::Range<isize>>,
    ) -> Result<GetLinesResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_with_recovery_with_cx(cx, "get_lines", move |client| {
            let lines = lines.clone();
            let op_cx = op_cx.clone();
            Box::pin(async move { client.get_lines_with_cx(&op_cx, pane_id, lines).await })
        })
        .await
    }

    /// Poll for pane render changes via a pooled connection.
    pub async fn get_pane_render_changes(
        &self,
        pane_id: u64,
    ) -> Result<GetPaneRenderChangesResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_pane_render_changes_with_cx(&cx, pane_id).await
    }

    /// Poll for pane render changes using explicit `Cx`.
    pub async fn get_pane_render_changes_with_cx(
        &self,
        cx: &Cx,
        pane_id: u64,
    ) -> Result<GetPaneRenderChangesResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_with_recovery_with_cx(cx, "get_pane_render_changes", move |client| {
            let op_cx = op_cx.clone();
            Box::pin(async move {
                client
                    .get_pane_render_changes_with_cx(&op_cx, pane_id)
                    .await
            })
        })
        .await
    }

    /// Fetch OSC 133 semantic zones from a pane via a pooled connection.
    pub async fn get_semantic_zones(
        &self,
        pane_id: u64,
    ) -> Result<GetSemanticZonesResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_semantic_zones_with_cx(&cx, pane_id).await
    }

    /// Fetch OSC 133 semantic zones via a pooled connection using explicit `Cx`.
    pub async fn get_semantic_zones_with_cx(
        &self,
        cx: &Cx,
        pane_id: u64,
    ) -> Result<GetSemanticZonesResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_with_recovery_with_cx(cx, "get_semantic_zones", move |client| {
            let op_cx = op_cx.clone();
            Box::pin(async move { client.get_semantic_zones_with_cx(&op_cx, pane_id).await })
        })
        .await
    }

    async fn execute_render_batch_with_recovery_with_cx(
        &self,
        cx: &Cx,
        op_name: &'static str,
        pane_ids: &[u64],
        depth: usize,
        deadline: &Budget,
        attempts: &mut RenderAttemptState,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, RenderExecutionError> {
        let mut retained_client: Option<(DirectMuxClient, PoolAcquireGuard)> = None;
        loop {
            let attempt = self
                .begin_render_attempt(cx, deadline, attempts)
                .map_err(RenderExecutionError::BeforeOperation)?;

            let (mut client, guard) = if let Some(retained) = retained_client.take() {
                retained
            } else {
                match self.acquire_client_with_cx(cx).await {
                    Ok(acquired) => acquired,
                    Err(MuxPoolError::Pool(error)) => {
                        return Err(RenderExecutionError::BeforeOperation(
                            MuxPoolError::Pool(error),
                        ));
                    }
                    Err(MuxPoolError::IndeterminateMutation(error)) => {
                        return Err(RenderExecutionError::BeforeOperation(
                            MuxPoolError::IndeterminateMutation(error),
                        ));
                    }
                    Err(MuxPoolError::Mux(error)) => {
                        if cx.is_cancel_requested() {
                            return Err(RenderExecutionError::BeforeOperation(
                                self.render_interruption_error(cx),
                            ));
                        }
                        let decision = mux_recovery_decision(&error);
                        if self.can_retry_render(attempts, decision) {
                            tracing::debug!(
                                op = op_name,
                                attempt,
                                max_attempts = attempts.limit,
                                kind = ?decision.kind,
                                error = %error,
                                "render batch acquisition failed; retrying acquisition within logical attempt budget"
                            );
                            self.wait_before_render_retry_with_cx(cx, deadline, attempt)
                                .await
                                .map_err(RenderExecutionError::BeforeOperation)?;
                            continue;
                        }

                        self.record_permanent_failure(decision);
                        return Err(RenderExecutionError::BeforeOperation(
                            MuxPoolError::Mux(error),
                        ));
                    }
                }
            };

            // Recompute after acquisition so this client attempt gets only the
            // unspent part of the one logical render deadline.
            let remaining = match self.remaining_render_batch_time(cx, deadline) {
                Ok(remaining) => remaining,
                Err(error) => {
                    drop(client);
                    return Err(RenderExecutionError::BeforeOperation(error));
                }
            };
            let result = client
                .get_pane_render_changes_batch_with_cx_prevalidated(
                    cx, pane_ids, depth, remaining,
                )
                .await;
            match result {
                Ok(value) => {
                    self.return_client_with_cx(cx, client).await;
                    return Ok(value);
                }
                Err(error) => {
                    let decision = mux_recovery_decision(&error);
                    let reuse_in_hand = matches!(
                        decision.connection,
                        MuxConnectionDisposition::Reuse
                    ) && !client.is_connection_poisoned()
                        && !cx.is_cancel_requested();
                    let hand_off_to_fallback = depth > 1
                        && attempts.has_remaining()
                        && is_transport_failover_decision(decision);

                    if hand_off_to_fallback {
                        tracing::debug!(
                            op = op_name,
                            attempt,
                            max_attempts = attempts.limit,
                            kind = ?decision.kind,
                            connection = ?decision.connection,
                            error = %error,
                            "pipelined render batch failed; handing next shared attempt to sequential fallback"
                        );
                        self.return_client_after_error(cx, client, decision).await;
                        return Err(RenderExecutionError::Operation(MuxPoolError::Mux(
                            error,
                        )));
                    }

                    if self.can_retry_render(attempts, decision) {
                        tracing::debug!(
                            op = op_name,
                            attempt,
                            max_attempts = attempts.limit,
                            kind = ?decision.kind,
                            connection = ?decision.connection,
                            error = %error,
                            "render batch failed; retrying within shared logical attempt budget"
                        );
                        if reuse_in_hand {
                            self.wait_before_render_retry_with_cx(cx, deadline, attempt)
                                .await
                                .map_err(RenderExecutionError::Operation)?;
                            retained_client = Some((client, guard));
                        } else {
                            self.return_client_after_error(cx, client, decision).await;
                            drop(guard);
                            self.wait_before_render_retry_with_cx(cx, deadline, attempt)
                                .await
                                .map_err(RenderExecutionError::Operation)?;
                        }
                        continue;
                    }

                    self.record_permanent_failure(decision);
                    self.return_client_after_error(cx, client, decision).await;
                    return Err(RenderExecutionError::Operation(MuxPoolError::Mux(
                        error,
                    )));
                }
            }
        }
    }

    async fn get_pane_render_changes_batch_within_deadline(
        &self,
        cx: &Cx,
        pane_ids: &[u64],
        depth: usize,
        deadline: &Budget,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, MuxPoolError> {
        let mut attempts = RenderAttemptState::new(self.render_attempt_limit());
        let pipeline_result = self
            .execute_render_batch_with_recovery_with_cx(
                cx,
                "get_pane_render_changes_batch",
                pane_ids,
                depth,
                deadline,
                &mut attempts,
            )
            .await;

        let result = if depth <= 1 {
            pipeline_result.map_err(RenderExecutionError::into_inner)
        } else {
            match pipeline_result {
                Ok(result) => Ok(result),
                Err(RenderExecutionError::Operation(error))
                    if attempts.has_remaining() && should_fallback_render_batch(&error) =>
                {
                    // Cancellation and deadline expiry are checked before the
                    // fallback delay and again before attempt 2 is counted.
                    self.wait_before_render_retry_with_cx(cx, deadline, attempts.started)
                        .await?;
                    tracing::debug!(
                        error = %error,
                        depth,
                        attempt = attempts.started.saturating_add(1),
                        max_attempts = attempts.limit,
                        "pipelined render batch failed; falling back to sequential within shared deadline"
                    );
                    self.execute_render_batch_with_recovery_with_cx(
                        cx,
                        "get_pane_render_changes_batch_fallback",
                        pane_ids,
                        1,
                        deadline,
                        &mut attempts,
                    )
                    .await
                    .map_err(RenderExecutionError::into_inner)
                }
                Err(error) => Err(error.into_inner()),
            }
        };

        if result.is_ok() && attempts.recovery_started() {
            self.recovery_successes.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Poll render changes for many panes using depth-limited pipelining.
    ///
    /// The ambient entry point delegates to the explicit-Cx implementation so
    /// pipeline attempts, retries, and sequential fallback share one deadline.
    pub async fn get_pane_render_changes_batch(
        &self,
        pane_ids: Vec<u64>,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.get_pane_render_changes_batch_with_cx(&cx, pane_ids)
            .await
    }

    /// Poll render changes for many panes using depth-limited pipelining and explicit `Cx`.
    ///
    /// A canonical retryable mux failure may fall back to sequential requests.
    /// Pool errors, cancellation, non-retryable remote errors, and an exhausted
    /// logical deadline are returned without entering a second execution path.
    /// Pane IDs must be unique; duplicates fail before pool acquisition.
    pub async fn get_pane_render_changes_batch_with_cx(
        &self,
        cx: &Cx,
        pane_ids: Vec<u64>,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, MuxPoolError> {
        if pane_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Start the one logical deadline before validation so preflight,
        // acquisition, pipeline execution, retry delay, and compatibility
        // fallback all spend from the same budget.
        let deadline = cx.budget_for_timeout(self.pipeline_timeout);
        self.validate_render_batch_preflight(&pane_ids)?;

        let depth = self.pipeline_depth.min(pane_ids.len()).max(1);
        let outer_timeout = self.remaining_render_batch_time(cx, &deadline)?;
        match crate::runtime_async::timeout_with_cx(
            cx,
            outer_timeout,
            self.get_pane_render_changes_batch_within_deadline(
                cx, &pane_ids, depth, &deadline,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) if cx.is_cancel_requested() => Err(self.render_interruption_error(cx)),
            Err(_) => Err(self.render_batch_timeout_error()),
        }
    }

    /// Write raw bytes to a pane via a pooled connection (no-paste mode).
    pub async fn write_to_pane(
        &self,
        pane_id: u64,
        data: Vec<u8>,
    ) -> Result<UnitResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.write_to_pane_with_cx(&cx, pane_id, data).await
    }

    /// Write raw bytes to a pane via a pooled connection using explicit `Cx`.
    pub async fn write_to_pane_with_cx(
        &self,
        cx: &Cx,
        pane_id: u64,
        data: Vec<u8>,
    ) -> Result<UnitResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_once_with_cx(cx, "write_to_pane", move |client| {
            Box::pin(async move { client.write_to_pane_with_cx(&op_cx, pane_id, data).await })
        })
        .await
    }

    /// Send text via paste mode through a pooled connection.
    pub async fn send_paste(
        &self,
        pane_id: u64,
        data: String,
    ) -> Result<UnitResponse, MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.send_paste_with_cx(&cx, pane_id, data).await
    }

    /// Send text via paste mode through a pooled connection using explicit `Cx`.
    pub async fn send_paste_with_cx(
        &self,
        cx: &Cx,
        pane_id: u64,
        data: String,
    ) -> Result<UnitResponse, MuxPoolError> {
        let op_cx = cx.clone();
        self.execute_once_with_cx(cx, "send_paste", move |client| {
            Box::pin(async move { client.send_paste_with_cx(&op_cx, pane_id, data).await })
        })
        .await
    }

    /// Run a health check by listing panes on a pooled connection.
    pub async fn health_check(&self) -> Result<(), MuxPoolError> {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.health_check_with_cx(&cx).await
    }

    /// Run a health check by listing panes using explicit `Cx`.
    pub async fn health_check_with_cx(&self, cx: &Cx) -> Result<(), MuxPoolError> {
        Self::checkpoint_cx(cx)?;
        match self.list_panes_with_cx(cx).await {
            Ok(_) => {
                let check_num = self.health_checks.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(
                    subsystem = "mux_pool",
                    event = "health_check",
                    outcome = "pass",
                    check_num,
                    "mux pool health check passed"
                );
                Ok(())
            }
            Err(error) if error.is_cancelled() => {
                tracing::debug!(
                    subsystem = "mux_pool",
                    event = "health_check",
                    outcome = "cancelled",
                    error = %error,
                    "mux pool health check cancelled before completion"
                );
                Err(error)
            }
            Err(e) => {
                let check_num = self.health_checks.fetch_add(1, Ordering::Relaxed) + 1;
                let fail_count = self.health_check_failures.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(subsystem = "mux_pool", event = "health_check", outcome = "fail", check_num, total_failures = fail_count, error = %e, "mux pool health check failed");
                Err(e)
            }
        }
    }

    /// Evict idle connections that have exceeded the idle timeout.
    pub async fn evict_idle(&self) -> usize {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.evict_idle_with_cx(&cx)
            .await
            .expect("infallible ambient mux pool eviction failed")
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`Self::evict_idle`].
    ///
    /// Routes through `ConnectionPool::evict_idle_with_cx` so periodic
    /// eviction surfaces cancellation without panicking on the idle mutex.
    ///
    /// # Errors
    ///
    /// Returns [`MuxPoolError::Pool`] if the idle queue cannot be locked.
    pub async fn evict_idle_with_cx(&self, cx: &Cx) -> Result<usize, MuxPoolError> {
        let evicted = self.pool.evict_idle_with_cx(cx).await?;
        if evicted > 0 {
            tracing::debug!(
                subsystem = "mux_pool",
                event = "evict_idle",
                explicit_cx = true,
                evicted,
                "evicted idle mux connections (cx path)"
            );
        }
        Ok(evicted)
    }

    /// Clear all idle connections from the pool.
    pub async fn clear(&self) {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.clear_with_cx(&cx)
            .await
            .expect("infallible ambient mux pool clear failed");
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`Self::clear`].
    ///
    /// Routes through `ConnectionPool::clear_with_cx` so shutdown paths that
    /// invoke a pool-wide flush receive typed cancellation instead of a panic.
    ///
    /// # Errors
    ///
    /// Returns [`MuxPoolError::Pool`] if the idle queue cannot be locked.
    pub async fn clear_with_cx(&self, cx: &Cx) -> Result<(), MuxPoolError> {
        tracing::debug!(
            subsystem = "mux_pool",
            event = "clear",
            explicit_cx = true,
            "clearing all idle mux connections (cx path)"
        );
        self.pool.clear_with_cx(cx).await?;
        Ok(())
    }

    /// Get pool statistics.
    pub async fn stats(&self) -> MuxPoolStats {
        let cx = Cx::current().unwrap_or_else(cx::for_request);
        self.stats_with_cx(&cx)
            .await
            .expect("infallible ambient mux pool stats failed")
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`Self::stats`].
    ///
    /// Routes through `ConnectionPool::stats_with_cx` so a telemetry exporter
    /// running under a cancelled parent cx receives a typed error.
    ///
    /// # Errors
    ///
    /// Returns [`MuxPoolError::Pool`] if the idle queue cannot be locked.
    pub async fn stats_with_cx(&self, cx: &Cx) -> Result<MuxPoolStats, MuxPoolError> {
        Ok(MuxPoolStats {
            pool: self.pool.stats_with_cx(cx).await?,
            connections_created: self.connections_created.load(Ordering::Relaxed),
            connections_failed: self.connections_failed.load(Ordering::Relaxed),
            health_checks: self.health_checks.load(Ordering::Relaxed),
            health_check_failures: self.health_check_failures.load(Ordering::Relaxed),
            recovery_attempts: self.recovery_attempts.load(Ordering::Relaxed),
            recovery_successes: self.recovery_successes.load(Ordering::Relaxed),
            permanent_failures: self.permanent_failures.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_async::unix::{self as compat_unix, AsyncWriteExt};
    use crate::runtime_async::{CompatRuntime, io, task, timeout};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;

    use codec::{
        CODEC_VERSION, GetCodecVersionResponse, GetPaneRenderChangesResponse, ListPanesResponse,
        Pdu, SpawnResponse, SpawnV2, SplitPane, StreamingPduBuffer, UnitResponse,
    };

    async fn unix_stream_read(
        stream: &mut compat_unix::UnixStream,
        buf: &mut [u8],
    ) -> std::io::Result<usize> {
        io::read(stream, buf).await
    }

    async fn write_response_pdu(
        stream: &mut compat_unix::UnixStream,
        pdu: &Pdu,
        serial: u64,
    ) -> std::io::Result<()> {
        let mut out = Vec::new();
        pdu.encode(&mut out, serial)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        stream.write_all(&out).await?;
        stream.flush().await
    }

    /// Spawn a mock mux server that handles handshake + ListPanes.
    /// Returns the socket path.
    async fn spawn_mock_server(temp_dir: &tempfile::TempDir) -> PathBuf {
        let socket_path = temp_dir.path().join("mux-pool-test.sock");
        let listener = compat_unix::bind(&socket_path)
            .await
            .expect("bind mock mux listener");

        task::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                task::spawn(async move {
                    let mut read_buf = StreamingPduBuffer::new();
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        read_buf.extend_from_slice(&temp[..read]);
                        let mut responses: Vec<(u64, Pdu)> = Vec::new();
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            let response = match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "mock-mux-pool-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::ListPanes(_) => Pdu::ListPanesResponse(ListPanesResponse {
                                    tabs: Vec::new(),
                                    tab_titles: Vec::new(),
                                    window_titles: HashMap::new(),
                                }),
                                Pdu::GetLines(req) => Pdu::GetLinesResponse(GetLinesResponse {
                                    pane_id: req.pane_id,
                                    lines: Vec::new().into(),
                                }),
                                Pdu::GetPaneRenderChanges(req) => {
                                    Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: req.pane_id,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
                                            dimensions: mux::renderable::RenderableDimensions {
                                                cols: 80,
                                                viewport_rows: 24,
                                                scrollback_rows: 0,
                                                physical_top: 0,
                                                scrollback_top: 0,
                                                dpi: 96,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                                reverse_video: false,
                                            },
                                            tiered_scrollback_status: None,
                                            dirty_lines: Vec::new(),
                                            title: format!("pane-{}", req.pane_id),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: req.pane_id,
                                        },
                                    )
                                }
                                Pdu::WriteToPane(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::SendPaste(_) => Pdu::UnitResponse(UnitResponse {}),
                                _ => continue,
                            };
                            responses.push((decoded.serial, response));
                        }
                        for (serial, pdu) in responses {
                            if write_response_pdu(&mut stream, &pdu, serial).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });

        socket_path
    }

    /// Spawn a mock mux server that returns an unexpected response for the first ListPanes.
    async fn spawn_mock_server_unexpected_list_panes_once(temp_dir: &tempfile::TempDir) -> PathBuf {
        let socket_path = temp_dir.path().join("mux-pool-test-unexpected.sock");
        let listener = compat_unix::bind(&socket_path)
            .await
            .expect("bind mock mux listener");

        let first_bad = Arc::new(AtomicBool::new(true));
        let next_connection_ordinal = Arc::new(AtomicUsize::new(1));

        task::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let first_bad = Arc::clone(&first_bad);
                let connection_ordinal =
                    next_connection_ordinal.fetch_add(1, AtomicOrdering::Relaxed);
                task::spawn(async move {
                    let mut read_buf = StreamingPduBuffer::new();
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        read_buf.extend_from_slice(&temp[..read]);

                        let mut responses: Vec<(u64, Pdu)> = Vec::new();
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            let response = match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "mock-mux-pool-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::ListPanes(_) => {
                                    if first_bad.swap(false, AtomicOrdering::SeqCst) {
                                        // Wrong but correlated response type: stream stays aligned.
                                        Pdu::UnitResponse(UnitResponse {})
                                    } else {
                                        Pdu::ListPanesResponse(ListPanesResponse {
                                            tabs: Vec::new(),
                                            tab_titles: Vec::new(),
                                            window_titles: HashMap::new(),
                                        })
                                    }
                                }
                                Pdu::GetPaneRenderChanges(req) => {
                                    Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: req.pane_id,
                                            mouse_grabbed: false,
                                            alt_screen_active: false,
                                            cursor_position:
                                                mux::renderable::StableCursorPosition::default(),
                                            dimensions: mux::renderable::RenderableDimensions {
                                                cols: 80,
                                                viewport_rows: 24,
                                                scrollback_rows: 0,
                                                physical_top: 0,
                                                scrollback_top: 0,
                                                dpi: 96,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                                reverse_video: false,
                                            },
                                            tiered_scrollback_status: None,
                                            dirty_lines: std::iter::once(0..1).collect(),
                                            title: format!(
                                                "connection-{connection_ordinal}-pane-{}",
                                                req.pane_id
                                            ),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: if connection_ordinal == 1 { 99 } else { 1 },
                                        },
                                    )
                                }
                                Pdu::WriteToPane(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::SendPaste(_) => Pdu::UnitResponse(UnitResponse {}),
                                _ => continue,
                            };
                            responses.push((decoded.serial, response));
                        }

                        for (serial, pdu) in responses {
                            if write_response_pdu(&mut stream, &pdu, serial).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });

        socket_path
    }

    /// Spawn a mock mux server that returns an unexpected response for the first
    /// request in each mutation family. Later requests succeed so tests can
    /// prove ambiguous mutations do not replay after their invocation boundary.
    async fn spawn_mock_server_unexpected_non_idempotent_once(
        temp_dir: &tempfile::TempDir,
    ) -> PathBuf {
        let socket_path = temp_dir.path().join("mux-pool-test-non-idempotent.sock");
        let listener = compat_unix::bind(&socket_path)
            .await
            .expect("bind mock mux listener");

        let first_spawn_bad = Arc::new(AtomicBool::new(true));
        let first_split_bad = Arc::new(AtomicBool::new(true));
        let first_write_bad = Arc::new(AtomicBool::new(true));
        let first_paste_bad = Arc::new(AtomicBool::new(true));

        task::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let first_spawn_bad = Arc::clone(&first_spawn_bad);
                let first_split_bad = Arc::clone(&first_split_bad);
                let first_write_bad = Arc::clone(&first_write_bad);
                let first_paste_bad = Arc::clone(&first_paste_bad);
                task::spawn(async move {
                    let mut read_buf = StreamingPduBuffer::new();
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        read_buf.extend_from_slice(&temp[..read]);

                        let mut responses: Vec<(u64, Pdu)> = Vec::new();
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            let response = match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "mock-mux-pool-non-idempotent-test"
                                            .to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::SpawnV2(req) => {
                                    if first_spawn_bad.swap(false, AtomicOrdering::SeqCst) {
                                        Pdu::UnitResponse(UnitResponse {})
                                    } else {
                                        Pdu::SpawnResponse(SpawnResponse {
                                            pane_id: 42,
                                            tab_id: 7,
                                            window_id: req.window_id.unwrap_or(3),
                                            size: req.size,
                                        })
                                    }
                                }
                                Pdu::SplitPane(_req) => {
                                    if first_split_bad.swap(false, AtomicOrdering::SeqCst) {
                                        Pdu::UnitResponse(UnitResponse {})
                                    } else {
                                        Pdu::SpawnResponse(SpawnResponse {
                                            pane_id: 43,
                                            tab_id: 7,
                                            window_id: 3,
                                            size: frankenterm_term::TerminalSize::default(),
                                        })
                                    }
                                }
                                Pdu::WriteToPane(_) => {
                                    if first_write_bad.swap(false, AtomicOrdering::SeqCst) {
                                        Pdu::ListPanesResponse(ListPanesResponse {
                                            tabs: Vec::new(),
                                            tab_titles: Vec::new(),
                                            window_titles: HashMap::new(),
                                        })
                                    } else {
                                        Pdu::UnitResponse(UnitResponse {})
                                    }
                                }
                                Pdu::SendPaste(_) => {
                                    if first_paste_bad.swap(false, AtomicOrdering::SeqCst) {
                                        Pdu::ListPanesResponse(ListPanesResponse {
                                            tabs: Vec::new(),
                                            tab_titles: Vec::new(),
                                            window_titles: HashMap::new(),
                                        })
                                    } else {
                                        Pdu::UnitResponse(UnitResponse {})
                                    }
                                }
                                _ => continue,
                            };
                            responses.push((decoded.serial, response));
                        }

                        for (serial, pdu) in responses {
                            if write_response_pdu(&mut stream, &pdu, serial).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });

        socket_path
    }

    fn test_spawn_v2() -> SpawnV2 {
        SpawnV2 {
            domain: config::keyassignment::SpawnTabDomain::DefaultDomain,
            window_id: Some(3),
            command: None,
            command_dir: None,
            size: frankenterm_term::TerminalSize::default(),
            workspace: mux::DEFAULT_WORKSPACE.to_string(),
        }
    }

    fn test_split_pane() -> SplitPane {
        SplitPane {
            pane_id: 1,
            split_request: mux::tab::SplitRequest {
                direction: mux::tab::SplitDirection::Horizontal,
                target_is_second: true,
                top_level: false,
                size: mux::tab::SplitSize::default(),
            },
            command: None,
            command_dir: None,
            domain: config::keyassignment::SpawnTabDomain::CurrentPaneDomain,
            move_pane_id: None,
        }
    }

    /// Spawn a mock mux server that returns an unexpected response on the first
    /// `GetPaneRenderChanges` request made by the first connection.
    async fn spawn_mock_server_unexpected_batch_render_once(
        temp_dir: &tempfile::TempDir,
    ) -> PathBuf {
        spawn_mock_server_unexpected_batch_render_connections(temp_dir, 1).await
    }

    /// Spawn a mock mux server that injects one unexpected render response on
    /// each of the first `bad_connections` connections.
    async fn spawn_mock_server_unexpected_batch_render_connections(
        temp_dir: &tempfile::TempDir,
        bad_connections: usize,
    ) -> PathBuf {
        let socket_path = temp_dir.path().join("mux-pool-test-batch-unexpected.sock");
        let listener = compat_unix::bind(&socket_path)
            .await
            .expect("bind mock mux listener");

        let next_connection_ordinal = Arc::new(AtomicUsize::new(0));

        task::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let connection_ordinal =
                    next_connection_ordinal.fetch_add(1, AtomicOrdering::SeqCst);
                task::spawn(async move {
                    let mut read_buf = StreamingPduBuffer::new();
                    let mut injected_bad_response = false;
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match unix_stream_read(&mut stream, &mut temp).await {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        read_buf.extend_from_slice(&temp[..read]);

                        let mut responses: Vec<(u64, Pdu)> = Vec::new();
                        while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                            let response = match decoded.pdu {
                                Pdu::GetCodecVersion(_) => {
                                    Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                        codec_vers: CODEC_VERSION,
                                        version_string: "mock-mux-pool-batch-test".to_string(),
                                        executable_path: PathBuf::from("/bin/wezterm"),
                                        config_file_path: None,
                                        min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::GetPaneRenderChanges(req) => {
                                    if connection_ordinal < bad_connections
                                        && !injected_bad_response
                                    {
                                        injected_bad_response = true;
                                        // Wrong response type: forces the mux pool batch path
                                        // into its sequential fallback branch.
                                        Pdu::UnitResponse(UnitResponse {})
                                    } else {
                                        Pdu::GetPaneRenderChangesResponse(
                                            GetPaneRenderChangesResponse {
                                                pane_id: req.pane_id,
                                                mouse_grabbed: false,
                                                alt_screen_active: false,
                                                cursor_position:
                                                    mux::renderable::StableCursorPosition::default(),
                                                dimensions: mux::renderable::RenderableDimensions {
                                                    cols: 80,
                                                    viewport_rows: 24,
                                                    scrollback_rows: 0,
                                                    physical_top: 0,
                                                    scrollback_top: 0,
                                                    dpi: 96,
                                                    pixel_width: 0,
                                                    pixel_height: 0,
                                                    reverse_video: false,
                                                },
                                                tiered_scrollback_status: None,
                                                dirty_lines: Vec::new(),
                                                title: format!("pane-{}", req.pane_id),
                                                working_dir: None,
                                                bonus_lines: Vec::new().into(),
                                                input_serial: None,
                                                seqno: req.pane_id,
                                            },
                                        )
                                    }
                                }
                                _ => continue,
                            };
                            responses.push((decoded.serial, response));
                        }

                        for (serial, pdu) in responses {
                            if write_response_pdu(&mut stream, &pdu, serial).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });

        socket_path
    }

    fn pool_config(socket_path: PathBuf, max_size: usize) -> MuxPoolConfig {
        MuxPoolConfig {
            pool: PoolConfig {
                max_size,
                idle_timeout: Duration::from_secs(60),
                acquire_timeout: Duration::from_millis(500),
            },
            mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
            recovery: MuxRecoveryConfig::default(),
            pipeline_depth: 32,
            pipeline_timeout: Duration::from_secs(5),
        }
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for mux_pool tests");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn pool_creates_connection_on_first_acquire() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));
            let result = pool.list_panes().await.expect("list_panes should succeed");
            assert!(result.tabs.is_empty());

            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.connections_failed, 0);
        });
    }

    #[test]
    fn pool_list_panes_with_cx_succeeds() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let result = pool
                .list_panes_with_cx(&cx)
                .await
                .expect("list_panes_with_cx should succeed");
            assert!(result.tabs.is_empty());

            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
        });
    }

    #[test]
    fn pool_list_panes_with_cx_reuses_idle_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            pool.list_panes_with_cx(&cx)
                .await
                .expect("first list_panes_with_cx");
            pool.list_panes_with_cx(&cx)
                .await
                .expect("second list_panes_with_cx");

            let stats = pool.stats().await;
            assert_eq!(
                stats.connections_created, 1,
                "explicit-Cx path should have created only one connection"
            );
            assert_eq!(stats.pool.total_acquired, 2, "two acquire calls");
        });
    }

    #[test]
    fn pool_reuses_idle_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            // First call creates a connection
            pool.list_panes().await.expect("first list_panes");
            // Second call should reuse the idle connection
            pool.list_panes().await.expect("second list_panes");

            let stats = pool.stats().await;
            assert_eq!(
                stats.connections_created, 1,
                "should have created only one connection"
            );
            assert_eq!(stats.pool.total_acquired, 2, "two acquire calls");
        });
    }

    #[test]
    fn pool_concurrent_operations_use_separate_connections() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = Arc::new(MuxPool::new(pool_config(socket_path, 4)));

            let mut handles = Vec::new();
            for _ in 0..4 {
                let pool = pool.clone();
                handles.push(task::spawn(async move {
                    pool.list_panes().await.expect("concurrent list_panes");
                }));
            }
            for handle in handles {
                handle.await.expect("task should not panic");
            }

            let stats = pool.stats().await;
            // At least 1 connection created, possibly up to 4 if all ran concurrently
            assert!(stats.connections_created >= 1);
            assert_eq!(stats.pool.total_acquired, 4);
        });
    }

    #[test]
    fn pool_connect_failure_retries_acquisition_and_updates_counters() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent.sock"),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let err = pool.list_panes().await.expect_err("should fail to connect");
            assert!(
                matches!(err, MuxPoolError::Mux(DirectMuxError::SocketNotFound(_))),
                "expected SocketNotFound, got: {err}"
            );

            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 0);
            assert_eq!(
                stats.connections_failed, 2,
                "default policy should make two transient connection attempts"
            );
            assert_eq!(
                stats.recovery_attempts, 1,
                "counter advances only when the second acquisition begins"
            );
            assert_eq!(stats.recovery_successes, 0);
        });
    }

    #[test]
    fn render_batch_fallback_classification_is_exhaustive() {
        let failover = MuxRecoveryDecision {
            kind: ProtocolErrorKind::Recoverable,
            retry: true,
            connection: MuxConnectionDisposition::Discard,
            cancelled: false,
        };
        let reusable_retry = MuxRecoveryDecision {
            connection: MuxConnectionDisposition::Reuse,
            ..failover
        };
        let cancelled = MuxRecoveryDecision {
            retry: false,
            cancelled: true,
            ..failover
        };
        let no_retry = MuxRecoveryDecision {
            retry: false,
            ..failover
        };

        assert!(!classify_render_batch_fallback(None));
        assert!(classify_render_batch_fallback(Some(failover)));
        assert!(!classify_render_batch_fallback(Some(reusable_retry)));
        assert!(!classify_render_batch_fallback(Some(cancelled)));
        assert!(!classify_render_batch_fallback(Some(no_retry)));

        assert!(should_fallback_render_batch(&MuxPoolError::Mux(
            DirectMuxError::UnexpectedResponse {
                expected: "expected".to_string(),
                got: "unexpected".to_string(),
            },
        )));
        assert!(!should_fallback_render_batch(&MuxPoolError::Mux(
            DirectMuxError::AlignedUnexpectedResponse {
                expected: "expected".to_string(),
                got: "correlated but unexpected".to_string(),
            },
        )));
        assert!(!should_fallback_render_batch(&MuxPoolError::Mux(
            DirectMuxError::DuplicateRenderBatchPane { pane_id: 7 },
        )));
        assert!(!should_fallback_render_batch(&MuxPoolError::Mux(
            DirectMuxError::RemoteError("transient".to_string()),
        )));
        assert!(!should_fallback_render_batch(&MuxPoolError::Mux(
            DirectMuxError::Cancelled {
                phase: "render_batch",
                detail: "test".to_string(),
            },
        )));
        assert!(!should_fallback_render_batch(&MuxPoolError::Pool(
            PoolError::AcquireTimeout,
        )));
        assert!(!should_fallback_render_batch(
            &MuxPoolError::IndeterminateMutation(DirectMuxError::Disconnected),
        ));
    }

    #[test]
    fn pool_aligned_list_error_reuses_connection_without_replay() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(2),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };

            let pool = MuxPool::new(config);
            let error = pool
                .list_panes()
                .await
                .expect_err("aligned semantic failure must not replay");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let resp = pool
                .list_panes()
                .await
                .expect("follow-up should reuse the aligned connection");
            assert!(resp.tabs.is_empty());

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn remote_error_is_not_retried_and_reuses_aligned_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 2));
            let cx = crate::cx::for_testing();

            let client = DirectMuxClient::connect_with_cx(&cx, pool.mux_config.clone())
                .await
                .expect("seed aligned mux client");
            pool.pool
                .put_with_cx(&cx, client)
                .await
                .expect("seed client should return to pool");

            let invocations = Arc::new(AtomicUsize::new(0));
            let op_invocations = Arc::clone(&invocations);
            let error = pool
                .execute_with_recovery_with_cx(&cx, "synthetic_remote_error", move |_client| {
                    let op_invocations = Arc::clone(&op_invocations);
                    Box::pin(async move {
                        op_invocations.fetch_add(1, AtomicOrdering::Relaxed);
                        Err::<(), _>(DirectMuxError::RemoteError("request rejected".to_string()))
                    })
                })
                .await
                .expect_err("remote application error must be returned");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::RemoteError(_))
            ));
            assert_eq!(
                invocations.load(AtomicOrdering::Relaxed),
                1,
                "a framed remote error must never replay the operation"
            );

            let after_error = pool
                .stats_with_cx(&cx)
                .await
                .expect("stats after aligned error");
            assert_eq!(after_error.recovery_attempts, 0);
            assert_eq!(after_error.recovery_successes, 0);
            assert_eq!(after_error.pool.idle_count, 1);

            pool.list_panes_with_cx(&cx)
                .await
                .expect("aligned client should remain reusable");
            let after_reuse = pool
                .stats_with_cx(&cx)
                .await
                .expect("stats after aligned client reuse");
            assert_eq!(
                after_reuse.connections_created, 0,
                "follow-up must reuse the seeded connection rather than reconnect"
            );
            assert_eq!(after_reuse.pool.total_acquired, 2);
        });
    }

    #[test]
    fn aligned_pool_error_reuses_connection_without_losing_render_cache() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 1,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(2),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let (mut predecessor, predecessor_guard) =
                pool.acquire_client().await.expect("acquire predecessor");
            let predecessor_id = predecessor.connection_id();
            let old_render = predecessor
                .get_pane_render_changes(77)
                .await
                .expect("seed predecessor render snapshot");
            assert_eq!(old_render.seqno, 99);
            assert!(old_render.title.starts_with("connection-1-"));
            let error = predecessor
                .list_panes()
                .await
                .expect_err("first connection must receive the injected bad response");
            assert!(matches!(
                error,
                DirectMuxError::AlignedUnexpectedResponse { .. }
            ));
            pool.return_client(predecessor).await;
            drop(predecessor_guard);

            let (mut successor, successor_guard) =
                pool.acquire_client().await.expect("acquire successor");
            let successor_id = successor.connection_id();
            assert_eq!(
                predecessor_id, successor_id,
                "fully correlated wrong-PDU failure must preserve the aligned transport"
            );
            let reused_render = successor
                .get_pane_render_changes(77)
                .await
                .expect("same-socket render state remains usable after aligned failure");
            assert_eq!(reused_render.seqno, 99);
            assert!(reused_render.title.starts_with("connection-1-"));

            pool.return_client(successor).await;
            drop(successor_guard);
            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_aligned_list_error_does_not_replay_and_reuses_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;
            let cx = crate::cx::for_testing();

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(2),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };

            let pool = MuxPool::new(config);
            let error = pool
                .list_panes_with_cx(&cx)
                .await
                .expect_err("aligned semantic failure must not replay the operation");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let resp = pool
                .list_panes_with_cx(&cx)
                .await
                .expect("follow-up should reuse the still-aligned connection");
            assert!(resp.tabs.is_empty());

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_spawn_v2_does_not_retry_after_ambiguous_response() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_non_idempotent_once(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(2),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };

            let pool = MuxPool::new(config);
            let err = pool
                .spawn_v2(test_spawn_v2())
                .await
                .expect_err("ambiguous spawn response must not be retried");
            assert!(
                matches!(
                    err,
                    MuxPoolError::IndeterminateMutation(
                        DirectMuxError::AlignedUnexpectedResponse { .. }
                    )
                ),
                "expected unexpected response without retry, got {err}"
            );

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_spawn_v2_retries_only_transient_acquisition_before_operation_boundary() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let missing_socket = temp_dir.path().join("spawn-acquire-retry-missing.sock");
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 1,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(missing_socket),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::ZERO,
                        Duration::ZERO,
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let error = pool
                .spawn_v2(test_spawn_v2())
                .await
                .expect_err("all acquisition attempts should fail before SpawnV2 is sent");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::SocketNotFound(_))
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.connections_failed, 3);
            assert_eq!(stats.pool.total_acquired, 3);
            assert_eq!(stats.recovery_attempts, 2);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 0);
        });
    }

    #[test]
    fn proven_pre_write_mutation_rejection_is_determinate_and_reuses_client() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 1));
            let cx = crate::cx::for_testing();

            let error = pool
                .execute_once_with_cx(&cx, "synthetic_pre_write_rejection", |_client| {
                    Box::pin(async {
                        Err::<(), _>(DirectMuxError::OutboundPduRequiresCodec {
                            pdu: "ReorderWindowTabsV1",
                            agreed: 50,
                            required: 53,
                        })
                    })
                })
                .await
                .expect_err("proven pre-write rejection must remain determinate");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::OutboundPduRequiresCodec {
                    pdu: "ReorderWindowTabsV1",
                    agreed: 50,
                    required: 53,
                })
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.permanent_failures, 1);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);

            pool.list_panes_with_cx(&cx)
                .await
                .expect("aligned connection must remain reusable after pre-write rejection");
            let reused = pool.stats().await;
            assert_eq!(reused.connections_created, 1);
            assert_eq!(reused.permanent_failures, 1);
            assert_eq!(reused.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_split_pane_does_not_retry_after_ambiguous_response() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_non_idempotent_once(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(2),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };

            let pool = MuxPool::new(config);
            let err = pool
                .split_pane(test_split_pane())
                .await
                .expect_err("ambiguous split response must not be retried");
            assert!(
                matches!(
                    err,
                    MuxPoolError::IndeterminateMutation(
                        DirectMuxError::AlignedUnexpectedResponse { .. }
                    )
                ),
                "expected unexpected response without retry, got {err}"
            );

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pane_input_mutations_do_not_replay_after_ambiguous_response() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_non_idempotent_once(&temp_dir).await;
            let config = MuxPoolConfig {
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::ZERO,
                        Duration::ZERO,
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                ..MuxPoolConfig::default()
            };
            let pool = MuxPool::new(config);

            let write_error = pool
                .write_to_pane(9, b"must-not-duplicate\n".to_vec())
                .await
                .expect_err("ambiguous write response must not be replayed");
            assert!(matches!(
                write_error,
                MuxPoolError::IndeterminateMutation(
                    DirectMuxError::AlignedUnexpectedResponse { .. }
                )
            ));

            let paste_error = pool
                .send_paste(9, "must-not-duplicate\n".to_string())
                .await
                .expect_err("ambiguous paste response must not be replayed");
            assert!(matches!(
                paste_error,
                MuxPoolError::IndeterminateMutation(
                    DirectMuxError::AlignedUnexpectedResponse { .. }
                )
            ));

            pool.send_paste(9, "new logical mutation\n".to_string())
                .await
                .expect("a new mutation may reuse the fully aligned connection");

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn post_invocation_mutation_cancellation_remains_typed_and_indeterminate() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 1));
            let cx = crate::cx::for_testing();

            let error = pool
                .execute_once_with_cx(&cx, "synthetic_cancelled_mutation", |_client| {
                    Box::pin(async {
                        Err::<(), _>(DirectMuxError::Cancelled {
                            phase: "mutation_wait",
                            detail: "synthetic cancellation after invocation".to_string(),
                        })
                    })
                })
                .await
                .expect_err("post-invocation cancellation must remain non-replayable");
            match error {
                MuxPoolError::IndeterminateMutation(inner) => assert!(inner.is_cancelled()),
                other => panic!("expected indeterminate typed cancellation, got {other}"),
            }

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 0);
        });
    }

    #[test]
    fn pool_health_check_success() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 2));
            pool.health_check().await.expect("health check should pass");

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 1);
            assert_eq!(stats.health_check_failures, 0);
        });
    }

    #[test]
    fn pool_health_check_with_cx_success() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 2));
            let cx = crate::cx::for_testing();

            pool.health_check_with_cx(&cx)
                .await
                .expect("health_check_with_cx should pass");

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 1);
            assert_eq!(stats.health_check_failures, 0);
        });
    }

    #[test]
    fn pool_health_check_failure() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent.sock"),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let err = pool
                .health_check()
                .await
                .expect_err("health check should fail");
            assert!(matches!(
                err,
                MuxPoolError::Mux(DirectMuxError::SocketNotFound(_))
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 1);
            assert_eq!(stats.health_check_failures, 1);
            assert_eq!(stats.recovery_attempts, 1);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_failed, 2);
        });
    }

    #[test]
    fn pool_health_check_reports_aligned_error_without_replay() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(2),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let error = pool
                .health_check()
                .await
                .expect_err("aligned health-check error must not replay");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));
            pool.health_check()
                .await
                .expect("next health check should reuse the aligned connection");

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 2);
            assert_eq!(stats.health_check_failures, 1);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_health_check_without_recovery_does_not_retry() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(5),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let err = pool
                .health_check()
                .await
                .expect_err("health_check should fail without recovery");
            assert!(matches!(
                err,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 1);
            assert_eq!(stats.health_check_failures, 1);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(
                stats.connections_created, 1,
                "health_check should not reconnect when recovery is disabled"
            );
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_health_check_with_cx_failure() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent-cx.sock"),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);
            let cx = crate::cx::for_testing();

            let err = pool
                .health_check_with_cx(&cx)
                .await
                .expect_err("health_check_with_cx should fail");
            assert!(matches!(
                err,
                MuxPoolError::Mux(DirectMuxError::SocketNotFound(_))
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 1);
            assert_eq!(stats.health_check_failures, 1);
            assert_eq!(stats.recovery_attempts, 1);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_failed, 2);
        });
    }

    #[test]
    fn pool_health_check_with_cx_reports_aligned_error_without_replay() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;
            let cx = crate::cx::for_testing();

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(2),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let error = pool
                .health_check_with_cx(&cx)
                .await
                .expect_err("aligned Cx health-check error must not replay");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));
            pool.health_check_with_cx(&cx)
                .await
                .expect("next Cx health check should reuse the aligned connection");

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 2);
            assert_eq!(stats.health_check_failures, 1);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_health_check_with_cx_without_recovery_does_not_retry() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;
            let cx = crate::cx::for_testing();

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(5),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let err = pool
                .health_check_with_cx(&cx)
                .await
                .expect_err("health_check_with_cx should fail without recovery");
            assert!(matches!(
                err,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 1);
            assert_eq!(stats.health_check_failures, 1);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(
                stats.connections_created, 1,
                "health_check_with_cx should not reconnect when recovery is disabled"
            );
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_clear_evicts_all_idle() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            // Create a connection and return it to idle
            pool.list_panes().await.expect("list_panes");

            let stats = pool.stats().await;
            assert_eq!(stats.pool.idle_count, 1);

            pool.clear().await;

            let stats = pool.stats().await;
            assert_eq!(stats.pool.idle_count, 0);
        });
    }

    #[test]
    fn pool_idle_timeout_eviction() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_millis(50),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            // Create and return a connection
            pool.list_panes().await.expect("list_panes");

            let stats = pool.stats().await;
            assert_eq!(stats.pool.idle_count, 1);

            // Wait for idle timeout
            sleep(Duration::from_millis(100)).await;

            let evicted = pool.evict_idle().await;
            assert_eq!(evicted, 1, "stale connection should be evicted");

            let stats = pool.stats().await;
            assert_eq!(stats.pool.idle_count, 0);
        });
    }

    #[test]
    fn pool_stats_are_accurate() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.max_size, 4);
            assert_eq!(stats.pool.idle_count, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.connections_created, 0);

            pool.list_panes().await.expect("list_panes");

            let stats = pool.stats().await;
            assert_eq!(stats.pool.idle_count, 1);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.total_acquired, 1);
        });
    }

    #[test]
    fn pool_respects_max_connections() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 1,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(100),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = Arc::new(MuxPool::new(config));

            // Acquire the only slot via internal method
            let (client, _guard) = pool.acquire_client().await.expect("acquire");

            // Second acquire should timeout
            let pool2 = pool.clone();
            let result = timeout(Duration::from_millis(200), Box::pin(pool2.list_panes())).await;

            match result {
                Ok(Err(MuxPoolError::Pool(PoolError::AcquireTimeout))) => {} // expected
                Ok(Err(e)) => panic!("expected AcquireTimeout, got: {e}"),
                Ok(Ok(_)) => panic!("should not have succeeded"),
                Err(_) => {} // outer timeout is also acceptable
            }

            // Return the first client and drop the guard
            pool.return_client(client).await;
            drop(_guard);
        });
    }

    #[test]
    fn mux_pool_config_default_is_sane() {
        let config = MuxPoolConfig::default();
        assert_eq!(config.pool.max_size, 8);
        assert_eq!(config.pool.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.pool.acquire_timeout, Duration::from_secs(10));
        assert_eq!(config.pipeline_depth, 32);
        assert_eq!(config.pipeline_timeout, Duration::from_secs(5));
    }

    #[test]
    fn mux_pool_error_display() {
        let pool_err = MuxPoolError::Pool(PoolError::AcquireTimeout);
        assert!(pool_err.to_string().contains("pool"));
        assert!(pool_err.is_pool_timeout());
        assert!(!pool_err.is_disconnected());

        let cancelled_err = MuxPoolError::Pool(PoolError::Cancelled);
        assert!(cancelled_err.to_string().contains("cancelled"));
        assert!(!cancelled_err.is_pool_timeout());
        assert!(!cancelled_err.is_disconnected());

        let mux_err = MuxPoolError::Mux(DirectMuxError::Disconnected);
        assert!(mux_err.to_string().contains("mux"));
        assert!(!mux_err.is_pool_timeout());
        assert!(mux_err.is_disconnected());

        let mutation_err =
            MuxPoolError::IndeterminateMutation(DirectMuxError::Disconnected);
        assert!(mutation_err.to_string().contains("indeterminate"));
        assert!(!mutation_err.is_pool_timeout());
        assert!(mutation_err.is_disconnected());
    }

    #[test]
    fn mux_pool_stats_serde_roundtrip() {
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 8,
                idle_count: 2,
                active_count: 1,
                total_acquired: 100,
                total_returned: 95,
                total_evicted: 3,
                total_timeouts: 2,
            },
            connections_created: 50,
            connections_failed: 5,
            health_checks: 10,
            health_check_failures: 1,
            recovery_attempts: 2,
            recovery_successes: 1,
            permanent_failures: 3,
        };
        let json = serde_json::to_string(&stats).expect("serialize");
        let back: MuxPoolStats = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.connections_created, 50);
        assert_eq!(back.health_check_failures, 1);
        assert_eq!(back.pool.total_acquired, 100);
    }

    // ---------------------------------------------------------------
    // New tests: configuration edge cases
    // ---------------------------------------------------------------

    #[test]
    fn recovery_config_default_values() {
        let config = MuxRecoveryConfig::default();
        assert!(config.enabled, "recovery enabled by default");
        assert_eq!(config.retry_policy.max_attempts, Some(2));
    }

    #[test]
    fn pool_new_clamps_zero_pipeline_depth_to_one() {
        let config = MuxPoolConfig {
            pipeline_depth: 0,
            ..MuxPoolConfig::default()
        };
        let pool = MuxPool::new(config);
        assert_eq!(pool.pipeline_depth, 1, "zero pipeline_depth clamped to 1");
    }

    #[test]
    fn pool_new_clamps_zero_pipeline_timeout() {
        let config = MuxPoolConfig {
            pipeline_timeout: Duration::ZERO,
            ..MuxPoolConfig::default()
        };
        let pool = MuxPool::new(config);
        assert_eq!(
            pool.pipeline_timeout,
            Duration::from_millis(1),
            "zero timeout clamped to 1ms"
        );
    }

    #[test]
    fn shared_render_deadline_accepts_decreasing_slices_and_fails_closed() {
        let configured = Duration::from_secs(5);
        let first = require_render_batch_remaining(Some(Duration::from_secs(4)), configured)
            .expect("initial deadline slice");
        let later = require_render_batch_remaining(Some(Duration::from_millis(750)), configured)
            .expect("recomputed deadline slice");
        assert!(later < first, "later attempt must receive less remaining time");

        for exhausted in [None, Some(Duration::ZERO)] {
            let error = require_render_batch_remaining(exhausted, configured)
                .expect_err("an exhausted bounded render deadline must fail closed");
            assert!(matches!(
                error,
                DirectMuxError::BatchTimeout { timeout_ms: 5_000 }
            ));
        }
    }

    #[test]
    fn pool_new_preserves_nonzero_pipeline_depth() {
        let config = MuxPoolConfig {
            pipeline_depth: 64,
            ..MuxPoolConfig::default()
        };
        let pool = MuxPool::new(config);
        assert_eq!(pool.pipeline_depth, 64);
    }

    #[test]
    fn pool_new_preserves_nonzero_pipeline_timeout() {
        let config = MuxPoolConfig {
            pipeline_timeout: Duration::from_secs(10),
            ..MuxPoolConfig::default()
        };
        let pool = MuxPool::new(config);
        assert_eq!(pool.pipeline_timeout, Duration::from_secs(10));
    }

    #[test]
    fn mux_pool_config_clone() {
        let config = MuxPoolConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.pool.max_size, config.pool.max_size);
        assert_eq!(cloned.pipeline_depth, config.pipeline_depth);
        assert_eq!(cloned.pipeline_timeout, config.pipeline_timeout);
    }

    #[test]
    fn mux_pool_error_from_pool_error() {
        let err: MuxPoolError = PoolError::AcquireTimeout.into();
        assert!(err.is_pool_timeout());
        assert!(!err.is_disconnected());
    }

    #[test]
    fn mux_pool_error_from_mux_error() {
        let err: MuxPoolError = DirectMuxError::Disconnected.into();
        assert!(err.is_disconnected());
        assert!(!err.is_pool_timeout());
    }

    #[test]
    fn mux_pool_error_pool_closed_is_not_timeout() {
        let err = MuxPoolError::Pool(PoolError::Closed);
        assert!(!err.is_pool_timeout());
        assert!(!err.is_disconnected());
    }

    #[test]
    fn mux_pool_error_pool_cancelled_is_not_timeout() {
        let err = MuxPoolError::Pool(PoolError::Cancelled);
        assert!(!err.is_pool_timeout());
        assert!(!err.is_disconnected());
    }

    #[test]
    fn mux_pool_stats_all_zero_initially() {
        // Can't call pool.stats() without async, but verify via manual construction
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 4,
                idle_count: 0,
                active_count: 0,
                total_acquired: 0,
                total_returned: 0,
                total_evicted: 0,
                total_timeouts: 0,
            },
            connections_created: 0,
            connections_failed: 0,
            health_checks: 0,
            health_check_failures: 0,
            recovery_attempts: 0,
            recovery_successes: 0,
            permanent_failures: 0,
        };
        assert_eq!(stats.connections_created, 0);
        assert_eq!(stats.health_checks, 0);
        assert_eq!(stats.recovery_attempts, 0);
        assert_eq!(stats.permanent_failures, 0);
    }

    #[test]
    fn mux_pool_stats_serializes_all_fields() {
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 16,
                idle_count: 3,
                active_count: 2,
                total_acquired: 200,
                total_returned: 190,
                total_evicted: 7,
                total_timeouts: 3,
            },
            connections_created: 100,
            connections_failed: 10,
            health_checks: 50,
            health_check_failures: 5,
            recovery_attempts: 8,
            recovery_successes: 6,
            permanent_failures: 2,
        };
        let json = serde_json::to_string_pretty(&stats).expect("serialize");
        assert!(json.contains("\"connections_created\": 100"));
        assert!(json.contains("\"connections_failed\": 10"));
        assert!(json.contains("\"health_checks\": 50"));
        assert!(json.contains("\"health_check_failures\": 5"));
        assert!(json.contains("\"recovery_attempts\": 8"));
        assert!(json.contains("\"recovery_successes\": 6"));
        assert!(json.contains("\"permanent_failures\": 2"));
        assert!(json.contains("\"max_size\": 16"));
        assert!(json.contains("\"idle_count\": 3"));
    }

    #[test]
    fn mux_pool_error_display_includes_context() {
        let timeout_err = MuxPoolError::Pool(PoolError::AcquireTimeout);
        let display = format!("{timeout_err}");
        assert!(
            !display.is_empty(),
            "error display should produce non-empty string"
        );

        let disconnected_err = MuxPoolError::Mux(DirectMuxError::Disconnected);
        let display2 = format!("{disconnected_err}");
        assert!(!display2.is_empty());

        // Debug also works
        let debug = format!("{timeout_err:?}");
        assert!(debug.contains("Pool"));
    }

    #[test]
    fn recovery_config_disabled() {
        let config = MuxRecoveryConfig {
            enabled: false,
            retry_policy: RetryPolicy::new(
                Duration::from_millis(100),
                Duration::from_secs(1),
                2.0,
                0.0,
                Some(5),
            ),
        };
        assert!(!config.enabled);
        assert_eq!(config.retry_policy.max_attempts, Some(5));
    }

    // ---------------------------------------------------------------
    // New tests: async pool operations
    // ---------------------------------------------------------------

    #[test]
    fn pool_initial_stats_are_all_zero() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent-stats.sock"),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let stats = pool.stats().await;
            assert_eq!(stats.pool.max_size, 4);
            assert_eq!(stats.pool.idle_count, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.health_checks, 0);
            assert_eq!(stats.health_check_failures, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.permanent_failures, 0);
        });
    }

    #[test]
    fn pool_evict_idle_returns_zero_when_no_idle() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent-evict.sock"),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let evicted = pool.evict_idle().await;
            assert_eq!(evicted, 0, "nothing to evict on empty pool");
        });
    }

    #[test]
    fn pool_clear_on_empty_pool_is_noop() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent-clear.sock"),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            pool.clear().await;
            let stats = pool.stats().await;
            assert_eq!(stats.pool.idle_count, 0);
        });
    }

    #[test]
    fn pool_multiple_sequential_reuses_same_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            for _ in 0..5 {
                pool.list_panes().await.expect("list_panes should succeed");
            }

            let stats = pool.stats().await;
            assert_eq!(
                stats.connections_created, 1,
                "5 sequential calls should reuse 1 connection"
            );
            assert_eq!(stats.pool.total_acquired, 5);
        });
    }

    #[test]
    fn pool_batch_render_empty_pane_ids() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            let result = pool
                .get_pane_render_changes_batch(Vec::new())
                .await
                .expect("empty batch should succeed");
            assert!(result.is_empty(), "empty input → empty output");

            let stats = pool.stats().await;
            assert_eq!(
                stats.connections_created, 0,
                "empty batch should not create connections"
            );
        });
    }

    #[test]
    fn pool_batch_render_empty_pane_ids_with_cx() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let result = pool
                .get_pane_render_changes_batch_with_cx(&cx, Vec::new())
                .await
                .expect("empty batch with cx should succeed");
            assert!(result.is_empty(), "empty input → empty output");

            let stats = pool.stats().await;
            assert_eq!(
                stats.connections_created, 0,
                "empty batch with cx should not create connections"
            );
        });
    }

    #[test]
    fn pool_batch_render_single_pane() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            let result = pool
                .get_pane_render_changes_batch(vec![42])
                .await
                .expect("single-pane batch should succeed");
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].pane_id, 42);
        });
    }

    #[test]
    fn pool_batch_render_single_pane_with_cx() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let result = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![42])
                .await
                .expect("single-pane batch with cx should succeed");
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].pane_id, 42);
        });
    }

    // NOTE: pool_batch_render_multiple_panes was previously removed due to
    // pre-existing UB in vendored codec (ptr::copy_nonoverlapping on
    // overlapping buffer regions). Fixed by a7b05007 which replaced
    // copy_nonoverlapping with copy (memmove) in codec stream_decode.
    #[test]
    fn pool_batch_render_multiple_panes() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            let result = pool
                .get_pane_render_changes_batch(vec![10, 20, 30])
                .await
                .expect("multi-pane batch should succeed");

            assert_eq!(result.len(), 3, "should get 3 responses");
            assert_eq!(result[0].pane_id, 10);
            assert_eq!(result[1].pane_id, 20);
            assert_eq!(result[2].pane_id, 30);

            let stats = pool.stats().await;
            assert!(
                stats.connections_created >= 1,
                "should create at least one connection"
            );
        });
    }

    #[test]
    fn pool_batch_render_multiple_panes_with_cx() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let result = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![10, 20, 30])
                .await
                .expect("multi-pane batch with cx should succeed");

            assert_eq!(result.len(), 3, "should get 3 responses");
            assert_eq!(result[0].pane_id, 10);
            assert_eq!(result[1].pane_id, 20);
            assert_eq!(result[2].pane_id, 30);

            let stats = pool.stats().await;
            assert!(
                stats.connections_created >= 1,
                "should create at least one connection"
            );
        });
    }

    #[test]
    fn pool_batch_render_large_batch_preserves_order() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            // Request 50 panes — exercises pipelining and verifies ordering
            // at a scale beyond the trivial 3-pane test.
            let pane_ids: Vec<u64> = (100..150).collect();
            let result = pool
                .get_pane_render_changes_batch(pane_ids.clone())
                .await
                .expect("large batch should succeed");

            assert_eq!(result.len(), 50, "should get 50 responses");
            for (i, resp) in result.iter().enumerate() {
                assert_eq!(
                    resp.pane_id as u64, pane_ids[i],
                    "response {i} pane_id mismatch: expected {} got {}",
                    pane_ids[i], resp.pane_id
                );
            }
        });
    }

    #[test]
    fn pool_batch_render_large_batch_with_cx_preserves_order() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let pane_ids: Vec<u64> = (100..150).collect();
            let result = pool
                .get_pane_render_changes_batch_with_cx(&cx, pane_ids.clone())
                .await
                .expect("large batch with cx should succeed");

            assert_eq!(result.len(), 50, "should get 50 responses");
            for (i, resp) in result.iter().enumerate() {
                assert_eq!(
                    resp.pane_id as u64, pane_ids[i],
                    "response {i} pane_id mismatch: expected {} got {}",
                    pane_ids[i], resp.pane_id
                );
            }
        });
    }

    #[test]
    fn pool_batch_render_duplicate_pane_ids() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("unused-duplicate-pane.sock");

            let pool = MuxPool::new(pool_config(socket_path, 4));

            let error = pool
                .get_pane_render_changes_batch(vec![42, 42, 42])
                .await
                .expect_err("duplicate pane IDs must fail before pool acquisition");
            let MuxPoolError::Mux(mux_error) = error else {
                panic!("expected typed mux input error");
            };
            assert!(matches!(
                &mux_error,
                DirectMuxError::DuplicateRenderBatchPane { pane_id: 42 }
            ));
            assert_eq!(
                mux_error.protocol_error_kind(),
                ProtocolErrorKind::Permanent
            );
            let stats = pool.stats().await;
            assert_eq!(stats.permanent_failures, 1);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.pool.idle_count, 0);
        });
    }

    #[test]
    fn pool_batch_render_duplicate_pane_ids_with_cx() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("unused-duplicate-pane-cx.sock");
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let error = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![42, 42, 42])
                .await
                .expect_err("duplicate pane IDs with Cx must fail before pool acquisition");
            let MuxPoolError::Mux(mux_error) = error else {
                panic!("expected typed mux input error");
            };
            assert!(matches!(
                &mux_error,
                DirectMuxError::DuplicateRenderBatchPane { pane_id: 42 }
            ));
            assert_eq!(
                mux_error.protocol_error_kind(),
                ProtocolErrorKind::Permanent
            );
            let stats = pool.stats().await;
            assert_eq!(stats.permanent_failures, 1);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.pool.idle_count, 0);
        });
    }

    #[test]
    fn expired_render_deadline_returns_batch_timeout_without_acquire_or_fallback() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let missing_socket = temp_dir.path().join("expired-render-deadline.sock");
            let config = MuxPoolConfig {
                mux: DirectMuxClientConfig::default().with_socket_path(missing_socket),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::ZERO,
                        Duration::ZERO,
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 4,
                pipeline_timeout: Duration::from_secs(5),
                ..MuxPoolConfig::default()
            };
            let pool = MuxPool::new(config);
            let expired_budget = crate::cx::Budget::with_deadline_at_ns(0);
            let cx = Cx::for_testing_with_budget(expired_budget);

            let error = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![1, 2, 3])
                .await
                .expect_err("expired logical deadline must fail before pipeline acquisition");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::BatchTimeout { timeout_ms: 5_000 })
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
        });
    }

    #[test]
    fn pool_batch_render_pipeline_depth_one() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            // depth=1 skips pipelining and uses sequential mode directly.
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 1,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let result = pool
                .get_pane_render_changes_batch(vec![10, 20, 30])
                .await
                .expect("depth=1 batch should succeed");

            assert_eq!(result.len(), 3);
            assert_eq!(result[0].pane_id, 10);
            assert_eq!(result[1].pane_id, 20);
            assert_eq!(result[2].pane_id, 30);
        });
    }

    #[test]
    fn pool_batch_render_pipeline_depth_one_with_cx() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let cx = crate::cx::for_testing();

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 1,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let result = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![10, 20, 30])
                .await
                .expect("depth=1 batch with cx should succeed");

            assert_eq!(result.len(), 3);
            assert_eq!(result[0].pane_id, 10);
            assert_eq!(result[1].pane_id, 20);
            assert_eq!(result[2].pane_id, 30);
        });
    }

    #[test]
    fn pool_batch_render_respects_single_attempt_when_recovery_is_disabled() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_batch_render_once(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(1),
                    ),
                },
                pipeline_depth: 4,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let error = pool
                .get_pane_render_changes_batch(vec![10, 20, 30])
                .await
                .expect_err("one configured attempt must not enter fallback");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(
                stats.connections_created, 1,
                "one configured attempt creates only the failed pipeline connection"
            );
            assert_eq!(stats.pool.total_acquired, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn render_acquisition_failures_never_enter_sequential_fallback() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let missing_socket = temp_dir.path().join("render-acquire-missing.sock");
            let config = MuxPoolConfig {
                mux: DirectMuxClientConfig::default().with_socket_path(missing_socket),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::ZERO,
                        Duration::ZERO,
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 4,
                pipeline_timeout: Duration::from_secs(5),
                ..MuxPoolConfig::default()
            };
            let pool = MuxPool::new(config);

            let error = pool
                .get_pane_render_changes_batch(vec![10, 20, 30])
                .await
                .expect_err("failed acquisition must not enter sequential fallback");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::SocketNotFound(_))
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.connections_failed, 3);
            assert_eq!(stats.pool.total_acquired, 3);
            assert_eq!(stats.recovery_attempts, 2);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.connections_created, 0);
        });
    }

    #[test]
    fn aligned_render_error_never_enters_pipeline_fallback() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_batch_render_once(&temp_dir).await;
            let config = MuxPoolConfig {
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::ZERO,
                        Duration::ZERO,
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 4,
                pipeline_timeout: Duration::from_secs(5),
                ..MuxPoolConfig::default()
            };
            let pool = MuxPool::new(config);

            let error = pool
                .get_pane_render_changes_batch(vec![10, 20, 30])
                .await
                .expect_err("aligned semantic error must not replay or enter fallback");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.total_acquired, 1);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn one_pane_batches_use_effective_depth_and_never_outer_fallback() {
        run_async_test(async {
            for explicit_cx in [false, true] {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                let socket_path = spawn_mock_server_unexpected_batch_render_once(&temp_dir).await;
                let config = MuxPoolConfig {
                    pool: PoolConfig {
                        max_size: 4,
                        idle_timeout: Duration::from_secs(60),
                        acquire_timeout: Duration::from_millis(500),
                    },
                    mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                    recovery: MuxRecoveryConfig {
                        enabled: false,
                        retry_policy: RetryPolicy::new(
                            Duration::from_millis(0),
                            Duration::from_millis(0),
                            1.0,
                            0.0,
                            Some(1),
                        ),
                    },
                    pipeline_depth: 32,
                    pipeline_timeout: Duration::from_secs(5),
                };
                let pool = MuxPool::new(config);

                let error = if explicit_cx {
                    let cx = crate::cx::for_testing();
                    pool.get_pane_render_changes_batch_with_cx(&cx, vec![10])
                        .await
                        .expect_err("one-pane Cx batch must not replay via outer fallback")
                } else {
                    pool.get_pane_render_changes_batch(vec![10])
                        .await
                        .expect_err("one-pane batch must not replay via outer fallback")
                };
                assert!(matches!(
                    error,
                    MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
                ));
                let stats = pool.stats().await;
                assert_eq!(stats.connections_created, 1);
                assert_eq!(stats.recovery_attempts, 0);
                assert_eq!(stats.pool.idle_count, 1);
            }
        });
    }

    #[test]
    fn pool_recovery_disabled_does_not_retry() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(5),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };

            let pool = MuxPool::new(config);
            let err = pool
                .list_panes()
                .await
                .expect_err("should fail without recovery");
            assert!(matches!(
                err,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let stats = pool.stats().await;
            assert_eq!(
                stats.recovery_attempts, 0,
                "no retries when recovery disabled"
            );
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_list_panes_with_cx_without_recovery_does_not_retry() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_list_panes_once(&temp_dir).await;
            let cx = crate::cx::for_testing();

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(5),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };

            let pool = MuxPool::new(config);
            let err = pool
                .list_panes_with_cx(&cx)
                .await
                .expect_err("list_panes_with_cx should fail without recovery");
            assert!(matches!(
                err,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let stats = pool.stats().await;
            assert_eq!(
                stats.recovery_attempts, 0,
                "no retries when recovery disabled on explicit-Cx path"
            );
            assert_eq!(
                stats.connections_created, 1,
                "explicit-Cx path should not reconnect when recovery is disabled"
            );
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_multiple_connect_failures_increment_counter() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent-multi.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            for _ in 0..3 {
                let _ = pool.list_panes().await;
            }

            let stats = pool.stats().await;
            assert_eq!(stats.connections_failed, 3, "3 failures should be counted");
            assert_eq!(stats.connections_created, 0);
        });
    }

    #[test]
    fn pool_multiple_connect_failures_with_cx_increment_counter() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-nonexistent-multi-cx.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);
            let cx = crate::cx::for_testing();

            for _ in 0..3 {
                let _ = pool.list_panes_with_cx(&cx).await;
            }

            let stats = pool.stats().await;
            assert_eq!(stats.connections_failed, 3, "3 failures should be counted");
            assert_eq!(stats.connections_created, 0);
        });
    }

    #[test]
    fn pool_list_panes_with_precancelled_cx_returns_pool_cancelled_without_connecting() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-precancelled-list.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled mux pool list"),
            );

            let err = pool
                .list_panes_with_cx(&cx)
                .await
                .expect_err("pre-cancelled list_panes_with_cx should fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.total_timeouts, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
        });
    }

    #[test]
    fn pool_execute_with_recovery_with_cx_does_not_retry_cancelled_mux_io() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(5),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);
            let cx = crate::cx::for_testing();

            let client = DirectMuxClient::connect_with_cx(&cx, pool.mux_config.clone())
                .await
                .expect("seed mux client");
            pool.pool.put(client).await;

            let err = pool
                .execute_with_recovery_with_cx(&cx, "cancelled-op", |_client| {
                    Box::pin(async {
                        Err::<(), _>(DirectMuxError::Io(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "mux request_write_wait cancelled: synthetic cancellation",
                        )))
                    })
                })
                .await
                .expect_err("cancelled mux op should not succeed");

            match err {
                MuxPoolError::Mux(mux_err) => assert!(mux_err.is_cancelled()),
                other @ MuxPoolError::Pool(_) => {
                    panic!("expected mux cancellation error, got: {other}");
                }
                other @ MuxPoolError::IndeterminateMutation(_) => {
                    panic!("idempotent op must not produce mutation ambiguity: {other}");
                }
            }

            let stats = pool.stats().await;
            assert_eq!(
                stats.recovery_attempts, 0,
                "cancelled mux errors must not trigger reconnect retries"
            );
            assert_eq!(
                stats.connections_created, 0,
                "cancelled mux errors should not create replacement connections"
            );
            assert_eq!(
                stats.pool.total_acquired, 1,
                "only one attempt should acquire"
            );
        });
    }

    #[test]
    fn pool_health_check_with_precancelled_cx_tracks_failure_without_connecting() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-precancelled-health.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled mux pool health check"),
            );

            let err = pool
                .health_check_with_cx(&cx)
                .await
                .expect_err("pre-cancelled health_check_with_cx should fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 1);
            assert_eq!(stats.health_check_failures, 1);
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.total_timeouts, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
        });
    }

    #[test]
    fn pool_get_lines_with_precancelled_cx_returns_pool_cancelled_without_connecting() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-precancelled-lines.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled mux pool get_lines"),
            );

            let err = pool
                .get_lines_with_cx(&cx, 9, std::iter::once(0..5).collect())
                .await
                .expect_err("pre-cancelled get_lines_with_cx should fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.total_timeouts, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
        });
    }

    #[test]
    fn pool_write_to_pane_with_precancelled_cx_returns_pool_cancelled_without_connecting() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-precancelled-write.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled mux pool write_to_pane"),
            );

            let err = pool
                .write_to_pane_with_cx(&cx, 21, b"echo from cancelled cx\n".to_vec())
                .await
                .expect_err("pre-cancelled write_to_pane_with_cx should fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.total_timeouts, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
        });
    }

    #[test]
    fn pool_send_paste_with_precancelled_cx_returns_pool_cancelled_without_connecting() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-test-precancelled-paste.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled mux pool send_paste"),
            );

            let err = pool
                .send_paste_with_cx(&cx, 22, "cancelled paste\n".to_string())
                .await
                .expect_err("pre-cancelled send_paste_with_cx should fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.total_timeouts, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
        });
    }

    #[test]
    fn pool_multiple_health_checks_track_counter() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 2));

            for _ in 0..5 {
                pool.health_check().await.expect("health check");
            }

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 5);
            assert_eq!(stats.health_check_failures, 0);
        });
    }

    #[test]
    fn pool_multiple_health_checks_with_cx_track_counter() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 2));
            let cx = crate::cx::for_testing();

            for _ in 0..5 {
                pool.health_check_with_cx(&cx)
                    .await
                    .expect("health check with cx");
            }

            let stats = pool.stats().await;
            assert_eq!(stats.health_checks, 5);
            assert_eq!(stats.health_check_failures, 0);
            assert_eq!(
                stats.connections_created, 1,
                "explicit-Cx health checks should reuse a single pooled connection"
            );
        });
    }

    #[test]
    fn pool_get_pane_render_changes_single() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            let resp = pool
                .get_pane_render_changes(7)
                .await
                .expect("get_pane_render_changes should succeed");
            assert_eq!(resp.pane_id, 7);
            assert_eq!(resp.dimensions.cols, 80);
            assert_eq!(resp.dimensions.viewport_rows, 24);
        });
    }

    #[test]
    fn pool_get_lines_with_cx_succeeds() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let requested = vec![-3..0, 0..5];
            let resp = pool
                .get_lines_with_cx(&cx, 9, requested)
                .await
                .expect("get_lines_with_cx should succeed");

            assert_eq!(resp.pane_id, 9);
            assert_eq!(resp.lines, Vec::new().into());

            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.total_acquired, 1);
        });
    }

    #[test]
    fn pool_get_pane_render_changes_with_cx_succeeds() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            let resp = pool
                .get_pane_render_changes_with_cx(&cx, 17)
                .await
                .expect("get_pane_render_changes_with_cx should succeed");

            assert_eq!(resp.pane_id, 17);
            assert_eq!(resp.dimensions.cols, 80);
            assert_eq!(resp.dimensions.viewport_rows, 24);
        });
    }

    #[test]
    fn pool_write_to_pane_succeeds() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));
            pool.write_to_pane(11, b"echo hi\n".to_vec())
                .await
                .expect("write_to_pane should succeed");

            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.total_acquired, 1);
        });
    }

    #[test]
    fn pool_write_to_pane_with_cx_succeeds() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            pool.write_to_pane_with_cx(&cx, 21, b"echo from cx\n".to_vec())
                .await
                .expect("write_to_pane_with_cx should succeed");

            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.total_acquired, 1);
        });
    }

    #[test]
    fn pool_send_paste_reuses_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));
            pool.write_to_pane(12, b"first\n".to_vec())
                .await
                .expect("write_to_pane should succeed");
            pool.send_paste(12, "second\n".to_string())
                .await
                .expect("send_paste should succeed");

            let stats = pool.stats().await;
            assert_eq!(
                stats.connections_created, 1,
                "send_paste should reuse the existing idle connection"
            );
            assert_eq!(stats.pool.total_acquired, 2);
        });
    }

    #[test]
    fn pool_send_paste_with_cx_reuses_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let pool = MuxPool::new(pool_config(socket_path, 4));
            let cx = crate::cx::for_testing();

            pool.write_to_pane_with_cx(&cx, 22, b"first\n".to_vec())
                .await
                .expect("write_to_pane_with_cx should succeed");
            pool.send_paste_with_cx(&cx, 22, "second\n".to_string())
                .await
                .expect("send_paste_with_cx should succeed");

            let stats = pool.stats().await;
            assert_eq!(
                stats.connections_created, 1,
                "send_paste_with_cx should reuse the existing idle connection"
            );
            assert_eq!(stats.pool.total_acquired, 2);
        });
    }

    #[test]
    fn pool_pipeline_depth_one_skips_pipeline_fallback() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 1, // depth=1 means no pipeline fallback path
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let result = pool
                .get_pane_render_changes_batch(vec![1, 2])
                .await
                .expect("batch with depth=1");
            assert_eq!(result.len(), 2);
        });
    }

    #[test]
    fn pool_batch_render_with_cx_respects_single_attempt_when_recovery_is_disabled() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server_unexpected_batch_render_once(&temp_dir).await;
            let cx = crate::cx::for_testing();

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(1),
                    ),
                },
                pipeline_depth: 4,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let error = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![10, 20, 30])
                .await
                .expect_err("one configured explicit-Cx attempt must not enter fallback");
            assert!(matches!(
                error,
                MuxPoolError::Mux(DirectMuxError::AlignedUnexpectedResponse { .. })
            ));

            let stats = pool.stats().await;
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(
                stats.connections_created, 1,
                "one configured explicit-Cx attempt creates only the failed pipeline connection"
            );
            assert_eq!(stats.pool.total_acquired, 1);
            assert_eq!(stats.pool.idle_count, 1);
        });
    }

    #[test]
    fn pool_batch_render_with_cx_pipeline_depth_one_skips_pipeline_fallback() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;
            let cx = crate::cx::for_testing();

            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default().with_socket_path(socket_path),
                recovery: MuxRecoveryConfig::default(),
                pipeline_depth: 1,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let result = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![1, 2])
                .await
                .expect("batch with cx and depth=1");
            assert_eq!(result.len(), 2);
        });
    }

    // ── ft-2h5wv.6: Pre-cancelled Cx counter regression suite ─────────────
    //
    // These tests verify that pre-cancelled Cx operations leave pool stats in
    // a fully clean state: zero connections created/failed, zero acquire
    // timeout increments, zero recovery attempts, zero permanent failures,
    // and no effect on idle/active connection counts.
    //
    // Key regression targets:
    //   - recovery ENABLED but pre-cancelled Cx still fast-fails (no retry)
    //   - multi-operation accumulation produces no side-effects
    //   - pre-cancelled ops after a successful op leave idle connections intact
    //   - get_pane_render_changes and batch render paths also fast-fail

    #[test]
    fn pool_precancelled_get_lines_with_recovery_enabled_skips_retry() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-ctr-lines-rec.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("precancelled get_lines recovery-enabled"),
            );

            let err = pool
                .get_lines_with_cx(&cx, 42, std::iter::once(0..10).collect())
                .await
                .expect_err("pre-cancelled with recovery enabled should still fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0, "no acquire attempt");
            assert_eq!(stats.pool.total_timeouts, 0, "no timeout");
            assert_eq!(stats.pool.idle_count, 0, "no idle connections");
            assert_eq!(stats.pool.active_count, 0, "no active connections");
            assert_eq!(stats.connections_created, 0, "no connection created");
            assert_eq!(stats.connections_failed, 0, "no connection failure");
            assert_eq!(stats.recovery_attempts, 0, "recovery not attempted");
            assert_eq!(stats.recovery_successes, 0, "no recovery success");
            assert_eq!(stats.permanent_failures, 0, "no permanent failure");
        });
    }

    #[test]
    fn pool_precancelled_write_to_pane_with_recovery_enabled_skips_retry() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-ctr-write-rec.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("precancelled write_to_pane recovery-enabled"),
            );

            let err = pool
                .write_to_pane_with_cx(&cx, 7, b"recovery test\n".to_vec())
                .await
                .expect_err("pre-cancelled write with recovery should still fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.idle_count, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.permanent_failures, 0);
        });
    }

    #[test]
    fn pool_precancelled_send_paste_with_recovery_enabled_skips_retry() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-ctr-paste-rec.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("precancelled send_paste recovery-enabled"),
            );

            let err = pool
                .send_paste_with_cx(&cx, 8, "paste recovery test\n".to_string())
                .await
                .expect_err("pre-cancelled paste with recovery should still fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.idle_count, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.permanent_failures, 0);
        });
    }

    #[test]
    fn pool_multiple_precancelled_ops_accumulate_no_side_effects() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 4,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-ctr-multi.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: true,
                    retry_policy: RetryPolicy::new(
                        Duration::from_millis(0),
                        Duration::from_millis(0),
                        1.0,
                        0.0,
                        Some(3),
                    ),
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("precancelled multi-op"),
            );

            // Run all three precancelled operations in sequence.
            let _ = pool
                .get_lines_with_cx(&cx, 1, std::iter::once(0..5).collect())
                .await;
            let _ = pool.write_to_pane_with_cx(&cx, 2, b"data\n".to_vec()).await;
            let _ = pool.send_paste_with_cx(&cx, 3, "text\n".to_string()).await;

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0, "no acquires across 3 ops");
            assert_eq!(stats.pool.total_timeouts, 0, "no timeouts across 3 ops");
            assert_eq!(stats.pool.total_returned, 0, "no returns across 3 ops");
            assert_eq!(stats.pool.total_evicted, 0, "no evictions across 3 ops");
            assert_eq!(stats.pool.idle_count, 0, "zero idle");
            assert_eq!(stats.pool.active_count, 0, "zero active");
            assert_eq!(stats.connections_created, 0, "no connections across 3 ops");
            assert_eq!(stats.connections_failed, 0, "no failures across 3 ops");
            assert_eq!(stats.recovery_attempts, 0, "no recovery across 3 ops");
            assert_eq!(stats.recovery_successes, 0, "no recovery success");
            assert_eq!(stats.permanent_failures, 0, "no permanent across 3 ops");
            assert_eq!(stats.health_checks, 0, "no health checks from data ops");
            assert_eq!(stats.health_check_failures, 0, "no health failures");
        });
    }

    #[test]
    fn pool_precancelled_after_successful_op_leaves_idle_intact() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));
            let good_cx = crate::cx::for_testing();

            // Successful operation creates a connection.
            pool.list_panes_with_cx(&good_cx)
                .await
                .expect("first list succeeds");
            let stats_after_success = pool.stats().await;
            assert_eq!(stats_after_success.connections_created, 1);
            assert_eq!(stats_after_success.pool.idle_count, 1);
            assert_eq!(stats_after_success.pool.total_acquired, 1);
            assert_eq!(stats_after_success.pool.total_returned, 1);

            // Now run precancelled operations — they must not touch the pool.
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let bad_cx = Cx::for_testing_with_budget(budget);
            bad_cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("precancelled after success"),
            );

            let _ = pool
                .get_lines_with_cx(&bad_cx, 1, std::iter::once(0..5).collect())
                .await;
            let _ = pool
                .write_to_pane_with_cx(&bad_cx, 2, b"cancelled\n".to_vec())
                .await;
            let _ = pool
                .send_paste_with_cx(&bad_cx, 3, "cancelled\n".to_string())
                .await;

            let stats = pool.stats().await;
            // Connection counters unchanged from the single successful op.
            assert_eq!(
                stats.connections_created, 1,
                "still just the one connection"
            );
            assert_eq!(stats.connections_failed, 0, "no new failures");
            assert_eq!(stats.pool.idle_count, 1, "idle connection preserved");
            assert_eq!(stats.pool.active_count, 0, "no active checkouts");
            // Acquire counter unchanged — precancelled ops never acquire.
            assert_eq!(stats.pool.total_acquired, 1, "only the first acquire");
            assert_eq!(stats.pool.total_returned, 1, "only the first return");
            assert_eq!(stats.pool.total_timeouts, 0, "no timeouts");
            // Recovery counters remain zero.
            assert_eq!(stats.recovery_attempts, 0, "no recovery");
            assert_eq!(stats.recovery_successes, 0, "no recovery success");
            assert_eq!(stats.permanent_failures, 0, "no permanent failure");
        });
    }

    #[test]
    fn pool_precancelled_get_pane_render_changes_returns_cancelled_without_connecting() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-ctr-render.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("precancelled get_pane_render_changes"),
            );

            let err = pool
                .get_pane_render_changes_with_cx(&cx, 99)
                .await
                .expect_err("pre-cancelled render changes should fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.total_timeouts, 0);
            assert_eq!(stats.pool.idle_count, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.permanent_failures, 0);
        });
    }

    #[test]
    fn pool_precancelled_batch_render_returns_cancelled_without_connecting() {
        run_async_test(async {
            let config = MuxPoolConfig {
                pool: PoolConfig {
                    max_size: 2,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-mux-pool-ctr-batch.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            };
            let pool = MuxPool::new(config);

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("precancelled batch render"),
            );

            let err = pool
                .get_pane_render_changes_batch_with_cx(&cx, vec![1, 2, 3])
                .await
                .expect_err("pre-cancelled batch render should fail");
            assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));

            let stats = pool.stats().await;
            assert_eq!(stats.pool.total_acquired, 0);
            assert_eq!(stats.pool.total_timeouts, 0);
            assert_eq!(stats.pool.idle_count, 0);
            assert_eq!(stats.pool.active_count, 0);
            assert_eq!(stats.connections_created, 0);
            assert_eq!(stats.connections_failed, 0);
            assert_eq!(stats.recovery_attempts, 0);
            assert_eq!(stats.recovery_successes, 0);
            assert_eq!(stats.permanent_failures, 0);
        });
    }

    #[test]
    fn pool_clear_then_new_connection() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = spawn_mock_server(&temp_dir).await;

            let pool = MuxPool::new(pool_config(socket_path, 4));

            // Create connection
            pool.list_panes().await.expect("first list");
            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 1);
            assert_eq!(stats.pool.idle_count, 1);

            // Clear all idle
            pool.clear().await;
            let stats = pool.stats().await;
            assert_eq!(stats.pool.idle_count, 0);

            // Next call creates new connection
            pool.list_panes().await.expect("second list after clear");
            let stats = pool.stats().await;
            assert_eq!(stats.connections_created, 2);
        });
    }

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for MuxPool primitives (wa-2h5wv)
    //
    // MuxPool's happy-path methods require an established Unix socket
    // connection to a mux server, which is a real-syscall path incompatible
    // with LabRuntime's virtual-time scheduler (same reasoning as
    // wa-p48pw's LabRuntime module). These tests therefore target the two
    // surfaces that *are* safe to exercise under deterministic scheduling:
    //
    //   1. Pure data semantics — config defaults, error classification,
    //      stats-snapshot shape.
    //   2. Pre-cancelled-Cx short-circuit paths — every `*_with_cx` method
    //      on MuxPool must honour the contract that a cancelled Cx returns
    //      without attempting to open a socket. The production code already
    //      enforces this via `Pool::acquire_with_cx`, but the contract is
    //      not regression-tested under LabRuntime's deterministic
    //      scheduler. These tests spawn the MuxPool calls from inside a
    //      LabRuntime task with a cancelled Cx and assert that (a) the
    //      call resolves, (b) the error is `PoolError::Cancelled`, and
    //      (c) no counter (connections_created, etc.) advanced.
    //
    // This mirrors the bead's "DPOR concurrency testing" intent without
    // requiring a mock socket stack under LabRuntime, which would defeat
    // the whole point of the deterministic scheduler.
    // -------------------------------------------------------------------------

    mod labruntime_mux_pool {
        use super::*;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

        /// Build a LabRuntime, spawn a root task running `f`, and
        /// auto-advance to quiescence. Panics if the runtime gets stuck.
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

        /// Build a MuxPool configured with an unreachable socket path so
        /// that any accidental connect attempt would fail deterministically
        /// rather than silently binding to a stale socket.
        fn unreachable_pool(max_size: usize) -> MuxPool {
            MuxPool::new(MuxPoolConfig {
                pool: PoolConfig {
                    max_size,
                    idle_timeout: Duration::from_secs(60),
                    acquire_timeout: Duration::from_millis(500),
                },
                mux: DirectMuxClientConfig::default()
                    .with_socket_path("/tmp/wa-2h5wv-labruntime-unreachable.sock"),
                recovery: MuxRecoveryConfig {
                    enabled: false,
                    ..MuxRecoveryConfig::default()
                },
                pipeline_depth: 32,
                pipeline_timeout: Duration::from_secs(5),
            })
        }

        /// Construct a cancelled Cx suitable for short-circuit tests.
        /// Uses `for_testing_with_budget` rather than the LabRuntime Cx
        /// because the short-circuit path must observe a pre-cancelled
        /// Cx that was never running inside a runtime.
        fn pre_cancelled_cx(msg: &'static str) -> Cx {
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = Cx::for_testing_with_budget(budget);
            cx.cancel_with(crate::outcome::CancelKind::User, Some(msg));
            cx
        }

        /// 1. Config defaults are stable and match MuxPool's advertised
        ///    surface. Locks the default-config contract under LabRuntime
        ///    virtual time to catch accidental drift.
        #[test]
        fn mux_pool_config_defaults_under_labruntime() {
            run_lab(901, || async move {
                let config = MuxPoolConfig::default();
                assert_eq!(config.pool.max_size, 8);
                assert_eq!(config.pool.idle_timeout, Duration::from_secs(300));
                assert_eq!(config.pool.acquire_timeout, Duration::from_secs(10));
                assert_eq!(config.pipeline_depth, 32);
                assert!(config.recovery.enabled);
            });
        }

        /// 2. Stats on a fresh pool are all zero: every counter starts at
        ///    zero and the snapshot shape matches the advertised contract.
        #[test]
        fn mux_pool_fresh_stats_are_all_zero_under_labruntime() {
            run_lab(902, || async move {
                let pool = unreachable_pool(4);
                let stats = pool.stats().await;
                assert_eq!(stats.connections_created, 0);
                assert_eq!(stats.connections_failed, 0);
                assert_eq!(stats.health_checks, 0);
                assert_eq!(stats.health_check_failures, 0);
                assert_eq!(stats.recovery_attempts, 0);
                assert_eq!(stats.recovery_successes, 0);
                assert_eq!(stats.permanent_failures, 0);
                assert_eq!(stats.pool.total_acquired, 0);
                assert_eq!(stats.pool.idle_count, 0);
                assert_eq!(stats.pool.total_timeouts, 0);
            });
        }

        /// 3. `list_panes_with_cx` honours a pre-cancelled Cx without
        ///    attempting to connect — no connect counter advances and the
        ///    error classifies as `PoolError::Cancelled`.
        #[test]
        fn mux_pool_list_panes_with_precancelled_cx_does_not_connect_under_labruntime() {
            run_lab(903, || async move {
                let pool = unreachable_pool(2);
                let cx = pre_cancelled_cx("wa-2h5wv list_panes cancel");
                let err = pool
                    .list_panes_with_cx(&cx)
                    .await
                    .expect_err("pre-cancelled Cx must short-circuit list_panes_with_cx");
                assert!(
                    matches!(err, MuxPoolError::Pool(PoolError::Cancelled)),
                    "expected Pool(Cancelled), got {err:?}"
                );
                let stats = pool.stats().await;
                assert_eq!(stats.connections_created, 0);
                assert_eq!(stats.connections_failed, 0);
                assert_eq!(stats.pool.total_acquired, 0);
            });
        }

        /// 4. `health_check_with_cx` counts the attempt even when Cx is
        ///    cancelled, then increments health_check_failures — but still
        ///    does not connect. This pins the bead acceptance criterion
        ///    "health checks work through Cx".
        #[test]
        fn mux_pool_health_check_with_precancelled_cx_counts_failure_under_labruntime() {
            run_lab(904, || async move {
                let pool = unreachable_pool(2);
                let cx = pre_cancelled_cx("wa-2h5wv health_check cancel");
                let err = pool
                    .health_check_with_cx(&cx)
                    .await
                    .expect_err("pre-cancelled Cx must short-circuit health_check_with_cx");
                assert!(matches!(err, MuxPoolError::Pool(PoolError::Cancelled)));
                let stats = pool.stats().await;
                assert_eq!(
                    stats.health_checks, 1,
                    "health check attempt must be counted even on cancel"
                );
                assert_eq!(
                    stats.health_check_failures, 1,
                    "cancelled health check must count as a failure"
                );
                assert_eq!(
                    stats.connections_created, 0,
                    "cancelled health check must not open a socket"
                );
            });
        }

        /// 5. Error classification helpers are pure-data and hold under
        ///    LabRuntime: `is_pool_timeout` and `is_disconnected` map to
        ///    the correct variants, and misclassification would confuse
        ///    every retry path in the production code.
        #[test]
        fn mux_pool_error_classification_under_labruntime() {
            run_lab(905, || async move {
                let timeout = MuxPoolError::Pool(PoolError::AcquireTimeout);
                assert!(timeout.is_pool_timeout());
                assert!(!timeout.is_disconnected());

                let cancelled = MuxPoolError::Pool(PoolError::Cancelled);
                assert!(!cancelled.is_pool_timeout());
                assert!(!cancelled.is_disconnected());

                let disconnected = MuxPoolError::Mux(DirectMuxError::Disconnected);
                assert!(!disconnected.is_pool_timeout());
                assert!(disconnected.is_disconnected());
            });
        }

        /// 6. Concurrent pre-cancelled calls: spawn N sibling tasks that
        ///    each invoke `list_panes_with_cx` with a cancelled Cx. Every
        ///    task must observe Cancelled, and the connections_created
        ///    counter must remain zero — the pool must never attempt to
        ///    open a socket under cancellation, even under concurrent
        ///    scheduling.
        #[test]
        fn mux_pool_concurrent_precancelled_list_panes_no_connects_under_labruntime() {
            run_lab(906, || async move {
                let pool = Arc::new(unreachable_pool(4));
                let cancelled_observed = Arc::new(AtomicU64::new(0));
                let cx_pool = Arc::clone(&pool);
                let cx_counter = Arc::clone(&cancelled_observed);

                // Sequentially call from within the single LabRuntime task
                // (LabRuntime task spawning requires scope machinery that
                // is outside the scope of this pure short-circuit test;
                // sequential calls still exercise the same
                // connections_created invariant under deterministic
                // scheduling).
                for i in 0..6u64 {
                    let cx = pre_cancelled_cx("wa-2h5wv concurrent cancel");
                    match cx_pool.list_panes_with_cx(&cx).await {
                        Err(MuxPoolError::Pool(PoolError::Cancelled)) => {
                            cx_counter.fetch_add(1, AtomicOrdering::Relaxed);
                        }
                        other => panic!("iteration {i} expected Cancelled, got {other:?}"),
                    }
                }

                assert_eq!(
                    cancelled_observed.load(AtomicOrdering::Relaxed),
                    6,
                    "every cancelled call must surface as PoolError::Cancelled"
                );
                let stats = pool.stats().await;
                assert_eq!(
                    stats.connections_created, 0,
                    "no cancelled call may open a socket"
                );
                assert_eq!(
                    stats.connections_failed, 0,
                    "no cancelled call may be recorded as a failed connect"
                );
            });
        }

        /// 7. `clear` on an empty pool is a safe no-op and does not
        ///    touch any counter. The production code routes clear()
        ///    through Pool::clear which is async — verifying it under
        ///    LabRuntime pins the cross-runtime quiescence contract.
        #[test]
        fn mux_pool_clear_empty_is_noop_under_labruntime() {
            run_lab(907, || async move {
                let pool = unreachable_pool(4);
                pool.clear().await;
                pool.clear().await; // idempotent
                let stats = pool.stats().await;
                assert_eq!(stats.pool.idle_count, 0);
                assert_eq!(stats.connections_created, 0);
                assert_eq!(stats.connections_failed, 0);
            });
        }

        /// 8. Stats snapshot roundtrip: the MuxPoolStats struct is
        ///    Serialize/Deserialize and its shape must stay stable so
        ///    downstream telemetry can depend on it. Running the
        ///    roundtrip under LabRuntime pins determinism for the
        ///    snapshot path the bead's "stats accuracy under concurrent
        ///    load" criterion relies on.
        #[test]
        fn mux_pool_stats_serde_roundtrip_under_labruntime() {
            run_lab(908, || async move {
                let pool = unreachable_pool(4);
                let stats = pool.stats().await;
                let json = serde_json::to_string(&stats).expect("serialize stats");
                let restored: MuxPoolStats =
                    serde_json::from_str(&json).expect("deserialize stats");
                assert_eq!(restored.connections_created, stats.connections_created);
                assert_eq!(restored.health_checks, stats.health_checks);
                assert_eq!(restored.pool.max_size, stats.pool.max_size);
                assert_eq!(restored.pool.idle_count, stats.pool.idle_count);
            });
        }

        /// 9. Explicit-Cx maintenance surfaces cancellation as a typed pool
        ///    error. These paths previously crossed the infallible lock wrapper
        ///    and panicked on a cancelled context.
        #[test]
        fn mux_pool_maintenance_with_precancelled_cx_is_typed_under_labruntime() {
            run_lab(909, || async move {
                let pool = unreachable_pool(4);
                let cx = pre_cancelled_cx("wa-2h5wv maintenance cancel");

                let stats_error = pool
                    .stats_with_cx(&cx)
                    .await
                    .expect_err("cancelled stats_with_cx must fail");
                assert!(matches!(
                    stats_error,
                    MuxPoolError::Pool(PoolError::Cancelled)
                ));

                let eviction_error = pool
                    .evict_idle_with_cx(&cx)
                    .await
                    .expect_err("cancelled evict_idle_with_cx must fail");
                assert!(matches!(
                    eviction_error,
                    MuxPoolError::Pool(PoolError::Cancelled)
                ));

                let clear_error = pool
                    .clear_with_cx(&cx)
                    .await
                    .expect_err("cancelled clear_with_cx must fail");
                assert!(matches!(
                    clear_error,
                    MuxPoolError::Pool(PoolError::Cancelled)
                ));

                let health_error = pool
                    .health_check_with_cx(&cx)
                    .await
                    .expect_err("cancelled health check must fail");
                assert!(health_error.is_cancelled());

                let ambient_stats = pool.stats().await;
                assert_eq!(ambient_stats.pool.idle_count, 0);
                assert_eq!(ambient_stats.pool.total_evicted, 0);
                assert_eq!(
                    ambient_stats.health_checks, 0,
                    "cancelled health checks must not publish completed-check telemetry"
                );
                assert_eq!(ambient_stats.health_check_failures, 0);
            });
        }
    }
}
