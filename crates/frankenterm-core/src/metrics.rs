//! Prometheus metrics endpoint (feature-gated).
//!
//! Exposes a minimal, safe metrics surface for ft watcher health.
//! Disabled by default and bound to localhost unless explicitly enabled.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::cost_tracker::CostAttributionEstimateSummary;
use crate::events::EventBus;
use crate::runtime::RuntimeHandle;
use crate::runtime_async::io::AsyncWriteExt;
use crate::runtime_async::net::{TcpListener, TcpStream};
use crate::runtime_async::notify::Notify;
use crate::runtime_async::task::{JoinErrorKind, JoinSet, JoinSetSettlement, SpawnError};
use crate::runtime_async::{AcquireError, Semaphore, TryAcquireError};
use tracing::{debug, warn};

/// Boxed future for async trait-like APIs without additional dependencies.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Snapshot of event-bus metrics for exporting.
#[derive(Debug, Clone, Default)]
pub struct EventBusSnapshot {
    pub events_published: u64,
    pub events_dropped_no_subscribers: u64,
    pub active_subscribers: u64,
    pub subscriber_lag_events: u64,
    pub capacity: usize,
    pub delta_queued: usize,
    pub detection_queued: usize,
    pub signal_queued: usize,
    pub delta_subscribers: usize,
    pub detection_subscribers: usize,
    pub signal_subscribers: usize,
    pub delta_oldest_lag_ms: Option<u64>,
    pub detection_oldest_lag_ms: Option<u64>,
    pub signal_oldest_lag_ms: Option<u64>,
}

/// Snapshot of runtime metrics for Prometheus rendering.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub uptime_seconds: f64,
    pub observed_panes: usize,
    pub capture_queue_depth: usize,
    pub capture_queue_capacity: usize,
    pub write_queue_depth: usize,
    pub segments_persisted: u64,
    pub events_recorded: u64,
    pub ingest_lag_avg_ms: f64,
    pub ingest_lag_max_ms: u64,
    pub ingest_lag_sum_ms: u64,
    pub ingest_lag_count: u64,
    pub db_last_write_age_ms: Option<u64>,
    // Native output coalescing metrics (wa-x4rq)
    pub native_output_input_events: u64,
    pub native_output_batches_emitted: u64,
    pub native_output_input_bytes: u64,
    pub native_output_emitted_bytes: u64,
    pub native_output_max_batch_events: u64,
    pub native_output_max_batch_bytes: u64,
    pub native_output_coalesce_ratio: f64,
    pub event_bus: Option<EventBusSnapshot>,
    /// Proxy-based, non-attested cost attribution estimates for Prometheus.
    pub cost_attribution_estimates: Vec<CostAttributionEstimateSummary>,
}

impl MetricsSnapshot {
    /// Render metrics in Prometheus text exposition format.
    #[must_use]
    pub fn render_prometheus(&self, prefix: &str) -> String {
        let mut output = String::new();
        let prefix = sanitize_prefix(prefix);

        push_gauge(
            &mut output,
            metric_name(&prefix, "uptime_seconds"),
            "Watcher uptime in seconds",
            format_float(self.uptime_seconds),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "observed_panes"),
            "Number of panes currently observed",
            self.observed_panes.to_string(),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "capture_queue_depth"),
            "Current capture queue depth",
            self.capture_queue_depth.to_string(),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "capture_queue_capacity"),
            "Maximum capture queue capacity",
            self.capture_queue_capacity.to_string(),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "write_queue_depth"),
            "Current storage write queue depth",
            self.write_queue_depth.to_string(),
        );
        push_counter(
            &mut output,
            metric_name(&prefix, "segments_persisted_total"),
            "Total output segments persisted",
            self.segments_persisted.to_string(),
        );
        push_counter(
            &mut output,
            metric_name(&prefix, "events_recorded_total"),
            "Total events recorded",
            self.events_recorded.to_string(),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "ingest_lag_avg_ms"),
            "Average ingest lag in milliseconds",
            format_float(self.ingest_lag_avg_ms),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "ingest_lag_max_ms"),
            "Maximum ingest lag in milliseconds",
            self.ingest_lag_max_ms.to_string(),
        );
        push_counter(
            &mut output,
            metric_name(&prefix, "ingest_lag_ms_sum"),
            "Sum of ingest lag samples in milliseconds",
            self.ingest_lag_sum_ms.to_string(),
        );
        push_counter(
            &mut output,
            metric_name(&prefix, "ingest_lag_ms_count"),
            "Count of ingest lag samples",
            self.ingest_lag_count.to_string(),
        );

        let db_age = self.db_last_write_age_ms.map_or(-1_i64, |ms| ms as i64);
        push_gauge(
            &mut output,
            metric_name(&prefix, "db_last_write_age_ms"),
            "Age in milliseconds since last DB write (-1 means unknown)",
            db_age.to_string(),
        );

        push_counter(
            &mut output,
            metric_name(&prefix, "native_output_input_events_total"),
            "Total native pane output events received (pre-coalesce)",
            self.native_output_input_events.to_string(),
        );
        push_counter(
            &mut output,
            metric_name(&prefix, "native_output_batches_emitted_total"),
            "Total native pane output batches emitted (post-coalesce)",
            self.native_output_batches_emitted.to_string(),
        );
        push_counter(
            &mut output,
            metric_name(&prefix, "native_output_input_bytes_total"),
            "Total native output bytes received (pre-coalesce)",
            self.native_output_input_bytes.to_string(),
        );
        push_counter(
            &mut output,
            metric_name(&prefix, "native_output_emitted_bytes_total"),
            "Total native output bytes emitted (post-coalesce)",
            self.native_output_emitted_bytes.to_string(),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "native_output_max_batch_events"),
            "Maximum number of input events merged into one emitted batch",
            self.native_output_max_batch_events.to_string(),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "native_output_max_batch_bytes"),
            "Maximum size in bytes of one emitted native output batch",
            self.native_output_max_batch_bytes.to_string(),
        );
        push_gauge(
            &mut output,
            metric_name(&prefix, "native_output_coalesce_ratio"),
            "Average native output coalescing ratio (input events / emitted batches)",
            format_float(self.native_output_coalesce_ratio),
        );

        if let Some(ref bus) = self.event_bus {
            push_counter(
                &mut output,
                metric_name(&prefix, "event_bus_events_published_total"),
                "Total events published to the event bus",
                bus.events_published.to_string(),
            );
            push_counter(
                &mut output,
                metric_name(&prefix, "event_bus_events_dropped_total"),
                "Events dropped due to no subscribers",
                bus.events_dropped_no_subscribers.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_active_subscribers"),
                "Current active event bus subscribers",
                bus.active_subscribers.to_string(),
            );
            push_counter(
                &mut output,
                metric_name(&prefix, "event_bus_subscriber_lag_events_total"),
                "Total lag events (slow subscribers)",
                bus.subscriber_lag_events.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_capacity"),
                "Event bus channel capacity",
                bus.capacity.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_delta_queued"),
                "Queued delta events",
                bus.delta_queued.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_detection_queued"),
                "Queued detection events",
                bus.detection_queued.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_signal_queued"),
                "Queued signal events",
                bus.signal_queued.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_delta_subscribers"),
                "Delta channel subscribers",
                bus.delta_subscribers.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_detection_subscribers"),
                "Detection channel subscribers",
                bus.detection_subscribers.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_signal_subscribers"),
                "Signal channel subscribers",
                bus.signal_subscribers.to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_delta_oldest_lag_ms"),
                "Age of oldest delta event in ms (-1 means none)",
                bus.delta_oldest_lag_ms
                    .map_or(-1_i64, |ms| i64::try_from(ms).unwrap_or(i64::MAX))
                    .to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_detection_oldest_lag_ms"),
                "Age of oldest detection event in ms (-1 means none)",
                bus.detection_oldest_lag_ms
                    .map_or(-1_i64, |ms| i64::try_from(ms).unwrap_or(i64::MAX))
                    .to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_signal_oldest_lag_ms"),
                "Age of oldest signal event in ms (-1 means none)",
                bus.signal_oldest_lag_ms
                    .map_or(-1_i64, |ms| i64::try_from(ms).unwrap_or(i64::MAX))
                    .to_string(),
            );
        }

        render_cost_attribution_estimates(&mut output, &prefix, &self.cost_attribution_estimates);

        output
    }
}

/// Collector trait for metrics snapshots.
pub trait MetricsCollector: Send + Sync {
    fn collect(&self) -> BoxFuture<'_, MetricsSnapshot>;
}

/// Metrics collector backed by a live observation runtime.
pub struct RuntimeMetricsCollector {
    runtime: Arc<RuntimeHandle>,
}

impl RuntimeMetricsCollector {
    #[must_use]
    pub fn new(runtime: Arc<RuntimeHandle>) -> Self {
        Self { runtime }
    }
}

impl MetricsCollector for RuntimeMetricsCollector {
    fn collect(&self) -> BoxFuture<'_, MetricsSnapshot> {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            let metrics = &runtime.metrics;
            let observed_panes = {
                let registry = runtime.registry.read().await;
                registry.observed_pane_ids().len()
            };
            let event_bus = runtime
                .event_bus
                .as_ref()
                .map(|bus| event_bus_snapshot(bus.as_ref()));
            let db_last_write_age_ms = metrics
                .last_db_write()
                .map(|ts| epoch_ms_u64().saturating_sub(ts));

            let native_output_input_events = metrics.native_output_input_events();
            let native_output_batches_emitted = metrics.native_output_batches_emitted();
            #[allow(clippy::cast_precision_loss)]
            let native_output_coalesce_ratio = if native_output_batches_emitted == 0 {
                0.0
            } else {
                native_output_input_events as f64 / native_output_batches_emitted as f64
            };

            MetricsSnapshot {
                uptime_seconds: runtime.start_time.elapsed().as_secs_f64(),
                observed_panes,
                capture_queue_depth: runtime.capture_queue_depth(),
                capture_queue_capacity: runtime.capture_queue_capacity(),
                write_queue_depth: runtime.write_queue_depth().await,
                segments_persisted: metrics.segments_persisted(),
                events_recorded: metrics.events_recorded(),
                ingest_lag_avg_ms: metrics.avg_ingest_lag_ms(),
                ingest_lag_max_ms: metrics.max_ingest_lag_ms(),
                ingest_lag_sum_ms: metrics.ingest_lag_sum_ms(),
                ingest_lag_count: metrics.ingest_lag_count(),
                db_last_write_age_ms,
                native_output_input_events,
                native_output_batches_emitted,
                native_output_input_bytes: metrics.native_output_input_bytes(),
                native_output_emitted_bytes: metrics.native_output_emitted_bytes(),
                native_output_max_batch_events: metrics.native_output_max_batch_events(),
                native_output_max_batch_bytes: metrics.native_output_max_batch_bytes(),
                native_output_coalesce_ratio,
                event_bus,
                cost_attribution_estimates: Vec::new(),
            }
        })
    }
}

/// Fixed metrics collector for tests.
#[derive(Clone)]
pub struct FixedMetricsCollector {
    snapshot: MetricsSnapshot,
}

impl FixedMetricsCollector {
    #[must_use]
    pub fn new(snapshot: MetricsSnapshot) -> Self {
        Self { snapshot }
    }
}

