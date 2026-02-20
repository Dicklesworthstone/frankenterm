//! Operational telemetry pipeline — structured metrics, histograms, resource tracking.
//!
//! Collects structured metrics from all FrankenTerm subsystems with platform-specific
//! resource observation. Provides:
//!
//! - **Resource snapshots**: RSS, virtual memory, FD count, disk I/O per process
//! - **Histograms**: Latency distributions with accurate quantile estimation
//! - **Circular metric buffer**: In-memory ring buffer with configurable retention
//! - **Platform abstraction**: Linux `/proc` and macOS `sysctl`/`vm_stat` behind
//!   a unified [`SystemMetrics`] trait
//! - **Long-term storage**: SQLite-backed hourly aggregates via [`TelemetryStore`]
//!
//! # Performance target
//!
//! Recording a metric point must cost < 100ns on the hot path.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::runtime_compat::sleep;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::{debug, info_span, warn};

// =============================================================================
// Configuration
// =============================================================================

/// Telemetry pipeline configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Interval between resource samples.
    pub sample_interval: Duration,

    /// Maximum number of metric points kept in the circular buffer.
    pub buffer_capacity: usize,

    /// Maximum number of histogram buckets.
    pub histogram_buckets: usize,

    /// Enable per-process resource collection (more expensive).
    pub per_process_metrics: bool,

    /// PID of the mux server process to monitor (0 = self).
    pub mux_server_pid: u32,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_secs(30),
            buffer_capacity: 120, // 1 hour at 30s intervals
            histogram_buckets: 1024,
            per_process_metrics: true,
            mux_server_pid: 0,
        }
    }
}

// =============================================================================
// Resource snapshot
// =============================================================================

/// Point-in-time resource observation for a single process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    /// Process ID.
    pub pid: u32,
    /// Resident set size in bytes.
    pub rss_bytes: u64,
    /// Virtual memory size in bytes.
    pub virt_bytes: u64,
    /// Number of open file descriptors.
    pub fd_count: u64,
    /// Cumulative bytes read (if available).
    pub io_read_bytes: Option<u64>,
    /// Cumulative bytes written (if available).
    pub io_write_bytes: Option<u64>,
    /// CPU usage percentage (0.0–100.0, sampled).
    pub cpu_percent: Option<f64>,
    /// Unix timestamp (seconds since epoch).
    pub timestamp_secs: u64,
}

impl ResourceSnapshot {
    /// Collect a resource snapshot for the given PID.
    ///
    /// Uses platform-specific APIs. Returns `None` if the process cannot be
    /// observed (e.g., it exited or permissions are denied).
    #[must_use]
    pub fn collect(pid: u32) -> Option<Self> {
        let effective_pid = if pid == 0 { std::process::id() } else { pid };

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let mut snap = ResourceSnapshot {
            pid: effective_pid,
            rss_bytes: 0,
            virt_bytes: 0,
            fd_count: 0,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: now,
        };

        collect_process_resources(effective_pid, &mut snap);
        Some(snap)
    }
}

// =============================================================================
// SystemMetrics trait — platform-abstracted resource collection
// =============================================================================

/// Platform-abstracted interface for collecting system and process metrics.
///
/// Implementations collect resource snapshots, system memory info, and CPU
/// counts using platform-specific APIs. The default implementation
/// ([`PlatformMetrics`]) delegates to the existing free functions in this
/// module.
pub trait SystemMetrics: Send + Sync {
    /// Collect a resource snapshot for the given PID (0 = self).
    fn collect_snapshot(&self, pid: u32) -> Option<ResourceSnapshot>;

    /// Return (total_bytes, available_bytes) of system memory.
    fn system_memory(&self) -> (u64, u64);

    /// Number of logical CPUs.
    fn cpu_count(&self) -> usize;
}

/// Default [`SystemMetrics`] implementation backed by platform-specific code.
pub struct PlatformMetrics;

impl SystemMetrics for PlatformMetrics {
    fn collect_snapshot(&self, pid: u32) -> Option<ResourceSnapshot> {
        ResourceSnapshot::collect(pid)
    }

    fn system_memory(&self) -> (u64, u64) {
        collect_system_memory()
    }

    fn cpu_count(&self) -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

/// Collect system-wide memory statistics: (total_bytes, available_bytes).
///
/// Uses `/proc/meminfo` on Linux and `sysctl`/`vm_stat` on macOS.
#[cfg(target_os = "linux")]
fn collect_system_memory() -> (u64, u64) {
    let mut total: u64 = 0;
    let mut available: u64 = 0;

    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(val) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = parse_kb_value(val) {
                    total = kb * 1024;
                }
            } else if let Some(val) = line.strip_prefix("MemAvailable:") {
                if let Some(kb) = parse_kb_value(val) {
                    available = kb * 1024;
                }
            }
        }
    }
    (total, available)
}

/// Collect system-wide memory statistics: (total_bytes, available_bytes).
#[cfg(target_os = "macos")]
fn collect_system_memory() -> (u64, u64) {
    // Total memory via sysctl
    let total = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u64>()
                    .ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    // Free pages via vm_stat
    let available = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let text = String::from_utf8_lossy(&o.stdout);
                let mut free_pages: u64 = 0;
                let page_size: u64 = 16384; // Apple Silicon default
                for line in text.lines() {
                    if line.starts_with("Pages free:") || line.starts_with("Pages inactive:") {
                        if let Some(val) = line.split(':').nth(1) {
                            if let Ok(n) = val.trim().trim_end_matches('.').parse::<u64>() {
                                free_pages += n;
                            }
                        }
                    }
                }
                Some(free_pages * page_size)
            } else {
                None
            }
        })
        .unwrap_or(0);

    (total, available)
}

/// Collect system-wide memory statistics (stub for unsupported platforms).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn collect_system_memory() -> (u64, u64) {
    (0, 0)
}

// =============================================================================
// Metric point
// =============================================================================

/// A single metric measurement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricPoint {
    /// Metric name (e.g., "capture_latency_us", "storage_write_us").
    pub name: String,
    /// Metric value.
    pub value: f64,
    /// Unix timestamp (seconds since epoch).
    pub timestamp_secs: u64,
    /// Optional tags for filtering/grouping.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,
}

impl MetricPoint {
    /// Create a new metric point with the current timestamp.
    #[must_use]
    pub fn new(name: impl Into<String>, value: f64) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        Self {
            name: name.into(),
            value,
            timestamp_secs: now,
            tags: HashMap::new(),
        }
    }

    /// Add a tag to this metric point.
    #[must_use]
    pub fn with_tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.insert(key.into(), value.into());
        self
    }
}

// =============================================================================
// Histogram
// =============================================================================

/// Fixed-capacity histogram for latency distributions.
///
/// Uses sorted insertion for exact quantile computation up to `max_samples`.
/// When capacity is exceeded, oldest samples are discarded (FIFO eviction).
///
/// This provides accurate p50/p95/p99 quantiles at the cost of O(n log n)
/// insertion, which is acceptable for the expected sample rates (<1000/sec per
/// histogram).
#[derive(Debug, Clone)]
pub struct Histogram {
    /// Name of this histogram.
    name: String,
    /// Stored samples in insertion order (for FIFO eviction).
    samples: Vec<f64>,
    /// Maximum number of samples to retain.
    max_samples: usize,
    /// Running count of all recorded values (including evicted).
    total_count: u64,
    /// Running sum of all recorded values.
    total_sum: f64,
    /// Minimum value seen.
    min: f64,
    /// Maximum value seen.
    max: f64,
}

impl Histogram {
    /// Create a new histogram with the given capacity.
    #[must_use]
    pub fn new(name: impl Into<String>, max_samples: usize) -> Self {
        Self {
            name: name.into(),
            samples: Vec::with_capacity(max_samples.min(1024)),
            max_samples: max_samples.max(1),
            total_count: 0,
            total_sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Record a value.
    pub fn record(&mut self, value: f64) {
        self.total_count += 1;
        self.total_sum += value;
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }

        if self.samples.len() >= self.max_samples {
            self.samples.remove(0);
        }
        self.samples.push(value);
    }

    /// Compute a quantile (0.0–1.0) from the retained samples.
    ///
    /// Returns `None` if no samples have been recorded.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }

        let mut sorted: Vec<f64> = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let idx = ((sorted.len() as f64 - 1.0) * q.clamp(0.0, 1.0)) as usize;
        Some(sorted[idx.min(sorted.len() - 1)])
    }

    /// p50 (median).
    #[must_use]
    pub fn p50(&self) -> Option<f64> {
        self.quantile(0.5)
    }

    /// p95.
    #[must_use]
    pub fn p95(&self) -> Option<f64> {
        self.quantile(0.95)
    }

    /// p99.
    #[must_use]
    pub fn p99(&self) -> Option<f64> {
        self.quantile(0.99)
    }

    /// Mean of all recorded values (including evicted).
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        if self.total_count == 0 {
            return None;
        }
        Some(self.total_sum / self.total_count as f64)
    }

    /// Total number of values recorded (including evicted).
    #[must_use]
    pub fn count(&self) -> u64 {
        self.total_count
    }

    /// Number of retained samples in the window.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.samples.len()
    }

    /// Name of this histogram.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Global min/max.
    #[must_use]
    pub fn min_max(&self) -> Option<(f64, f64)> {
        if self.total_count == 0 {
            return None;
        }
        Some((self.min, self.max))
    }

    /// Produce a serializable summary.
    #[must_use]
    pub fn summary(&self) -> HistogramSummary {
        HistogramSummary {
            name: self.name.clone(),
            count: self.total_count,
            retained: self.samples.len() as u64,
            mean: self.mean(),
            min: if self.total_count > 0 {
                Some(self.min)
            } else {
                None
            },
            max: if self.total_count > 0 {
                Some(self.max)
            } else {
                None
            },
            p50: self.p50(),
            p95: self.p95(),
            p99: self.p99(),
        }
    }
}

/// Serializable histogram summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramSummary {
    pub name: String,
    pub count: u64,
    pub retained: u64,
    pub mean: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

// =============================================================================
// Circular metric buffer
// =============================================================================

/// Thread-safe circular buffer for time-series metric storage.
///
/// Stores the most recent `capacity` resource snapshots, evicting the oldest
/// when full.
pub struct CircularMetricBuffer {
    snapshots: RwLock<Vec<ResourceSnapshot>>,
    capacity: usize,
    total_recorded: AtomicU64,
}

