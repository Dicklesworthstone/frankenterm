//! Test fixtures for asupersync-based testing.
//!
//! Provides mock types, test data generators, and simulation helpers
//! used across LabRuntime integration tests.

use asupersync::runtime::RuntimeBuilder;
use asupersync::sync::Mutex;
use asupersync::{Budget, CancelKind, Cx};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// MockPool — simulates a connection pool using Semaphore + Mutex
// ---------------------------------------------------------------------------

/// A mock connection pool for testing pool patterns with asupersync primitives.
pub struct MockPool {
    gate: asupersync::sync::Semaphore,
    state: Mutex<MockPoolState>,
    total_acquired: AtomicU64,
    checked_out: AtomicU64,
    capacity: usize,
}

struct MockPoolState {
    available: Vec<u64>,
    next_id: u64,
}

impl MockPool {
    /// Create a new mock pool with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let available: Vec<u64> = (0..capacity as u64).collect();
        Self {
            gate: asupersync::sync::Semaphore::new(capacity),
            state: Mutex::new(MockPoolState {
                available,
                next_id: capacity as u64,
            }),
            total_acquired: AtomicU64::new(0),
            checked_out: AtomicU64::new(0),
            capacity,
        }
    }

    /// Acquire a connection from the pool.
    pub async fn acquire(&self, cx: &Cx) -> Result<MockConnection, String> {
        let _permit = self
            .gate
            .acquire(cx, 1)
            .await
            .map_err(|e| format!("semaphore acquire failed: {e}"))?;

        let mut state = self
            .state
            .lock(cx)
            .await
            .map_err(|e| format!("mutex lock failed: {e}"))?;

        let conn_id = state.available.pop().unwrap_or_else(|| {
            let id = state.next_id;
            state.next_id += 1;
            id
        });

        self.total_acquired.fetch_add(1, Ordering::Relaxed);
        self.checked_out.fetch_add(1, Ordering::Relaxed);
        Ok(MockConnection { id: conn_id })
    }

    /// Return a connection to the pool.
    pub async fn release(&self, cx: &Cx, conn: MockConnection) -> Result<(), String> {
        let mut state = self
            .state
            .lock(cx)
            .await
            .map_err(|e| format!("mutex lock failed: {e}"))?;
        state.available.push(conn.id);
        self.checked_out.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    /// Get the total number of acquire operations.
    #[must_use]
    pub fn total_acquired(&self) -> u64 {
        self.total_acquired.load(Ordering::Relaxed)
    }

    /// Get the pool capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get the number of connections currently available (not checked out).
    #[must_use]
    pub fn available_permits(&self) -> usize {
        let out = self.checked_out.load(Ordering::Relaxed) as usize;
        self.capacity.saturating_sub(out)
    }
}

/// A mock connection from `MockPool`.
#[derive(Debug)]
pub struct MockConnection {
    pub id: u64,
}

// ---------------------------------------------------------------------------
// MockMuxClient — simulates a mux client returning canned responses
// ---------------------------------------------------------------------------

/// A mock mux client that returns pre-configured responses.
pub struct MockMuxClient {
    pane_content: Mutex<HashMap<u64, String>>,
    request_count: AtomicU64,
}

impl MockMuxClient {
    /// Create a new mock mux client.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pane_content: Mutex::new(HashMap::new()),
            request_count: AtomicU64::new(0),
        }
    }

    /// Set the content for a specific pane.
    pub async fn set_pane_content(
        &self,
        cx: &Cx,
        pane_id: u64,
        content: String,
    ) -> Result<(), String> {
        let mut map = self
            .pane_content
            .lock(cx)
            .await
            .map_err(|e| format!("lock failed: {e}"))?;
        map.insert(pane_id, content);
        Ok(())
    }

    /// Get the content for a specific pane (simulates a mux request).
    pub async fn get_pane_text(&self, cx: &Cx, pane_id: u64) -> Result<String, String> {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        let map = self
            .pane_content
            .lock(cx)
            .await
            .map_err(|e| format!("lock failed: {e}"))?;
        map.get(&pane_id)
            .cloned()
            .ok_or_else(|| format!("pane {pane_id} not found"))
    }

    /// Get the total request count.
    #[must_use]
    pub fn request_count(&self) -> u64 {
        self.request_count.load(Ordering::Relaxed)
    }
}

impl Default for MockMuxClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// MockUnixStream — in-memory loopback stream pair for IPC testing
// ---------------------------------------------------------------------------

/// One half of a bidirectional in-memory byte stream.
///
/// Created in pairs via [`mock_unix_stream_pair`]. Each side can write bytes
/// that the other side reads, simulating a Unix socket loopback connection
/// without touching the filesystem.
pub struct MockUnixStream {
    /// Shared buffer: bytes written by the remote, available for us to read.
    read_buf: Arc<Mutex<std::collections::VecDeque<u8>>>,
    /// Shared buffer: bytes we write, available for the remote to read.
    write_buf: Arc<Mutex<std::collections::VecDeque<u8>>>,
    /// Total bytes written through this end.
    bytes_written: AtomicU64,
    /// Total bytes read through this end.
    bytes_read: AtomicU64,
    /// Whether this end has been closed.
    closed: std::sync::atomic::AtomicBool,
}