impl MetricsCollector for FixedMetricsCollector {
    fn collect(&self) -> BoxFuture<'_, MetricsSnapshot> {
        let snapshot = self.snapshot.clone();
        Box::pin(async move { snapshot })
    }
}

/// Metrics server handle.
#[must_use = "metrics server handles must be joined to prove terminal settlement"]
pub struct MetricsServerHandle {
    task: crate::watchdog::WatchdogHandle,
    local_addr: SocketAddr,
}

impl MetricsServerHandle {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn wait(self) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.wait_with_cx(&cx).await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`wait`].
    ///
    /// Signals the server's shared shutdown flag and retains task ownership
    /// through graceful completion or bounded abort settlement. A pre-cancelled
    /// caller is given an independent bounded cleanup context and cannot detach
    /// the accept loop.
    pub async fn wait_with_cx(self, cx: &crate::cx::Cx) {
        if let Err(error) = self.task.join_with_cx(cx).await {
            warn!(
                error = %error,
                "Metrics server task did not reach clean terminal settlement"
            );
        }
    }
}

const METRICS_CONNECTION_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const METRICS_CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(5);
const METRICS_MAX_CONCURRENT_CONNECTIONS: usize = 64;
const METRICS_ACCEPT_ERROR_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const METRICS_ACCEPT_ERROR_MAX_BACKOFF: Duration = Duration::from_secs(1);

// `std::io::ErrorKind` has no portable NotSocket variant. These symbolic raw
// codes cover FrankenTerm's Apple/BSD and x86_64/aarch64 Linux target classes
// without adding a libc dependency to the no-default-features build.
#[cfg(all(
    unix,
    any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
const UNIX_ENOTSOCK_RAW: i32 = 38;
#[cfg(all(
    unix,
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const UNIX_ENOTSOCK_RAW: i32 = 88;

#[cfg(all(test, feature = "metrics"))]
struct MetricsConnectionTestProbe {
    active_tasks: std::sync::atomic::AtomicUsize,
    total_admitted_tasks: std::sync::atomic::AtomicUsize,
    capacity_waits: std::sync::atomic::AtomicUsize,
    changed: Notify,
}

#[cfg(all(test, feature = "metrics"))]
impl MetricsConnectionTestProbe {
    fn new() -> Self {
        Self {
            active_tasks: std::sync::atomic::AtomicUsize::new(0),
            total_admitted_tasks: std::sync::atomic::AtomicUsize::new(0),
            capacity_waits: std::sync::atomic::AtomicUsize::new(0),
            changed: Notify::new(),
        }
    }

    fn task_started(self: &Arc<Self>) -> MetricsConnectionTestTaskGuard {
        self.total_admitted_tasks.fetch_add(1, Ordering::SeqCst);
        self.active_tasks.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
        MetricsConnectionTestTaskGuard {
            probe: Arc::clone(self),
        }
    }

    fn record_capacity_wait(&self) {
        self.capacity_waits.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    fn active_tasks(&self) -> usize {
        self.active_tasks.load(Ordering::SeqCst)
    }

    fn total_admitted_tasks(&self) -> usize {
        self.total_admitted_tasks.load(Ordering::SeqCst)
    }

    fn capacity_waits(&self) -> usize {
        self.capacity_waits.load(Ordering::SeqCst)
    }

    async fn wait_until<F>(&self, description: &'static str, predicate: F)
    where
        F: FnMut() -> bool,
    {
        crate::runtime_async::timeout(
            Duration::from_secs(5),
            self.changed.wait_until(predicate),
        )
        .await
        .unwrap_or_else(|error| panic!("timed out waiting for {description}: {error}"));
    }
}

#[cfg(all(test, feature = "metrics"))]
struct MetricsConnectionTestTaskGuard {
    probe: Arc<MetricsConnectionTestProbe>,
}

#[cfg(all(test, feature = "metrics"))]
impl Drop for MetricsConnectionTestTaskGuard {
    fn drop(&mut self) {
        self.probe.active_tasks.fetch_sub(1, Ordering::SeqCst);
        self.probe.changed.notify_waiters();
    }
}

fn metrics_spawn_admission_is_transient(error: &SpawnError) -> bool {
    matches!(error, SpawnError::RegionAtCapacity { .. })
}

fn metrics_accept_error_is_permanent(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::Unsupported
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
    ) {
        return true;
    }

    // `ErrorKind` deliberately collapses some platform errors into `Other`.
    // A listener whose descriptor/socket is invalid cannot recover by retrying
    // accept, so recognize the stable platform error numbers as well. Resource
    // exhaustion such as EMFILE/WSAEMFILE remains retryable, but is throttled
    // by the bounded exponential backoff below.
    #[cfg(unix)]
    if error.raw_os_error() == Some(9) {
        // POSIX EBADF on every supported Unix target.
        return true;
    }
    #[cfg(all(
        unix,
        any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            all(
                any(target_os = "linux", target_os = "android"),
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        )
    ))]
    if error.raw_os_error() == Some(UNIX_ENOTSOCK_RAW) {
        return true;
    }
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(6 | 10_038)) {
        // ERROR_INVALID_HANDLE / WSAENOTSOCK.
        return true;
    }

    false
}

/// Shared bounded backoff state for transient accept and task-admission
/// failures. Each lane owns a separate instance so a healthy accept does not
/// erase a continuing region-capacity streak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MetricsRetryBackoff {
    retry_delay: Duration,
    consecutive_errors: u64,
}

impl MetricsRetryBackoff {
    const fn new() -> Self {
        Self {
            retry_delay: METRICS_ACCEPT_ERROR_INITIAL_BACKOFF,
            consecutive_errors: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn record_failure(&mut self) -> (u64, Duration) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        let retry_delay = self.retry_delay;
        self.retry_delay = self
            .retry_delay
            .saturating_mul(2)
            .min(METRICS_ACCEPT_ERROR_MAX_BACKOFF);
        (self.consecutive_errors, retry_delay)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetricsConnectionTaskDrainOutcome {
    Settled,
    SettledWithFailure {
        first_non_benign_failure: JoinErrorKind,
    },
    TimedOut {
        active_tasks: usize,
        unacknowledged_tasks: usize,
        first_non_benign_failure: Option<JoinErrorKind>,
    },
    Incomplete {
        active_tasks: usize,
        unacknowledged_tasks: usize,
        first_non_benign_failure: Option<JoinErrorKind>,
    },
}

fn classify_metrics_connection_task_drain(
    timed_out: bool,
    first_non_benign_failure: Option<JoinErrorKind>,
    settlement: JoinSetSettlement,
) -> MetricsConnectionTaskDrainOutcome {
    match settlement {
        JoinSetSettlement::Settled => first_non_benign_failure.map_or(
            MetricsConnectionTaskDrainOutcome::Settled,
            |first_non_benign_failure| {
                MetricsConnectionTaskDrainOutcome::SettledWithFailure {
                    first_non_benign_failure,
                }
            },
        ),
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } if timed_out => MetricsConnectionTaskDrainOutcome::TimedOut {
            active_tasks,
            unacknowledged_tasks,
            first_non_benign_failure,
        },
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } => MetricsConnectionTaskDrainOutcome::Incomplete {
            active_tasks,
            unacknowledged_tasks,
            first_non_benign_failure,
        },
    }
}

const fn metrics_connection_join_failure_is_benign(failure: JoinErrorKind) -> bool {
    matches!(
        failure,
        JoinErrorKind::Aborted | JoinErrorKind::ContextCancelled
    )
}

async fn settle_metrics_connection_tasks(
    connection_tasks: &mut JoinSet<()>,
) -> MetricsConnectionTaskDrainOutcome {
    connection_tasks.abort_all();
    let drain_cx = crate::cx::for_request();
    let mut first_non_benign_failure = None;
    let drain_result = crate::runtime_async::timeout_with_cx(
        &drain_cx,
        METRICS_CONNECTION_TASK_DRAIN_TIMEOUT,
        async {
            loop {
                match connection_tasks.drain_next_with_cx(&drain_cx).await {
                    Ok(Some(Ok(()))) => {}
                    Ok(Some(Err(error))) => {
                        let failure = error.kind();
                        if metrics_connection_join_failure_is_benign(failure) {
                            debug!(
                                failure_class = ?failure,
                                "Metrics connection task stopped during server shutdown"
                            );
                        } else {
                            first_non_benign_failure.get_or_insert(failure);
                            if failure == JoinErrorKind::WakerRegistrationFailed {
                                warn!(
                                    event = "metrics_connection_task_join_observation_quarantined",
                                    failure_class = ?failure,
                                    terminal_authority_retained = true,
                                    "Metrics connection task join observation failed; trusted drain retained terminal authority"
                                );
                            } else {
                                warn!(
                                    event = "metrics_connection_task_join_failure_observed",
                                    failure_class = ?failure,
                                    "Metrics connection task failed during trusted shutdown drain"
                                );
                            }
                        }
                    }
                    Ok(None) => return Ok(()),
                    Err(drain_error) => {
                        let failure = drain_error.kind();
                        if !metrics_connection_join_failure_is_benign(failure) {
                            first_non_benign_failure.get_or_insert(failure);
                        }
                        return Err(failure);
                    }
                }
            }
        },
    )
    .await;
    if let Ok(Err(drain_failure)) = &drain_result {
        warn!(
            failure_class = ?drain_failure,
            "Metrics connection task drain context failed before terminal settlement"
        );
    }
    classify_metrics_connection_task_drain(
        drain_result.is_err(),
        first_non_benign_failure,
        connection_tasks.settlement(),
    )
}

/// Minimal Prometheus metrics server.
pub struct MetricsServer {
    bind: String,
    prefix: String,
    collector: Arc<dyn MetricsCollector>,
    shutdown_flag: Arc<AtomicBool>,
    /// Must be set to `true` to bind on non-localhost addresses.
    allow_public_bind: bool,
    #[cfg(all(test, feature = "metrics"))]
    connection_test_probe: Option<Arc<MetricsConnectionTestProbe>>,
    #[cfg(all(test, feature = "metrics"))]
    connection_io_timeout: Duration,
}

impl MetricsServer {
    #[must_use]
    pub fn new(
        bind: impl Into<String>,
        prefix: impl Into<String>,
        collector: Arc<dyn MetricsCollector>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            bind: bind.into(),
            prefix: prefix.into(),
            collector,
            shutdown_flag,
            allow_public_bind: false,
            #[cfg(all(test, feature = "metrics"))]
            connection_test_probe: None,
            #[cfg(all(test, feature = "metrics"))]
            connection_io_timeout: METRICS_CONNECTION_IO_TIMEOUT,
        }
    }

    /// Explicitly opt in to binding on a non-localhost address.
    #[must_use]
    pub fn with_dangerous_public_bind(mut self) -> Self {
        self.allow_public_bind = true;
        self
    }

    #[cfg(all(test, feature = "metrics"))]
    fn with_connection_test_probe(mut self, probe: Arc<MetricsConnectionTestProbe>) -> Self {
        self.connection_test_probe = Some(probe);
        self
    }

    #[cfg(all(test, feature = "metrics"))]
    fn with_connection_io_timeout_for_test(mut self, timeout: Duration) -> Self {
        self.connection_io_timeout = timeout;
        self
    }

