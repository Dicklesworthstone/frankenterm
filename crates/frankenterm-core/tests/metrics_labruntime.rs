//! LabRuntime port of `#[tokio::test]` async tests from `metrics.rs`.
//!
//! Each test that previously used `#[tokio::test]` is wrapped in
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })`.
//! Feature-gated behind `asupersync-runtime` and `metrics`.

#![cfg(all(feature = "asupersync-runtime", feature = "metrics"))]

mod common;

use common::fixtures::RuntimeFixture;

use frankenterm_core::metrics::{
    FixedMetricsCollector, MetricsServer, MetricsSnapshot,
};
use frankenterm_core::runtime_compat::io::{AsyncReadExt, AsyncWriteExt};
use frankenterm_core::runtime_compat::net::TcpStream;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// ===========================================================================
// render_prometheus_includes_prefix
// ===========================================================================

#[test]
fn render_prometheus_includes_prefix() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
        };

        let rendered = snapshot.render_prometheus("wa");
        assert!(rendered.contains("wa_observed_panes"));
        assert!(rendered.contains("wa_segments_persisted_total"));
        assert!(rendered.contains("wa_ingest_lag_ms_count"));
    });
}

// ===========================================================================
// metrics_server_serves_metrics
// ===========================================================================

#[test]
fn metrics_server_serves_metrics() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
        };

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let collector = Arc::new(FixedMetricsCollector::new(snapshot));
        let server =
            MetricsServer::new("127.0.0.1:0", "wa", collector, shutdown_flag.clone());
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

// ===========================================================================
// metrics_server_refuses_public_bind_without_opt_in
// ===========================================================================

#[test]
fn metrics_server_refuses_public_bind_without_opt_in() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

// ===========================================================================
// metrics_server_allows_public_bind_with_opt_in
// ===========================================================================

#[test]
fn metrics_server_allows_public_bind_with_opt_in() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