impl MockUnixStream {
    /// Write bytes to the stream (delivered to the remote end).
    pub async fn write(&self, cx: &Cx, data: &[u8]) -> Result<usize, String> {
        if self.closed.load(Ordering::Relaxed) {
            return Err("stream closed".to_string());
        }
        let mut target = self
            .write_buf
            .lock(cx)
            .await
            .map_err(|e| format!("write lock failed: {e}"))?;
        target.extend(data);
        self.bytes_written
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(data.len())
    }

    /// Read available bytes from the stream (sent by the remote end).
    pub async fn read(&self, cx: &Cx, max_bytes: usize) -> Result<Vec<u8>, String> {
        let mut buf = self
            .read_buf
            .lock(cx)
            .await
            .map_err(|e| format!("read lock failed: {e}"))?;
        let count = max_bytes.min(buf.len());
        let data: Vec<u8> = buf.drain(..count).collect();
        self.bytes_read
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(data)
    }

    /// Total bytes written through this end.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    /// Total bytes read through this end.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read.load(Ordering::Relaxed)
    }

    /// Close this end of the stream.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// Whether this end is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// Create a connected pair of in-memory mock streams.
///
/// Data written to one side can be read from the other, simulating a Unix
/// socket pair without filesystem access.
#[must_use]
pub fn mock_unix_stream_pair() -> (Arc<MockUnixStream>, Arc<MockUnixStream>) {
    // Shared buffers: A's write_buf = B's read_buf and vice versa
    let buf_a_to_b = Arc::new(Mutex::new(std::collections::VecDeque::<u8>::new()));
    let buf_b_to_a = Arc::new(Mutex::new(std::collections::VecDeque::<u8>::new()));

    let stream_a = Arc::new(MockUnixStream {
        read_buf: buf_b_to_a.clone(),  // A reads what B writes
        write_buf: buf_a_to_b.clone(), // A writes to B's read buffer
        bytes_written: AtomicU64::new(0),
        bytes_read: AtomicU64::new(0),
        closed: std::sync::atomic::AtomicBool::new(false),
    });

    let stream_b = Arc::new(MockUnixStream {
        read_buf: buf_a_to_b,  // B reads what A writes
        write_buf: buf_b_to_a, // B writes to A's read buffer
        bytes_written: AtomicU64::new(0),
        bytes_read: AtomicU64::new(0),
        closed: std::sync::atomic::AtomicBool::new(false),
    });

    (stream_a, stream_b)
}

// ---------------------------------------------------------------------------
// TestPaneData — test data generator for pane state
// ---------------------------------------------------------------------------

/// Generate test pane data with configurable parameters.
pub struct TestPaneData;

impl TestPaneData {
    /// Generate a vector of pane IDs from 0..count.
    #[must_use]
    pub fn pane_ids(count: usize) -> Vec<u64> {
        (0..count as u64).collect()
    }

    /// Generate mock pane content of the specified line count.
    #[must_use]
    pub fn pane_content(pane_id: u64, lines: usize) -> String {
        (0..lines)
            .map(|i| format!("[pane-{pane_id}] line {i}: output data\n"))
            .collect()
    }