    pub async fn start(self) -> Result<MetricsServerHandle> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.start_with_cx(&cx).await
    }

    /// Start the metrics server bound to the caller's asupersync capability
    /// context (ft-xbnl0.2.3 Cx-first entry point).
    ///
    /// Clones `cx` into the spawned accept-loop task so budget-driven
    /// cancellation from the outer scope propagates through to the
    /// accept-poll timeout via
    /// [`crate::runtime_async::timeout_with_cx`]. Both the
    /// `shutdown_flag` and `cx.is_cancel_requested()` are checked each
    /// loop iteration so either cancellation path terminates the server
    /// promptly without waiting on the 250ms accept poll.
    ///
    /// Pre-flight: if `cx` is already cancelled on entry, the method
    /// returns `Err(Error::RuntimeOperation { .. })` without attempting to bind
    /// the TCP listener — an operator who has abandoned the metrics
    /// server should not leave a socket in LISTEN state.
    ///
    /// The legacy [`start`](Self::start) entry point is preserved for
    /// non-migrated callers; this is strictly additive.
    pub async fn start_with_cx(self, cx: &crate::cx::Cx) -> Result<MetricsServerHandle> {
        if cx.is_cancel_requested() {
            return Err(crate::Error::runtime_cancelled(
                "metrics start",
                "capability context already cancelled",
            ));
        }

        if !is_localhost_bind(&self.bind) && !self.allow_public_bind {
            return Err(crate::Error::runtime_backend(
                "metrics bind validation",
                format!(
                    "refusing to bind metrics on public address '{}' — use --dangerous-bind-any to override",
                    self.bind
                ),
            ));
        }
        if !is_localhost_bind(&self.bind) {
            warn!(
                bind = %self.bind,
                "binding metrics endpoint on non-localhost address — endpoint may be remotely reachable"
            );
        }

        let bind_addr = self.bind.clone();
        let listener = TcpListener::bind(bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let prefix = sanitize_prefix(&self.prefix);
        let collector = Arc::clone(&self.collector);
        let handle_shutdown_flag = Arc::clone(&self.shutdown_flag);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let shutdown_notify = Arc::new(Notify::new());
        let task_shutdown_notify = Arc::clone(&shutdown_notify);
        #[cfg(all(test, feature = "metrics"))]
        let connection_test_probe = self.connection_test_probe.clone();
        #[cfg(all(test, feature = "metrics"))]
        let connection_io_timeout = self.connection_io_timeout;
        #[cfg(not(all(test, feature = "metrics")))]
        let connection_io_timeout = METRICS_CONNECTION_IO_TIMEOUT;
        let join = crate::runtime_async::task::spawn_with_cx(cx, move |accept_cx| async move {
            use futures::future::{Either, select};

            let accept_poll_interval = Duration::from_millis(250);
            let mut connection_tasks = JoinSet::new();
            // Do not let stalled scrapers consume the runtime's global task
            // admission budget. Each connection also carries an I/O deadline
            // below, but the semaphore is the fail-fast bound while those
            // deadlines are pending.
            let connection_permits =
                Arc::new(Semaphore::new(METRICS_MAX_CONCURRENT_CONNECTIONS));
            let mut connection_capacity_waits = 0_u64;
            let mut accept_error_backoff = MetricsRetryBackoff::new();
            let mut spawn_capacity_backoff = MetricsRetryBackoff::new();
            loop {
                while let Some(join_result) = connection_tasks.try_join_next() {
                    if let Err(error) = join_result {
                        warn!(
                            failure_class = ?error.kind(),
                            "Metrics connection task failed"
                        );
                    }
                }

                if shutdown_flag.load(Ordering::SeqCst) || accept_cx.checkpoint().is_err() {
                    break;
                }

                // Reserve bounded task capacity before accepting a socket. If
                // all scrapers are stalled, leaving clients in the kernel
                // backlog applies backpressure without an accept/drop/log hot
                // loop on the runtime thread. The contended path wakes as soon
                // as a permit is released and polls shutdown at most every
                // accept interval.
                let connection_permit =
                    match Arc::clone(&connection_permits).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(TryAcquireError::NoPermits) => {
                            connection_capacity_waits =
                                connection_capacity_waits.saturating_add(1);
                            #[cfg(all(test, feature = "metrics"))]
                            if let Some(probe) = connection_test_probe.as_ref() {
                                probe.record_capacity_wait();
                            }
                            if connection_capacity_waits.is_power_of_two() {
                                warn!(
                                    connection_capacity_waits,
                                    max_connections = METRICS_MAX_CONCURRENT_CONNECTIONS,
                                    "Metrics listener waiting for bounded connection capacity"
                                );
                            }
                            let shutdown_wait =
                                std::pin::pin!(task_shutdown_notify.wait_until(|| {
                                    shutdown_flag.load(Ordering::SeqCst)
                                }));
                            let capacity_wait = std::pin::pin!(
                                crate::runtime_async::timeout_with_cx(
                                    &accept_cx,
                                    accept_poll_interval,
                                    Arc::clone(&connection_permits)
                                        .acquire_owned_with_cx(&accept_cx),
                                )
                            );
                            match select(shutdown_wait, capacity_wait).await {
                                Either::Left(((), _)) => break,
                                Either::Right((Ok(Ok(permit)), _)) => permit,
                                Either::Right((Ok(Err(AcquireError::Closed)), _)) => {
                                    warn!(
                                        "Metrics connection admission semaphore closed; stopping listener"
                                    );
                                    break;
                                }
                                Either::Right((Ok(Err(_)), _)) => break,
                                Either::Right((Err(_), _)) => continue,
                            }
                        }
                        Err(TryAcquireError::Closed) => {
                            warn!(
                                "Metrics connection admission semaphore closed; stopping listener"
                            );
                            break;
                        }
                    };

                let shutdown_wait = std::pin::pin!(task_shutdown_notify.wait_until(|| {
                    shutdown_flag.load(Ordering::SeqCst)
                }));
                let accept_wait = std::pin::pin!(crate::runtime_async::timeout_with_cx(
                    &accept_cx,
                    accept_poll_interval,
                    listener.accept(),
                ));
                let accept_result = match select(shutdown_wait, accept_wait).await {
                    Either::Left(((), _)) => break,
                    Either::Right((result, _)) => result,
                };

                match accept_result {
                    Ok(Ok((socket, peer))) => {
                        accept_error_backoff.reset();
                        if shutdown_flag.load(Ordering::SeqCst)
                            || accept_cx.checkpoint().is_err()
                        {
                            drop(socket);
                            break;
                        }
                        let collector = Arc::clone(&collector);
                        let prefix = prefix.clone();
                        #[cfg(all(test, feature = "metrics"))]
                        let task_test_probe = connection_test_probe.as_ref().map(Arc::clone);
                        match crate::runtime_async::task::try_spawn_with_cx(
                            &accept_cx,
                            move |conn_cx| async move {
                                let _connection_permit = connection_permit;
                                #[cfg(all(test, feature = "metrics"))]
                                let _connection_test_guard =
                                    task_test_probe.map(|probe| probe.task_started());
                                if let Err(err) =
                                    handle_connection_with_cx(
                                        &conn_cx,
                                        socket,
                                        &prefix,
                                        collector,
                                        connection_io_timeout,
                                    )
                                    .await
                                {
                                    debug!(error = %err, peer = %peer, "Metrics connection failed");
                                }
                            },
                        ) {
                            Ok(handle) => {
                                spawn_capacity_backoff.reset();
                                connection_tasks.insert_handle(handle);
                            }
                            Err(error) if metrics_spawn_admission_is_transient(&error) => {
                                let (consecutive_spawn_capacity_errors, retry_delay) =
                                    spawn_capacity_backoff.record_failure();
                                if consecutive_spawn_capacity_errors.is_power_of_two() {
                                    warn!(
                                        error_code = error.code(),
                                        consecutive_spawn_capacity_errors,
                                        retry_backoff_ms = retry_delay.as_millis(),
                                        "Metrics connection task region is at capacity; applying bounded retry backoff"
                                    );
                                }
                                let shutdown_wait =
                                    std::pin::pin!(task_shutdown_notify.wait_until(|| {
                                        shutdown_flag.load(Ordering::SeqCst)
                                    }));
                                let retry_wait =
                                    std::pin::pin!(crate::runtime_async::sleep_with_cx(
                                        &accept_cx,
                                        retry_delay,
                                    ));
                                match select(shutdown_wait, retry_wait).await {
                                    Either::Left(((), _)) | Either::Right((Err(_), _)) => break,
                                    Either::Right((Ok(()), _)) => {}
                                }
                            }
                            Err(error) => {
                                warn!(
                                    error_code = error.code(),
                                    "Metrics connection task admission failed; stopping listener"
                                );
                                break;
                            }
                        }
                    }
                    Ok(Err(err)) => {
                        if metrics_accept_error_is_permanent(&err) {
                            warn!(
                                error = %err,
                                error_kind = ?err.kind(),
                                raw_os_error = ?err.raw_os_error(),
                                "Metrics listener stopped after permanent accept failure"
                            );
                            break;
                        }

                        let (consecutive_accept_errors, retry_delay) =
                            accept_error_backoff.record_failure();
                        if consecutive_accept_errors.is_power_of_two() {
                            warn!(
                                error = %err,
                                error_kind = ?err.kind(),
                                raw_os_error = ?err.raw_os_error(),
                                consecutive_accept_errors,
                                retry_backoff_ms = retry_delay.as_millis(),
                                "Metrics listener accept failed; applying bounded retry backoff"
                            );
                        }
                        let shutdown_wait = std::pin::pin!(task_shutdown_notify.wait_until(|| {
                            shutdown_flag.load(Ordering::SeqCst)
                        }));
                        let retry_wait = std::pin::pin!(crate::runtime_async::sleep_with_cx(
                            &accept_cx,
                            retry_delay,
                        ));
                        match select(shutdown_wait, retry_wait).await {
                            Either::Left(((), _)) | Either::Right((Err(_), _)) => break,
                            Either::Right((Ok(()), _)) => {}
                        }
                    }
                    // `timeout_with_cx` returns Err on either the poll
                    // interval elapsing OR the Cx being cancelled. Loop
                    // and re-evaluate shutdown / cancellation on the
                    // next iteration.
                    Err(_) => {}
                }
            }

            match settle_metrics_connection_tasks(&mut connection_tasks).await {
                MetricsConnectionTaskDrainOutcome::Settled => {}
                MetricsConnectionTaskDrainOutcome::SettledWithFailure {
                    first_non_benign_failure,
                } => {
                    warn!(
                        event = "metrics_connection_task_settled_with_failure",
                        failure_class = ?first_non_benign_failure,
                        terminally_settled = true,
                        "Metrics connection tasks settled after a non-benign join failure"
                    );
                }
                MetricsConnectionTaskDrainOutcome::TimedOut {
                    active_tasks,
                    unacknowledged_tasks,
                    first_non_benign_failure,
                } => {
                    warn!(
                        event = "metrics_connection_task_drain_timeout",
                        active_tasks,
                        unacknowledged_tasks,
                        first_non_benign_failure = ?first_non_benign_failure,
                        remaining_tasks = connection_tasks.len(),
                        orphan_risk = true,
                        "Metrics connection tasks missed bounded terminal settlement"
                    );
                }
                MetricsConnectionTaskDrainOutcome::Incomplete {
                    active_tasks,
                    unacknowledged_tasks,
                    first_non_benign_failure,
                } => {
                    warn!(
                        event = "metrics_connection_task_settlement_incomplete",
                        active_tasks,
                        unacknowledged_tasks,
                        first_non_benign_failure = ?first_non_benign_failure,
                        remaining_tasks = connection_tasks.len(),
                        orphan_risk = true,
                        "Metrics connection task drain ended without terminal settlement"
                    );
                }
            }
        });

        Ok(MetricsServerHandle {
            task: crate::watchdog::WatchdogHandle::adopt_shutdown_task_with_notify(
                join,
                handle_shutdown_flag,
                shutdown_notify,
            ),
            local_addr,
        })
    }
}