impl CircularMetricBuffer {
    /// Create a new buffer with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            snapshots: RwLock::new(Vec::with_capacity(capacity.min(256))),
            capacity: capacity.max(1),
            total_recorded: AtomicU64::new(0),
        }
    }

    /// Push a new snapshot into the buffer.
    pub fn push(&self, snapshot: ResourceSnapshot) {
        let mut buf = self.snapshots.write().expect("buffer lock poisoned");
        if buf.len() >= self.capacity {
            buf.remove(0);
        }
        buf.push(snapshot);
        self.total_recorded.fetch_add(1, Ordering::Relaxed);
    }

    /// Get all retained snapshots (oldest first).
    #[must_use]
    pub fn snapshots(&self) -> Vec<ResourceSnapshot> {
        self.snapshots.read().expect("buffer lock poisoned").clone()
    }

    /// Get the most recent snapshot.
    #[must_use]
    pub fn latest(&self) -> Option<ResourceSnapshot> {
        self.snapshots
            .read()
            .expect("buffer lock poisoned")
            .last()
            .cloned()
    }

    /// Number of retained snapshots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.snapshots.read().expect("buffer lock poisoned").len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total number of snapshots ever recorded (including evicted).
    #[must_use]
    pub fn total_recorded(&self) -> u64 {
        self.total_recorded.load(Ordering::Relaxed)
    }

    /// Current capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl std::fmt::Debug for CircularMetricBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircularMetricBuffer")
            .field("capacity", &self.capacity)
            .field("len", &self.len())
            .field("total_recorded", &self.total_recorded())
            .finish()
    }
}

// =============================================================================
// Metric registry
// =============================================================================

/// Thread-safe registry for named histograms and counters.
///
/// Subsystems register their histograms at startup and record values on the
/// hot path. The registry provides a unified view for snapshots and export.
pub struct MetricRegistry {
    histograms: RwLock<HashMap<String, Histogram>>,
    counters: RwLock<HashMap<String, AtomicU64Wrapper>>,
}

/// Wrapper to allow AtomicU64 inside a HashMap behind RwLock.
struct AtomicU64Wrapper(AtomicU64);

impl std::fmt::Debug for AtomicU64Wrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.load(Ordering::Relaxed))
    }
}

impl MetricRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            histograms: RwLock::new(HashMap::new()),
            counters: RwLock::new(HashMap::new()),
        }
    }

    /// Register a histogram. If already registered, this is a no-op.
    pub fn register_histogram(&self, name: impl Into<String>, max_samples: usize) {
        let name = name.into();
        let mut map = self.histograms.write().expect("histogram lock poisoned");
        map.entry(name.clone())
            .or_insert_with(|| Histogram::new(name, max_samples));
    }

    /// Record a value into a named histogram.
    ///
    /// If the histogram is not registered, the value is silently dropped.
    pub fn record_histogram(&self, name: &str, value: f64) {
        let mut map = self.histograms.write().expect("histogram lock poisoned");
        if let Some(h) = map.get_mut(name) {
            h.record(value);
        }
    }

    /// Increment a named counter by 1.
    pub fn increment_counter(&self, name: &str) {
        self.add_counter(name, 1);
    }

    /// Add a value to a named counter.
    pub fn add_counter(&self, name: &str, delta: u64) {
        let map = self.counters.read().expect("counter lock poisoned");
        if let Some(w) = map.get(name) {
            w.0.fetch_add(delta, Ordering::Relaxed);
            return;
        }
        drop(map);
        let mut map = self.counters.write().expect("counter lock poisoned");
        map.entry(name.to_string())
            .or_insert_with(|| AtomicU64Wrapper(AtomicU64::new(0)))
            .0
            .fetch_add(delta, Ordering::Relaxed);
    }

    /// Read a counter value.
    #[must_use]
    pub fn counter_value(&self, name: &str) -> u64 {
        let map = self.counters.read().expect("counter lock poisoned");
        map.get(name)
            .map(|w| w.0.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Get summaries of all registered histograms.
    #[must_use]
    pub fn histogram_summaries(&self) -> Vec<HistogramSummary> {
        let map = self.histograms.read().expect("histogram lock poisoned");
        map.values().map(|h| h.summary()).collect()
    }

    /// Get all counter values.
    #[must_use]
    pub fn counter_values(&self) -> HashMap<String, u64> {
        let map = self.counters.read().expect("counter lock poisoned");
        map.iter()
            .map(|(k, v)| (k.clone(), v.0.load(Ordering::Relaxed)))
            .collect()
    }

    /// Number of registered histograms.
    #[must_use]
    pub fn histogram_count(&self) -> usize {
        self.histograms
            .read()
            .expect("histogram lock poisoned")
            .len()
    }

    /// Number of registered counters.
    #[must_use]
    pub fn counter_count(&self) -> usize {
        self.counters.read().expect("counter lock poisoned").len()
    }
}

impl Default for MetricRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MetricRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricRegistry")
            .field("histograms", &self.histogram_count())
            .field("counters", &self.counter_count())
            .finish()
    }
}

// =============================================================================
// Telemetry collector
// =============================================================================

/// Serializable telemetry snapshot for the entire pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    /// Unix timestamp when this snapshot was taken.
    pub timestamp_secs: u64,
    /// Most recent resource snapshot.
    pub resource: Option<ResourceSnapshot>,
    /// Histogram summaries.
    pub histograms: Vec<HistogramSummary>,
    /// Counter values.
    pub counters: HashMap<String, u64>,
    /// Number of resource samples in the buffer.
    pub buffer_samples: u64,
    /// Total resource samples ever collected.
    pub total_samples: u64,
}

/// Central telemetry collector.
///
/// Owns a [`CircularMetricBuffer`] for resource snapshots and a
/// [`MetricRegistry`] for histograms and counters. Provides an async `run()`
/// loop for periodic resource sampling and a `snapshot()` method for on-demand
/// state capture.
pub struct TelemetryCollector {
    config: TelemetryConfig,
    buffer: Arc<CircularMetricBuffer>,
    registry: Arc<MetricRegistry>,
    shutdown: Arc<AtomicBool>,
    sample_count: AtomicU64,
}

impl TelemetryCollector {
    /// Create a new telemetry collector.
    #[must_use]
    pub fn new(config: TelemetryConfig) -> Self {
        let buffer = Arc::new(CircularMetricBuffer::new(config.buffer_capacity));
        let registry = Arc::new(MetricRegistry::new());

        Self {
            config,
            buffer,
            registry,
            shutdown: Arc::new(AtomicBool::new(false)),
            sample_count: AtomicU64::new(0),
        }
    }

    /// Get a shared reference to the metric registry.
    #[must_use]
    pub fn registry(&self) -> Arc<MetricRegistry> {
        Arc::clone(&self.registry)
    }

    /// Get a shared reference to the resource buffer.
    #[must_use]
    pub fn buffer(&self) -> Arc<CircularMetricBuffer> {
        Arc::clone(&self.buffer)
    }

    /// Signal the collector to shut down.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Whether shutdown has been signaled.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Take a single resource sample and push it into the buffer.
    pub fn sample_once(&self) {
        let pid = self.config.mux_server_pid;
        if let Some(snap) = ResourceSnapshot::collect(pid) {
            self.buffer.push(snap);
            self.sample_count.fetch_add(1, Ordering::Relaxed);
            debug!(
                pid,
                samples = self.sample_count.load(Ordering::Relaxed),
                "Telemetry sample collected"
            );
        } else {
            warn!(pid, "Failed to collect telemetry sample");
        }
    }

    /// Run the collection loop until shutdown is signaled.
    ///
    /// Samples resource metrics at `config.sample_interval`.
    pub async fn run(&self) {
        let interval = self.config.sample_interval.max(Duration::from_secs(1));
        let mut first_tick = true;

        loop {
            if !first_tick {
                sleep(interval).await;
            }
            first_tick = false;

            if self.shutdown.load(Ordering::SeqCst) {
                debug!("Telemetry collector shutting down");
                break;
            }

            self.sample_once();
        }
    }

    /// Produce a serializable telemetry snapshot.
    #[must_use]
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        TelemetrySnapshot {
            timestamp_secs: now,
            resource: self.buffer.latest(),
            histograms: self.registry.histogram_summaries(),
            counters: self.registry.counter_values(),
            buffer_samples: self.buffer.len() as u64,
            total_samples: self.sample_count.load(Ordering::Relaxed),
        }
    }

    /// Number of samples collected so far.
    #[must_use]
    pub fn sample_count(&self) -> u64 {
        self.sample_count.load(Ordering::Relaxed)
    }

    /// The collector's configuration.
    #[must_use]
    pub fn config(&self) -> &TelemetryConfig {
        &self.config
    }
}

impl std::fmt::Debug for TelemetryCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryCollector")
            .field("config", &self.config)
            .field("sample_count", &self.sample_count())
            .field("buffer", &self.buffer)
            .field("registry", &self.registry)
            .finish()
    }
}

// =============================================================================
// Platform-specific resource collection
// =============================================================================

/// Populate a ResourceSnapshot with platform-specific process metrics.
#[cfg(target_os = "linux")]
fn collect_process_resources(pid: u32, snap: &mut ResourceSnapshot) {
    // RSS and virtual memory from /proc/<pid>/status
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in status.lines() {
            if let Some(val) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = parse_kb_value(val) {
                    snap.rss_bytes = kb * 1024;
                }
            } else if let Some(val) = line.strip_prefix("VmSize:") {
                if let Some(kb) = parse_kb_value(val) {
                    snap.virt_bytes = kb * 1024;
                }
            }
        }
    }

    // FD count from /proc/<pid>/fd/
    if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        snap.fd_count = entries.count() as u64;
    }

    // I/O stats from /proc/<pid>/io
    if let Ok(io_data) = std::fs::read_to_string(format!("/proc/{pid}/io")) {
        for line in io_data.lines() {
            if let Some(val) = line.strip_prefix("read_bytes: ") {
                snap.io_read_bytes = val.trim().parse().ok();
            } else if let Some(val) = line.strip_prefix("write_bytes: ") {
                snap.io_write_bytes = val.trim().parse().ok();
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn collect_process_resources(pid: u32, snap: &mut ResourceSnapshot) {
    // Use ps to get RSS and VSZ for the specific PID
    if let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "rss=,vsz=", "-p", &pid.to_string()])
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let parts: Vec<&str> = text.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(rss_kb) = parts[0].parse::<u64>() {
                    snap.rss_bytes = rss_kb * 1024;
                }
                if let Ok(vsz_kb) = parts[1].parse::<u64>() {
                    snap.virt_bytes = vsz_kb * 1024;
                }
            }
        }
    }

    // FD count from /dev/fd (for self) or lsof for other PIDs
    if pid == std::process::id() {
        if let Ok(entries) = std::fs::read_dir("/dev/fd") {
            snap.fd_count = entries.count() as u64;
        }
    } else {
        // For other PIDs, use lsof -p <pid> | wc -l as approximation
        if let Ok(output) = std::process::Command::new("sh")
            .args(["-c", &format!("lsof -p {pid} 2>/dev/null | wc -l")])
            .output()
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                if let Ok(count) = text.trim().parse::<u64>() {
                    // lsof includes a header line
                    snap.fd_count = count.saturating_sub(1);
                }
            }
        }
    }

    // macOS has no /proc/<pid>/io equivalent; I/O stats stay None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn collect_process_resources(_pid: u32, _snap: &mut ResourceSnapshot) {
    // No platform-specific collection available
}

/// Parse a value like "  12345 kB" → Some(12345).
#[cfg(target_os = "linux")]
fn parse_kb_value(s: &str) -> Option<u64> {
    s.trim().strip_suffix("kB")?.trim().parse().ok()
}

// =============================================================================
// Timing helper
// =============================================================================

