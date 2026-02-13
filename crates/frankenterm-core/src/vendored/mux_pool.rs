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
//! - On success, the connection is returned to the pool for reuse.
//! - On error, the connection is dropped (buffer state may be corrupt).
//! - The underlying `Pool<C>` provides semaphore-based concurrency limiting
//!   and idle timeout eviction.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::pool::{Pool, PoolAcquireGuard, PoolConfig, PoolError, PoolStats};
use crate::retry::RetryPolicy;
use crate::runtime_compat::sleep;

use super::mux_client::{
    DirectMuxClient, DirectMuxClientConfig, DirectMuxError, ProtocolErrorKind,
};
use codec::{GetLinesResponse, GetPaneRenderChangesResponse, ListPanesResponse, UnitResponse};

/// Error type for mux pool operations.
#[derive(Debug, thiserror::Error)]
pub enum MuxPoolError {
    /// The pool could not acquire a slot (timeout or closed).
    #[error("pool: {0}")]
    Pool(#[from] PoolError),
    /// The mux client encountered an error.
    #[error("mux: {0}")]
    Mux(#[from] DirectMuxError),
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
        matches!(self, Self::Mux(DirectMuxError::Disconnected))
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

type MuxOpFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DirectMuxError>> + Send + 'a>>;

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

    /// Acquire a client from the pool or create a new one.
    ///
    /// Returns the client and a guard that holds the concurrency slot.
    /// The guard must be dropped after the client is returned (or discarded).
    async fn acquire_client(&self) -> Result<(DirectMuxClient, PoolAcquireGuard), MuxPoolError> {
        let result = self.pool.acquire().await?;
        let (conn, guard) = result.into_parts();
        let client = match conn {
            Some(c) => c,
            None => match DirectMuxClient::connect(self.mux_config.clone()).await {
                Ok(client) => {
                    self.connections_created.fetch_add(1, Ordering::Relaxed);
                    client
                }
                Err(e) => {
                    self.connections_failed.fetch_add(1, Ordering::Relaxed);
                    return Err(MuxPoolError::Mux(e));
                }
            },
        };
        Ok((client, guard))
    }

    /// Return a healthy client to the pool for reuse.
    async fn return_client(&self, client: DirectMuxClient) {
        self.pool.put(client).await;
    }

    async fn execute_with_recovery<T, Op>(
        &self,
        op_name: &'static str,
        mut op: Op,
    ) -> Result<T, MuxPoolError>
    where
        Op: for<'a> FnMut(&'a mut DirectMuxClient) -> MuxOpFuture<'a, T>,
    {
        let max_attempts = if self.recovery.enabled {
            self.recovery.retry_policy.max_attempts.unwrap_or(1).max(1)
        } else {
            1
        };

        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);

            let (mut client, _guard) = self.acquire_client().await?;
            let result = op(&mut client).await;
            match result {
                Ok(value) => {
                    self.return_client(client).await;
                    if attempt > 1 {
                        self.recovery_successes.fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(value);
                }
                Err(err) => {
                    let kind = err.protocol_error_kind();
                    let can_retry = self.recovery.enabled
                        && attempt < max_attempts
                        && matches!(
                            kind,
                            ProtocolErrorKind::Recoverable | ProtocolErrorKind::Transient
                        );
                    if can_retry {
                        self.recovery_attempts.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(
                            op = op_name,
                            attempt,
                            max_attempts,
                            kind = ?kind,
                            error = %err,
                            "mux pool op failed; reconnecting and retrying"
                        );

                        let delay = self
                            .recovery
                            .retry_policy
                            .delay_for_attempt(attempt.saturating_sub(1));
                        if !delay.is_zero() {
                            sleep(delay).await;
                        }
                        continue;
                    }

                    if kind == ProtocolErrorKind::Permanent {
                        self.permanent_failures.fetch_add(1, Ordering::Relaxed);
                    }

                    tracing::debug!(
                        op = op_name,
                        attempt,
                        max_attempts,
                        kind = ?kind,
                        error = %err,
                        "mux pool op failed; dropping client"
                    );
                    return Err(MuxPoolError::Mux(err));
                }
            }
        }
    }

    /// List all panes via a pooled connection.
    pub async fn list_panes(&self) -> Result<ListPanesResponse, MuxPoolError> {
        self.execute_with_recovery("list_panes", |client| Box::pin(client.list_panes()))
            .await
    }

    /// Get lines from a pane via a pooled connection.
    pub async fn get_lines(
        &self,
        pane_id: u64,
        lines: Vec<std::ops::Range<isize>>,
    ) -> Result<GetLinesResponse, MuxPoolError> {
        self.execute_with_recovery("get_lines", move |client| {
            let lines = lines.clone();
            Box::pin(client.get_lines(pane_id, lines))
        })
        .await
    }