/// Handles one accepted metrics connection under an explicit capability
/// context and one finite end-to-end I/O timeout.
///
/// Pre-flight `cx.checkpoint()` folded into `crate::Error::RuntimeOperation`
/// so a cancelled server shutdown interrupts the per-connection
/// body before any TCP read. The underlying I/O primitive does not observe the
/// Cx directly, so the complete read-and-response exchange shares one finite
/// timeout and is followed by a structural checkpoint/budget classification.
async fn handle_connection_with_cx(
    cx: &crate::cx::Cx,
    mut socket: TcpStream,
    prefix: &str,
    collector: Arc<dyn MetricsCollector>,
    io_timeout: Duration,
) -> Result<()> {
    metrics_connection_checkpoint(cx, "metrics handle_connection")?;
    metrics_connection_io_with_cx(
        cx,
        "metrics connection I/O",
        io_timeout,
        async move {
            let mut buf = [0_u8; 8192];
            let read_len = crate::runtime_async::io::read(&mut socket, &mut buf).await?;
            if read_len == 0 {
                return Ok(());
            }
            metrics_connection_checkpoint(cx, "metrics handle_connection_after_read")?;
            let request_bytes = buf[..read_len].to_vec();
            handle_connection_impl(socket, prefix, collector, request_bytes).await
        },
    )
    .await?
}

async fn metrics_connection_io_with_cx<F, T>(
    cx: &crate::cx::Cx,
    operation: &'static str,
    io_timeout: Duration,
    future: F,
) -> Result<T>
where
    F: Future<Output = T>,
{
    match crate::runtime_async::timeout_with_cx_typed(cx, io_timeout, future).await {
        Ok(output) => {
            metrics_connection_checkpoint(cx, operation)?;
            Ok(output)
        }
        Err(crate::runtime_async::TimeoutError::Elapsed) => match cx.checkpoint() {
            Err(error) => Err(metrics_context_error(cx, operation, Some(&error))),
            Ok(()) => match metrics_context_source(cx, None) {
                Some(source) => Err(crate::Error::RuntimeOperation { operation, source }),
                None => Err(crate::Error::runtime_backend(
                    operation,
                    "metrics connection I/O timeout elapsed",
                )),
            },
        },
    }
}

fn metrics_connection_checkpoint(cx: &crate::cx::Cx, operation: &'static str) -> Result<()> {
    match cx.checkpoint() {
        Err(error) => Err(metrics_context_error(cx, operation, Some(&error))),
        Ok(()) => match metrics_context_source(cx, None) {
            Some(source) => Err(crate::Error::RuntimeOperation { operation, source }),
            None => Ok(()),
        },
    }
}

fn metrics_context_error(
    cx: &crate::cx::Cx,
    operation: &'static str,
    error: Option<&crate::runtime_async::ContextError>,
) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: metrics_context_source(cx, error)
            .unwrap_or(crate::error::RuntimeOperationSource::ContextFailure),
    }
}

fn metrics_context_source(
    cx: &crate::cx::Cx,
    error: Option<&crate::runtime_async::ContextError>,
) -> Option<crate::error::RuntimeOperationSource> {
    use crate::error::RuntimeOperationSource;
    use crate::outcome::CancelKind;
    use crate::runtime_async::ContextErrorKind;

    if let Some(error) = error {
        match error.kind() {
            ContextErrorKind::DeadlineExceeded => {
                return Some(RuntimeOperationSource::DeadlineExceeded);
            }
            ContextErrorKind::CancelTimeout => {
                return Some(RuntimeOperationSource::Cancelled(
                    "capability cancellation cleanup timed out".to_string(),
                ));
            }
            ContextErrorKind::PollQuotaExhausted => {
                return Some(RuntimeOperationSource::PollQuotaExhausted);
            }
            ContextErrorKind::CostQuotaExhausted => {
                return Some(RuntimeOperationSource::CostBudgetExhausted);
            }
            ContextErrorKind::Cancelled => {}
            _ => return Some(RuntimeOperationSource::ContextFailure),
        }
    }

    let root_source = match cx.root_cancel_cause().map(|reason| reason.kind) {
        Some(CancelKind::Deadline | CancelKind::Timeout) => {
            Some(RuntimeOperationSource::DeadlineExceeded)
        }
        Some(CancelKind::PollQuota) => Some(RuntimeOperationSource::PollQuotaExhausted),
        Some(CancelKind::CostBudget) => Some(RuntimeOperationSource::CostBudgetExhausted),
        Some(
            CancelKind::User
            | CancelKind::FailFast
            | CancelKind::RaceLost
            | CancelKind::ParentCancelled
            | CancelKind::ResourceUnavailable
            | CancelKind::Shutdown
            | CancelKind::LinkedExit,
        ) => Some(RuntimeOperationSource::Cancelled(
            "capability context ended during metrics connection I/O".to_string(),
        )),
        None => None,
    };
    if root_source.is_some() {
        return root_source;
    }

    // A budget-aware timer can be the first observer of an exhausted budget,
    // before the Cx has materialized a root cancellation cause. Preserve the
    // finite cause from the content-free budget snapshot rather than
    // misreporting deadline, poll, or cost exhaustion as a generic I/O timeout.
    let budget = cx.budget_stats();
    if budget.deadline.at.is_some() && budget.deadline.remaining.is_none() {
        Some(RuntimeOperationSource::DeadlineExceeded)
    } else if budget.polls.remaining == Some(0) {
        Some(RuntimeOperationSource::PollQuotaExhausted)
    } else if budget.cost.remaining == Some(0) {
        Some(RuntimeOperationSource::CostBudgetExhausted)
    } else if error.is_some() {
        Some(RuntimeOperationSource::Cancelled(
            "capability context ended during metrics connection I/O".to_string(),
        ))
    } else {
        None
    }
}

/// Shared response-formatting and write body for the Cx-aware connection
/// handler. Keeping parsing/rendering below the timeout boundary makes the
/// read, collection, formatting, and response write one bounded exchange.
async fn handle_connection_impl(
    mut socket: TcpStream,
    prefix: &str,
    collector: Arc<dyn MetricsCollector>,
    buf: Vec<u8>,
) -> Result<()> {
    let request = String::from_utf8_lossy(&buf);
    let mut lines = request.lines();
    let first_line = lines.next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();

    match (method, path) {
        ("GET", "/metrics") => {
            let snapshot = collector.collect().await;
            let body = snapshot.render_prometheus(prefix);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await?;
        }
        _ => {
            let body = "not found";
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await?;
        }
    }

    // Explicitly shut down the write half to ensure the TCP connection is
    // terminated cleanly rather than relying on implicit drop.
    {
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }
    Ok(())
}

fn event_bus_snapshot(bus: &EventBus) -> EventBusSnapshot {
    let metrics = bus.metrics().snapshot();
    let stats = bus.stats();

    EventBusSnapshot {
        events_published: metrics.events_published,
        events_dropped_no_subscribers: metrics.events_dropped_no_subscribers,
        active_subscribers: metrics.active_subscribers,
        subscriber_lag_events: metrics.subscriber_lag_events,
        capacity: stats.capacity,
        delta_queued: stats.delta_queued,
        detection_queued: stats.detection_queued,
        signal_queued: stats.signal_queued,
        delta_subscribers: stats.delta_subscribers,
        detection_subscribers: stats.detection_subscribers,
        signal_subscribers: stats.signal_subscribers,
        delta_oldest_lag_ms: stats.delta_oldest_lag_ms,
        detection_oldest_lag_ms: stats.detection_oldest_lag_ms,
        signal_oldest_lag_ms: stats.signal_oldest_lag_ms,
    }
}

fn epoch_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn metric_name(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}_{name}")
    }
}

fn sanitize_prefix(prefix: &str) -> String {
    let mut sanitized = String::with_capacity(prefix.len());
    for ch in prefix.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    sanitized
}

fn is_localhost_bind(bind: &str) -> bool {
    if let Ok(addr) = bind.parse::<SocketAddr>() {
        return addr.ip().is_loopback();
    }

    // Accept common hostname-style binds like "localhost:9090".
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind).trim();
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

fn format_float(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        "0".to_string()
    }
}

fn push_counter(output: &mut String, name: String, help: &str, value: String) {
    push_metric(output, name, help, "counter", value);
}

fn push_gauge(output: &mut String, name: String, help: &str, value: String) {
    push_metric(output, name, help, "gauge", value);
}

fn push_metric(output: &mut String, name: String, help: &str, metric_type: &str, value: String) {
    use std::fmt::Write as _;

    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} {metric_type}");
    let _ = writeln!(output, "{name} {value}");
}

fn render_cost_attribution_estimates(
    output: &mut String,
    prefix: &str,
    estimates: &[CostAttributionEstimateSummary],
) {
    if estimates.is_empty() {
        return;
    }

    push_cost_attribution_gauge(
        output,
        prefix,
        "cost_attribution_proxy_estimated_cost_usd",
        "Estimated cost attributed by pane-activity proxy; not billing or attestation truth",
        estimates,
        |estimate| format_float(estimate.estimated_cost_usd),
    );
    push_cost_attribution_gauge(
        output,
        prefix,
        "cost_attribution_proxy_estimated_tokens",
        "Estimated tokens attributed by pane-activity proxy",
        estimates,
        |estimate| estimate.estimated_tokens.to_string(),
    );
    push_cost_attribution_gauge(
        output,
        prefix,
        "cost_attribution_proxy_bytes_captured",
        "Captured bytes used as the activity proxy for attribution",
        estimates,
        |estimate| estimate.bytes_captured.to_string(),
    );
    push_cost_attribution_gauge(
        output,
        prefix,
        "cost_attribution_proxy_active_seconds",
        "Active-state seconds used as the activity proxy for attribution",
        estimates,
        |estimate| format_float(estimate.active_seconds),
    );
    push_cost_attribution_gauge(
        output,
        prefix,
        "cost_attribution_proxy_detection_count",
        "Detection count used as the activity proxy for attribution",
        estimates,
        |estimate| estimate.detection_count.to_string(),
    );
    push_cost_attribution_gauge(
        output,
        prefix,
        "cost_attribution_proxy_pane_count",
        "Distinct panes contributing to this proxy attribution estimate",
        estimates,
        |estimate| estimate.pane_count.to_string(),
    );
    push_cost_attribution_gauge(
        output,
        prefix,
        "cost_attribution_proxy_record_count",
        "Proxy attribution samples folded into this estimate",
        estimates,
        |estimate| estimate.record_count.to_string(),
    );
}