    /// Generate a set of pane contents for multiple panes.
    #[must_use]
    pub fn multi_pane_content(pane_count: usize, lines_per_pane: usize) -> HashMap<u64, String> {
        (0..pane_count as u64)
            .map(|id| (id, Self::pane_content(id, lines_per_pane)))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// RuntimeFixture — pre-built runtime for common test patterns
// ---------------------------------------------------------------------------

/// A pre-built asupersync runtime for integration tests.
pub struct RuntimeFixture {
    runtime: asupersync::runtime::Runtime,
}

impl RuntimeFixture {
    /// Create a single-threaded runtime fixture.
    #[must_use]
    pub fn current_thread() -> Self {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        Self { runtime }
    }

    /// Run an async test within this runtime.
    pub fn block_on<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.runtime.block_on(future)
    }

    /// Get the runtime handle for spawning tasks.
    #[must_use]
    pub fn handle(&self) -> asupersync::runtime::RuntimeHandle {
        self.runtime.handle()
    }
}

// ---------------------------------------------------------------------------
// Cancellation testing helpers
// ---------------------------------------------------------------------------

/// Create a `Cx` that is already cancelled with the given kind.
#[must_use]
pub fn cancelled_cx(kind: CancelKind, message: &'static str) -> Cx {
    let budget = Budget::new().with_poll_quota(0);
    let cx = Cx::for_testing_with_budget(budget);
    cx.cancel_with(kind, Some(message));
    cx
}

/// Create a `Cx` with an exhausted budget (simulates timeout).
#[must_use]
pub fn timeout_cx() -> Cx {
    cancelled_cx(CancelKind::Timeout, "test timeout")
}

/// Create a `Cx` with a user cancellation.
#[must_use]
pub fn user_cancelled_cx() -> Cx {
    cancelled_cx(CancelKind::User, "test user cancel")
}

/// Create a healthy test `Cx` with default budget.
#[must_use]
pub fn healthy_cx() -> Cx {
    Cx::for_testing()
}

// ---------------------------------------------------------------------------
// SimulatedNetwork — fault injection wrapper for MockUnixStream
// ---------------------------------------------------------------------------

/// Configuration for simulated network behavior.
#[derive(Debug, Clone)]
pub struct SimulatedNetworkConfig {
    /// Probability (0.0..1.0) of a read returning an error.
    pub read_error_rate: f64,
    /// Probability (0.0..1.0) of a write returning an error.
    pub write_error_rate: f64,
    /// Probability (0.0..1.0) of silently dropping written data.
    pub drop_rate: f64,
    /// Maximum bytes delivered per read (simulates fragmentation).
    pub max_read_bytes: Option<usize>,
}

impl Default for SimulatedNetworkConfig {
    fn default() -> Self {
        Self {
            read_error_rate: 0.0,
            write_error_rate: 0.0,
            drop_rate: 0.0,
            max_read_bytes: None,
        }
    }
}

impl SimulatedNetworkConfig {
    /// A healthy network with no faults.
    #[must_use]
    pub fn healthy() -> Self {
        Self::default()
    }

    /// A lossy network suitable for chaos testing.
    #[must_use]
    pub fn lossy() -> Self {
        Self {
            read_error_rate: 0.05,
            write_error_rate: 0.05,
            drop_rate: 0.10,
            max_read_bytes: Some(64),
        }
    }

    /// A hostile network with high fault rates.
    #[must_use]
    pub fn hostile() -> Self {
        Self {
            read_error_rate: 0.20,
            write_error_rate: 0.20,
            drop_rate: 0.30,
            max_read_bytes: Some(16),
        }
    }
}

/// Wraps a `MockUnixStream` with deterministic fault injection.
///
/// Uses a simple xorshift64 PRNG seeded at construction for reproducible
/// failure patterns.
pub struct SimulatedNetwork {
    inner: Arc<MockUnixStream>,
    config: SimulatedNetworkConfig,
    rng_state: AtomicU64,
    errors_injected: AtomicU64,
    drops_injected: AtomicU64,
}

impl SimulatedNetwork {
    /// Wrap a mock stream with simulated network behavior.
    #[must_use]
    pub fn new(inner: Arc<MockUnixStream>, config: SimulatedNetworkConfig, seed: u64) -> Self {
        Self {
            inner,
            config,
            rng_state: AtomicU64::new(seed),
            errors_injected: AtomicU64::new(0),
            drops_injected: AtomicU64::new(0),
        }
    }

    /// Write through the simulated network.
    pub async fn write(&self, cx: &Cx, data: &[u8]) -> Result<usize, String> {
        if self.should_inject(self.config.write_error_rate) {
            self.errors_injected.fetch_add(1, Ordering::Relaxed);
            return Err("simulated write error".to_string());
        }
        if self.should_inject(self.config.drop_rate) {
            self.drops_injected.fetch_add(1, Ordering::Relaxed);
            return Ok(data.len()); // Silently drop
        }
        self.inner.write(cx, data).await
    }

    /// Read through the simulated network.
    pub async fn read(&self, cx: &Cx, max_bytes: usize) -> Result<Vec<u8>, String> {
        if self.should_inject(self.config.read_error_rate) {
            self.errors_injected.fetch_add(1, Ordering::Relaxed);
            return Err("simulated read error".to_string());
        }
        let limit = match self.config.max_read_bytes {
            Some(max_frag) => max_bytes.min(max_frag),
            None => max_bytes,
        };
        self.inner.read(cx, limit).await
    }

    /// Total errors injected (read + write).
    #[must_use]
    pub fn errors_injected(&self) -> u64 {
        self.errors_injected.load(Ordering::Relaxed)
    }

    /// Total silent drops.
    #[must_use]
    pub fn drops_injected(&self) -> u64 {
        self.drops_injected.load(Ordering::Relaxed)
    }

    /// Access the underlying stream directly (bypasses fault injection).
    #[must_use]
    pub fn inner(&self) -> &MockUnixStream {
        &self.inner
    }

    /// Xorshift64 PRNG for deterministic fault decisions.
    fn next_random(&self) -> f64 {
        let mut state = self.rng_state.load(Ordering::Relaxed);
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.rng_state.store(state, Ordering::Relaxed);
        (state as f64) / (u64::MAX as f64)
    }

    fn should_inject(&self, rate: f64) -> bool {
        rate > 0.0 && self.next_random() < rate
    }
}