    /// Poll for pane render changes via a pooled connection.
    pub async fn get_pane_render_changes(
        &self,
        pane_id: u64,
    ) -> Result<GetPaneRenderChangesResponse, MuxPoolError> {
        self.execute_with_recovery("get_pane_render_changes", |client| {
            Box::pin(client.get_pane_render_changes(pane_id))
        })
        .await
    }

    /// Poll render changes for many panes using depth-limited pipelining.
    ///
    /// If pipelining fails, falls back to sequential requests on a fresh
    /// connection so callers still receive results.
    pub async fn get_pane_render_changes_batch(
        &self,
        pane_ids: Vec<u64>,
    ) -> Result<Vec<GetPaneRenderChangesResponse>, MuxPoolError> {
        if pane_ids.is_empty() {
            return Ok(Vec::new());
        }

        let depth = self.pipeline_depth;
        let timeout = self.pipeline_timeout;
        let pane_ids_for_pipeline = pane_ids.clone();
        let pipeline_result = self
            .execute_with_recovery("get_pane_render_changes_batch", move |client| {
                let pane_ids = pane_ids_for_pipeline.clone();
                Box::pin(async move {
                    client
                        .get_pane_render_changes_batch(&pane_ids, depth, timeout)
                        .await
                })
            })
            .await;

        if depth <= 1 {
            return pipeline_result;
        }

        match pipeline_result {
            Ok(result) => Ok(result),
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    depth,
                    "pipelined render batch failed; falling back to sequential"
                );
                self.execute_with_recovery(
                    "get_pane_render_changes_batch_fallback",
                    move |client| {
                        let pane_ids = pane_ids.clone();
                        Box::pin(async move {
                            client
                                .get_pane_render_changes_batch(&pane_ids, 1, timeout)
                                .await
                        })
                    },
                )
                .await
            }
        }
    }

    /// Write raw bytes to a pane via a pooled connection (no-paste mode).
    pub async fn write_to_pane(
        &self,
        pane_id: u64,
        data: Vec<u8>,
    ) -> Result<UnitResponse, MuxPoolError> {
        self.execute_with_recovery("write_to_pane", move |client| {
            let data = data.clone();
            Box::pin(client.write_to_pane(pane_id, data))
        })
        .await
    }

    /// Send text via paste mode through a pooled connection.
    pub async fn send_paste(
        &self,
        pane_id: u64,
        data: String,
    ) -> Result<UnitResponse, MuxPoolError> {
        self.execute_with_recovery("send_paste", move |client| {
            let data = data.clone();
            Box::pin(client.send_paste(pane_id, data))
        })
        .await
    }

    /// Run a health check by listing panes on a pooled connection.
    pub async fn health_check(&self) -> Result<(), MuxPoolError> {
        self.health_checks.fetch_add(1, Ordering::Relaxed);
        match self.list_panes().await {
            Ok(_) => Ok(()),
            Err(e) => {
                self.health_check_failures.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// Evict idle connections that have exceeded the idle timeout.
    pub async fn evict_idle(&self) -> usize {
        self.pool.evict_idle().await
    }

    /// Clear all idle connections from the pool.
    pub async fn clear(&self) {
        self.pool.clear().await;
    }

    /// Get pool statistics.
    pub async fn stats(&self) -> MuxPoolStats {
        MuxPoolStats {
            pool: self.pool.stats().await,
            connections_created: self.connections_created.load(Ordering::Relaxed),
            connections_failed: self.connections_failed.load(Ordering::Relaxed),
            health_checks: self.health_checks.load(Ordering::Relaxed),
            health_check_failures: self.health_check_failures.load(Ordering::Relaxed),
            recovery_attempts: self.recovery_attempts.load(Ordering::Relaxed),
            recovery_successes: self.recovery_successes.load(Ordering::Relaxed),
            permanent_failures: self.permanent_failures.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_compat::timeout;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::time::Duration;

    use codec::{
        CODEC_VERSION, GetCodecVersionResponse, GetPaneRenderChangesResponse, ListPanesResponse,
        Pdu, UnitResponse,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Spawn a mock mux server that handles handshake + ListPanes.
    /// Returns the socket path.
    async fn spawn_mock_server(temp_dir: &tempfile::TempDir) -> PathBuf {
        let socket_path = temp_dir.path().join("mux-pool-test.sock");
        let listener =
            tokio::net::UnixListener::bind(&socket_path).expect("bind mock mux listener");

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                tokio::spawn(async move {
                    let mut read_buf = Vec::new();
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match stream.read(&mut temp).await {
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
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::ListPanes(_) => Pdu::ListPanesResponse(ListPanesResponse {
                                    tabs: Vec::new(),
                                    tab_titles: Vec::new(),
                                    window_titles: HashMap::new(),
                                }),
                                Pdu::GetPaneRenderChanges(req) => {
                                    Pdu::GetPaneRenderChangesResponse(
                                        GetPaneRenderChangesResponse {
                                            pane_id: req.pane_id,
                                            mouse_grabbed: false,
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
                                            dirty_lines: Vec::new(),
                                            title: format!("pane-{}", req.pane_id),
                                            working_dir: None,
                                            bonus_lines: Vec::new().into(),
                                            input_serial: None,
                                            seqno: req.pane_id,
                                        },
                                    )
                                }
                                _ => continue,
                            };
                            responses.push((decoded.serial, response));
                        }
                        for (serial, pdu) in responses {
                            let mut out = Vec::new();
                            pdu.encode(&mut out, serial).expect("encode response");
                            if stream.write_all(&out).await.is_err() {
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
        let listener =
            tokio::net::UnixListener::bind(&socket_path).expect("bind mock mux listener");

        let first_bad = Arc::new(AtomicBool::new(true));

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };

                let first_bad = Arc::clone(&first_bad);
                tokio::spawn(async move {
                    let mut read_buf = Vec::new();
                    loop {
                        let mut temp = vec![0u8; 4096];
                        let read = match stream.read(&mut temp).await {
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
                                    })
                                }
                                Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                                Pdu::ListPanes(_) => {
                                    if first_bad.swap(false, AtomicOrdering::SeqCst) {
                                        // Wrong response type: triggers UnexpectedResponse.
                                        Pdu::UnitResponse(UnitResponse {})
                                    } else {
                                        Pdu::ListPanesResponse(ListPanesResponse {
                                            tabs: Vec::new(),
                                            tab_titles: Vec::new(),
                                            window_titles: HashMap::new(),
                                        })
                                    }
                                }
                                _ => continue,
                            };
                            responses.push((decoded.serial, response));
                        }

                        for (serial, pdu) in responses {
                            let mut out = Vec::new();
                            pdu.encode(&mut out, serial).expect("encode response");
                            if stream.write_all(&out).await.is_err() {
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

    #[tokio::test]
    async fn pool_creates_connection_on_first_acquire() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = spawn_mock_server(&temp_dir).await;

        let pool = MuxPool::new(pool_config(socket_path, 4));
        let result = pool.list_panes().await.expect("list_panes should succeed");
        assert!(result.tabs.is_empty());

        let stats = pool.stats().await;
        assert_eq!(stats.connections_created, 1);
        assert_eq!(stats.connections_failed, 0);
    }

    #[tokio::test]
    async fn pool_reuses_idle_connection() {
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
    }

    #[tokio::test]
    async fn pool_concurrent_operations_use_separate_connections() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = spawn_mock_server(&temp_dir).await;

        let pool = Arc::new(MuxPool::new(pool_config(socket_path, 4)));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
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
    }

    #[tokio::test]
    async fn pool_connect_failure_increments_counter() {
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
        assert_eq!(stats.connections_failed, 1);
    }

    #[tokio::test]
    async fn pool_recovers_from_unexpected_response_by_reconnecting() {
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
        let resp = pool
            .list_panes()
            .await
            .expect("list_panes should recover after reconnect");
        assert!(resp.tabs.is_empty());

        let stats = pool.stats().await;
        assert_eq!(stats.recovery_attempts, 1);
        assert_eq!(stats.recovery_successes, 1);
        assert_eq!(stats.connections_created, 2);
    }

    #[tokio::test]
    async fn pool_health_check_success() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let socket_path = spawn_mock_server(&temp_dir).await;

        let pool = MuxPool::new(pool_config(socket_path, 2));
        pool.health_check().await.expect("health check should pass");

        let stats = pool.stats().await;
        assert_eq!(stats.health_checks, 1);
        assert_eq!(stats.health_check_failures, 0);
    }

    #[tokio::test]
    async fn pool_health_check_failure() {
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

        pool.health_check()
            .await
            .expect_err("health check should fail");

        let stats = pool.stats().await;
        assert_eq!(stats.health_checks, 1);
        assert_eq!(stats.health_check_failures, 1);
    }

    #[tokio::test]
    async fn pool_clear_evicts_all_idle() {
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
    }

    #[tokio::test]
    async fn pool_idle_timeout_eviction() {
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
    }

    #[tokio::test]
    async fn pool_stats_are_accurate() {
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
    }

    #[tokio::test]
    async fn pool_respects_max_connections() {
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
        let result = timeout(Duration::from_millis(200), pool2.list_panes()).await;

        match result {
            Ok(Err(MuxPoolError::Pool(PoolError::AcquireTimeout))) => {} // expected
            Ok(Err(e)) => panic!("expected AcquireTimeout, got: {e}"),
            Ok(Ok(_)) => panic!("should not have succeeded"),
            Err(_) => {} // outer timeout is also acceptable
        }

        // Return the first client and drop the guard
        pool.return_client(client).await;
        drop(_guard);
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

        let mux_err = MuxPoolError::Mux(DirectMuxError::Disconnected);
        assert!(mux_err.to_string().contains("mux"));
        assert!(!mux_err.is_pool_timeout());
        assert!(mux_err.is_disconnected());
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
}