fn push_cost_attribution_gauge<F>(
    output: &mut String,
    prefix: &str,
    metric_suffix: &str,
    help: &str,
    estimates: &[CostAttributionEstimateSummary],
    value: F,
) where
    F: Fn(&CostAttributionEstimateSummary) -> String,
{
    use std::fmt::Write as _;

    let name = metric_name(prefix, metric_suffix);
    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} gauge");
    for estimate in estimates {
        let labels = cost_attribution_labels(estimate);
        let _ = writeln!(output, "{name}{labels} {}", value(estimate));
    }
}

fn cost_attribution_labels(estimate: &CostAttributionEstimateSummary) -> String {
    let labels = [
        ("kind", estimate.kind.as_str()),
        ("attribution_id", estimate.attribution_id.as_str()),
        ("estimate_label", estimate.estimate_label.as_str()),
        ("methodology", estimate.methodology.as_str()),
        (
            "attestation_eligible",
            if estimate.attestation_eligible {
                "true"
            } else {
                "false"
            },
        ),
    ];
    format_prometheus_labels(&labels)
}

fn format_prometheus_labels(labels: &[(&str, &str)]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("{");
    for (index, (key, value)) in labels.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, r#"{key}="{}""#, escape_prometheus_label_value(value));
    }
    out.push('}');
    out
}

