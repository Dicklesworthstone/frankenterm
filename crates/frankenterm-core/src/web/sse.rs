//! Server-Sent Events endpoint surface for Wave 4B migration.

use super::error::json_err;
use super::extractors::{
    parse_u64, redact_json_value, request_runtime_limits, require_event_bus,
    require_storage_and_event_bus,
};
use super::{
    STREAM_CHANNEL_BUFFER, STREAM_MAX_CONSECUTIVE_DROPS, STREAM_SCHEMA_VERSION, WebRuntimeLimits,
};
use crate::events::{Event, RecvError};
use crate::policy::Redactor;
use crate::runtime_async::{mpsc, select, sleep, task};
use crate::storage::{SegmentScanQuery, StorageHandle};
use crate::web_framework::{QueryString, Request, Response, StatusCode, sse_stream_response};
use asupersync::stream::Stream;
use serde_json::json;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ============================================================================
// br-ft-95fd3: SSE drop observability counters
// ============================================================================

/// br-ft-95fd3: cumulative count of SSE events dropped due to a
/// per-stream channel-full condition since process load. Pre-fix
/// the per-stream `consecutive_drops` local was the only signal —
/// resetting on every successful send and never aggregating across
/// streams — so operators reading runtime metrics had ZERO insight
/// into cross-stream drop rates. This counter bumps on every
/// `mpsc::TrySendError::Full(_)` outcome inside
/// [`send_rate_limited_sse`].
///
/// Same observability defect family as ft-jyywz
/// (audit_chain_export_dropped_count), ft-zkthg
/// (replay_capture_policy_decision_drop_count), ft-luav8
/// (mcp_audit_failure), ft-8na0z (mcp_proxy mount_failure), and
/// the broader runtime/policy "make drops observable" pattern.
static SSE_EVENTS_DROPPED_COUNT: AtomicU64 = AtomicU64::new(0);

/// br-ft-95fd3: cumulative count of SSE streams terminated by the
/// consecutive-drops cap (`STREAM_MAX_CONSECUTIVE_DROPS = 64`)
/// since process load. Distinct from
/// [`SSE_EVENTS_DROPPED_COUNT`]: that counter tracks individual
/// dropped EVENTS; this one tracks STREAMS that hit the cap and
/// got force-terminated. A high stream-termination rate signals
/// sustained backpressure rather than transient bursts.
static SSE_STREAMS_TERMINATED_BY_DROP_CAP_COUNT: AtomicU64 = AtomicU64::new(0);

/// br-ft-bn6qi: cumulative count of `SystemTime::now() <
/// UNIX_EPOCH` events observed by [`epoch_ms_now`] since process
/// load. Pre-fix the `unwrap_or_default()` fallback silently
/// substituted `0` for every event during a clock anomaly window
/// (NTP fail, container restart with un-synced clock, hostile CI
/// fixture clock value). Operators trying to correlate event
/// ordering then saw a flat line at epoch zero with no diagnostic.
/// This counter bumps on every Err branch of `duration_since`, so
/// a single observability scrape surfaces the misconfiguration.
///
/// Same observability defect family as the existing
/// `mcp_clock_anomaly` counter (ft-rnpuc).
static EPOCH_CLOCK_ANOMALY_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of SSE events dropped because the per-stream
/// channel was full. See [`SSE_EVENTS_DROPPED_COUNT`].
#[must_use]
pub fn sse_events_dropped_count() -> u64 {
    SSE_EVENTS_DROPPED_COUNT.load(Ordering::Relaxed)
}

/// Cumulative count of SSE streams terminated by the
/// consecutive-drops cap. See [`SSE_STREAMS_TERMINATED_BY_DROP_CAP_COUNT`].
#[must_use]
pub fn sse_streams_terminated_by_drop_cap_count() -> u64 {
    SSE_STREAMS_TERMINATED_BY_DROP_CAP_COUNT.load(Ordering::Relaxed)
}

/// br-ft-bn6qi: cumulative count of clock-before-1970 anomalies
/// observed by [`epoch_ms_now`]. See [`EPOCH_CLOCK_ANOMALY_COUNT`].
#[must_use]
pub fn epoch_clock_anomaly_count() -> u64 {
    EPOCH_CLOCK_ANOMALY_COUNT.load(Ordering::Relaxed)
}

/// Test helper: reset the drop counter so regression tests can
/// assert post-bump values without state leakage between tests.
#[cfg(test)]
pub(crate) fn reset_sse_events_dropped_count_for_test() {
    SSE_EVENTS_DROPPED_COUNT.store(0, Ordering::Relaxed);
}

/// Test helper: reset the stream-termination counter.
#[cfg(test)]
pub(crate) fn reset_sse_streams_terminated_by_drop_cap_count_for_test() {
    SSE_STREAMS_TERMINATED_BY_DROP_CAP_COUNT.store(0, Ordering::Relaxed);
}

/// br-ft-bn6qi: test helper to reset the clock-anomaly counter.
#[cfg(test)]
pub(crate) fn reset_epoch_clock_anomaly_count_for_test() {
    EPOCH_CLOCK_ANOMALY_COUNT.store(0, Ordering::Relaxed);
}