/// Lightweight scope timer that records elapsed time into a histogram.
///
/// Usage:
/// ```
/// use frankenterm_core::telemetry::{MetricRegistry, ScopeTimer};
///
/// let registry = MetricRegistry::new();
/// registry.register_histogram("op_latency_us", 1024);
/// {
///     let _timer = ScopeTimer::new(&registry, "op_latency_us");
///     // ... operation ...
/// }
/// // elapsed microseconds recorded automatically on drop
/// ```
pub struct ScopeTimer<'a> {
    registry: &'a MetricRegistry,
    name: &'a str,
    start: Instant,
}

impl<'a> ScopeTimer<'a> {
    /// Start timing.
    #[must_use]
    pub fn new(registry: &'a MetricRegistry, name: &'a str) -> Self {
        Self {
            registry,
            name,
            start: Instant::now(),
        }
    }
}

impl Drop for ScopeTimer<'_> {
    fn drop(&mut self) {
        let elapsed_us = self.start.elapsed().as_nanos() as f64 / 1000.0;
        self.registry.record_histogram(self.name, elapsed_us);
    }
}

// =============================================================================
// Long-term storage: SQLite hourly aggregates
// =============================================================================

/// Hourly aggregate of resource snapshots for long-term storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlyAggregate {
    /// Start of the hour (unix timestamp truncated to hour boundary).
    pub hour_ts: u64,
    /// Number of snapshots aggregated.
    pub sample_count: u32,
    /// Mean RSS bytes across all snapshots in the hour.
    pub mean_rss_bytes: u64,
    /// Peak RSS bytes in the hour.
    pub peak_rss_bytes: u64,
    /// Mean FD count.
    pub mean_fd_count: u64,
    /// Peak FD count.
    pub peak_fd_count: u64,
    /// Mean CPU percent (if available).
    pub mean_cpu_percent: Option<f64>,
}

/// Errors from [`TelemetryStore`] operations.
#[derive(Debug)]
pub enum TelemetryStoreError {
    /// SQLite error.
    Sqlite(rusqlite::Error),
    /// Schema or migration error.
    Schema(String),
}

impl std::fmt::Display for TelemetryStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "SQLite error: {e}"),
            Self::Schema(msg) => write!(f, "Schema error: {msg}"),
        }
    }
}

impl std::error::Error for TelemetryStoreError {}

impl From<rusqlite::Error> for TelemetryStoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// DDL for the telemetry hourly aggregates table.
const TELEMETRY_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS telemetry_hourly (
    hour_ts         INTEGER PRIMARY KEY,
    sample_count    INTEGER NOT NULL,
    mean_rss_bytes  INTEGER NOT NULL,
    peak_rss_bytes  INTEGER NOT NULL,
    mean_fd_count   INTEGER NOT NULL,
    peak_fd_count   INTEGER NOT NULL,
    mean_cpu_pct    REAL
);
";

/// SQLite-backed long-term telemetry storage.
///
/// Stores hourly aggregates with configurable retention. Uses WAL mode for
/// concurrent read/write safety.
pub struct TelemetryStore {
    conn: Connection,
    retention_hours: u64,
}

impl TelemetryStore {
    /// Open or create a telemetry store at the given path.
    pub fn open(db_path: &Path, retention_days: u32) -> Result<Self, TelemetryStoreError> {
        let _span = info_span!("telemetry_store_open", path = %db_path.display()).entered();

        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(TELEMETRY_SCHEMA)?;

        Ok(Self {
            conn,
            retention_hours: u64::from(retention_days) * 24,
        })
    }

    /// Open an in-memory store (for testing).
    pub fn open_in_memory(retention_days: u32) -> Result<Self, TelemetryStoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(TELEMETRY_SCHEMA)?;

