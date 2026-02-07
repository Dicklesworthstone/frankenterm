//! Prometheus metrics endpoint (feature-gated).
//!
//! Exposes a minimal, safe metrics surface for wa watcher health.
//! Disabled by default and bound to localhost unless explicitly enabled.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::Result;
use crate::events::EventBus;
use crate::runtime::RuntimeHandle;

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
    pub event_bus: Option<EventBusSnapshot>,
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
                    .map_or(-1_i64, |ms| ms as i64)
                    .to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_detection_oldest_lag_ms"),
                "Age of oldest detection event in ms (-1 means none)",
                bus.detection_oldest_lag_ms
                    .map_or(-1_i64, |ms| ms as i64)
                    .to_string(),
            );
            push_gauge(
                &mut output,
                metric_name(&prefix, "event_bus_signal_oldest_lag_ms"),
                "Age of oldest signal event in ms (-1 means none)",
                bus.signal_oldest_lag_ms
                    .map_or(-1_i64, |ms| ms as i64)
                    .to_string(),
            );
        }

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
                event_bus,
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
pub struct MetricsServerHandle {
    join: JoinHandle<()>,
    local_addr: SocketAddr,
}

impl MetricsServerHandle {
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn wait(self) {
        let _ = self.join.await;
    }
}

/// Minimal Prometheus metrics server.
pub struct MetricsServer {
    bind: String,
    prefix: String,
    collector: Arc<dyn MetricsCollector>,
    shutdown_flag: Arc<AtomicBool>,
    /// Must be set to `true` to bind on non-localhost addresses.
    allow_public_bind: bool,
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
        }
    }

    /// Explicitly opt in to binding on a non-localhost address.
    #[must_use]
    pub fn with_dangerous_public_bind(mut self) -> Self {
        self.allow_public_bind = true;
        self
    }

    pub async fn start(self) -> Result<MetricsServerHandle> {
        if !is_localhost_bind(&self.bind) && !self.allow_public_bind {
            return Err(crate::Error::Runtime(format!(
                "refusing to bind metrics on public address '{}' — use --dangerous-bind-any to override",
                self.bind
            )));
        }
        if !is_localhost_bind(&self.bind) {
            warn!(
                bind = %self.bind,
                "binding metrics endpoint on non-localhost address — endpoint may be remotely reachable"
            );
        }

        let listener = TcpListener::bind(&self.bind).await?;
        let local_addr = listener.local_addr()?;
        let prefix = sanitize_prefix(&self.prefix);
        let collector = Arc::clone(&self.collector);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);

        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accept = listener.accept() => {
                        match accept {
                            Ok((socket, peer)) => {
                                let collector = Arc::clone(&collector);
                                let prefix = prefix.clone();
                                tokio::spawn(async move {
                                    if let Err(err) = handle_connection(socket, &prefix, collector).await {
                                        debug!(error = %err, peer = %peer, "Metrics connection failed");
                                    }
                                });
                            }
                            Err(err) => {
                                warn!(error = %err, "Metrics listener accept failed");
                            }
                        }
                    }
                    () = wait_for_shutdown(Arc::clone(&shutdown_flag)) => break,
                }
            }
        });

        Ok(MetricsServerHandle { join, local_addr })
    }
}

async fn handle_connection(
    mut socket: TcpStream,
    prefix: &str,
    collector: Arc<dyn MetricsCollector>,
) -> Result<()> {
    let mut buf = [0_u8; 8192];
    let read_len = socket.read(&mut buf).await?;
    if read_len == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..read_len]);
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

async fn wait_for_shutdown(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn render_prometheus_includes_prefix() {
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
            event_bus: None,
        };

        let rendered = snapshot.render_prometheus("wa");
        assert!(rendered.contains("wa_observed_panes"));
        assert!(rendered.contains("wa_segments_persisted_total"));
        assert!(rendered.contains("wa_ingest_lag_ms_count"));
    }

    #[tokio::test]
    async fn metrics_server_serves_metrics() {
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
            event_bus: None,
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
    }

    #[test]
    fn localhost_bind_detection() {
        assert!(is_localhost_bind("127.0.0.1:9090"));
        assert!(is_localhost_bind("localhost:9090"));
        assert!(is_localhost_bind("[::1]:9090"));
        assert!(!is_localhost_bind("0.0.0.0:9090"));
    }

    #[tokio::test]
    async fn metrics_server_refuses_public_bind_without_opt_in() {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
        let server = MetricsServer::new("0.0.0.0:0", "wa", collector, shutdown_flag);

        let err = match server.start().await {
            Ok(_) => panic!("public bind should be refused"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("refusing to bind metrics on public address")
        );
    }

    #[tokio::test]
    async fn metrics_server_allows_public_bind_with_opt_in() {
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let collector = Arc::new(FixedMetricsCollector::new(MetricsSnapshot::default()));
        let server = MetricsServer::new("0.0.0.0:0", "wa", collector, Arc::clone(&shutdown_flag))
            .with_dangerous_public_bind();

        let handle = server
            .start()
            .await
            .expect("public bind allowed with opt-in");
        shutdown_flag.store(true, Ordering::SeqCst);
        handle.wait().await;
    }
}