fn escape_prometheus_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str(r"\\"),
            '"' => escaped.push_str(r#"\""#),
            '\n' => escaped.push_str(r"\n"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn metrics_accept_error_classifier_stops_permanent_failures() {
        for kind in [
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::Unsupported,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::BrokenPipe,
        ] {
            let error = std::io::Error::new(kind, "permanent accept failure");
            assert!(metrics_accept_error_is_permanent(&error), "kind={kind:?}");
        }

        for kind in [
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::OutOfMemory,
            std::io::ErrorKind::Other,
        ] {
            let error = std::io::Error::new(kind, "retryable accept failure");
            assert!(
                !metrics_accept_error_is_permanent(&error),
                "kind={kind:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn metrics_accept_error_classifier_distinguishes_ebadf_from_emfile() {
        let invalid_descriptor = std::io::Error::from_raw_os_error(9);
        let descriptor_exhaustion = std::io::Error::from_raw_os_error(24);
        assert!(metrics_accept_error_is_permanent(&invalid_descriptor));
        assert!(!metrics_accept_error_is_permanent(&descriptor_exhaustion));

        #[cfg(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            all(
                any(target_os = "linux", target_os = "android"),
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        ))]
        {
            let not_a_socket = std::io::Error::from_raw_os_error(UNIX_ENOTSOCK_RAW);
            assert!(metrics_accept_error_is_permanent(&not_a_socket));
        }
    }

    #[cfg(windows)]
    #[test]
    fn metrics_accept_error_classifier_distinguishes_wsaenotsock_from_wsaemfile() {
        let invalid_socket = std::io::Error::from_raw_os_error(10_038);
        let descriptor_exhaustion = std::io::Error::from_raw_os_error(10_024);
        assert!(metrics_accept_error_is_permanent(&invalid_socket));
        assert!(!metrics_accept_error_is_permanent(&descriptor_exhaustion));
    }

    #[test]
    fn metrics_retry_backoff_is_bounded_and_resets_after_success() {
        let mut backoff = MetricsRetryBackoff::new();
        let mut previous_delay = Duration::ZERO;
        for expected_count in 1..=128 {
            let (count, delay) = backoff.record_failure();
            assert_eq!(count, expected_count);
            assert!(delay >= previous_delay);
            assert!(delay <= METRICS_ACCEPT_ERROR_MAX_BACKOFF);
            previous_delay = delay;
        }
        assert_eq!(previous_delay, METRICS_ACCEPT_ERROR_MAX_BACKOFF);

        backoff.reset();
        assert_eq!(
            backoff.record_failure(),
            (1, METRICS_ACCEPT_ERROR_INITIAL_BACKOFF),
        );
    }

    #[test]
    fn metrics_connection_admission_is_bounded_and_releases_capacity() {
        let permits = Arc::new(Semaphore::new(METRICS_MAX_CONCURRENT_CONNECTIONS));
        let mut held = Vec::with_capacity(METRICS_MAX_CONCURRENT_CONNECTIONS);
        for _ in 0..METRICS_MAX_CONCURRENT_CONNECTIONS {
            held.push(
                Arc::clone(&permits)
                    .try_acquire_owned()
                    .expect("configured metrics connection slot must be available"),
            );
        }
        assert_eq!(permits.available_permits(), 0);
        assert!(matches!(
            Arc::clone(&permits).try_acquire_owned(),
            Err(TryAcquireError::NoPermits)
        ));

        drop(held.pop());
        assert!(Arc::clone(&permits).try_acquire_owned().is_ok());
        assert!(
            !METRICS_CONNECTION_IO_TIMEOUT.is_zero(),
            "every admitted metrics connection must carry a finite I/O deadline"
        );
    }

    #[test]
    fn metrics_spawn_capacity_is_transient_but_runtime_loss_is_terminal() {
        let cx = crate::cx::for_testing();
        let at_capacity = SpawnError::RegionAtCapacity {
            region: cx.region_id(),
            limit: METRICS_MAX_CONCURRENT_CONNECTIONS,
            live: METRICS_MAX_CONCURRENT_CONNECTIONS,
        };
        assert!(metrics_spawn_admission_is_transient(&at_capacity));
        assert!(!metrics_spawn_admission_is_transient(
            &SpawnError::RuntimeUnavailable
        ));
    }

    #[test]
    fn metrics_connection_spawn_admission_failure_drops_owned_resources() {
        struct DropProbe(Arc<AtomicBool>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let previous_handle = crate::runtime_async::current_runtime_handle();
        crate::runtime_async::clear_runtime_handle();

        let permits = Arc::new(Semaphore::new(1));
        let connection_permit = Arc::clone(&permits)
            .try_acquire_owned()
            .expect("single metrics connection slot must be available");
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe(Arc::clone(&dropped));
        let cx = crate::cx::for_testing();
        let spawn_result = crate::runtime_async::task::try_spawn_with_cx(
            &cx,
            move |_connection_cx| async move {
                let _connection_permit = connection_permit;
                let _probe = probe;
                std::future::pending::<()>().await;
            },
        );
        let rejected = matches!(
            spawn_result,
            Err(crate::runtime_async::task::SpawnError::RuntimeUnavailable)
        );

        if let Some(previous_handle) = previous_handle {
            crate::runtime_async::install_runtime_handle(previous_handle);
        }

        assert!(rejected, "missing runtime must reject connection admission");
        assert_eq!(
            permits.available_permits(),
            1,
            "rejected spawn must drop and release its owned connection permit"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "rejected spawn must drop every resource owned by its task closure"
        );
    }

    #[test]
    fn metrics_connection_context_errors_preserve_finite_cause_class() {
        let live_cx = crate::cx::for_testing();
        assert!(matches!(
            metrics_context_error(&live_cx, "metrics test", None),
            crate::Error::RuntimeOperation {
                source: crate::error::RuntimeOperationSource::ContextFailure,
                ..
            }
        ));

        let cancelled_cx = crate::cx::for_testing();
        cancelled_cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("content that must not cross the metrics error boundary"),
        );
        assert!(matches!(
            metrics_context_error(&cancelled_cx, "metrics test", None),
            crate::Error::RuntimeOperation {
                source: crate::error::RuntimeOperationSource::Cancelled(_),
                ..
            }
        ));
    }

    #[test]
    fn metrics_connection_context_errors_preserve_checkpoint_error_kind() {
        use crate::error::RuntimeOperationSource;
        use crate::runtime_async::{ContextError, ContextErrorKind};

        let cx = crate::cx::for_testing();
        let cases = [
            (
                ContextErrorKind::DeadlineExceeded,
                RuntimeOperationSource::DeadlineExceeded,
            ),
            (
                ContextErrorKind::PollQuotaExhausted,
                RuntimeOperationSource::PollQuotaExhausted,
            ),
            (
                ContextErrorKind::CostQuotaExhausted,
                RuntimeOperationSource::CostBudgetExhausted,
            ),
        ];

        for (kind, expected) in cases {
            let error = ContextError::new(kind)
                .with_message("secret checkpoint detail must not cross the metrics boundary");
            let actual = metrics_context_error(&cx, "metrics test", Some(&error));
            assert!(
                matches!(
                    &actual,
                    crate::Error::RuntimeOperation { source, .. } if *source == expected
                ),
                "checkpoint kind {kind:?} must retain its finite structural class"
            );
            assert!(
                !actual.to_string().contains("secret checkpoint detail"),
                "checkpoint detail must remain content-free"
            );
        }
    }

    #[test]
    fn metrics_connection_context_errors_detect_unmaterialized_budget_exhaustion() {
        use crate::error::RuntimeOperationSource;

        let cases = [
            (
                crate::cx::Budget::new()
                    .with_deadline(crate::runtime_async::RuntimeTime::ZERO),
                RuntimeOperationSource::DeadlineExceeded,
            ),
            (
                crate::cx::Budget::new().with_poll_quota(0),
                RuntimeOperationSource::PollQuotaExhausted,
            ),
            (
                crate::cx::Budget::new().with_cost_quota(0),
                RuntimeOperationSource::CostBudgetExhausted,
            ),
        ];

        for (budget, expected) in cases {
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            assert!(
                cx.root_cancel_cause().is_none(),
                "test precondition: budget exhaustion must not yet be materialized as cancellation"
            );
            let actual = metrics_context_error(&cx, "metrics test", None);
            assert!(
                matches!(
                    actual,
                    crate::Error::RuntimeOperation { source, .. } if source == expected
                ),
                "content-free budget stats must preserve {expected:?} before cancellation materializes"
            );
        }
    }

    #[test]
    fn metrics_connection_drain_truth_gives_settlement_precedence() {
        assert_eq!(
            classify_metrics_connection_task_drain(true, None, JoinSetSettlement::Settled),
            MetricsConnectionTaskDrainOutcome::Settled,
        );
        assert_eq!(
            classify_metrics_connection_task_drain(
                true,
                None,
                JoinSetSettlement::Incomplete {
                    active_tasks: 2,
                    unacknowledged_tasks: 1,
                },
            ),
            MetricsConnectionTaskDrainOutcome::TimedOut {
                active_tasks: 2,
                unacknowledged_tasks: 1,
                first_non_benign_failure: None,
            },
        );
    }

    #[test]
    fn metrics_connection_drain_truth_preserves_non_benign_failure() {
        assert_eq!(
            classify_metrics_connection_task_drain(
                false,
                Some(JoinErrorKind::TaskFailed),
                JoinSetSettlement::Settled,
            ),
            MetricsConnectionTaskDrainOutcome::SettledWithFailure {
                first_non_benign_failure: JoinErrorKind::TaskFailed,
            },
        );
        assert_eq!(
            classify_metrics_connection_task_drain(
                true,
                Some(JoinErrorKind::WakerRegistrationFailed),
                JoinSetSettlement::Incomplete {
                    active_tasks: 2,
                    unacknowledged_tasks: 1,
                },
            ),
            MetricsConnectionTaskDrainOutcome::TimedOut {
                active_tasks: 2,
                unacknowledged_tasks: 1,
                first_non_benign_failure: Some(JoinErrorKind::WakerRegistrationFailed),
            },
        );
    }

    #[test]
    fn metrics_connection_join_failure_classifier_keeps_expected_shutdown_benign() {
        assert!(metrics_connection_join_failure_is_benign(
            JoinErrorKind::Aborted
        ));
        assert!(metrics_connection_join_failure_is_benign(
            JoinErrorKind::ContextCancelled
        ));
        for failure in [
            JoinErrorKind::DeadlineExceeded,
            JoinErrorKind::PollQuotaExhausted,
            JoinErrorKind::CostBudgetExhausted,
            JoinErrorKind::ContextFailure,
            JoinErrorKind::TaskFailed,
            JoinErrorKind::WakerRegistrationFailed,
        ] {
            assert!(
                !metrics_connection_join_failure_is_benign(failure),
                "failure={failure:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // MetricsSnapshot defaults
    // -----------------------------------------------------------------------

    #[test]
    fn metrics_snapshot_default_is_zeroed() {
        let snap = MetricsSnapshot::default();
        assert!(snap.uptime_seconds.abs() < f64::EPSILON);
        assert_eq!(snap.observed_panes, 0);
        assert_eq!(snap.capture_queue_depth, 0);
        assert_eq!(snap.capture_queue_capacity, 0);
        assert_eq!(snap.write_queue_depth, 0);
        assert_eq!(snap.segments_persisted, 0);
        assert_eq!(snap.events_recorded, 0);
        assert!(snap.ingest_lag_avg_ms.abs() < f64::EPSILON);
        assert_eq!(snap.ingest_lag_max_ms, 0);
        assert_eq!(snap.ingest_lag_sum_ms, 0);
        assert_eq!(snap.ingest_lag_count, 0);
        assert!(snap.db_last_write_age_ms.is_none());
        assert_eq!(snap.native_output_input_events, 0);
        assert_eq!(snap.native_output_batches_emitted, 0);
        assert!(snap.event_bus.is_none());
        assert!(snap.cost_attribution_estimates.is_empty());
    }

    #[test]
    fn event_bus_snapshot_default_is_zeroed() {
        let snap = EventBusSnapshot::default();
        assert_eq!(snap.events_published, 0);
        assert_eq!(snap.events_dropped_no_subscribers, 0);
        assert_eq!(snap.active_subscribers, 0);
        assert_eq!(snap.capacity, 0);
        assert!(snap.delta_oldest_lag_ms.is_none());
        assert!(snap.detection_oldest_lag_ms.is_none());
        assert!(snap.signal_oldest_lag_ms.is_none());
    }

    // -----------------------------------------------------------------------
    // Prometheus rendering
    // -----------------------------------------------------------------------

    #[test]
    fn render_prometheus_empty_prefix() {
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus("");
        // Without prefix, metric names should not start with _.
        assert!(rendered.contains("uptime_seconds"));
        assert!(rendered.contains("observed_panes"));
        assert!(!rendered.contains("__uptime_seconds"));
    }

    #[test]
    fn render_prometheus_with_prefix() {
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_uptime_seconds"));
        assert!(rendered.contains("ft_segments_persisted_total"));
        assert!(rendered.contains("ft_ingest_lag_avg_ms"));
    }

    #[test]
    fn render_prometheus_sanitizes_prefix() {
        let snap = MetricsSnapshot::default();
        // Special chars in prefix should be replaced with _.
        let rendered = snap.render_prometheus("my-app.v2");
        assert!(rendered.contains("my_app_v2_uptime_seconds"));
    }

    #[test]
    fn render_prometheus_db_write_age_none_renders_minus_one() {
        let snap = MetricsSnapshot {
            db_last_write_age_ms: None,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("t");
        assert!(rendered.contains("t_db_last_write_age_ms -1"));
    }

    #[test]
    fn render_prometheus_db_write_age_some_renders_value() {
        let snap = MetricsSnapshot {
            db_last_write_age_ms: Some(42),
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("t");
        assert!(rendered.contains("t_db_last_write_age_ms 42"));
    }

    #[test]
    fn render_prometheus_with_nonzero_values() {
        let snap = MetricsSnapshot {
            uptime_seconds: 123.456,
            observed_panes: 5,
            capture_queue_depth: 10,
            capture_queue_capacity: 100,
            segments_persisted: 999,
            events_recorded: 1234,
            ingest_lag_avg_ms: 2.5,
            ingest_lag_max_ms: 8,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_uptime_seconds 123.456"));
        assert!(rendered.contains("ft_observed_panes 5"));
        assert!(rendered.contains("ft_capture_queue_depth 10"));
        assert!(rendered.contains("ft_segments_persisted_total 999"));
        assert!(rendered.contains("ft_events_recorded_total 1234"));
    }

    #[test]
    fn render_prometheus_includes_help_and_type_lines() {
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus("x");
        // Each metric should have HELP and TYPE lines.
        assert!(rendered.contains("# HELP x_uptime_seconds"));
        assert!(rendered.contains("# TYPE x_uptime_seconds gauge"));
        assert!(rendered.contains("# HELP x_segments_persisted_total"));
        assert!(rendered.contains("# TYPE x_segments_persisted_total counter"));
    }

    #[test]
    fn render_prometheus_includes_event_bus_when_present() {
        let snap = MetricsSnapshot {
            event_bus: Some(EventBusSnapshot {
                events_published: 100,
                events_dropped_no_subscribers: 5,
                active_subscribers: 3,
                subscriber_lag_events: 2,
                capacity: 1024,
                delta_queued: 10,
                detection_queued: 20,
                signal_queued: 30,
                delta_subscribers: 1,
                detection_subscribers: 1,
                signal_subscribers: 1,
                delta_oldest_lag_ms: Some(50),
                detection_oldest_lag_ms: None,
                signal_oldest_lag_ms: Some(75),
            }),
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_event_bus_events_published_total 100"));
        assert!(rendered.contains("ft_event_bus_events_dropped_total 5"));
        assert!(rendered.contains("ft_event_bus_active_subscribers 3"));
        assert!(rendered.contains("ft_event_bus_capacity 1024"));
        assert!(rendered.contains("ft_event_bus_delta_queued 10"));
        assert!(rendered.contains("ft_event_bus_delta_oldest_lag_ms 50"));
        // None → -1
        assert!(rendered.contains("ft_event_bus_detection_oldest_lag_ms -1"));
        assert!(rendered.contains("ft_event_bus_signal_oldest_lag_ms 75"));
    }

    #[test]
    fn render_prometheus_excludes_event_bus_when_absent() {
        let snap = MetricsSnapshot {
            event_bus: None,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        assert!(!rendered.contains("event_bus_events_published"));
        assert!(!rendered.contains("event_bus_capacity"));
    }

    #[test]
    fn render_prometheus_native_output_metrics() {
        let snap = MetricsSnapshot {
            native_output_input_events: 500,
            native_output_batches_emitted: 100,
            native_output_input_bytes: 50_000,
            native_output_emitted_bytes: 48_000,
            native_output_max_batch_events: 10,
            native_output_max_batch_bytes: 8192,
            native_output_coalesce_ratio: 5.0,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_native_output_input_events_total 500"));
        assert!(rendered.contains("ft_native_output_batches_emitted_total 100"));
        assert!(rendered.contains("ft_native_output_coalesce_ratio 5"));
    }

    #[test]
    fn render_prometheus_cost_attribution_estimates_are_labeled_proxy_gauges() {
        let snap = MetricsSnapshot {
            cost_attribution_estimates: vec![CostAttributionEstimateSummary {
                kind: crate::cost_tracker::CostAttributionKind::Bead,
                attribution_id: "ft-7h5da.8.5".to_string(),
                estimate_label: crate::cost_tracker::COST_ATTRIBUTION_ESTIMATE_LABEL.to_string(),
                methodology: crate::cost_tracker::COST_ATTRIBUTION_METHODOLOGY.to_string(),
                attestation_eligible: false,
                pane_count: 2,
                bytes_captured: 3_072,
                active_seconds: 20.0,
                detection_count: 5,
                estimated_tokens: 1_200,
                estimated_cost_usd: 0.06,
                record_count: 2,
                last_updated_ms: 200,
            }],
            ..MetricsSnapshot::default()
        };

        let rendered = snap.render_prometheus("ft");
        let labels = r#"{kind="bead",attribution_id="ft-7h5da.8.5",estimate_label="proxy_estimate",methodology="pane_activity_proxy_non_attested",attestation_eligible="false"}"#;
        assert!(rendered.contains("# TYPE ft_cost_attribution_proxy_estimated_cost_usd gauge"));
        assert!(rendered.contains(&format!(
            "ft_cost_attribution_proxy_estimated_cost_usd{labels} 0.06"
        )));
        assert!(rendered.contains(&format!(
            "ft_cost_attribution_proxy_estimated_tokens{labels} 1200"
        )));
        assert!(rendered.contains(&format!(
            "ft_cost_attribution_proxy_bytes_captured{labels} 3072"
        )));
        assert!(rendered.contains(&format!(
            "ft_cost_attribution_proxy_active_seconds{labels} 20"
        )));
        assert!(rendered.contains(&format!(
            "ft_cost_attribution_proxy_detection_count{labels} 5"
        )));
        assert!(rendered.contains(&format!("ft_cost_attribution_proxy_pane_count{labels} 2")));
        assert!(!rendered.contains(r#"attestation_eligible="true""#));
    }

    // -----------------------------------------------------------------------
    // format_float edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn format_float_nan_renders_zero() {
        assert_eq!(format_float(f64::NAN), "0");
    }

    #[test]
    fn format_float_infinity_renders_zero() {
        assert_eq!(format_float(f64::INFINITY), "0");
        assert_eq!(format_float(f64::NEG_INFINITY), "0");
    }

    #[test]
    fn format_float_normal_renders_value() {
        assert_eq!(format_float(3.15), "3.15");
        assert_eq!(format_float(0.0), "0");
    }

    // -----------------------------------------------------------------------
    // sanitize_prefix
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_prefix_alphanumeric_passes_through() {
        assert_eq!(sanitize_prefix("abc_123"), "abc_123");
    }

    #[test]
    fn sanitize_prefix_special_chars_replaced() {
        assert_eq!(sanitize_prefix("my-app.v2"), "my_app_v2");
        assert_eq!(sanitize_prefix("a/b\\c"), "a_b_c");
    }

    #[test]
    fn sanitize_prefix_empty_stays_empty() {
        assert_eq!(sanitize_prefix(""), "");
    }

    // -----------------------------------------------------------------------
    // is_localhost_bind
    // -----------------------------------------------------------------------

    #[test]
    fn localhost_detection_comprehensive() {
        // Should be localhost.
        assert!(is_localhost_bind("127.0.0.1:9090"));
        assert!(is_localhost_bind("localhost:9090"));
        assert!(is_localhost_bind("[::1]:9090"));
        // When parsed as SocketAddr, ::1 is loopback.
        assert!(is_localhost_bind("[::1]:8080"));

        // Should NOT be localhost.
        assert!(!is_localhost_bind("0.0.0.0:9090"));
        assert!(!is_localhost_bind("192.168.1.1:9090"));
        assert!(!is_localhost_bind("example.com:9090"));
    }

    // -----------------------------------------------------------------------
    // metric_name
    // -----------------------------------------------------------------------

    #[test]
    fn metric_name_with_prefix() {
        assert_eq!(metric_name("ft", "uptime"), "ft_uptime");
    }

    #[test]
    fn metric_name_without_prefix() {
        assert_eq!(metric_name("", "uptime"), "uptime");
    }

    // -----------------------------------------------------------------------
    // FixedMetricsCollector
    // -----------------------------------------------------------------------

    #[test]
    fn fixed_metrics_collector_is_clone() {
        let snap = MetricsSnapshot::default();
        let collector = FixedMetricsCollector::new(snap);
        let _cloned = collector.clone();
    }

    // -----------------------------------------------------------------------
    // Expanded pure unit tests (wa-1u90p.7.1)
    // -----------------------------------------------------------------------

    #[test]
    fn metrics_snapshot_clone() {
        let snap = MetricsSnapshot {
            uptime_seconds: 42.5,
            observed_panes: 3,
            capture_queue_depth: 10,
            segments_persisted: 99,
            ..MetricsSnapshot::default()
        };
        let c = snap.clone();
        assert!((c.uptime_seconds - 42.5).abs() < f64::EPSILON);
        assert_eq!(c.observed_panes, 3);
        assert_eq!(c.segments_persisted, 99);
    }

    #[test]
    fn metrics_snapshot_debug() {
        let snap = MetricsSnapshot::default();
        let dbg = format!("{:?}", snap);
        assert!(dbg.contains("MetricsSnapshot"));
        assert!(dbg.contains("uptime_seconds"));
    }

    #[test]
    fn event_bus_snapshot_clone() {
        let snap = EventBusSnapshot {
            events_published: 100,
            active_subscribers: 3,
            capacity: 1024,
            ..EventBusSnapshot::default()
        };
        let c = snap.clone();
        assert_eq!(c.events_published, 100);
        assert_eq!(c.active_subscribers, 3);
        assert_eq!(c.capacity, 1024);
    }

    #[test]
    fn event_bus_snapshot_debug() {
        let snap = EventBusSnapshot::default();
        let dbg = format!("{:?}", snap);
        assert!(dbg.contains("EventBusSnapshot"));
        assert!(dbg.contains("events_published"));
    }

    #[test]
    fn render_prometheus_output_is_nonempty() {
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus("ft");
        assert!(!rendered.is_empty());
    }

    #[test]
    fn render_prometheus_counters_are_counters() {
        let snap = MetricsSnapshot {
            segments_persisted: 10,
            events_recorded: 20,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        // Counter metrics should have _total suffix and "counter" type
        assert!(rendered.contains("# TYPE ft_segments_persisted_total counter"));
        assert!(rendered.contains("# TYPE ft_events_recorded_total counter"));
    }

    #[test]
    fn render_prometheus_gauges_are_gauges() {
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("# TYPE ft_uptime_seconds gauge"));
        assert!(rendered.contains("# TYPE ft_observed_panes gauge"));
        assert!(rendered.contains("# TYPE ft_capture_queue_depth gauge"));
    }

    #[test]
    fn render_prometheus_ingest_lag_histogram_fields() {
        let snap = MetricsSnapshot {
            ingest_lag_avg_ms: 5.0,
            ingest_lag_max_ms: 20,
            ingest_lag_sum_ms: 100,
            ingest_lag_count: 20,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_ingest_lag_avg_ms"));
        assert!(rendered.contains("ft_ingest_lag_max_ms"));
        assert!(rendered.contains("ft_ingest_lag_ms_sum"));
        assert!(rendered.contains("ft_ingest_lag_ms_count"));
    }

    #[test]
    fn render_prometheus_write_queue_depth() {
        let snap = MetricsSnapshot {
            write_queue_depth: 42,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_write_queue_depth 42"));
    }

    #[test]
    fn format_float_negative_renders_value() {
        assert_eq!(format_float(-1.5), "-1.5");
    }

    #[test]
    fn format_float_very_small_renders_value() {
        let result = format_float(0.001);
        assert!(result.contains("0.001"));
    }

    #[test]
    fn format_float_integer_no_trailing_zeros() {
        // 42.0 should render as "42" (no trailing .0)
        assert_eq!(format_float(42.0), "42");
    }

    #[test]
    fn sanitize_prefix_unicode_replaced() {
        let result = sanitize_prefix("café");
        // Non-ASCII chars should be replaced with _
        assert!(result.starts_with("caf"));
    }

    #[test]
    fn sanitize_prefix_underscores_preserved() {
        assert_eq!(sanitize_prefix("a_b_c"), "a_b_c");
    }

    #[test]
    fn is_localhost_bind_ipv6_loopback() {
        assert!(is_localhost_bind("[::1]:8080"));
    }

    #[test]
    fn is_localhost_bind_unparseable() {
        // Unparseable addresses should not be considered localhost
        assert!(!is_localhost_bind("not-an-address"));
    }

    #[test]
    fn metric_name_double_underscore_avoided() {
        // When prefix is present, result should be prefix_name, not prefix__name
        let name = metric_name("ft", "test");
        assert!(!name.contains("__"));
    }

    #[test]
    fn render_prometheus_event_bus_all_lag_none() {
        let snap = MetricsSnapshot {
            event_bus: Some(EventBusSnapshot {
                delta_oldest_lag_ms: None,
                detection_oldest_lag_ms: None,
                signal_oldest_lag_ms: None,
                ..EventBusSnapshot::default()
            }),
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        // All lag metrics should render as -1
        assert!(rendered.contains("ft_event_bus_delta_oldest_lag_ms -1"));
        assert!(rendered.contains("ft_event_bus_detection_oldest_lag_ms -1"));
        assert!(rendered.contains("ft_event_bus_signal_oldest_lag_ms -1"));
    }

    #[test]
    fn render_prometheus_native_output_zeros() {
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_native_output_input_events_total 0"));
        assert!(rendered.contains("ft_native_output_batches_emitted_total 0"));
    }

    #[test]
    fn render_prometheus_coalesce_ratio_float() {
        let snap = MetricsSnapshot {
            native_output_coalesce_ratio: 3.15,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus("ft");
        assert!(rendered.contains("ft_native_output_coalesce_ratio 3.15"));
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;
    use crate::runtime_async::CompatRuntime;
    use crate::runtime_async::io::{AsyncReadExt, AsyncWriteExt};

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("create runtime");
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

    const METRICS_TEST_REQUEST: &[u8] = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";

    async fn read_metrics_test_response(stream: &mut TcpStream) -> Vec<u8> {
        let mut response = Vec::new();
        crate::runtime_async::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("metrics test response must arrive before the safety timeout")
            .expect("metrics test response read must succeed");
        response
    }

    async fn stop_metrics_test_server(
        shutdown_flag: &Arc<AtomicBool>,
        handle: MetricsServerHandle,
    ) {
        shutdown_flag.store(true, Ordering::SeqCst);
        crate::runtime_async::timeout(Duration::from_secs(5), handle.wait())
            .await
            .expect("metrics test server must settle before the safety timeout");
    }

    #[test]
    fn metrics_connection_drain_trusted_polls_registration_failure_to_settlement() {
        run_async_test(async {
            let mut connection_tasks = JoinSet::new();
            connection_tasks.spawn(std::future::pending::<()>());
            connection_tasks.force_join_registration_failure_for_test();

            assert_eq!(
                settle_metrics_connection_tasks(&mut connection_tasks).await,
                MetricsConnectionTaskDrainOutcome::SettledWithFailure {
                    first_non_benign_failure: JoinErrorKind::WakerRegistrationFailed,
                },
                "forced caller-waker registration failure must remain visible after trusted settlement",
            );
            assert_eq!(connection_tasks.settlement(), JoinSetSettlement::Settled);
        });
    }

    #[test]
    fn metrics_connection_drain_treats_expected_abort_as_clean_settlement() {
        run_async_test(async {
            let mut connection_tasks = JoinSet::new();
            connection_tasks.spawn(std::future::pending::<()>());

            assert_eq!(
                settle_metrics_connection_tasks(&mut connection_tasks).await,
                MetricsConnectionTaskDrainOutcome::Settled,
                "an acknowledged shutdown abort is not a connection-task failure",
            );
            assert_eq!(connection_tasks.settlement(), JoinSetSettlement::Settled);
        });
    }

    #[test]
    fn render_prometheus_includes_prefix() {
        run_async_test(async {
            let snapshot = MetricsSnapshot {
                uptime_seconds: 1.0,
                observed_panes: 2,
                capture_queue_depth: 3,
                capture_queue_capacity: 10,
                write_queue_depth: 4,
                segments_persisted: 5,
                events_recorded: 6,
                ingest_lag_avg_ms: 1.5,
                ingest_lag_max_ms: 4,
                ingest_lag_sum_ms: 9,
                ingest_lag_count: 3,
                db_last_write_age_ms: Some(100),
                native_output_input_events: 0,
                native_output_batches_emitted: 0,
                native_output_input_bytes: 0,
                native_output_emitted_bytes: 0,
                native_output_max_batch_events: 0,
                native_output_max_batch_bytes: 0,
                native_output_coalesce_ratio: 0.0,
                event_bus: None,
                cost_attribution_estimates: Vec::new(),
            };

            let rendered = snapshot.render_prometheus("wa");
            assert!(rendered.contains("wa_observed_panes"));
            assert!(rendered.contains("wa_segments_persisted_total"));
            assert!(rendered.contains("wa_ingest_lag_ms_count"));
        });
    }

    #[test]
    fn metrics_server_serves_metrics() {
        run_async_test(async {
            let snapshot = MetricsSnapshot {
                uptime_seconds: 2.0,
                observed_panes: 1,
                capture_queue_depth: 0,
                capture_queue_capacity: 1,
                write_queue_depth: 0,
                segments_persisted: 7,
                events_recorded: 8,
                ingest_lag_avg_ms: 0.0,
                ingest_lag_max_ms: 0,
                ingest_lag_sum_ms: 0,
                ingest_lag_count: 0,
                db_last_write_age_ms: None,
                native_output_input_events: 0,
                native_output_batches_emitted: 0,
                native_output_input_bytes: 0,
                native_output_emitted_bytes: 0,
                native_output_max_batch_events: 0,
                native_output_max_batch_bytes: 0,
                native_output_coalesce_ratio: 0.0,
                event_bus: None,
                cost_attribution_estimates: Vec::new(),
            };

            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(snapshot));
            let server = MetricsServer::new("127.0.0.1:0", "wa", collector, shutdown_flag.clone());
            let handle = server.start().await.expect("metrics server start");

            let mut stream = TcpStream::connect(handle.local_addr())
                .await
                .expect("connect metrics");
            stream
                .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .expect("send request");

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read response");
            let response = String::from_utf8_lossy(&buf);
            assert!(response.contains("200 OK"));
            assert!(response.contains("wa_segments_persisted_total"));

            shutdown_flag.store(true, Ordering::SeqCst);
            handle.wait().await;
        });
    }

    #[test]
    fn localhost_bind_detection() {
        assert!(is_localhost_bind("127.0.0.1:9090"));
        assert!(is_localhost_bind("localhost:9090"));
        assert!(is_localhost_bind("[::1]:9090"));
        assert!(!is_localhost_bind("0.0.0.0:9090"));
    }

    /// ft-xbnl0.2.3 Cx-first: `start_with_cx` with a pre-cancelled Cx
    /// must refuse to bind the TCP listener and surface an
    /// `Error::RuntimeOperation` describing the cancellation. An operator who
    /// has already abandoned the server should not leave a socket in
    /// LISTEN state.
    #[test]
    fn metrics_server_start_with_cx_pre_cancelled_refuses_to_bind() {
        run_async_test(async {
            let snapshot = MetricsSnapshot::default();
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(snapshot));
            let server = MetricsServer::new("127.0.0.1:0", "wa", collector, shutdown_flag.clone());

            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("ft-xbnl0.2.3 metrics precancel"),
            );

            let result = server.start_with_cx(&cx).await;
            assert!(result.is_err(), "pre-cancelled start_with_cx should fail");
            let Err(err) = result else {
                return;
            };

            let msg = err.to_string();
            assert!(
                msg.contains("cancelled") || msg.contains("abort"),
                "pre-cancelled start_with_cx must surface a cancellation-shaped error, got: {msg}"
            );
            assert!(
                !shutdown_flag.load(Ordering::SeqCst),
                "Cx-cancelled start must not touch the shutdown flag"
            );
        });
    }

    /// ft-xbnl0.2.3 Cx-first: happy path — `start_with_cx` with a live
    /// Cx spawns the accept loop, serves a request, and shuts down
    /// cleanly via the shutdown flag. Pins no-regression on the normal
    /// path.
    #[test]
    fn metrics_server_start_with_cx_happy_path_serves_request() {
        run_async_test(async {
            let snapshot = MetricsSnapshot {
                observed_panes: 42,
                ..MetricsSnapshot::default()
            };
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(snapshot));
            let server = MetricsServer::new("127.0.0.1:0", "wa", collector, shutdown_flag.clone());
            let cx = crate::cx::for_request();

            let handle = server
                .start_with_cx(&cx)
                .await
                .expect("happy-path start_with_cx");

            let mut stream = TcpStream::connect(handle.local_addr())
                .await
                .expect("connect metrics");
            stream
                .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .expect("send request");

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read response");
            let response = String::from_utf8_lossy(&buf);
            assert!(response.contains("200 OK"));
            assert!(response.contains("wa_observed_panes"));

            shutdown_flag.store(true, Ordering::SeqCst);
            handle.wait().await;
        });
    }

    #[test]
    fn metrics_server_caps_stalled_connections_and_admits_waiter_after_release() {
        run_async_test(async {
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
            let probe = Arc::new(MetricsConnectionTestProbe::new());
            let server = MetricsServer::new(
                "127.0.0.1:0",
                "wa",
                collector,
                Arc::clone(&shutdown_flag),
            )
            .with_connection_test_probe(Arc::clone(&probe))
            .with_connection_io_timeout_for_test(Duration::from_secs(30));
            let handle = server.start().await.expect("start bounded metrics server");

            let mut stalled_clients = Vec::with_capacity(METRICS_MAX_CONCURRENT_CONNECTIONS);
            for _ in 0..METRICS_MAX_CONCURRENT_CONNECTIONS {
                stalled_clients.push(
                    TcpStream::connect(handle.local_addr())
                        .await
                        .expect("connect stalled metrics client"),
                );
            }
            probe
                .wait_until("all bounded metrics tasks to start", || {
                    probe.active_tasks() == METRICS_MAX_CONCURRENT_CONNECTIONS
                })
                .await;
            probe
                .wait_until("metrics listener to enter capacity wait", || {
                    probe.capacity_waits() > 0
                })
                .await;

            let mut waiting_client = TcpStream::connect(handle.local_addr())
                .await
                .expect("connect waiting metrics client");
            waiting_client
                .write_all(METRICS_TEST_REQUEST)
                .await
                .expect("send waiting metrics request");
            assert_eq!(
                probe.total_admitted_tasks(),
                METRICS_MAX_CONCURRENT_CONNECTIONS,
                "the waiting client must remain in the kernel backlog while all permits are held"
            );

            let mut released_client = stalled_clients.swap_remove(0);
            released_client
                .write_all(METRICS_TEST_REQUEST)
                .await
                .expect("release one stalled metrics client");
            let released_response = read_metrics_test_response(&mut released_client).await;
            assert!(String::from_utf8_lossy(&released_response).contains("200 OK"));

            probe
                .wait_until("waiting metrics client admission after permit release", || {
                    probe.total_admitted_tasks() == METRICS_MAX_CONCURRENT_CONNECTIONS + 1
                })
                .await;
            let waiting_response = read_metrics_test_response(&mut waiting_client).await;
            assert!(String::from_utf8_lossy(&waiting_response).contains("200 OK"));

            stop_metrics_test_server(&shutdown_flag, handle).await;
            assert_eq!(
                probe.active_tasks(),
                0,
                "server shutdown must settle every remaining stalled connection task"
            );
        });
    }

    #[test]
    fn metrics_server_shutdown_wakes_capacity_wait_and_settles_stalled_tasks() {
        run_async_test(async {
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
            let probe = Arc::new(MetricsConnectionTestProbe::new());
            let server = MetricsServer::new(
                "127.0.0.1:0",
                "wa",
                collector,
                Arc::clone(&shutdown_flag),
            )
            .with_connection_test_probe(Arc::clone(&probe))
            .with_connection_io_timeout_for_test(Duration::from_secs(30));
            let handle = server.start().await.expect("start bounded metrics server");

            let mut stalled_clients = Vec::with_capacity(METRICS_MAX_CONCURRENT_CONNECTIONS);
            for _ in 0..METRICS_MAX_CONCURRENT_CONNECTIONS {
                stalled_clients.push(
                    TcpStream::connect(handle.local_addr())
                        .await
                        .expect("connect stalled metrics client"),
                );
            }
            probe
                .wait_until("all shutdown-test metrics tasks to start", || {
                    probe.active_tasks() == METRICS_MAX_CONCURRENT_CONNECTIONS
                })
                .await;
            probe
                .wait_until("shutdown-test listener to enter capacity wait", || {
                    probe.capacity_waits() > 0
                })
                .await;

            stop_metrics_test_server(&shutdown_flag, handle).await;
            assert_eq!(
                probe.active_tasks(),
                0,
                "shutdown notification must wake the capacity wait and settle all handlers"
            );
        });
    }

    #[test]
    fn metrics_server_drops_silent_connection_after_real_io_timeout() {
        run_async_test(async {
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
            let probe = Arc::new(MetricsConnectionTestProbe::new());
            let server = MetricsServer::new(
                "127.0.0.1:0",
                "wa",
                collector,
                Arc::clone(&shutdown_flag),
            )
            .with_connection_test_probe(Arc::clone(&probe))
            .with_connection_io_timeout_for_test(Duration::from_millis(25));
            let handle = server.start().await.expect("start timeout metrics server");

            let mut silent_client = TcpStream::connect(handle.local_addr())
                .await
                .expect("connect silent metrics client");
            probe
                .wait_until("silent metrics connection task to start", || {
                    probe.active_tasks() == 1
                })
                .await;
            probe
                .wait_until("silent metrics connection I/O timeout", || {
                    probe.active_tasks() == 0
                })
                .await;

            let response = read_metrics_test_response(&mut silent_client).await;
            assert!(
                response.is_empty(),
                "a client that sends no request must receive no bytes before the I/O timeout closes it"
            );
            assert_eq!(probe.total_admitted_tasks(), 1);

            stop_metrics_test_server(&shutdown_flag, handle).await;
        });
    }

    /// ft-xbnl0.2.4 tick 322: Mid-flight cx-cancel contract for the
    /// metrics server accept loop.
    ///
    /// `start_with_cx` clones the `Cx` into the spawned accept-loop
    /// task. Cancelling that cx *after* the server has started must
    /// cause the loop to exit without requiring the shutdown flag.
    /// The two signals are redundant-on-purpose (an operator may
    /// cancel the cx while the shutdown flag is still unset, e.g. on
    /// program termination via a signal handler), and this test pins
    /// that the cx path alone is sufficient to stop the accept loop.
    ///
    /// Structure: start the server with a live cx, cancel the cx,
    /// wait for the handle to complete, assert the wait returns
    /// within a bounded time (the accept poll interval is 250 ms, so
    /// worst case is one poll + task cleanup). Complements ticks 319
    /// (pre-cancel-refuses-bind) and 320 (wait_with_cx on pre-cancel),
    /// which exercise other timings of the cancel signal.
    #[test]
    fn metrics_server_start_with_cx_mid_flight_cancel_stops_accept_loop() {
        run_async_test(async {
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
            let server = MetricsServer::new("127.0.0.1:0", "wa", collector, shutdown_flag.clone());
            let cx = crate::cx::for_request();

            let handle = server.start_with_cx(&cx).await.expect("start_with_cx");

            // Cancel the cx while the server is running. The shutdown
            // flag is intentionally NOT flipped — this test pins that
            // the cx path alone is sufficient to terminate the accept
            // loop.
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("mid-flight cancel of metrics accept loop"),
            );

            let start = std::time::Instant::now();
            handle.wait().await;
            let elapsed = start.elapsed();

            // Accept poll interval is 250ms; the loop checks
            // `task_cx.is_cancel_requested()` at the top of each
            // iteration. Bound generously to tolerate scheduling
            // variance on loaded CI hosts, but a regression that
            // removed the cx check would never exit (we'd hit the
            // test's outer runtime shutdown after minutes).
            assert!(
                elapsed < Duration::from_secs(2),
                "accept loop must exit after cx-cancel without shutdown flag; took {elapsed:?}"
            );

            // Sanity: the flag is still unset — proves the cancel took
            // effect via the cx path, not the flag path.
            assert!(
                !shutdown_flag.load(Ordering::SeqCst),
                "shutdown flag should still be false at test end"
            );
        });
    }

    /// A pre-cancelled waiter must still signal shutdown and retain ownership
    /// until the accept loop acknowledges terminal settlement. Returning early
    /// by dropping the join handle would silently detach a live listener.
    #[test]
    fn metrics_server_wait_with_precancelled_cx_returns_quickly() {
        run_async_test(async {
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
            let server = MetricsServer::new("127.0.0.1:0", "wa", collector, shutdown_flag.clone());

            let handle = server.start().await.expect("start metrics");

            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel metrics wait"),
            );

            let start = std::time::Instant::now();
            handle.wait_with_cx(&cx).await;
            let elapsed = start.elapsed();

            assert!(
                elapsed < Duration::from_millis(500),
                "wait_with_cx on cancelled cx should settle quickly, took {elapsed:?}"
            );

            assert!(
                shutdown_flag.load(Ordering::SeqCst),
                "wait_with_cx must signal the shared shutdown flag before joining"
            );
        });
    }

    #[test]
    fn metrics_server_refuses_public_bind_without_opt_in() {
        run_async_test(async {
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
            let server = MetricsServer::new("0.0.0.0:0", "wa", collector, shutdown_flag);

            let result = server.start().await;
            assert!(result.is_err(), "public bind should be refused");
            let Err(err) = result else {
                return;
            };
            assert!(
                err.to_string()
                    .contains("refusing to bind metrics on public address")
            );
        });
    }

    #[test]
    fn metrics_server_allows_public_bind_with_opt_in() {
        run_async_test(async {
            let shutdown_flag = Arc::new(AtomicBool::new(false));
            let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
            let server =
                MetricsServer::new("0.0.0.0:0", "wa", collector, Arc::clone(&shutdown_flag))
                    .with_dangerous_public_bind();

            let handle = server
                .start()
                .await
                .expect("public bind allowed with opt-in");
            shutdown_flag.store(true, Ordering::SeqCst);
            handle.wait().await;
        });
    }
}