#[inline]
fn record_sse_event_dropped() {
    SSE_EVENTS_DROPPED_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_sse_stream_terminated_by_drop_cap() {
    SSE_STREAMS_TERMINATED_BY_DROP_CAP_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[inline]
fn record_epoch_clock_anomaly() {
    EPOCH_CLOCK_ANOMALY_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy)]
pub(super) enum EventStreamChannel {
    All,
    Deltas,
    Detections,
    Signals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeltaCatchup {
    Complete,
    MoreAvailable,
    Stop,
}

pub(super) fn parse_stream_max_hz(qs: &QueryString<'_>, runtime_limits: WebRuntimeLimits) -> u64 {
    qs.get("max_hz")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|hz| *hz > 0)
        .unwrap_or(runtime_limits.stream_default_max_hz)
        .min(runtime_limits.stream_max_max_hz)
}

pub(super) fn parse_event_stream_channel(
    qs: &QueryString<'_>,
) -> std::result::Result<EventStreamChannel, Response> {
    match qs.get("channel") {
        None => Ok(EventStreamChannel::All),
        Some(channel) if channel.eq_ignore_ascii_case("all") => Ok(EventStreamChannel::All),
        Some(channel)
            if channel.eq_ignore_ascii_case("deltas") || channel.eq_ignore_ascii_case("delta") =>
        {
            Ok(EventStreamChannel::Deltas)
        }
        Some(channel)
            if channel.eq_ignore_ascii_case("detections")
                || channel.eq_ignore_ascii_case("detection") =>
        {
            Ok(EventStreamChannel::Detections)
        }
        Some(channel)
            if channel.eq_ignore_ascii_case("signals")
                || channel.eq_ignore_ascii_case("signal") =>
        {
            Ok(EventStreamChannel::Signals)
        }
        Some(other) => Err(json_err(
            StatusCode::BAD_REQUEST,
            "invalid_channel",
            format!(
                "Invalid stream channel '{other}'. Expected one of: all, delta(s), detection(s), signal(s)"
            ),
        )),
    }
}

/// Wall-clock UNIX epoch in milliseconds for SSE event payloads.
///
/// br-ft-bn6qi: `SystemTime::now().duration_since(UNIX_EPOCH)` returns
/// `Err(SystemTimeError)` when the system clock is BEFORE 1970-01-01
/// (NTP fail, container restart with un-synced clock, hostile CI
/// fixture). The pre-fix `unwrap_or_default()` silently substituted a
/// zero `Duration` for every emitted event during the anomaly window,
/// producing `ts_ms = 0` on every SSE frame with no operator-visible
/// diagnostic. We now fail loud:
///
///   * Bump [`EPOCH_CLOCK_ANOMALY_COUNT`] on the Err branch so a
///     single observability scrape surfaces the misconfiguration.
///   * Emit a structured `tracing::warn` (target: ft.web.clock) so
///     log-aggregating consumers see one record per anomaly window
///     plus the by-how-much delta (the `SystemTimeError` carries the
///     pre-epoch duration).
///   * Still return `0` so callers downstream don't have to handle
///     `Result` — the contract stays scalar — but the anomaly is now
///     observable through the counter, not silent.
fn epoch_ms_now() -> i64 {
    epoch_ms_from(SystemTime::now())
}

/// br-ft-bn6qi: testable inner helper. Takes a `SystemTime` so a
/// regression test can inject `UNIX_EPOCH - 100s` (a valid
/// pre-epoch SystemTime) to exercise the Err branch without
/// depending on the host system clock.
fn epoch_ms_from(now: SystemTime) -> i64 {
    match now.duration_since(UNIX_EPOCH) {
        Ok(ts) => i64::try_from(ts.as_millis()).unwrap_or(0),
        Err(err) => {
            record_epoch_clock_anomaly();
            tracing::warn!(
                target: "ft.web.clock",
                event = "epoch_clock_anomaly",
                pre_epoch_secs = err.duration().as_secs(),
                "system clock is before UNIX_EPOCH; SSE event timestamp falling back to 0 (br-ft-bn6qi)"
            );
            0
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SseEvent {
    data: Option<String>,
    event_type: Option<String>,
    id: Option<String>,
    comment: Option<String>,
}

impl SseEvent {
    pub(super) fn new(data: impl Into<String>) -> Self {
        Self {
            data: Some(data.into()),
            event_type: None,
            id: None,
            comment: None,
        }
    }

    pub(super) fn comment(comment: impl Into<String>) -> Self {
        Self {
            data: None,
            event_type: None,
            id: None,
            comment: Some(comment.into()),
        }
    }

    pub(super) fn event_type(mut self, event_type: impl Into<String>) -> Self {
        self.event_type = Some(event_type.into());
        self
    }

    pub(super) fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);

        if let Some(comment) = &self.comment {
            for line in comment.lines() {
                out.extend_from_slice(b": ");
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
        }

        if let Some(event_type) = &self.event_type {
            out.extend_from_slice(b"event: ");
            out.extend_from_slice(event_type.as_bytes());
            out.push(b'\n');
        }

        if let Some(id) = &self.id {
            out.extend_from_slice(b"id: ");
            out.extend_from_slice(id.as_bytes());
            out.push(b'\n');
        }

        if let Some(data) = &self.data {
            for line in data.lines() {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(line.as_bytes());
                out.push(b'\n');
            }
            if data.is_empty() {
                out.extend_from_slice(b"data: \n");
            }
        }

        out.push(b'\n');
        out
    }
}

struct SseResponse<S> {
    stream: S,
}

impl<S> SseResponse<S>
where
    S: Stream<Item = Vec<u8>> + Send + 'static,
{
    fn new(stream: S) -> Self {
        Self { stream }
    }

    fn into_response(self) -> Response {
        sse_stream_response(self.stream)
    }
}

/// Returns a future that completes when the receiver side of the channel is
/// dropped (i.e. the client disconnected). This bridges the richer
/// `Sender::closed()` API for runtimes that only expose `is_closed()`.
async fn sender_closed<T>(tx: &mpsc::Sender<T>) {
    // Poll periodically rather than truly registering a waker, since
    // asupersync `Sender::is_closed` is a simple atomic load.
    loop {
        if tx.is_closed() {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

struct SseByteStream {
    rx: mpsc::Receiver<SseEvent>,
    recv_state: SseRecvState,
}

struct SseRecvState {
    cx: asupersync::Cx,
}

impl SseRecvState {
    fn new() -> Self {
        Self {
            // Inherit the ambient handler Cx so a cancellation propagated
            // from the web server's parent — an HTTP client disconnect, a
            // graceful-shutdown signal from `shutdown_with_cx`, or a
            // parent-scope cancel — reaches the inner `poll_recv` on this
            // SSE stream. The earlier revision unconditionally called
            // `crate::cx::for_request()`, which produces a detached
            // request-scoped Cx with no parent chain; `poll_recv(&self.cx,
            // …)` would then wait for events until the upstream Sender
            // dropped, delaying cleanup of the mpsc receiver and its
            // associated channels past the point where the HTTP client
            // already gave up. Matches the inherit-or-fallback idiom at
            // sse.rs:343 / :440 / :680, handlers.rs:208 / :317, and the
            // tailer fix landed in dfbf0a31.
            cx: crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request),
        }
    }

    fn poll_recv(
        &self,
        rx: &mut mpsc::Receiver<SseEvent>,
        poll_cx: &mut Context<'_>,
    ) -> Poll<Option<SseEvent>> {
        match rx.poll_recv(&self.cx, poll_cx) {
            Poll::Ready(Ok(event)) => Poll::Ready(Some(event)),
            Poll::Ready(Err(_)) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    // Explicit quarantine for legacy non-asupersync builds.
    //
    // Owner: `ft-xbnl0.2.5`.
    // Removal path: drop this note once the workspace no longer supports
    // non-`asupersync-runtime` SSE builds.
}

impl SseByteStream {
    fn new(rx: mpsc::Receiver<SseEvent>) -> Self {
        Self {
            rx,
            recv_state: SseRecvState::new(),
        }
    }
}

impl Stream for SseByteStream {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.recv_state.poll_recv(&mut this.rx, cx) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(event.to_bytes())),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(super) fn make_stream_frame(
    stream: &'static str,
    kind: &'static str,
    seq: u64,
    data: serde_json::Value,
) -> serde_json::Value {
    json!({
        "schema": STREAM_SCHEMA_VERSION,
        "stream": stream,
        "kind": kind,
        "seq": seq,
        "ts_ms": epoch_ms_now(),
        "data": data
    })
}

pub(super) fn frame_to_sse(
    event_type: &'static str,
    seq: u64,
    frame: serde_json::Value,
) -> Option<SseEvent> {
    serde_json::to_string(&frame)
        .inspect_err(|e| tracing::warn!(error = %e, event_type, "SSE frame serialization failed"))
        .ok()
        .map(|body| {
            SseEvent::new(body)
                .event_type(event_type)
                .id(seq.to_string())
        })
}

async fn send_rate_limited_sse(
    tx: &mpsc::Sender<SseEvent>,
    event: SseEvent,
    next_emit_at: &mut Instant,
    min_interval: Duration,
    consecutive_drops: &mut u64,
) -> bool {
    let now = Instant::now();
    if *next_emit_at > now {
        sleep(*next_emit_at - now).await;
    }
    *next_emit_at = Instant::now().checked_add(min_interval).unwrap_or_else(|| {
        // `min_interval` is normally query-bounded, but keep this path panic-free
        // if future callers bypass the parser.
        Instant::now()
    });

    match tx.try_send(event).map_err(mpsc::TrySendError::from) {
        Ok(()) => {
            *consecutive_drops = 0;
            true
        }
        Err(mpsc::TrySendError::Full(_)) => {
            // br-ft-95fd3: bump the process-global drop counter so
            // operators can observe SSE backpressure across all
            // active streams (not just the per-stream local).
            record_sse_event_dropped();
            *consecutive_drops += 1;
            let still_alive = *consecutive_drops < STREAM_MAX_CONSECUTIVE_DROPS;
            if !still_alive {
                // br-ft-95fd3: this stream just hit the
                // STREAM_MAX_CONSECUTIVE_DROPS cap and the caller
                // will terminate it on the false return. Bump the
                // stream-termination counter so operators can
                // distinguish individual dropped events (the
                // common case under transient bursts) from
                // forced-stream-termination (sustained
                // backpressure, page-worthy).
                record_sse_stream_terminated_by_drop_cap();
            }
            still_alive
        }
        Err(mpsc::TrySendError::Closed(_)) => false,
    }
}

pub(super) fn event_matches_pane(event: &Event, pane_filter: Option<u64>) -> bool {
    pane_filter.is_none_or(|pane_id| event.pane_id() == Some(pane_id))
}

async fn emit_new_segment_frames(
    storage: &StorageHandle,
    pane_filter: Option<u64>,
    started_at_ms: i64,
    after_id: &mut Option<i64>,
    runtime_limits: WebRuntimeLimits,
    redactor: &Redactor,
    tx: &mpsc::Sender<SseEvent>,
    seq: &mut u64,
    next_emit_at: &mut Instant,
    min_interval: Duration,
    consecutive_drops: &mut u64,
) -> DeltaCatchup {
    for page_index in 0..runtime_limits.stream_scan_max_pages {
        let query = SegmentScanQuery {
            after_id: *after_id,
            pane_id: pane_filter,
            since: Some(started_at_ms),
            until: None,
            limit: runtime_limits.stream_scan_limit,
        };

        // ft-xbnl0.2.3 tick 257: cx-first SSE delta-stream scan.
        let scan_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        let segments = match storage.scan_segments_with_cx(&scan_cx, query).await {
            Ok(segments) => segments,
            Err(err) => {
                *seq += 1;
                let frame = make_stream_frame(
                    "deltas",
                    "error",
                    *seq,
                    json!({
                        "code": "storage_error",
                        "message": redactor.redact(&err.to_string())
                    }),
                );
                if let Some(event) = frame_to_sse("error", *seq, frame) {
                    let _ = send_rate_limited_sse(
                        tx,
                        event,
                        next_emit_at,
                        min_interval,
                        consecutive_drops,
                    )
                    .await;
                }
                return DeltaCatchup::Stop;
            }
        };

        if segments.is_empty() {
            return DeltaCatchup::Complete;
        }

        let page_len = segments.len();
        for segment in segments {
            *after_id = Some(segment.id);

            *seq += 1;
            let frame = make_stream_frame(
                "deltas",
                "delta",
                *seq,
                json!({
                    "segment_id": segment.id,
                    "pane_id": segment.pane_id,
                    "seq": segment.seq,
                    "captured_at": segment.captured_at,
                    "content_len": segment.content_len,
                    "content": redactor.redact(&segment.content),
                }),
            );

            if let Some(event) = frame_to_sse("delta", *seq, frame) {
                if !send_rate_limited_sse(tx, event, next_emit_at, min_interval, consecutive_drops)
                    .await
                {
                    return DeltaCatchup::Stop;
                }
            }
        }

        if page_len < runtime_limits.stream_scan_limit {
            return DeltaCatchup::Complete;
        }

        if page_index + 1 == runtime_limits.stream_scan_max_pages {
            return DeltaCatchup::MoreAvailable;
        }
    }

    DeltaCatchup::Complete
}

async fn drain_new_segment_frames(
    storage: &StorageHandle,
    pane_filter: Option<u64>,
    started_at_ms: i64,
    after_id: &mut Option<i64>,
    runtime_limits: WebRuntimeLimits,
    redactor: &Redactor,
    tx: &mpsc::Sender<SseEvent>,
    seq: &mut u64,
    next_emit_at: &mut Instant,
    min_interval: Duration,
    consecutive_drops: &mut u64,
) -> bool {
    loop {
        match emit_new_segment_frames(
            storage,
            pane_filter,
            started_at_ms,
            after_id,
            runtime_limits,
            redactor,
            tx,
            seq,
            next_emit_at,
            min_interval,
            consecutive_drops,
        )
        .await
        {
            DeltaCatchup::Complete => return true,
            DeltaCatchup::MoreAvailable => task::yield_now().await,
            DeltaCatchup::Stop => return false,
        }
    }
}

pub(super) fn handle_stream_events(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);
    let pane_filter = parse_u64(&qs, "pane_id");
    let runtime_limits = request_runtime_limits(req);
    let max_hz = parse_stream_max_hz(&qs, runtime_limits);
    let channel = match parse_event_stream_channel(&qs) {
        Ok(channel) => channel,
        Err(resp) => return Box::pin(async move { resp }),
    };
    let result = require_event_bus(req);

    Box::pin(async move {
        let (event_bus, redactor) = match result {
            Ok(r) => r,
            Err(resp) => return resp,
        };

        let mut subscriber = match channel {
            EventStreamChannel::All => event_bus.subscribe(),
            EventStreamChannel::Deltas => event_bus.subscribe_deltas(),
            EventStreamChannel::Detections => event_bus.subscribe_detections(),
            EventStreamChannel::Signals => event_bus.subscribe_signals(),
        };

        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_BUFFER);
        {
            let stream_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            task::spawn_with_cx(&stream_cx, move |child_cx| async move {
                let min_interval = Duration::from_millis((1000 / max_hz.max(1)).max(1));
                let mut next_emit_at = Instant::now();
                let mut seq = 0_u64;
                let mut consecutive_drops = 0_u64;

                seq += 1;
                let ready = make_stream_frame(
                    "events",
                    "ready",
                    seq,
                    json!({
                        "channel": format!("{channel:?}").to_lowercase(),
                        "max_hz": max_hz,
                        "pane_id": pane_filter
                    }),
                );
                if let Some(event) = frame_to_sse("ready", seq, ready) {
                    if !send_rate_limited_sse(
                        &tx,
                        event,
                        &mut next_emit_at,
                        min_interval,
                        &mut consecutive_drops,
                    )
                    .await
                    {
                        return;
                    }
                }

                loop {
                    let recv_result = select! {
                        () = sender_closed(&tx) => break,
                        recv = crate::runtime_async::timeout_with_cx(
                            &child_cx,
                            Duration::from_secs(runtime_limits.stream_keepalive_secs),
                            subscriber.recv_cx(&child_cx),
                        ) => recv,
                    };

                    match recv_result {
                        Ok(Ok(event)) => {
                            if !event_matches_pane(&event, pane_filter) {
                                continue;
                            }

                            let mut event_json =
                                serde_json::to_value(&event).unwrap_or_else(|_| {
                                    json!({
                                        "error": "event_serialization_failed"
                                    })
                                });
                            redact_json_value(&mut event_json, &redactor);

                            seq += 1;
                            let frame = make_stream_frame(
                                "events",
                                "event",
                                seq,
                                json!({ "event": event_json }),
                            );
                            if let Some(event) = frame_to_sse("event", seq, frame) {
                                if !send_rate_limited_sse(
                                    &tx,
                                    event,
                                    &mut next_emit_at,
                                    min_interval,
                                    &mut consecutive_drops,
                                )
                                .await
                                {
                                    break;
                                }
                            }
                        }
                        Ok(Err(RecvError::Lagged { missed_count })) => {
                            seq += 1;
                            let frame = make_stream_frame(
                                "events",
                                "lag",
                                seq,
                                json!({ "missed_count": missed_count }),
                            );
                            if let Some(event) = frame_to_sse("lag", seq, frame) {
                                if !send_rate_limited_sse(
                                    &tx,
                                    event,
                                    &mut next_emit_at,
                                    min_interval,
                                    &mut consecutive_drops,
                                )
                                .await
                                {
                                    break;
                                }
                            }
                        }
                        Ok(Err(RecvError::Cancelled)) => break,
                        Ok(Err(RecvError::Closed)) => break,
                        Err(_) => {
                            if tx.try_send(SseEvent::comment("keepalive")).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        SseResponse::new(SseByteStream::new(rx)).into_response()
    })
}

pub(super) fn handle_stream_deltas(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    // br-ft-x2oyy: function-level delta SSE keepalive contract. The spawned
    // stream task may opportunistically enqueue comments while waiting for
    // runtime events; dropped keepalives are acceptable when the client is
    // already backpressured or disconnected.
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);
    let pane_filter = parse_u64(&qs, "pane_id");
    let runtime_limits = request_runtime_limits(req);
    let max_hz = parse_stream_max_hz(&qs, runtime_limits);
    let result = require_storage_and_event_bus(req);

    Box::pin(async move {
        let (storage, event_bus, redactor) = match result {
            Ok(r) => r,
            Err(resp) => return resp,
        };

        let mut subscriber = event_bus.subscribe_deltas();
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_BUFFER);
        let started_at_ms = epoch_ms_now();
        {
            let stream_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            task::spawn_with_cx(&stream_cx, move |child_cx| async move {
                let min_interval = Duration::from_millis((1000 / max_hz.max(1)).max(1));
                let mut next_emit_at = Instant::now();
                let mut seq = 0_u64;
                let mut consecutive_drops = 0_u64;
                let mut after_id: Option<i64> = None;

                seq += 1;
                let ready = make_stream_frame(
                    "deltas",
                    "ready",
                    seq,
                    json!({
                        "max_hz": max_hz,
                        "pane_id": pane_filter
                    }),
                );
                if let Some(event) = frame_to_sse("ready", seq, ready) {
                    if !send_rate_limited_sse(
                        &tx,
                        event,
                        &mut next_emit_at,
                        min_interval,
                        &mut consecutive_drops,
                    )
                    .await
                    {
                        return;
                    }
                }

                loop {
                    let recv_result = select! {
                        () = sender_closed(&tx) => break,
                        recv = crate::runtime_async::timeout_with_cx(
                            &child_cx,
                            Duration::from_secs(runtime_limits.stream_keepalive_secs),
                            subscriber.recv_cx(&child_cx),
                        ) => recv,
                    };

                    match recv_result {
                        Ok(Ok(Event::SegmentCaptured { pane_id, .. })) => {
                            if pane_filter.is_some_and(|pid| pid != pane_id) {
                                continue;
                            }
                            if !drain_new_segment_frames(
                                &storage,
                                pane_filter,
                                started_at_ms,
                                &mut after_id,
                                runtime_limits,
                                &redactor,
                                &tx,
                                &mut seq,
                                &mut next_emit_at,
                                min_interval,
                                &mut consecutive_drops,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        Ok(Ok(Event::GapDetected {
                            pane_id,
                            seq_before,
                            seq_after,
                            reason,
                            detected_at_ms,
                        })) => {
                            if pane_filter.is_some_and(|pid| pid != pane_id) {
                                continue;
                            }

                            seq += 1;
                            let frame = make_stream_frame(
                                "deltas",
                                "gap",
                                seq,
                                json!({
                                    "pane_id": pane_id,
                                    "seq_before": seq_before,
                                    "seq_after": seq_after,
                                    "reason": redactor.redact(&reason),
                                    "detected_at_ms": detected_at_ms,
                                }),
                            );
                            if let Some(event) = frame_to_sse("gap", seq, frame) {
                                if !send_rate_limited_sse(
                                    &tx,
                                    event,
                                    &mut next_emit_at,
                                    min_interval,
                                    &mut consecutive_drops,
                                )
                                .await
                                {
                                    break;
                                }
                            }
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(RecvError::Lagged { missed_count })) => {
                            seq += 1;
                            let frame = make_stream_frame(
                                "deltas",
                                "lag",
                                seq,
                                json!({ "missed_count": missed_count }),
                            );
                            if let Some(event) = frame_to_sse("lag", seq, frame) {
                                if !send_rate_limited_sse(
                                    &tx,
                                    event,
                                    &mut next_emit_at,
                                    min_interval,
                                    &mut consecutive_drops,
                                )
                                .await
                                {
                                    break;
                                }
                            }

                            if !drain_new_segment_frames(
                                &storage,
                                pane_filter,
                                started_at_ms,
                                &mut after_id,
                                runtime_limits,
                                &redactor,
                                &tx,
                                &mut seq,
                                &mut next_emit_at,
                                min_interval,
                                &mut consecutive_drops,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        Ok(Err(RecvError::Cancelled)) => break,
                        Ok(Err(RecvError::Closed)) => break,
                        Err(_) => {
                            // br-ft-x2oyy: intentional best-effort keepalive;
                            // try_send failure means the SSE client is backpressured or gone.
                            match tx
                                .try_send(SseEvent::comment("keepalive"))
                                .map_err(mpsc::TrySendError::from)
                            {
                                Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                                Err(mpsc::TrySendError::Closed(_)) => break,
                            }
                            if !drain_new_segment_frames(
                                &storage,
                                pane_filter,
                                started_at_ms,
                                &mut after_id,
                                runtime_limits,
                                &redactor,
                                &tx,
                                &mut seq,
                                &mut next_emit_at,
                                min_interval,
                                &mut consecutive_drops,
                            )
                            .await
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }

        SseResponse::new(SseByteStream::new(rx)).into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        EventStreamChannel, STREAM_MAX_CONSECUTIVE_DROPS, SseEvent, mpsc,
        parse_event_stream_channel,
    };
    use crate::runtime_async::CompatRuntime;
    use crate::web_framework::QueryString;

    fn default_limits() -> super::super::WebRuntimeLimits {
        super::super::resolve_runtime_limits(None)
    }

    /// Per ft-22x4r: every async test in supported paths must drive the
    /// project asupersync runtime instead of `#[tokio::test]`. This
    /// helper builds a thread-isolated current-thread runtime, runs the
    /// future to completion, and absorbs the asupersync TLS destructor
    /// panics that fire when the runtime is dropped after the future
    /// has spawned `Sleep`/`Cx` cleanup tasks. Mirrors the helper used
    /// in `runtime.rs` and `snapshot_engine.rs` test modules.
    fn run_async_test_isolated<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()>,
    {
        let result = std::thread::Builder::new()
            .name("sse-test-isolated".into())
            .spawn(move || {
                let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
                    .build()
                    .expect("failed to build runtime for sse tests");

                let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(f());
                }));

                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(runtime);
                }));
                crate::runtime_async::clear_runtime_handle();

                if let Err(payload) = test_result {
                    std::panic::resume_unwind(payload);
                }
            })
            .expect("failed to spawn isolated test thread")
            .join();

        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn parse_stream_max_hz(qs: &QueryString<'_>) -> u64 {
        super::parse_stream_max_hz(qs, default_limits())
    }

    // ── parse_event_stream_channel ───────────────────────────────────

    #[test]
    fn parse_event_stream_channel_is_case_insensitive() {
        let qs = QueryString::parse("channel=DELTAS");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::Deltas)
        ));

        let qs = QueryString::parse("channel=Signal");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::Signals)
        ));

        let qs = QueryString::parse("channel=Detection");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::Detections)
        ));
    }

    #[test]
    fn parse_event_stream_channel_invalid_still_errors() {
        let qs = QueryString::parse("channel=unknown");
        assert!(parse_event_stream_channel(&qs).is_err());
    }

    #[test]
    fn parse_event_stream_channel_no_param_defaults_to_all() {
        let qs = QueryString::parse("");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::All)
        ));
    }

    #[test]
    fn parse_event_stream_channel_explicit_all() {
        let qs = QueryString::parse("channel=all");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::All)
        ));
    }

    #[test]
    fn parse_event_stream_channel_singular_forms() {
        let qs = QueryString::parse("channel=delta");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::Deltas)
        ));

        let qs = QueryString::parse("channel=detection");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::Detections)
        ));

        let qs = QueryString::parse("channel=signal");
        assert!(matches!(
            parse_event_stream_channel(&qs),
            Ok(EventStreamChannel::Signals)
        ));
    }

    // ── parse_stream_max_hz ──────────────────────────────────────────

    #[test]
    fn parse_stream_max_hz_default_when_absent() {
        let qs = QueryString::parse("");
        assert_eq!(
            parse_stream_max_hz(&qs),
            default_limits().stream_default_max_hz
        );
    }

    #[test]
    fn parse_stream_max_hz_explicit_value() {
        let qs = QueryString::parse("max_hz=100");
        assert_eq!(parse_stream_max_hz(&qs), 100);
    }

    #[test]
    fn parse_stream_max_hz_clamped_to_max() {
        let qs = QueryString::parse("max_hz=99999");
        assert_eq!(parse_stream_max_hz(&qs), default_limits().stream_max_max_hz);
    }

    #[test]
    fn parse_stream_max_hz_zero_uses_default() {
        let qs = QueryString::parse("max_hz=0");
        assert_eq!(
            parse_stream_max_hz(&qs),
            default_limits().stream_default_max_hz
        );
    }

    #[test]
    fn parse_stream_max_hz_invalid_uses_default() {
        let qs = QueryString::parse("max_hz=notanumber");
        assert_eq!(
            parse_stream_max_hz(&qs),
            default_limits().stream_default_max_hz
        );
    }

    #[test]
    fn parse_stream_max_hz_uses_runtime_limits_ft_9ahut() {
        let limits = super::super::WebRuntimeLimits {
            max_list_limit: 50,
            default_list_limit: 10,
            max_request_body_bytes: 1024,
            stream_default_max_hz: 7,
            stream_max_max_hz: 9,
            stream_keepalive_secs: 3,
            stream_scan_limit: 4,
            stream_scan_max_pages: 2,
        };

        let qs = QueryString::parse("");
        assert_eq!(super::parse_stream_max_hz(&qs, limits), 7);

        let qs = QueryString::parse("max_hz=100");
        assert_eq!(super::parse_stream_max_hz(&qs, limits), 9);

        let qs = QueryString::parse("max_hz=0");
        assert_eq!(super::parse_stream_max_hz(&qs, limits), 7);
    }

    // ── SseEvent serialization ───────────────────────────────────────

    #[test]
    fn sse_event_data_only() {
        let event = SseEvent::new("hello world");
        let bytes = event.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text, "data: hello world\n\n");
    }

    #[test]
    fn sse_event_with_event_type_and_id() {
        let event = SseEvent::new("payload").event_type("detection").id("42");
        let bytes = event.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("event: detection\n"));
        assert!(text.contains("id: 42\n"));
        assert!(text.contains("data: payload\n"));
        assert!(text.ends_with("\n\n"));
    }

    #[test]
    fn sse_event_multiline_data() {
        let event = SseEvent::new("line1\nline2\nline3");
        let bytes = event.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("data: line1\n"));
        assert!(text.contains("data: line2\n"));
        assert!(text.contains("data: line3\n"));
    }

    #[test]
    fn sse_event_comment() {
        let event = SseEvent::comment("keepalive");
        let bytes = event.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text, ": keepalive\n\n");
    }

    #[test]
    fn sse_event_empty_data() {
        let event = SseEvent::new("");
        let bytes = event.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text, "data: \n\n");
    }

    // ── br-ft-95fd3: SSE drop observability counter tests ─────────────

    /// Cross-test serialization for the global counters. Multiple
    /// tests reset + bump shared atomics, so they must not run
    /// concurrently or one test's reset will clobber another's
    /// pre-condition. Same shape as the existing
    /// `proxy_counter_test_lock` in mcp_proxy.rs.
    fn drop_counter_test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{LazyLock, Mutex};
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// br-ft-95fd3: a successful send does NOT bump either drop
    /// counter. Vacuous-regression guard against the bumpers
    /// false-positiving on the happy path.
    #[test]
    fn send_rate_limited_sse_ok_path_does_not_bump_drop_counter_ft_95fd3() {
        run_async_test_isolated(|| async {
            let _guard = drop_counter_test_lock();
            super::reset_sse_events_dropped_count_for_test();
            super::reset_sse_streams_terminated_by_drop_cap_count_for_test();

            // Generous channel capacity so every send succeeds.
            let (tx, _rx) = mpsc::channel::<super::SseEvent>(16);
            let mut next_emit_at = std::time::Instant::now();
            let mut consecutive_drops: u64 = 0;
            let still_alive = super::send_rate_limited_sse(
                &tx,
                super::SseEvent::new("ok-payload"),
                &mut next_emit_at,
                std::time::Duration::ZERO,
                &mut consecutive_drops,
            )
            .await;

            assert!(still_alive, "ft-95fd3: ok-path must keep stream alive");
            assert_eq!(
                consecutive_drops, 0,
                "ft-95fd3: ok-path must not bump local"
            );
            assert_eq!(
                super::sse_events_dropped_count(),
                0,
                "ft-95fd3: ok-path must NOT bump the global event-drop counter"
            );
            assert_eq!(
                super::sse_streams_terminated_by_drop_cap_count(),
                0,
                "ft-95fd3: ok-path must NOT bump the global stream-termination counter"
            );
        });
    }

    /// br-ft-95fd3: a Full send DOES bump the global event-drop
    /// counter. Pre-fix the only signal was the per-stream local
    /// `consecutive_drops`, never aggregated across streams; this
    /// test pins that the global counter now reflects the drop.
    #[test]
    fn send_rate_limited_sse_full_path_bumps_event_drop_counter_ft_95fd3() {
        run_async_test_isolated(|| async {
            let _guard = drop_counter_test_lock();
            super::reset_sse_events_dropped_count_for_test();
            super::reset_sse_streams_terminated_by_drop_cap_count_for_test();
            let before_events = super::sse_events_dropped_count();
            let before_streams = super::sse_streams_terminated_by_drop_cap_count();

            // Capacity-1 channel + a pre-filled slot → next send is Full.
            let (tx, _rx) = mpsc::channel::<super::SseEvent>(1);
            assert!(
                tx.try_send(super::SseEvent::new("filler")).is_ok(),
                "first send fits in cap=1 channel"
            );

            let mut next_emit_at = std::time::Instant::now();
            let mut consecutive_drops: u64 = 0;
            let still_alive = super::send_rate_limited_sse(
                &tx,
                super::SseEvent::new("dropped-payload"),
                &mut next_emit_at,
                std::time::Duration::ZERO,
                &mut consecutive_drops,
            )
            .await;

            // Per-stream local incremented; stream still alive (1 < 64).
            assert!(
                still_alive,
                "ft-95fd3: 1 drop is well below STREAM_MAX_CONSECUTIVE_DROPS"
            );
            assert_eq!(
                consecutive_drops, 1,
                "ft-95fd3: per-stream local must increment on Full"
            );

            // Global event-drop counter MUST bump.
            assert_eq!(
                super::sse_events_dropped_count() - before_events,
                1,
                "ft-95fd3: Full send must bump the global event-drop counter exactly once"
            );
            // Stream-termination counter MUST NOT bump (still under cap).
            assert_eq!(
                super::sse_streams_terminated_by_drop_cap_count() - before_streams,
                0,
                "ft-95fd3: still-alive stream must NOT bump the termination counter"
            );
        });
    }

    /// br-ft-95fd3: hitting the consecutive-drops cap bumps the
    /// stream-termination counter. Pins the distinct-class
    /// invariant — a high stream-termination rate is page-worthy
    /// (sustained backpressure) while individual event drops are
    /// the common burst case.
    #[test]
    fn send_rate_limited_sse_drop_cap_bumps_termination_counter_ft_95fd3() {
        run_async_test_isolated(|| async {
            let _guard = drop_counter_test_lock();
            super::reset_sse_events_dropped_count_for_test();
            super::reset_sse_streams_terminated_by_drop_cap_count_for_test();

            let (tx, _rx) = mpsc::channel::<super::SseEvent>(1);
            assert!(
                tx.try_send(super::SseEvent::new("filler")).is_ok(),
                "first send fits in cap=1 channel"
            );

            let mut next_emit_at = std::time::Instant::now();
            // Pre-load consecutive_drops to 1 below the cap so the next
            // Full send pushes us OVER and triggers termination.
            let mut consecutive_drops: u64 = STREAM_MAX_CONSECUTIVE_DROPS - 1;

            let still_alive = super::send_rate_limited_sse(
                &tx,
                super::SseEvent::new("cap-trigger"),
                &mut next_emit_at,
                std::time::Duration::ZERO,
                &mut consecutive_drops,
            )
            .await;

            assert!(
                !still_alive,
                "ft-95fd3: hitting STREAM_MAX_CONSECUTIVE_DROPS must terminate the stream"
            );
            assert_eq!(
                consecutive_drops, STREAM_MAX_CONSECUTIVE_DROPS,
                "ft-95fd3: per-stream local must reflect the cap"
            );
            assert_eq!(
                super::sse_events_dropped_count(),
                1,
                "ft-95fd3: the cap-triggering drop must also bump the event-drop counter"
            );
            assert_eq!(
                super::sse_streams_terminated_by_drop_cap_count(),
                1,
                "ft-95fd3: hitting the cap must bump the stream-termination counter exactly once"
            );
        });
    }

    /// br-ft-95fd3: the two counters are distinct atomics — a bump
    /// of one must not spill into the other. Property-style pin
    /// against a future refactor that accidentally uses the wrong
    /// counter.
    #[test]
    fn drop_counters_are_independent_ft_95fd3() {
        let _guard = drop_counter_test_lock();
        super::reset_sse_events_dropped_count_for_test();
        super::reset_sse_streams_terminated_by_drop_cap_count_for_test();
        assert_eq!(super::sse_events_dropped_count(), 0);
        assert_eq!(super::sse_streams_terminated_by_drop_cap_count(), 0);

        super::record_sse_event_dropped();
        assert_eq!(super::sse_events_dropped_count(), 1);
        assert_eq!(
            super::sse_streams_terminated_by_drop_cap_count(),
            0,
            "ft-95fd3: event-drop bump must NOT spill into termination counter"
        );

        super::record_sse_stream_terminated_by_drop_cap();
        assert_eq!(
            super::sse_events_dropped_count(),
            1,
            "ft-95fd3: termination bump must NOT spill into event-drop counter"
        );
        assert_eq!(super::sse_streams_terminated_by_drop_cap_count(), 1);
    }

    // ── br-ft-bn6qi: clock-anomaly observability ──

    #[test]
    fn epoch_ms_from_post_epoch_returns_positive_no_anomaly_ft_bn6qi() {
        // Sanity: a valid post-epoch timestamp returns a positive
        // ms value and does NOT bump the anomaly counter.
        let _guard = drop_counter_test_lock();
        super::reset_epoch_clock_anomaly_count_for_test();

        // ~2024-01-01 in seconds since epoch.
        let post_epoch = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_704_067_200);
        let ms = super::epoch_ms_from(post_epoch);
        assert!(ms > 0, "post-epoch ms must be positive, got {ms}");
        assert_eq!(
            super::epoch_clock_anomaly_count(),
            0,
            "post-epoch path must NOT bump anomaly counter"
        );
    }

    #[test]
    fn epoch_ms_from_pre_epoch_bumps_counter_ft_bn6qi() {
        // br-ft-bn6qi: a pre-epoch SystemTime triggers the Err
        // branch. The function returns 0 (preserving the scalar
        // contract callers depend on) and bumps the
        // process-global anomaly counter so a single observability
        // scrape surfaces the misconfiguration.
        let _guard = drop_counter_test_lock();
        super::reset_epoch_clock_anomaly_count_for_test();

        let pre_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(100);
        let ms = super::epoch_ms_from(pre_epoch);
        assert_eq!(ms, 0, "pre-epoch must fall back to 0");
        assert_eq!(
            super::epoch_clock_anomaly_count(),
            1,
            "pre-epoch must bump anomaly counter exactly once"
        );

        // A second call must bump the counter again (the contract
        // counts every event, not just first-occurrence).
        let _ = super::epoch_ms_from(pre_epoch);
        assert_eq!(super::epoch_clock_anomaly_count(), 2);
    }

    #[test]
    fn epoch_ms_anomaly_counter_independent_of_drop_counters_ft_bn6qi() {
        // The clock-anomaly counter must not bleed into the SSE
        // drop counters and vice versa.
        let _guard = drop_counter_test_lock();
        super::reset_epoch_clock_anomaly_count_for_test();
        super::reset_sse_events_dropped_count_for_test();
        super::reset_sse_streams_terminated_by_drop_cap_count_for_test();

        let pre_epoch = std::time::UNIX_EPOCH - std::time::Duration::from_secs(50);
        let _ = super::epoch_ms_from(pre_epoch);

        assert_eq!(super::epoch_clock_anomaly_count(), 1);
        assert_eq!(super::sse_events_dropped_count(), 0);
        assert_eq!(super::sse_streams_terminated_by_drop_cap_count(), 0);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,
            ..proptest::test_runner::Config::default()
        })]

        /// br-ft-bn6qi: for any SystemTime drawn from
        /// `UNIX_EPOCH ± offset_secs`, epoch_ms_from returns
        ///   - a NON-NEGATIVE i64 always (the contract)
        ///   - 0 iff the input was strictly before UNIX_EPOCH
        ///     (the fallback is never confused with a real ts=0)
        ///   - bumps the anomaly counter exactly once per pre-
        ///     epoch input, never on a post-epoch input.
        /// Pin the contract against fuzzed inputs so the
        /// fail-loud counter cannot regress to silent-default.
        #[test]
        fn epoch_ms_from_anomaly_iff_pre_epoch_ft_bn6qi(
            offset_secs in -10_000_i64..=10_000_i64,
        ) {
            let _guard = drop_counter_test_lock();
            super::reset_epoch_clock_anomaly_count_for_test();
            let before = super::epoch_clock_anomaly_count();

            let now = if offset_secs >= 0 {
                std::time::UNIX_EPOCH
                    + std::time::Duration::from_secs(offset_secs as u64)
            } else {
                std::time::UNIX_EPOCH
                    - std::time::Duration::from_secs((-offset_secs) as u64)
            };
            let ms = super::epoch_ms_from(now);
            let after = super::epoch_clock_anomaly_count();
            let bumped = after > before;
            let is_pre_epoch = offset_secs < 0;
            proptest::prop_assert_eq!(
                bumped,
                is_pre_epoch,
                "br-ft-bn6qi: counter must bump iff input is pre-epoch; offset_secs={} bumped={} is_pre_epoch={}",
                offset_secs, bumped, is_pre_epoch
            );
            proptest::prop_assert!(ms >= 0, "br-ft-bn6qi: epoch_ms_from must never return negative");
            if is_pre_epoch {
                proptest::prop_assert_eq!(
                    ms, 0,
                    "br-ft-bn6qi: pre-epoch input must produce timestamp=0 (the documented fallback)"
                );
            } else {
                // Post-epoch with offset==0 is exactly UNIX_EPOCH → ms=0,
                // but does NOT bump the counter (already pinned by `bumped`).
                let expected_ms: i64 = (offset_secs as i64) * 1000;
                proptest::prop_assert_eq!(
                    ms, expected_ms,
                    "br-ft-bn6qi: post-epoch ms must equal offset_secs*1000"
                );
            }
        }
    }
}