        Ok(Self {
            conn,
            retention_hours: u64::from(retention_days) * 24,
        })
    }

    /// Persist a single hourly aggregate. Upserts on `hour_ts`.
    pub fn persist_aggregate(&self, agg: &HourlyAggregate) -> Result<(), TelemetryStoreError> {
        let _span = info_span!("telemetry_persist", hour_ts = agg.hour_ts).entered();

        self.conn.execute(
            "INSERT OR REPLACE INTO telemetry_hourly \
             (hour_ts, sample_count, mean_rss_bytes, peak_rss_bytes, \
              mean_fd_count, peak_fd_count, mean_cpu_pct) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                agg.hour_ts as i64,
                agg.sample_count,
                agg.mean_rss_bytes as i64,
                agg.peak_rss_bytes as i64,
                agg.mean_fd_count as i64,
                agg.peak_fd_count as i64,
                agg.mean_cpu_percent,
            ],
        )?;
        Ok(())
    }

    /// Compute an hourly aggregate from a slice of snapshots.
    ///
    /// Returns `None` if the slice is empty.
    #[must_use]
    pub fn aggregate_snapshots(
        hour_ts: u64,
        snapshots: &[ResourceSnapshot],
    ) -> Option<HourlyAggregate> {
        if snapshots.is_empty() {
            return None;
        }

        let n = snapshots.len() as u64;
        let sum_rss: u64 = snapshots.iter().map(|s| s.rss_bytes).sum();
        let peak_rss = snapshots.iter().map(|s| s.rss_bytes).max().unwrap_or(0);
        let sum_fd: u64 = snapshots.iter().map(|s| s.fd_count).sum();
        let peak_fd = snapshots.iter().map(|s| s.fd_count).max().unwrap_or(0);

        let cpu_values: Vec<f64> = snapshots.iter().filter_map(|s| s.cpu_percent).collect();
        let mean_cpu = if cpu_values.is_empty() {
            None
        } else {
            Some(cpu_values.iter().sum::<f64>() / cpu_values.len() as f64)
        };

        Some(HourlyAggregate {
            hour_ts,
            sample_count: snapshots.len() as u32,
            mean_rss_bytes: sum_rss / n,
            peak_rss_bytes: peak_rss,
            mean_fd_count: sum_fd / n,
            peak_fd_count: peak_fd,
            mean_cpu_percent: mean_cpu,
        })
    }

    /// Flush the current circular buffer contents as an aggregate for the
    /// current hour. Returns the number of snapshots aggregated.
    pub fn flush_buffer(
        &self,
        buffer: &CircularMetricBuffer,
    ) -> Result<usize, TelemetryStoreError> {
        let _span = info_span!("telemetry_flush_buffer").entered();

        let snapshots = buffer.snapshots();
        if snapshots.is_empty() {
            return Ok(0);
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let hour_ts = now - (now % 3600);
        let count = snapshots.len();

        if let Some(agg) = Self::aggregate_snapshots(hour_ts, &snapshots) {
            self.persist_aggregate(&agg)?;
        }

        // Prune old data while we're at it
        self.prune_old_data(now)?;

        Ok(count)
    }

    /// Query hourly aggregates within a time range.
    pub fn query_history(
        &self,
        from_ts: u64,
        to_ts: u64,
    ) -> Result<Vec<HourlyAggregate>, TelemetryStoreError> {
        let _span = info_span!("telemetry_query", from_ts, to_ts).entered();

        let mut stmt = self.conn.prepare(
            "SELECT hour_ts, sample_count, mean_rss_bytes, peak_rss_bytes, \
                    mean_fd_count, peak_fd_count, mean_cpu_pct \
             FROM telemetry_hourly \
             WHERE hour_ts >= ?1 AND hour_ts <= ?2 \
             ORDER BY hour_ts",
        )?;

        let rows = stmt.query_map(rusqlite::params![from_ts as i64, to_ts as i64], |row| {
            Ok(HourlyAggregate {
                hour_ts: row.get::<_, i64>(0)? as u64,
                sample_count: row.get(1)?,
                mean_rss_bytes: row.get::<_, i64>(2)? as u64,
                peak_rss_bytes: row.get::<_, i64>(3)? as u64,
                mean_fd_count: row.get::<_, i64>(4)? as u64,
                peak_fd_count: row.get::<_, i64>(5)? as u64,
                mean_cpu_percent: row.get(6)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Delete aggregates older than the configured retention period.
    fn prune_old_data(&self, now_secs: u64) -> Result<(), TelemetryStoreError> {
        let cutoff = now_secs.saturating_sub(self.retention_hours * 3600);
        self.conn.execute(
            "DELETE FROM telemetry_hourly WHERE hour_ts < ?1",
            rusqlite::params![cutoff as i64],
        )?;
        Ok(())
    }

    /// Number of hourly aggregates stored.
    pub fn aggregate_count(&self) -> Result<u64, TelemetryStoreError> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM telemetry_hourly", [], |row| {
                    row.get(0)
                })?;
        Ok(count as u64)
    }
}

impl std::fmt::Debug for TelemetryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryStore")
            .field("retention_hours", &self.retention_hours)
            .finish_non_exhaustive()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a minimal ResourceSnapshot for tests.
    fn make_snap(pid: u32, rss: u64, fd: u64, ts: u64) -> ResourceSnapshot {
        ResourceSnapshot {
            pid,
            rss_bytes: rss,
            virt_bytes: rss * 2,
            fd_count: fd,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: ts,
        }
    }

    // -- Config ---------------------------------------------------------------

    #[test]
    fn config_defaults() {
        let config = TelemetryConfig::default();
        assert_eq!(config.sample_interval, Duration::from_secs(30));
        assert_eq!(config.buffer_capacity, 120);
        assert_eq!(config.histogram_buckets, 1024);
        assert!(config.per_process_metrics);
        assert_eq!(config.mux_server_pid, 0);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = TelemetryConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: TelemetryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.buffer_capacity, config.buffer_capacity);
    }

    #[test]
    fn config_clone_and_debug() {
        let config = TelemetryConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.buffer_capacity, 120);
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("TelemetryConfig"));
        assert!(dbg.contains("120"));
    }

    #[test]
    fn config_custom_values_serde() {
        let config = TelemetryConfig {
            sample_interval: Duration::from_millis(500),
            buffer_capacity: 60,
            histogram_buckets: 512,
            per_process_metrics: false,
            mux_server_pid: 42,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: TelemetryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sample_interval, Duration::from_millis(500));
        assert_eq!(back.buffer_capacity, 60);
        assert_eq!(back.histogram_buckets, 512);
        assert!(!back.per_process_metrics);
        assert_eq!(back.mux_server_pid, 42);
    }

    #[test]
    fn config_serde_default_from_empty_json() {
        // Thanks to #[serde(default)], empty object should produce default config
        let back: TelemetryConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(back.buffer_capacity, 120);
        assert_eq!(back.mux_server_pid, 0);
    }

    // -- ResourceSnapshot -----------------------------------------------------

    #[test]
    fn resource_snapshot_collect_self() {
        let snap = ResourceSnapshot::collect(0).expect("should collect self");
        assert_eq!(snap.pid, std::process::id());
        assert!(snap.timestamp_secs > 0);
        // On supported platforms, RSS should be non-zero for the current process
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(snap.rss_bytes > 0, "RSS should be non-zero for self");
    }

    #[test]
    fn resource_snapshot_serde_roundtrip() {
        let snap = ResourceSnapshot {
            pid: 1234,
            rss_bytes: 1024 * 1024,
            virt_bytes: 4 * 1024 * 1024,
            fd_count: 42,
            io_read_bytes: Some(5000),
            io_write_bytes: Some(3000),
            cpu_percent: Some(12.5),
            timestamp_secs: 1700000000,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ResourceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 1234);
        assert_eq!(back.rss_bytes, 1024 * 1024);
        assert_eq!(back.fd_count, 42);
        assert_eq!(back.io_read_bytes, Some(5000));
    }

    #[test]
    fn resource_snapshot_zero_values() {
        let snap = ResourceSnapshot {
            pid: 0,
            rss_bytes: 0,
            virt_bytes: 0,
            fd_count: 0,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ResourceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 0);
        assert_eq!(back.rss_bytes, 0);
        assert!(back.io_read_bytes.is_none());
        assert!(back.cpu_percent.is_none());
    }

    #[test]
    fn resource_snapshot_u64_max_values() {
        let snap = ResourceSnapshot {
            pid: u32::MAX,
            rss_bytes: u64::MAX,
            virt_bytes: u64::MAX,
            fd_count: u64::MAX,
            io_read_bytes: Some(u64::MAX),
            io_write_bytes: Some(u64::MAX),
            cpu_percent: Some(100.0),
            timestamp_secs: u64::MAX,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ResourceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, u32::MAX);
        assert_eq!(back.rss_bytes, u64::MAX);
        assert_eq!(back.io_read_bytes, Some(u64::MAX));
    }

    #[test]
    fn resource_snapshot_clone_and_debug() {
        let snap = make_snap(42, 1024, 10, 1000);
        let cloned = snap.clone();
        assert_eq!(cloned.pid, 42);
        assert_eq!(cloned.rss_bytes, 1024);
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("ResourceSnapshot"));
        assert!(dbg.contains("1024"));
    }

    #[test]
    fn resource_snapshot_serde_with_none_optionals() {
        let snap = ResourceSnapshot {
            pid: 1,
            rss_bytes: 100,
            virt_bytes: 200,
            fd_count: 5,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: 999,
        };
        let json = serde_json::to_string(&snap).unwrap();
        // None fields should serialize as null
        assert!(json.contains("null"));
        let back: ResourceSnapshot = serde_json::from_str(&json).unwrap();
        assert!(back.io_read_bytes.is_none());
        assert!(back.io_write_bytes.is_none());
        assert!(back.cpu_percent.is_none());
    }

    #[test]
    fn resource_snapshot_mixed_optionals() {
        let snap = ResourceSnapshot {
            pid: 10,
            rss_bytes: 4096,
            virt_bytes: 8192,
            fd_count: 20,
            io_read_bytes: Some(1000),
            io_write_bytes: None,
            cpu_percent: Some(50.5),
            timestamp_secs: 1700000000,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ResourceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.io_read_bytes, Some(1000));
        assert!(back.io_write_bytes.is_none());
        assert!((back.cpu_percent.unwrap() - 50.5).abs() < f64::EPSILON);
    }

    // -- MetricPoint ----------------------------------------------------------

    #[test]
    fn metric_point_new() {
        let mp = MetricPoint::new("capture_latency_us", 42.5);
        assert_eq!(mp.name, "capture_latency_us");
        assert!((mp.value - 42.5).abs() < f64::EPSILON);
        assert!(mp.timestamp_secs > 0);
        assert!(mp.tags.is_empty());
    }

    #[test]
    fn metric_point_with_tags() {
        let mp = MetricPoint::new("latency", 10.0)
            .with_tag("pane", "42")
            .with_tag("op", "write");
        assert_eq!(mp.tags.len(), 2);
        assert_eq!(mp.tags["pane"], "42");
        assert_eq!(mp.tags["op"], "write");
    }

    #[test]
    fn metric_point_serde_roundtrip() {
        let mp = MetricPoint::new("test", 99.9).with_tag("env", "prod");
        let json = serde_json::to_string(&mp).unwrap();
        let back: MetricPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test");
        assert_eq!(back.tags["env"], "prod");
    }

    #[test]
    fn metric_point_empty_tags_skip_in_serde() {
        let mp = MetricPoint::new("no_tags", 1.0);
        let json = serde_json::to_string(&mp).unwrap();
        // skip_serializing_if = "HashMap::is_empty" means no "tags" key
        assert!(!json.contains("\"tags\""));
    }

    #[test]
    fn metric_point_tag_overwrite() {
        let mp = MetricPoint::new("m", 1.0)
            .with_tag("key", "old")
            .with_tag("key", "new");
        assert_eq!(mp.tags.len(), 1);
        assert_eq!(mp.tags["key"], "new");
    }

    #[test]
    fn metric_point_zero_and_negative_values() {
        let mp_zero = MetricPoint::new("z", 0.0);
        assert!((mp_zero.value - 0.0).abs() < f64::EPSILON);

        let mp_neg = MetricPoint::new("n", -100.5);
        assert!((mp_neg.value - (-100.5)).abs() < f64::EPSILON);
    }

    #[test]
    fn metric_point_clone_and_debug() {
        let mp = MetricPoint::new("test_metric", 3.25);
        let cloned = mp.clone();
        assert_eq!(cloned.name, "test_metric");
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("MetricPoint"));
        assert!(dbg.contains("test_metric"));
    }

    #[test]
    fn metric_point_empty_name() {
        let mp = MetricPoint::new("", 0.0);
        assert_eq!(mp.name, "");
        let json = serde_json::to_string(&mp).unwrap();
        let back: MetricPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "");
    }

    // -- Histogram ------------------------------------------------------------

    #[test]
    fn histogram_empty() {
        let h = Histogram::new("test", 100);
        assert_eq!(h.count(), 0);
        assert_eq!(h.retained(), 0);
        assert!(h.p50().is_none());
        assert!(h.mean().is_none());
        assert!(h.min_max().is_none());
    }

    #[test]
    fn histogram_single_value() {
        let mut h = Histogram::new("test", 100);
        h.record(42.0);
        assert_eq!(h.count(), 1);
        assert_eq!(h.retained(), 1);
        assert!((h.p50().unwrap() - 42.0).abs() < f64::EPSILON);
        assert!((h.p95().unwrap() - 42.0).abs() < f64::EPSILON);
        assert!((h.mean().unwrap() - 42.0).abs() < f64::EPSILON);
        assert_eq!(h.min_max(), Some((42.0, 42.0)));
    }

    #[test]
    fn histogram_quantiles() {
        let mut h = Histogram::new("test", 1000);
        // Record values 1..=100
        for i in 1..=100 {
            h.record(i as f64);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.retained(), 100);

        let p50 = h.p50().unwrap();
        assert!((p50 - 50.0).abs() <= 1.0, "p50={p50}, expected ~50");

        let p95 = h.p95().unwrap();
        assert!((p95 - 95.0).abs() <= 1.0, "p95={p95}, expected ~95");

        let p99 = h.p99().unwrap();
        assert!((p99 - 99.0).abs() <= 1.0, "p99={p99}, expected ~99");

        let mean = h.mean().unwrap();
        assert!((mean - 50.5).abs() < 0.1, "mean={mean}, expected ~50.5");

        assert_eq!(h.min_max(), Some((1.0, 100.0)));
    }

    #[test]
    fn histogram_eviction() {
        let mut h = Histogram::new("test", 10);
        // Record 20 values
        for i in 0..20 {
            h.record(i as f64);
        }
        assert_eq!(h.count(), 20);
        assert_eq!(h.retained(), 10);
        // min/max track all-time
        assert_eq!(h.min_max(), Some((0.0, 19.0)));
        // retained samples are 10..19 (FIFO eviction)
        let p50 = h.p50().unwrap();
        assert!((10.0..=19.0).contains(&p50), "p50={p50}");
    }

    #[test]
    fn histogram_summary() {
        let mut h = Histogram::new("latency", 100);
        for i in 1..=50 {
            h.record(i as f64);
        }
        let s = h.summary();
        assert_eq!(s.name, "latency");
        assert_eq!(s.count, 50);
        assert_eq!(s.retained, 50);
        assert!(s.mean.is_some());
        assert!(s.p50.is_some());
        assert!(s.p95.is_some());
        assert!(s.p99.is_some());
    }

    #[test]
    fn histogram_summary_serde() {
        let mut h = Histogram::new("test", 100);
        h.record(1.0);
        h.record(2.0);
        let s = h.summary();
        let json = serde_json::to_string(&s).unwrap();
        let back: HistogramSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.count, 2);
    }

    #[test]
    fn histogram_max_samples_zero_clamped_to_one() {
        let mut h = Histogram::new("clamped", 0);
        // max_samples is clamped to 1
        h.record(10.0);
        h.record(20.0);
        assert_eq!(h.retained(), 1);
        assert_eq!(h.count(), 2);
        // Only the last value is retained
        assert!((h.p50().unwrap() - 20.0).abs() < f64::EPSILON);
        // But min/max track all-time
        assert_eq!(h.min_max(), Some((10.0, 20.0)));
    }

    #[test]
    fn histogram_name_accessor() {
        let h = Histogram::new("my_histogram", 100);
        assert_eq!(h.name(), "my_histogram");
    }

    #[test]
    fn histogram_quantile_boundary_values() {
        let mut h = Histogram::new("boundary", 100);
        for i in 1..=10 {
            h.record(i as f64);
        }
        // q=0.0 should return the minimum retained sample
        let q0 = h.quantile(0.0).unwrap();
        assert!(
            (q0 - 1.0).abs() < f64::EPSILON,
            "q(0.0) = {}, expected 1.0",
            q0
        );

        // q=1.0 should return the maximum retained sample
        let q1 = h.quantile(1.0).unwrap();
        assert!(
            (q1 - 10.0).abs() < f64::EPSILON,
            "q(1.0) = {}, expected 10.0",
            q1
        );
    }

    #[test]
    fn histogram_quantile_clamping() {
        let mut h = Histogram::new("clamp", 100);
        h.record(5.0);
        h.record(10.0);
        // Negative quantile clamped to 0.0
        let q_neg = h.quantile(-1.0).unwrap();
        assert!(
            (q_neg - 5.0).abs() < f64::EPSILON,
            "q(-1.0) should clamp to min"
        );

        // Quantile > 1.0 clamped to 1.0
        let q_big = h.quantile(2.0).unwrap();
        assert!(
            (q_big - 10.0).abs() < f64::EPSILON,
            "q(2.0) should clamp to max"
        );
    }

    #[test]
    fn histogram_two_identical_values() {
        let mut h = Histogram::new("dup", 100);
        h.record(7.0);
        h.record(7.0);
        assert_eq!(h.count(), 2);
        assert!((h.p50().unwrap() - 7.0).abs() < f64::EPSILON);
        assert!((h.mean().unwrap() - 7.0).abs() < f64::EPSILON);
        assert_eq!(h.min_max(), Some((7.0, 7.0)));
    }

    #[test]
    fn histogram_descending_order_values() {
        let mut h = Histogram::new("desc", 100);
        for i in (1..=10).rev() {
            h.record(i as f64);
        }
        // Quantiles should still work correctly despite insertion order
        let p50 = h.p50().unwrap();
        assert!((p50 - 5.0).abs() <= 1.0, "p50 = {}, expected ~5", p50);
        assert_eq!(h.min_max(), Some((1.0, 10.0)));
    }

    #[test]
    fn histogram_summary_empty() {
        let h = Histogram::new("empty_summary", 100);
        let s = h.summary();
        assert_eq!(s.name, "empty_summary");
        assert_eq!(s.count, 0);
        assert_eq!(s.retained, 0);
        assert!(s.mean.is_none());
        assert!(s.min.is_none());
        assert!(s.max.is_none());
        assert!(s.p50.is_none());
        assert!(s.p95.is_none());
        assert!(s.p99.is_none());
    }

    #[test]
    fn histogram_clone() {
        let mut h = Histogram::new("orig", 100);
        h.record(1.0);
        h.record(2.0);
        h.record(3.0);
        let cloned = h.clone();
        assert_eq!(cloned.count(), 3);
        assert_eq!(cloned.name(), "orig");
        assert!((cloned.mean().unwrap() - 2.0).abs() < f64::EPSILON);
        // Mutating original should not affect clone
        h.record(100.0);
        assert_eq!(cloned.count(), 3);
        assert_eq!(h.count(), 4);
    }

    #[test]
    fn histogram_debug() {
        let h = Histogram::new("dbg_hist", 50);
        let dbg = format!("{:?}", h);
        assert!(dbg.contains("Histogram"));
        assert!(dbg.contains("dbg_hist"));
    }

    #[test]
    fn histogram_large_dataset() {
        let mut h = Histogram::new("large", 500);
        for i in 0..10_000 {
            h.record(i as f64);
        }
        assert_eq!(h.count(), 10_000);
        assert_eq!(h.retained(), 500);
        assert_eq!(h.min_max(), Some((0.0, 9999.0)));
        let mean = h.mean().unwrap();
        // Mean of 0..9999 = 4999.5
        assert!(
            (mean - 4999.5).abs() < 0.1,
            "mean = {}, expected ~4999.5",
            mean
        );
    }

    #[test]
    fn histogram_negative_values() {
        let mut h = Histogram::new("neg", 100);
        h.record(-10.0);
        h.record(-5.0);
        h.record(0.0);
        h.record(5.0);
        assert_eq!(h.min_max(), Some((-10.0, 5.0)));
        let mean = h.mean().unwrap();
        assert!(
            (mean - (-2.5)).abs() < f64::EPSILON,
            "mean = {}, expected -2.5",
            mean
        );
    }

    #[test]
    fn histogram_mean_includes_evicted() {
        let mut h = Histogram::new("mean_evict", 2);
        h.record(10.0);
        h.record(20.0);
        h.record(30.0); // evicts 10.0
        assert_eq!(h.retained(), 2);
        assert_eq!(h.count(), 3);
        // Mean should be (10+20+30)/3 = 20.0, not (20+30)/2 = 25.0
        assert!((h.mean().unwrap() - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn histogram_summary_serde_roundtrip_full() {
        let mut h = Histogram::new("full_rt", 100);
        for i in 1..=20 {
            h.record(i as f64);
        }
        let summary = h.summary();
        let json = serde_json::to_string(&summary).unwrap();
        let back: HistogramSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "full_rt");
        assert_eq!(back.count, 20);
        assert_eq!(back.retained, 20);
        assert!(back.mean.is_some());
        assert!(back.min.is_some());
        assert!(back.max.is_some());
        assert!((back.min.unwrap() - 1.0).abs() < f64::EPSILON);
        assert!((back.max.unwrap() - 20.0).abs() < f64::EPSILON);
    }

    // -- CircularMetricBuffer -------------------------------------------------

    #[test]
    fn buffer_empty() {
        let buf = CircularMetricBuffer::new(10);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.total_recorded(), 0);
        assert!(buf.latest().is_none());
    }

    #[test]
    fn buffer_push_and_read() {
        let buf = CircularMetricBuffer::new(10);
        let snap = ResourceSnapshot {
            pid: 1,
            rss_bytes: 100,
            virt_bytes: 200,
            fd_count: 5,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: 1000,
        };
        buf.push(snap.clone());
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.total_recorded(), 1);
        let latest = buf.latest().unwrap();
        assert_eq!(latest.pid, 1);
        assert_eq!(latest.rss_bytes, 100);
    }

    #[test]
    fn buffer_eviction() {
        let buf = CircularMetricBuffer::new(3);
        for i in 0..5 {
            buf.push(ResourceSnapshot {
                pid: i,
                rss_bytes: 0,
                virt_bytes: 0,
                fd_count: 0,
                io_read_bytes: None,
                io_write_bytes: None,
                cpu_percent: None,
                timestamp_secs: i as u64,
            });
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.total_recorded(), 5);
        let snaps = buf.snapshots();
        // Should retain the 3 most recent: pid 2, 3, 4
        assert_eq!(snaps[0].pid, 2);
        assert_eq!(snaps[1].pid, 3);
        assert_eq!(snaps[2].pid, 4);
    }

    #[test]
    fn buffer_capacity() {
        let buf = CircularMetricBuffer::new(50);
        assert_eq!(buf.capacity(), 50);
    }

    #[test]
    fn buffer_capacity_zero_clamped_to_one() {
        let buf = CircularMetricBuffer::new(0);
        assert_eq!(buf.capacity(), 1);
        buf.push(make_snap(1, 100, 5, 1000));
        buf.push(make_snap(2, 200, 10, 2000));
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.total_recorded(), 2);
        assert_eq!(buf.latest().unwrap().pid, 2);
    }

    #[test]
    fn buffer_capacity_one_single_element() {
        let buf = CircularMetricBuffer::new(1);
        assert_eq!(buf.capacity(), 1);
        buf.push(make_snap(10, 1024, 5, 100));
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.latest().unwrap().pid, 10);

        buf.push(make_snap(20, 2048, 10, 200));
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.total_recorded(), 2);
        assert_eq!(buf.latest().unwrap().pid, 20);
        // Snapshots should only contain the latest
        let snaps = buf.snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].pid, 20);
    }

    #[test]
    fn buffer_debug_impl() {
        let buf = CircularMetricBuffer::new(10);
        buf.push(make_snap(1, 100, 5, 1000));
        let dbg = format!("{:?}", buf);
        assert!(dbg.contains("CircularMetricBuffer"));
        assert!(dbg.contains("capacity"));
        assert!(dbg.contains("10"));
        assert!(dbg.contains("len"));
    }

    #[test]
    fn buffer_is_empty_after_push() {
        let buf = CircularMetricBuffer::new(5);
        assert!(buf.is_empty());
        buf.push(make_snap(1, 100, 5, 1000));
        assert!(!buf.is_empty());
    }

    #[test]
    fn buffer_snapshots_ordering() {
        let buf = CircularMetricBuffer::new(5);
        for i in 0..5 {
            buf.push(make_snap(i, i as u64 * 100, i as u64, i as u64 * 1000));
        }
        let snaps = buf.snapshots();
        for (i, snap) in snaps.iter().enumerate().take(5) {
            assert_eq!(
                snap.pid, i as u32,
                "snapshot ordering mismatch at index {}",
                i
            );
        }
    }

    #[test]
    fn buffer_wrap_around_preserves_order() {
        let buf = CircularMetricBuffer::new(3);
        // Push 7 elements, capacity 3
        for i in 0..7u32 {
            buf.push(make_snap(i, 0, 0, i as u64));
        }
        let snaps = buf.snapshots();
        assert_eq!(snaps.len(), 3);
        // Should be 4, 5, 6
        assert_eq!(snaps[0].pid, 4);
        assert_eq!(snaps[1].pid, 5);
        assert_eq!(snaps[2].pid, 6);
        assert_eq!(buf.total_recorded(), 7);
    }

    // -- MetricRegistry -------------------------------------------------------

    #[test]
    fn registry_histogram_lifecycle() {
        let reg = MetricRegistry::new();
        reg.register_histogram("latency", 100);
        reg.record_histogram("latency", 10.0);
        reg.record_histogram("latency", 20.0);
        reg.record_histogram("latency", 30.0);

        let summaries = reg.histogram_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].count, 3);
    }

    #[test]
    fn registry_counter_lifecycle() {
        let reg = MetricRegistry::new();
        reg.increment_counter("captures");
        reg.increment_counter("captures");
        reg.add_counter("captures", 5);

        assert_eq!(reg.counter_value("captures"), 7);
    }

    #[test]
    fn registry_unregistered_histogram_noop() {
        let reg = MetricRegistry::new();
        // Recording to an unregistered histogram should not panic
        reg.record_histogram("nonexistent", 42.0);
        assert_eq!(reg.histogram_count(), 0);
    }

    #[test]
    fn registry_counter_auto_creates() {
        let reg = MetricRegistry::new();
        // Counter is auto-created on first increment
        reg.increment_counter("new_counter");
        assert_eq!(reg.counter_value("new_counter"), 1);
        assert_eq!(reg.counter_count(), 1);
    }

    #[test]
    fn registry_counter_values_snapshot() {
        let reg = MetricRegistry::new();
        reg.add_counter("a", 10);
        reg.add_counter("b", 20);
        let vals = reg.counter_values();
        assert_eq!(vals["a"], 10);
        assert_eq!(vals["b"], 20);
    }

    #[test]
    fn registry_duplicate_register_noop() {
        let reg = MetricRegistry::new();
        reg.register_histogram("h", 100);
        reg.record_histogram("h", 5.0);
        reg.register_histogram("h", 200); // should not reset
        let summaries = reg.histogram_summaries();
        assert_eq!(summaries[0].count, 1); // data preserved
    }

    #[test]
    fn registry_default_impl() {
        let reg = MetricRegistry::default();
        assert_eq!(reg.histogram_count(), 0);
        assert_eq!(reg.counter_count(), 0);
    }

    #[test]
    fn registry_debug_impl() {
        let reg = MetricRegistry::new();
        reg.register_histogram("h1", 100);
        reg.add_counter("c1", 1);
        let dbg = format!("{:?}", reg);
        assert!(dbg.contains("MetricRegistry"));
        assert!(dbg.contains("histograms"));
        assert!(dbg.contains("counters"));
    }

    #[test]
    fn registry_multiple_histograms() {
        let reg = MetricRegistry::new();
        reg.register_histogram("latency", 100);
        reg.register_histogram("throughput", 200);
        reg.register_histogram("errors", 50);
        assert_eq!(reg.histogram_count(), 3);

        reg.record_histogram("latency", 10.0);
        reg.record_histogram("throughput", 100.0);
        reg.record_histogram("throughput", 200.0);

        let summaries = reg.histogram_summaries();
        assert_eq!(summaries.len(), 3);
        // Find each by name
        let lat = summaries.iter().find(|s| s.name == "latency").unwrap();
        assert_eq!(lat.count, 1);
        let thr = summaries.iter().find(|s| s.name == "throughput").unwrap();
        assert_eq!(thr.count, 2);
        let err = summaries.iter().find(|s| s.name == "errors").unwrap();
        assert_eq!(err.count, 0);
    }

    #[test]
    fn registry_counter_nonexistent_returns_zero() {
        let reg = MetricRegistry::new();
        assert_eq!(reg.counter_value("does_not_exist"), 0);
    }

    #[test]
    fn registry_add_counter_delta_zero() {
        let reg = MetricRegistry::new();
        reg.add_counter("zero_delta", 0);
        // Counter is auto-created but value stays 0
        assert_eq!(reg.counter_value("zero_delta"), 0);
        assert_eq!(reg.counter_count(), 1);
    }

    #[test]
    fn registry_multiple_counters() {
        let reg = MetricRegistry::new();
        reg.add_counter("reads", 100);
        reg.add_counter("writes", 50);
        reg.add_counter("errors", 3);
        reg.increment_counter("reads");

        assert_eq!(reg.counter_count(), 3);
        assert_eq!(reg.counter_value("reads"), 101);
        assert_eq!(reg.counter_value("writes"), 50);
        assert_eq!(reg.counter_value("errors"), 3);
    }

    // -- ScopeTimer -----------------------------------------------------------

    #[test]
    fn scope_timer_records() {
        let reg = MetricRegistry::new();
        reg.register_histogram("op_us", 100);
        {
            let _timer = ScopeTimer::new(&reg, "op_us");
            std::thread::sleep(Duration::from_millis(1));
        }
        let summaries = reg.histogram_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].count, 1);
        // Should have recorded some positive microsecond value
        assert!(summaries[0].p50.unwrap() > 0.0);
    }

    #[test]
    fn scope_timer_unregistered_histogram_noop() {
        let reg = MetricRegistry::new();
        // Timer on unregistered histogram should not panic on drop
        {
            let _timer = ScopeTimer::new(&reg, "unregistered");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(reg.histogram_count(), 0);
    }

    #[test]
    fn scope_timer_multiple_recordings() {
        let reg = MetricRegistry::new();
        reg.register_histogram("multi_timer", 100);
        for _ in 0..5 {
            let _timer = ScopeTimer::new(&reg, "multi_timer");
            // Instant drop
        }
        let summaries = reg.histogram_summaries();
        let h = summaries.iter().find(|s| s.name == "multi_timer").unwrap();
        assert_eq!(h.count, 5);
    }

    // -- TelemetryCollector ---------------------------------------------------

    #[test]
    fn collector_creation() {
        let collector = TelemetryCollector::new(TelemetryConfig::default());
        assert_eq!(collector.sample_count(), 0);
        assert!(!collector.is_shutdown());
    }

    #[test]
    fn collector_sample_once() {
        let collector = TelemetryCollector::new(TelemetryConfig {
            mux_server_pid: 0, // self
            ..Default::default()
        });
        collector.sample_once();
        assert_eq!(collector.sample_count(), 1);
        assert_eq!(collector.buffer().len(), 1);

        let snap = collector.buffer().latest().unwrap();
        assert_eq!(snap.pid, std::process::id());
    }

    #[test]
    fn collector_snapshot() {
        let collector = TelemetryCollector::new(TelemetryConfig::default());
        collector.sample_once();

        let registry = collector.registry();
        registry.register_histogram("test_h", 100);
        registry.record_histogram("test_h", 42.0);
        registry.increment_counter("test_c");

        let snap = collector.snapshot();
        assert!(snap.timestamp_secs > 0);
        assert!(snap.resource.is_some());
        assert_eq!(snap.buffer_samples, 1);
        assert_eq!(snap.total_samples, 1);
        assert_eq!(snap.histograms.len(), 1);
        assert_eq!(snap.counters["test_c"], 1);
    }

    #[test]
    fn collector_shutdown() {
        let collector = TelemetryCollector::new(TelemetryConfig::default());
        assert!(!collector.is_shutdown());
        collector.shutdown();
        assert!(collector.is_shutdown());
    }

    #[tokio::test]
    async fn collector_run_and_shutdown() {
        let collector = Arc::new(TelemetryCollector::new(TelemetryConfig {
            sample_interval: Duration::from_millis(50),
            mux_server_pid: 0,
            ..Default::default()
        }));

        let c = Arc::clone(&collector);
        let handle = crate::runtime_compat::task::spawn(async move {
            c.run().await;
        });

        // Let it collect a few samples (macOS subprocess sampling is slow)
        crate::runtime_compat::sleep(Duration::from_millis(500)).await;
        collector.shutdown();
        handle.await.unwrap();

        // Should have collected at least 1 sample (first tick is immediate)
        assert!(
            collector.sample_count() >= 1,
            "sample_count={}, expected >= 1",
            collector.sample_count()
        );
    }

    #[test]
    fn telemetry_snapshot_serde() {
        let snap = TelemetrySnapshot {
            timestamp_secs: 1700000000,
            resource: Some(ResourceSnapshot {
                pid: 1,
                rss_bytes: 1024,
                virt_bytes: 2048,
                fd_count: 10,
                io_read_bytes: None,
                io_write_bytes: None,
                cpu_percent: None,
                timestamp_secs: 1700000000,
            }),
            histograms: vec![],
            counters: HashMap::new(),
            buffer_samples: 5,
            total_samples: 10,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_samples, 10);
        assert_eq!(back.resource.unwrap().pid, 1);
    }

    #[test]
    fn collector_config_accessor() {
        let config = TelemetryConfig {
            buffer_capacity: 42,
            ..Default::default()
        };
        let collector = TelemetryCollector::new(config);
        assert_eq!(collector.config().buffer_capacity, 42);
        assert_eq!(collector.config().mux_server_pid, 0);
    }

    #[test]
    fn collector_debug_impl() {
        let collector = TelemetryCollector::new(TelemetryConfig::default());
        let dbg = format!("{:?}", collector);
        assert!(dbg.contains("TelemetryCollector"));
        assert!(dbg.contains("config"));
        assert!(dbg.contains("sample_count"));
    }

    #[test]
    fn collector_snapshot_empty_registry() {
        let collector = TelemetryCollector::new(TelemetryConfig::default());
        let snap = collector.snapshot();
        assert!(snap.resource.is_none());
        assert!(snap.histograms.is_empty());
        assert!(snap.counters.is_empty());
        assert_eq!(snap.buffer_samples, 0);
        assert_eq!(snap.total_samples, 0);
    }

    #[test]
    fn collector_multiple_samples() {
        let collector = TelemetryCollector::new(TelemetryConfig {
            mux_server_pid: 0,
            ..Default::default()
        });
        collector.sample_once();
        collector.sample_once();
        collector.sample_once();
        assert_eq!(collector.sample_count(), 3);
        assert_eq!(collector.buffer().len(), 3);
    }

    #[test]
    fn collector_buffer_capacity_matches_config() {
        let config = TelemetryConfig {
            buffer_capacity: 5,
            mux_server_pid: 0,
            ..Default::default()
        };
        let collector = TelemetryCollector::new(config);
        assert_eq!(collector.buffer().capacity(), 5);

        // Push more than capacity
        for _ in 0..10 {
            collector.sample_once();
        }
        assert_eq!(collector.buffer().len(), 5);
        assert_eq!(collector.sample_count(), 10);
    }

    // -- Thread safety --------------------------------------------------------

    #[test]
    fn registry_concurrent_access() {
        let reg = Arc::new(MetricRegistry::new());
        reg.register_histogram("concurrent", 1000);

        let mut handles = vec![];
        for i in 0..10 {
            let reg = Arc::clone(&reg);
            handles.push(std::thread::spawn(move || {
                for j in 0..100 {
                    reg.record_histogram("concurrent", (i * 100 + j) as f64);
                    reg.increment_counter("ops");
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(reg.counter_value("ops"), 1000);
        let summaries = reg.histogram_summaries();
        assert_eq!(summaries[0].count, 1000);
    }

    #[test]
    fn buffer_concurrent_push() {
        let buf = Arc::new(CircularMetricBuffer::new(100));
        let mut handles = vec![];

        for i in 0..10 {
            let buf = Arc::clone(&buf);
            handles.push(std::thread::spawn(move || {
                for j in 0..20 {
                    buf.push(ResourceSnapshot {
                        pid: i * 20 + j,
                        rss_bytes: 0,
                        virt_bytes: 0,
                        fd_count: 0,
                        io_read_bytes: None,
                        io_write_bytes: None,
                        cpu_percent: None,
                        timestamp_secs: 0,
                    });
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(buf.total_recorded(), 200);
        assert_eq!(buf.len(), 100); // capacity = 100
    }

    #[test]
    fn registry_concurrent_counters_many_keys() {
        let reg = Arc::new(MetricRegistry::new());
        let mut handles = vec![];
        for i in 0..5 {
            let reg = Arc::clone(&reg);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    let name = format!("counter_{}", i);
                    reg.increment_counter(&name);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        for i in 0..5 {
            assert_eq!(reg.counter_value(&format!("counter_{}", i)), 50);
        }
    }

    // -- Platform-specific tests ----------------------------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parse_kb_value() {
        assert_eq!(parse_kb_value("  12345 kB"), Some(12345));
        assert_eq!(parse_kb_value("0 kB"), Some(0));
        assert_eq!(parse_kb_value("invalid"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn platform_resource_collection() {
        let snap = ResourceSnapshot::collect(0).unwrap();
        assert!(snap.rss_bytes > 0, "RSS should be positive for self");
        // FD count should be at least a few (stdin, stdout, stderr)
        assert!(snap.fd_count >= 3, "FD count should be >= 3");
    }

    // -- SystemMetrics trait --------------------------------------------------

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn platform_metrics_collect_self() {
        let pm = PlatformMetrics;
        let snap = pm.collect_snapshot(0).expect("should collect self");
        assert_eq!(snap.pid, std::process::id());
        assert!(snap.rss_bytes > 0);
    }

    #[test]
    fn platform_metrics_system_memory() {
        let pm = PlatformMetrics;
        let (total, _available) = pm.system_memory();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(total > 0, "total memory should be positive");
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = total; // stub returns 0 on unsupported
    }

    #[test]
    fn platform_metrics_cpu_count() {
        let pm = PlatformMetrics;
        assert!(pm.cpu_count() >= 1);
    }

    // -- TelemetryStore -------------------------------------------------------

    #[test]
    fn store_open_in_memory() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        assert_eq!(store.aggregate_count().unwrap(), 0);
    }

    #[test]
    fn store_persist_and_query() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        let agg = HourlyAggregate {
            hour_ts: 1700000000,
            sample_count: 120,
            mean_rss_bytes: 50 * 1024 * 1024,
            peak_rss_bytes: 80 * 1024 * 1024,
            mean_fd_count: 42,
            peak_fd_count: 60,
            mean_cpu_percent: Some(15.5),
        };
        store.persist_aggregate(&agg).unwrap();
        assert_eq!(store.aggregate_count().unwrap(), 1);

        let results = store.query_history(0, i64::MAX as u64).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hour_ts, 1700000000);
        assert_eq!(results[0].sample_count, 120);
        assert_eq!(results[0].peak_rss_bytes, 80 * 1024 * 1024);
    }

    #[test]
    fn store_idempotent_persist() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        let agg = HourlyAggregate {
            hour_ts: 1700000000,
            sample_count: 100,
            mean_rss_bytes: 50_000_000,
            peak_rss_bytes: 80_000_000,
            mean_fd_count: 42,
            peak_fd_count: 60,
            mean_cpu_percent: None,
        };
        store.persist_aggregate(&agg).unwrap();
        // Persist again with updated values — should upsert
        let agg2 = HourlyAggregate {
            sample_count: 200,
            ..agg
        };
        store.persist_aggregate(&agg2).unwrap();
        assert_eq!(store.aggregate_count().unwrap(), 1);
        let results = store.query_history(0, i64::MAX as u64).unwrap();
        assert_eq!(results[0].sample_count, 200);
    }

    #[test]
    fn store_aggregate_snapshots() {
        let snapshots = vec![
            ResourceSnapshot {
                pid: 1,
                rss_bytes: 100,
                virt_bytes: 200,
                fd_count: 10,
                io_read_bytes: None,
                io_write_bytes: None,
                cpu_percent: Some(10.0),
                timestamp_secs: 1000,
            },
            ResourceSnapshot {
                pid: 1,
                rss_bytes: 200,
                virt_bytes: 400,
                fd_count: 20,
                io_read_bytes: None,
                io_write_bytes: None,
                cpu_percent: Some(20.0),
                timestamp_secs: 1030,
            },
        ];

        let agg = TelemetryStore::aggregate_snapshots(1000, &snapshots).unwrap();
        assert_eq!(agg.hour_ts, 1000);
        assert_eq!(agg.sample_count, 2);
        assert_eq!(agg.mean_rss_bytes, 150); // (100+200)/2
        assert_eq!(agg.peak_rss_bytes, 200);
        assert_eq!(agg.mean_fd_count, 15); // (10+20)/2
        assert_eq!(agg.peak_fd_count, 20);
        assert!((agg.mean_cpu_percent.unwrap() - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn store_aggregate_snapshots_empty() {
        assert!(TelemetryStore::aggregate_snapshots(1000, &[]).is_none());
    }

    #[test]
    fn store_flush_buffer() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        let buf = CircularMetricBuffer::new(100);

        buf.push(ResourceSnapshot {
            pid: 1,
            rss_bytes: 1024,
            virt_bytes: 2048,
            fd_count: 5,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: 1700000000,
        });
        buf.push(ResourceSnapshot {
            pid: 1,
            rss_bytes: 2048,
            virt_bytes: 4096,
            fd_count: 10,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: 1700000030,
        });

        let count = store.flush_buffer(&buf).unwrap();
        assert_eq!(count, 2);
        assert_eq!(store.aggregate_count().unwrap(), 1);
    }

    #[test]
    fn store_query_range() {
        let store = TelemetryStore::open_in_memory(30).unwrap();

        for hour in 0..5 {
            let agg = HourlyAggregate {
                hour_ts: 1700000000 + hour * 3600,
                sample_count: 120,
                mean_rss_bytes: 50_000_000,
                peak_rss_bytes: 80_000_000,
                mean_fd_count: 42,
                peak_fd_count: 60,
                mean_cpu_percent: None,
            };
            store.persist_aggregate(&agg).unwrap();
        }

        // Query a sub-range: hours 1-3
        let results = store
            .query_history(1700000000 + 3600, 1700000000 + 3 * 3600)
            .unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn store_hourly_aggregate_serde() {
        let agg = HourlyAggregate {
            hour_ts: 1700000000,
            sample_count: 120,
            mean_rss_bytes: 50_000_000,
            peak_rss_bytes: 80_000_000,
            mean_fd_count: 42,
            peak_fd_count: 60,
            mean_cpu_percent: Some(15.5),
        };
        let json = serde_json::to_string(&agg).unwrap();
        let back: HourlyAggregate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hour_ts, 1700000000);
        assert_eq!(back.peak_rss_bytes, 80_000_000);
        assert!((back.mean_cpu_percent.unwrap() - 15.5).abs() < f64::EPSILON);
    }

    // -- NEW: Additional TelemetryStore tests ---------------------------------

    #[test]
    fn store_debug_impl() {
        let store = TelemetryStore::open_in_memory(7).unwrap();
        let dbg = format!("{:?}", store);
        assert!(dbg.contains("TelemetryStore"));
        assert!(dbg.contains("retention_hours"));
        // 7 days * 24 = 168 hours
        assert!(dbg.contains("168"));
    }

    #[test]
    fn store_aggregate_no_cpu() {
        let snapshots = vec![make_snap(1, 500, 10, 1000), make_snap(1, 600, 12, 1030)];
        let agg = TelemetryStore::aggregate_snapshots(1000, &snapshots).unwrap();
        assert!(agg.mean_cpu_percent.is_none());
        assert_eq!(agg.mean_rss_bytes, 550);
        assert_eq!(agg.peak_rss_bytes, 600);
    }

    #[test]
    fn store_query_empty_range() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        let agg = HourlyAggregate {
            hour_ts: 1700000000,
            sample_count: 1,
            mean_rss_bytes: 100,
            peak_rss_bytes: 100,
            mean_fd_count: 5,
            peak_fd_count: 5,
            mean_cpu_percent: None,
        };
        store.persist_aggregate(&agg).unwrap();

        // Query a range that doesn't include any data
        let results = store.query_history(2000000000, 2000003600).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn store_flush_empty_buffer() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        let buf = CircularMetricBuffer::new(100);
        let count = store.flush_buffer(&buf).unwrap();
        assert_eq!(count, 0);
        assert_eq!(store.aggregate_count().unwrap(), 0);
    }

    #[test]
    fn store_hourly_aggregate_clone_and_debug() {
        let agg = HourlyAggregate {
            hour_ts: 1700000000,
            sample_count: 10,
            mean_rss_bytes: 1000,
            peak_rss_bytes: 2000,
            mean_fd_count: 5,
            peak_fd_count: 10,
            mean_cpu_percent: Some(25.0),
        };
        let cloned = agg.clone();
        assert_eq!(cloned.hour_ts, 1700000000);
        assert_eq!(cloned.sample_count, 10);
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("HourlyAggregate"));
    }

    #[test]
    fn store_hourly_aggregate_serde_none_cpu() {
        let agg = HourlyAggregate {
            hour_ts: 1700000000,
            sample_count: 1,
            mean_rss_bytes: 100,
            peak_rss_bytes: 200,
            mean_fd_count: 5,
            peak_fd_count: 10,
            mean_cpu_percent: None,
        };
        let json = serde_json::to_string(&agg).unwrap();
        let back: HourlyAggregate = serde_json::from_str(&json).unwrap();
        assert!(back.mean_cpu_percent.is_none());
        assert_eq!(back.hour_ts, 1700000000);
    }

    #[test]
    fn store_aggregate_single_snapshot() {
        let snapshots = vec![ResourceSnapshot {
            pid: 42,
            rss_bytes: 1000,
            virt_bytes: 2000,
            fd_count: 7,
            io_read_bytes: Some(500),
            io_write_bytes: Some(300),
            cpu_percent: Some(5.0),
            timestamp_secs: 1700000000,
        }];
        let agg = TelemetryStore::aggregate_snapshots(1700000000, &snapshots).unwrap();
        assert_eq!(agg.sample_count, 1);
        assert_eq!(agg.mean_rss_bytes, 1000);
        assert_eq!(agg.peak_rss_bytes, 1000);
        assert_eq!(agg.mean_fd_count, 7);
        assert_eq!(agg.peak_fd_count, 7);
        assert!((agg.mean_cpu_percent.unwrap() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn store_aggregate_mixed_cpu_some_none() {
        let snapshots = vec![
            ResourceSnapshot {
                pid: 1,
                rss_bytes: 100,
                virt_bytes: 200,
                fd_count: 5,
                io_read_bytes: None,
                io_write_bytes: None,
                cpu_percent: Some(20.0),
                timestamp_secs: 1000,
            },
            ResourceSnapshot {
                pid: 1,
                rss_bytes: 200,
                virt_bytes: 400,
                fd_count: 10,
                io_read_bytes: None,
                io_write_bytes: None,
                cpu_percent: None, // no CPU for this one
                timestamp_secs: 1030,
            },
            ResourceSnapshot {
                pid: 1,
                rss_bytes: 300,
                virt_bytes: 600,
                fd_count: 15,
                io_read_bytes: None,
                io_write_bytes: None,
                cpu_percent: Some(40.0),
                timestamp_secs: 1060,
            },
        ];
        let agg = TelemetryStore::aggregate_snapshots(1000, &snapshots).unwrap();
        // mean_cpu should only average the two Some values: (20+40)/2 = 30
        assert!((agg.mean_cpu_percent.unwrap() - 30.0).abs() < f64::EPSILON);
        assert_eq!(agg.sample_count, 3);
    }

    #[test]
    fn store_multiple_persist_different_hours() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        for i in 0..10u64 {
            let agg = HourlyAggregate {
                hour_ts: 1700000000 + i * 3600,
                sample_count: (i + 1) as u32,
                mean_rss_bytes: 1000 * (i + 1),
                peak_rss_bytes: 2000 * (i + 1),
                mean_fd_count: 5 + i,
                peak_fd_count: 10 + i,
                mean_cpu_percent: Some(i as f64),
            };
            store.persist_aggregate(&agg).unwrap();
        }
        assert_eq!(store.aggregate_count().unwrap(), 10);
        let results = store.query_history(0, u64::MAX / 2).unwrap();
        assert_eq!(results.len(), 10);
        // Should be ordered by hour_ts
        for i in 0..9 {
            assert!(results[i].hour_ts < results[i + 1].hour_ts);
        }
    }

    // -- NEW: TelemetryStoreError tests ---------------------------------------

    #[test]
    fn store_error_display_schema() {
        let err = TelemetryStoreError::Schema("bad migration".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Schema error"));
        assert!(display.contains("bad migration"));
    }

    #[test]
    fn store_error_debug() {
        let err = TelemetryStoreError::Schema("test".to_string());
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("Schema"));
        assert!(dbg.contains("test"));
    }

    #[test]
    fn store_error_is_std_error() {
        let err = TelemetryStoreError::Schema("test".to_string());
        // Verify it implements std::error::Error
        let _: &dyn std::error::Error = &err;
    }

    // -- NEW: TelemetrySnapshot tests -----------------------------------------

    #[test]
    fn telemetry_snapshot_clone_and_debug() {
        let snap = TelemetrySnapshot {
            timestamp_secs: 1700000000,
            resource: None,
            histograms: vec![],
            counters: HashMap::new(),
            buffer_samples: 0,
            total_samples: 0,
        };
        let cloned = snap.clone();
        assert_eq!(cloned.timestamp_secs, 1700000000);
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("TelemetrySnapshot"));
    }

    #[test]
    fn telemetry_snapshot_serde_no_resource() {
        let snap = TelemetrySnapshot {
            timestamp_secs: 100,
            resource: None,
            histograms: vec![],
            counters: HashMap::new(),
            buffer_samples: 0,
            total_samples: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert!(back.resource.is_none());
        assert!(back.histograms.is_empty());
    }

    #[test]
    fn telemetry_snapshot_serde_with_counters_and_histograms() {
        let mut counters = HashMap::new();
        counters.insert("reads".to_string(), 100);
        counters.insert("writes".to_string(), 50);

        let snap = TelemetrySnapshot {
            timestamp_secs: 1700000000,
            resource: None,
            histograms: vec![HistogramSummary {
                name: "latency".to_string(),
                count: 10,
                retained: 10,
                mean: Some(5.0),
                min: Some(1.0),
                max: Some(10.0),
                p50: Some(5.0),
                p95: Some(9.0),
                p99: Some(10.0),
            }],
            counters,
            buffer_samples: 3,
            total_samples: 5,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.histograms.len(), 1);
        assert_eq!(back.histograms[0].name, "latency");
        assert_eq!(back.counters["reads"], 100);
        assert_eq!(back.counters["writes"], 50);
        assert_eq!(back.buffer_samples, 3);
        assert_eq!(back.total_samples, 5);
    }

    // -- NEW: HistogramSummary tests ------------------------------------------

    #[test]
    fn histogram_summary_clone_and_debug() {
        let s = HistogramSummary {
            name: "test_summary".to_string(),
            count: 5,
            retained: 5,
            mean: Some(10.0),
            min: Some(1.0),
            max: Some(20.0),
            p50: Some(10.0),
            p95: Some(18.0),
            p99: Some(19.5),
        };
        let cloned = s.clone();
        assert_eq!(cloned.name, "test_summary");
        assert_eq!(cloned.count, 5);
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("HistogramSummary"));
    }

    #[test]
    fn histogram_summary_all_none_serde() {
        let s = HistogramSummary {
            name: "empty".to_string(),
            count: 0,
            retained: 0,
            mean: None,
            min: None,
            max: None,
            p50: None,
            p95: None,
            p99: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: HistogramSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.count, 0);
        assert!(back.mean.is_none());
        assert!(back.min.is_none());
        assert!(back.max.is_none());
        assert!(back.p50.is_none());
        assert!(back.p95.is_none());
        assert!(back.p99.is_none());
    }

    // -- NEW: Integration patterns -------------------------------------------

    #[test]
    fn integration_snapshot_to_buffer_to_store() {
        // End-to-end: collect snapshot -> push to buffer -> flush to store
        let buf = CircularMetricBuffer::new(10);
        let snap = ResourceSnapshot::collect(0).expect("should collect self");
        buf.push(snap);

        let store = TelemetryStore::open_in_memory(30).unwrap();
        let count = store.flush_buffer(&buf).unwrap();
        assert_eq!(count, 1);
        assert_eq!(store.aggregate_count().unwrap(), 1);
    }

    #[test]
    fn integration_collector_to_store() {
        let collector = TelemetryCollector::new(TelemetryConfig {
            mux_server_pid: 0,
            buffer_capacity: 10,
            ..Default::default()
        });
        collector.sample_once();
        collector.sample_once();

        let store = TelemetryStore::open_in_memory(30).unwrap();
        let count = store.flush_buffer(&collector.buffer()).unwrap();
        assert_eq!(count, 2);

        // Verify the collector's snapshot reflects the state
        let snap = collector.snapshot();
        assert_eq!(snap.buffer_samples, 2);
        assert_eq!(snap.total_samples, 2);
        assert!(snap.resource.is_some());
    }

    #[test]
    fn integration_registry_with_multiple_histograms_and_counters() {
        let reg = MetricRegistry::new();
        reg.register_histogram("capture_latency_us", 500);
        reg.register_histogram("storage_write_us", 500);
        reg.register_histogram("query_latency_us", 500);

        // Simulate some workload
        for i in 0..100 {
            reg.record_histogram("capture_latency_us", (i % 50) as f64);
            reg.record_histogram("storage_write_us", (i % 30) as f64 * 2.0);
            reg.increment_counter("total_captures");
            if i % 5 == 0 {
                reg.record_histogram("query_latency_us", i as f64);
                reg.increment_counter("queries");
            }
        }

        assert_eq!(reg.histogram_count(), 3);
        assert_eq!(reg.counter_count(), 2);
        assert_eq!(reg.counter_value("total_captures"), 100);
        assert_eq!(reg.counter_value("queries"), 20);

        let summaries = reg.histogram_summaries();
        let capture = summaries
            .iter()
            .find(|s| s.name == "capture_latency_us")
            .unwrap();
        assert_eq!(capture.count, 100);
        let storage = summaries
            .iter()
            .find(|s| s.name == "storage_write_us")
            .unwrap();
        assert_eq!(storage.count, 100);
        let query = summaries
            .iter()
            .find(|s| s.name == "query_latency_us")
            .unwrap();
        assert_eq!(query.count, 20);
    }

    #[test]
    fn integration_scope_timer_with_collector() {
        let collector = TelemetryCollector::new(TelemetryConfig::default());
        let reg = collector.registry();
        reg.register_histogram("operation_us", 100);

        {
            let _t = ScopeTimer::new(&reg, "operation_us");
            // Simulate some work
            let mut sum = 0u64;
            for i in 0..1000 {
                sum = sum.wrapping_add(i);
            }
            let _ = sum;
        }

        let snap = collector.snapshot();
        assert_eq!(snap.histograms.len(), 1);
        assert_eq!(snap.histograms[0].count, 1);
        assert!(snap.histograms[0].p50.unwrap() >= 0.0);
    }

    #[test]
    fn integration_store_multiple_flushes() {
        let store = TelemetryStore::open_in_memory(30).unwrap();
        let buf = CircularMetricBuffer::new(100);

        // First flush
        buf.push(make_snap(1, 1000, 10, 1700000000));
        let c1 = store.flush_buffer(&buf).unwrap();
        assert_eq!(c1, 1);

        // Second flush with more data
        buf.push(make_snap(1, 2000, 20, 1700000030));
        buf.push(make_snap(1, 3000, 30, 1700000060));
        let c2 = store.flush_buffer(&buf).unwrap();
        assert_eq!(c2, 3); // buffer still has all 3 (no drain on flush)

        // Store has 1 aggregate (upserted for the same hour)
        assert_eq!(store.aggregate_count().unwrap(), 1);
    }

    // -- NEW: AtomicU64Wrapper Debug ------------------------------------------

    #[test]
    fn atomic_u64_wrapper_debug() {
        let w = AtomicU64Wrapper(AtomicU64::new(42));
        let dbg = format!("{:?}", w);
        assert_eq!(dbg, "42");
    }

    // -- NEW: Additional edge-case and coverage tests -------------------------

    #[test]
    fn config_partial_json_uses_defaults_for_missing_fields() {
        // Provide only some fields; missing ones should fall back to Default.
        let json = r#"{"buffer_capacity": 256, "per_process_metrics": false}"#;
        let config: TelemetryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.buffer_capacity, 256);
        assert!(!config.per_process_metrics);
        // Unspecified fields should be defaults
        assert_eq!(config.sample_interval, Duration::from_secs(30));
        assert_eq!(config.histogram_buckets, 1024);
        assert_eq!(config.mux_server_pid, 0);
    }

    #[test]
    fn metric_point_serde_deserialize_without_tags_field() {
        // MetricPoint has `#[serde(default, skip_serializing_if = ...)]` on tags.
        // Verify we can deserialize JSON that entirely omits the "tags" key.
        let json = r#"{"name":"test","value":3.25,"timestamp_secs":1700000000}"#;
        let mp: MetricPoint = serde_json::from_str(json).unwrap();
        assert_eq!(mp.name, "test");
        assert!((mp.value - 3.25).abs() < f64::EPSILON);
        assert_eq!(mp.timestamp_secs, 1700000000);
        assert!(mp.tags.is_empty());
    }

    #[test]
    fn histogram_quantile_two_samples_interpolation() {
        // With exactly 2 samples, q=0.5 should return the first (index 0)
        // because idx = ((2-1) * 0.5) as usize = 0.
        let mut h = Histogram::new("two", 100);
        h.record(100.0);
        h.record(200.0);

        let q0 = h.quantile(0.0).unwrap();
        assert!(
            (q0 - 100.0).abs() < f64::EPSILON,
            "q(0.0) = {}, expected 100.0",
            q0
        );

        let q50 = h.quantile(0.5).unwrap();
        assert!(
            (q50 - 100.0).abs() < f64::EPSILON,
            "q(0.5) = {}, expected 100.0",
            q50
        );

        let q1 = h.quantile(1.0).unwrap();
        assert!(
            (q1 - 200.0).abs() < f64::EPSILON,
            "q(1.0) = {}, expected 200.0",
            q1
        );

        // Mean should reflect both, including evicted tracking
        assert!((h.mean().unwrap() - 150.0).abs() < f64::EPSILON);
    }

    #[test]
    fn telemetry_snapshot_serde_with_multiple_histograms() {
        let snap = TelemetrySnapshot {
            timestamp_secs: 1700000000,
            resource: None,
            histograms: vec![
                HistogramSummary {
                    name: "h1".to_string(),
                    count: 10,
                    retained: 10,
                    mean: Some(5.0),
                    min: Some(1.0),
                    max: Some(10.0),
                    p50: Some(5.0),
                    p95: Some(9.0),
                    p99: Some(10.0),
                },
                HistogramSummary {
                    name: "h2".to_string(),
                    count: 0,
                    retained: 0,
                    mean: None,
                    min: None,
                    max: None,
                    p50: None,
                    p95: None,
                    p99: None,
                },
                HistogramSummary {
                    name: "h3".to_string(),
                    count: 1,
                    retained: 1,
                    mean: Some(42.0),
                    min: Some(42.0),
                    max: Some(42.0),
                    p50: Some(42.0),
                    p95: Some(42.0),
                    p99: Some(42.0),
                },
            ],
            counters: HashMap::new(),
            buffer_samples: 0,
            total_samples: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.histograms.len(), 3);
        assert_eq!(back.histograms[0].name, "h1");
        assert_eq!(back.histograms[1].name, "h2");
        assert!(back.histograms[1].mean.is_none());
        assert_eq!(back.histograms[2].count, 1);
        assert!((back.histograms[2].mean.unwrap() - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn store_retention_hours_calculated_correctly() {
        // 7 days = 168 hours, 0 days = 0 hours, 365 days = 8760 hours
        let store_7 = TelemetryStore::open_in_memory(7).unwrap();
        let dbg_7 = format!("{:?}", store_7);
        assert!(
            dbg_7.contains("168"),
            "7 days should be 168 hours, got: {}",
            dbg_7
        );

        let store_0 = TelemetryStore::open_in_memory(0).unwrap();
        let dbg_0 = format!("{:?}", store_0);
        assert!(
            dbg_0.contains("0"),
            "0 days should be 0 hours, got: {}",
            dbg_0
        );

        let store_365 = TelemetryStore::open_in_memory(365).unwrap();
        let dbg_365 = format!("{:?}", store_365);
        assert!(
            dbg_365.contains("8760"),
            "365 days should be 8760 hours, got: {}",
            dbg_365
        );
    }
}
