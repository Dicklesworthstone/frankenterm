//! Server-Sent Events endpoint surface for Wave 4B migration.

use super::error::json_err;
use super::extractors::{
    parse_pane_filter, redact_json_value, request_runtime_limits, require_event_bus,
};
use super::{
    STREAM_CHANNEL_BUFFER, STREAM_MAX_CONSECUTIVE_DROPS, STREAM_SCHEMA_VERSION, WebRuntimeLimits,
};
use crate::events::{Event, RecvError};
use crate::policy::Redactor;
use crate::runtime_async::stream::Stream;
use crate::runtime_async::{mpsc, sleep, task};
use crate::storage::{
    EventRetentionCheck, EventRetentionStatus, EventStreamPage, EventStreamQuery, StorageHandle,
};
use crate::web_framework::{
    QueryString, Request, Response, StatusCode, WebStreamLifecycle, sse_stream_response,
};
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "sse_delta.rs"]
mod delta;
pub(super) use delta::handle_stream_deltas;

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
fn reset_sse_events_dropped_count_for_test() {
    SSE_EVENTS_DROPPED_COUNT.store(0, Ordering::Relaxed);
}

/// Test helper: reset the stream-termination counter.
#[cfg(test)]
fn reset_sse_streams_terminated_by_drop_cap_count_for_test() {
    SSE_STREAMS_TERMINATED_BY_DROP_CAP_COUNT.store(0, Ordering::Relaxed);
}

/// br-ft-bn6qi: test helper to reset the clock-anomaly counter.
#[cfg(test)]
fn reset_epoch_clock_anomaly_count_for_test() {
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

pub(super) fn parse_stream_max_hz(qs: &QueryString<'_>, runtime_limits: WebRuntimeLimits) -> u64 {
    qs.get("max_hz")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|hz| *hz > 0)
        .unwrap_or(runtime_limits.stream_default_max_hz)
        .min(runtime_limits.stream_max_max_hz)
}

/// Minimum spacing between frames of a stream limited to `max_hz`
/// (4tenz.12): `ceil(1 s / max_hz)` in nanoseconds, as `ft robot
/// watch-events --max-hz` spaces events. Millisecond floor division let
/// rates that do not divide 1000 (3 Hz) run over their limit. A zero rate
/// never gets here (`parse_stream_max_hz` maps it to the default) and is
/// spaced as 1 Hz, not unlimited.
fn stream_min_interval(max_hz: u64) -> Duration {
    Duration::from_nanos(1_000_000_000_u64.div_ceil(max_hz.max(1)))
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

struct SseByteStream {
    rx: Option<mpsc::Receiver<SseEvent>>,
    recv_state: SseRecvState,
    // The body owns this future directly: no detached producer task, delayed
    // receiver-closed polling, or unobserved JoinHandle survives body Drop.
    producer: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
}

struct SseRecvState {
    cx: crate::cx::Cx,
}

impl SseRecvState {
    fn new(cx: crate::cx::Cx) -> Self {
        // Runtime-backed connection context, not server shutdown authority.
        // The separate lifecycle waiter supplies the latter explicitly.
        Self { cx }
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
    fn new(
        cx: crate::cx::Cx,
        rx: mpsc::Receiver<SseEvent>,
        producer: Pin<Box<dyn Future<Output = ()> + Send>>,
        shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    ) -> Self {
        Self {
            rx: Some(rx),
            recv_state: SseRecvState::new(cx),
            producer: Some(producer),
            shutdown,
        }
    }

    fn finish(&mut self) -> Poll<Option<Vec<u8>>> {
        self.producer = None;
        self.rx = None;
        Poll::Ready(None)
    }
}

impl Stream for SseByteStream {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.rx.is_none() || this.shutdown.as_mut().poll(cx).is_ready() {
            return this.finish();
        }
        if let Some(producer) = this.producer.as_mut()
            && producer.as_mut().poll(cx).is_ready()
        {
            this.producer = None;
        }
        // A signal observed during production wins over buffered output.
        if this.shutdown.as_mut().poll(cx).is_ready() {
            return this.finish();
        }
        let Some(rx) = this.rx.as_mut() else {
            return Poll::Ready(None);
        };
        match this.recv_state.poll_recv(rx, cx) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(event.to_bytes())),
            Poll::Ready(None) => this.finish(),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// End a stream before its event identifier can wrap or be reused.
fn advance_stream_sequence(seq: &mut u64) -> bool {
    let Some(next) = seq.checked_add(1) else {
        tracing::warn!(target: "wa.web", "web_stream_sequence_exhausted");
        return false;
    };
    *seq = next;
    true
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

/// Like [`frame_to_sse`], but the SSE `id` is an epoch- and pane-scoped
/// durable checkpoint. Live-only events and diagnostics omit it, preserving
/// the last durable boundary; the frame's own `seq` stays per-connection.
fn frame_to_sse_with_event_id(
    event_type: &'static str,
    event_id: Option<String>,
    frame: serde_json::Value,
) -> Option<SseEvent> {
    serde_json::to_string(&frame)
        .inspect_err(|e| tracing::warn!(error = %e, event_type, "SSE frame serialization failed"))
        .ok()
        .map(|body| {
            let event = SseEvent::new(body).event_type(event_type);
            match event_id {
                Some(id) => event.id(id),
                None => event,
            }
        })
}

/// The persisted event id carried by a bus event, if it has one.
fn persisted_event_id(event: &Event) -> Option<i64> {
    match event {
        Event::PatternDetected { event_id, .. } => *event_id,
        _ => None,
    }
}

/// Bound memory per replay query, not the amount of recoverable history.
const EVENT_STREAM_REPLAY_PAGE_SIZE: usize = 256;
/// Return to the live-only subscriber between bounded durable scan turns.
const EVENT_STREAM_REPLAY_PAGES_PER_TURN: usize = 4;
const EVENT_CURSOR_PREFIX: &str = "event-v1:";
const INVALID_EVENT_CURSOR: &str = "invalid_event_cursor";

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventResumeCursor {
    epoch: String,
    after_id: i64,
}

impl EventResumeCursor {
    fn encode(&self, pane_filter: Option<u64>) -> String {
        match pane_filter {
            Some(pane) => format!(
                "{EVENT_CURSOR_PREFIX}{}:pane:{pane}:{}",
                self.epoch, self.after_id
            ),
            None => format!("{EVENT_CURSOR_PREFIX}{}:all:{}", self.epoch, self.after_id),
        }
    }
}

fn event_cursor_decimal(value: &str) -> Option<u64> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn parse_event_cursor(
    value: &str,
    pane_filter: Option<u64>,
) -> Result<EventResumeCursor, &'static str> {
    if value.len() > 128 {
        return Err(INVALID_EVENT_CURSOR);
    }
    let suffix = value
        .strip_prefix(EVENT_CURSOR_PREFIX)
        .ok_or(INVALID_EVENT_CURSOR)?;
    let (epoch, scoped_id) = suffix.split_once(':').ok_or(INVALID_EVENT_CURSOR)?;
    if epoch.len() != 32
        || !epoch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(INVALID_EVENT_CURSOR);
    }
    let (scope, id) = if let Some(id) = scoped_id.strip_prefix("all:") {
        (None, id)
    } else if let Some(scoped_id) = scoped_id.strip_prefix("pane:") {
        let (pane, id) = scoped_id.split_once(':').ok_or(INVALID_EVENT_CURSOR)?;
        (
            Some(event_cursor_decimal(pane).ok_or(INVALID_EVENT_CURSOR)?),
            id,
        )
    } else {
        return Err(INVALID_EVENT_CURSOR);
    };
    let after_id = event_cursor_decimal(id)
        .and_then(|id| i64::try_from(id).ok())
        .ok_or(INVALID_EVENT_CURSOR)?;
    if scope != pane_filter {
        return Err("event_cursor_scope_mismatch");
    }
    Ok(EventResumeCursor {
        epoch: epoch.to_owned(),
        after_id,
    })
}

/// A valid reconnect header supersedes the URL's bootstrap token. A nonempty
/// malformed header, a numeric legacy cursor, or a changed pane scope cannot
/// silently become a fresh subscription or adopt the database's current epoch.
fn parse_event_resume(
    last_event_id: Option<&[u8]>,
    cursor: Option<&str>,
    legacy_since_id: Option<&str>,
    pane_filter: Option<u64>,
) -> Result<Option<EventResumeCursor>, &'static str> {
    if let Some(bytes) = last_event_id {
        if bytes.len() > 128 || bytes.iter().any(|&byte| matches!(byte, b'\r' | b'\n' | 0)) {
            return Err(INVALID_EVENT_CURSOR);
        }
        let value = std::str::from_utf8(bytes)
            .map_err(|_| INVALID_EVENT_CURSOR)?
            .trim_matches([' ', '\t']);
        if !value.is_empty() {
            return parse_event_cursor(value, pane_filter).map(Some);
        }
    }
    if legacy_since_id.is_some() {
        return Err(INVALID_EVENT_CURSOR);
    }
    cursor
        .map(|value| parse_event_cursor(value, pane_filter))
        .transpose()
}

/// The scan position may include deliberately filtered rows. The admitted
/// position changes only after a durable frame enters the owned response
/// queue. Neither is persisted on the server: reconnect starts at the client's
/// last parsed SSE id, so a dropped socket cannot acknowledge buffered rows.
struct EventReplayCursor {
    epoch: String,
    scan_after_id: i64,
    admitted_after_id: i64,
}

impl EventReplayCursor {
    fn new(cursor: EventResumeCursor) -> Self {
        Self {
            epoch: cursor.epoch,
            scan_after_id: cursor.after_id,
            admitted_after_id: cursor.after_id,
        }
    }

    fn token_at(&self, pane_filter: Option<u64>, after_id: i64) -> String {
        EventResumeCursor {
            epoch: self.epoch.clone(),
            after_id,
        }
        .encode(pane_filter)
    }
}

struct CheckedEventPage {
    page: EventStreamPage,
    check: EventRetentionCheck,
}

fn event_page_checked_through(page: &EventStreamPage) -> i64 {
    if page.events.len() >= EVENT_STREAM_REPLAY_PAGE_SIZE {
        page.events
            .last()
            .map_or(page.retention.max_event_id, |event| event.id)
    } else {
        page.retention.max_event_id
    }
}

async fn read_event_replay_page(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    query: EventStreamQuery,
    epoch: &str,
) -> crate::Result<CheckedEventPage> {
    let after_id = query.after_id.unwrap_or(0);
    let page = storage.get_events_stream_page_with_cx(cx, query).await?;
    let through_id = event_page_checked_through(&page).max(after_id);
    let check = storage
        .check_event_retention_in_epoch_with_cx(cx, after_id, through_id, epoch)
        .await?;
    Ok(CheckedEventPage { page, check })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventReplayProgress {
    CaughtUp,
    More,
    Stopped,
}

/// A persisted cursor must never advance over a channel-full drop. Keep the
/// frame owned by the body while waiting for capacity, using the connection's
/// context for both waits. Body drop and server shutdown still dispose of the
/// producer through `SseByteStream`; no detached retry task is created.
async fn send_replay_sse(
    cx: &crate::cx::Cx,
    tx: &mpsc::Sender<SseEvent>,
    event: SseEvent,
    next_emit_at: &mut Instant,
    min_interval: Duration,
) -> bool {
    if cx.checkpoint().is_err() {
        return false;
    }
    let now = Instant::now();
    if *next_emit_at > now
        && crate::runtime_async::sleep_with_cx(cx, *next_emit_at - now)
            .await
            .is_err()
    {
        return false;
    }
    if tx.send(cx, event).await.is_err() {
        return false;
    }
    *next_emit_at = Instant::now()
        .checked_add(min_interval)
        .unwrap_or_else(Instant::now);
    true
}

/// Send a finite failure class without replacing Last-Event-ID. The caller
/// then ends the stream, so reconnect retries the undelivered history instead
/// of allowing a newer live event to conceal the failed replay.
async fn emit_event_replay_error(
    cx: &crate::cx::Cx,
    tx: &mpsc::Sender<SseEvent>,
    seq: &mut u64,
    next_emit_at: &mut Instant,
    min_interval: Duration,
    after_id: i64,
    code: &'static str,
) {
    if !advance_stream_sequence(seq) {
        return;
    }
    let frame = make_stream_frame(
        "events",
        "error",
        *seq,
        json!({ "code": code, "resume_after_id": after_id }),
    );
    if let Some(event) = frame_to_sse_with_event_id("error", None, frame) {
        let _ = send_replay_sse(cx, tx, event, next_emit_at, min_interval).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_event_cursor_discontinuity(
    cx: &crate::cx::Cx,
    tx: &mpsc::Sender<SseEvent>,
    seq: &mut u64,
    next_emit_at: &mut Instant,
    min_interval: Duration,
    cursor: &EventReplayCursor,
    pane_filter: Option<u64>,
    check: &EventRetentionCheck,
) {
    let reason = match check.status {
        EventRetentionStatus::CompleteNoPruning => return,
        EventRetentionStatus::Pruned { .. } => "retention_pruned",
        EventRetentionStatus::LegacyHistoryUnavailable { .. } => "legacy_history_unavailable",
        EventRetentionStatus::CursorEpochMismatch => "cursor_epoch_mismatch",
        EventRetentionStatus::CursorAheadOfHighWater { .. } => "cursor_ahead_of_high_water",
    };
    if !advance_stream_sequence(seq) {
        return;
    }
    let mut data = json!({
        "code": "event_cursor_discontinuity",
        "terminal": true,
        "reason": reason,
        "resume_cursor": cursor.token_at(pane_filter, cursor.admitted_after_id),
        "checked_through_id": check.checked_through_id,
        "current_cursor_epoch": check.snapshot.cursor_epoch,
        "evidence_from_event_id": check.snapshot.evidence_from_event_id,
        "max_event_id": check.snapshot.max_event_id,
    });
    if let EventRetentionStatus::Pruned { interval } = &check.status {
        data["deleted_interval"] = json!({
            "start_id": interval.start_id,
            "end_id": interval.end_id,
        });
    }
    let frame = make_stream_frame("events", "cursor_discontinuity", *seq, data);
    if let Some(event) = frame_to_sse_with_event_id("cursor_discontinuity", None, frame) {
        let _ = send_replay_sse(cx, tx, event, next_emit_at, min_interval).await;
    }
}

/// Drain retained detections in ascending id order with bounded pages. The
/// fetch seam is shared by production storage and failure-injection tests;
/// tests exercise the actual framing, redaction, cursor and backpressure path.
#[allow(clippy::too_many_arguments)]
async fn replay_event_frames<F, Fut>(
    cx: &crate::cx::Cx,
    mut fetch: F,
    pane_filter: Option<u64>,
    cursor: &mut EventReplayCursor,
    redactor: &Redactor,
    tx: &mpsc::Sender<SseEvent>,
    seq: &mut u64,
    next_emit_at: &mut Instant,
    min_interval: Duration,
) -> EventReplayProgress
where
    F: FnMut(EventStreamQuery) -> Fut,
    Fut: Future<Output = crate::Result<CheckedEventPage>>,
{
    for _ in 0..EVENT_STREAM_REPLAY_PAGES_PER_TURN {
        if cx.checkpoint().is_err() {
            return EventReplayProgress::Stopped;
        }
        let after_id = cursor.scan_after_id;
        let query = EventStreamQuery {
            after_id: Some(after_id),
            limit: Some(EVENT_STREAM_REPLAY_PAGE_SIZE),
            pane_id: pane_filter,
            ..EventStreamQuery::default()
        };
        let CheckedEventPage { page, check } = match fetch(query).await {
            Ok(page) => page,
            Err(_) => {
                emit_event_replay_error(
                    cx,
                    tx,
                    seq,
                    next_emit_at,
                    min_interval,
                    cursor.admitted_after_id,
                    "event_replay_storage_error",
                )
                .await;
                return EventReplayProgress::Stopped;
            }
        };
        if !matches!(check.status, EventRetentionStatus::CompleteNoPruning) {
            emit_event_cursor_discontinuity(
                cx,
                tx,
                seq,
                next_emit_at,
                min_interval,
                cursor,
                pane_filter,
                &check,
            )
            .await;
            return EventReplayProgress::Stopped;
        }
        let page_len = page.events.len();
        let checked_through = event_page_checked_through(&page).max(after_id);
        // Validate the whole page before emitting any of it. A broken storage
        // ordering/filter contract must not move the cursor past unseen rows.
        let mut previous = after_id;
        let valid_page = page_len <= EVENT_STREAM_REPLAY_PAGE_SIZE
            && page.retention.cursor_epoch == cursor.epoch
            && check.snapshot.cursor_epoch == cursor.epoch
            && check.snapshot.max_event_id >= page.retention.max_event_id
            && check.snapshot.generation >= page.retention.generation
            && check.requested_after_id == after_id
            && check.requested_through_id == checked_through
            && check.checked_through_id == checked_through
            && page.retention.max_event_id >= checked_through
            && page.events.iter().all(|row| {
                let valid = row.id > previous
                    && row.id <= page.retention.max_event_id
                    && pane_filter.is_none_or(|pane_id| row.pane_id == pane_id);
                previous = row.id;
                valid
            });
        if !valid_page {
            emit_event_replay_error(
                cx,
                tx,
                seq,
                next_emit_at,
                min_interval,
                cursor.admitted_after_id,
                "event_replay_invalid_page",
            )
            .await;
            return EventReplayProgress::Stopped;
        }
        for stored in page.events {
            let event_id = stored.id;
            let event = super::server::stored_event_to_bus_event(stored);
            let Ok(mut event_json) = serde_json::to_value(&event) else {
                emit_event_replay_error(
                    cx,
                    tx,
                    seq,
                    next_emit_at,
                    min_interval,
                    cursor.admitted_after_id,
                    "event_replay_serialization_error",
                )
                .await;
                return EventReplayProgress::Stopped;
            };
            redact_json_value(&mut event_json, redactor);
            if !advance_stream_sequence(seq) {
                return EventReplayProgress::Stopped;
            }
            let frame = make_stream_frame(
                "events",
                "event",
                *seq,
                json!({ "event": event_json, "replayed": true, "delivery": "durable" }),
            );
            let Some(event) = frame_to_sse_with_event_id(
                "event",
                Some(cursor.token_at(pane_filter, event_id)),
                frame,
            ) else {
                return EventReplayProgress::Stopped;
            };
            if !send_replay_sse(cx, tx, event, next_emit_at, min_interval).await {
                return EventReplayProgress::Stopped;
            }
            cursor.admitted_after_id = event_id;
        }
        // An empty/short filtered page proves the entire scanned interval;
        // all its missing rows were deliberately excluded by the SQL filter.
        // This does not manufacture an SSE id for a live-only frame.
        cursor.scan_after_id = checked_through;
        tracing::debug!(
            target: "wa.web",
            scanned_after_id = after_id,
            scanned_through_id = checked_through,
            admitted_through_id = cursor.admitted_after_id,
            rows = page_len,
            "event_stream_durable_page"
        );
        if page_len < EVENT_STREAM_REPLAY_PAGE_SIZE {
            return EventReplayProgress::CaughtUp;
        }
        task::yield_now().await;
    }
    EventReplayProgress::More
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

pub(super) fn handle_stream_events(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);
    let pane_filter = match parse_pane_filter(&qs) {
        Ok(pane_id) => pane_id,
        Err(response) => return Box::pin(async move { response }),
    };
    let runtime_limits = request_runtime_limits(req);
    let max_hz = parse_stream_max_hz(&qs, runtime_limits);
    let channel = match parse_event_stream_channel(&qs) {
        Ok(channel) => channel,
        Err(resp) => return Box::pin(async move { resp }),
    };
    let result = require_event_bus(req);
    let lifecycle = req.get_extension::<WebStreamLifecycle>().cloned();
    let resume = match parse_event_resume(
        req.headers().get("last-event-id"),
        qs.get("cursor"),
        qs.get("since_id"),
        pane_filter,
    ) {
        Ok(resume) => resume,
        Err(code) => {
            return Box::pin(async move {
                json_err(
                    StatusCode::BAD_REQUEST,
                    code,
                    "Use a complete event-v1 cursor from this stream with the same pane filter",
                )
            });
        }
    };
    let replay_storage = req
        .get_extension::<super::middleware::AppState>()
        .and_then(|state| state.storage.clone());
    let wants_detections = matches!(
        channel,
        EventStreamChannel::All | EventStreamChannel::Detections
    );

    Box::pin(async move {
        let (event_bus, redactor) = match result {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        if resume.is_some() && !wants_detections {
            return json_err(
                StatusCode::BAD_REQUEST,
                "event_cursor_channel_mismatch",
                "Durable event cursors require the all or detections channel",
            );
        }
        if wants_detections && replay_storage.is_none() {
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "event_replay_unavailable",
                "Persisted event storage is required for all and detections streams",
            );
        }

        let Some(lifecycle) = lifecycle else {
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "stream_lifecycle_unavailable",
                "Stream lifecycle is unavailable",
            );
        };
        let Some(stream_cx) = crate::cx::Cx::current() else {
            return json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "stream_context_unavailable",
                "Stream runtime context is unavailable",
            );
        };
        let shutdown = lifecycle.shutdown_waiter(&stream_cx);
        let recv_cx = stream_cx.clone();

        let mut subscriber = match channel {
            EventStreamChannel::All => event_bus.subscribe(),
            EventStreamChannel::Deltas => event_bus.subscribe_deltas(),
            EventStreamChannel::Detections => event_bus.subscribe_detections(),
            EventStreamChannel::Signals => event_bus.subscribe_signals(),
        };

        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_BUFFER);
        let producer = async move {
            let child_cx = stream_cx;
            let min_interval = stream_min_interval(max_hz);
            let mut next_emit_at = Instant::now();
            let mut seq = 0_u64;
            let durable_storage = if wants_detections {
                replay_storage.as_ref()
            } else {
                None
            };
            // Subscribe before capturing the initial high-water mark. Fresh
            // connections begin there, while every later detection is read
            // from storage regardless of notification order or loss.
            let mut cursor = if let Some(storage) = durable_storage {
                let initial = match resume {
                    Some(cursor) => cursor,
                    None => match storage
                        .get_event_retention_snapshot_with_cx(&child_cx)
                        .await
                    {
                        Ok(snapshot) => EventResumeCursor {
                            epoch: snapshot.cursor_epoch,
                            after_id: snapshot.max_event_id,
                        },
                        Err(_) => {
                            emit_event_replay_error(
                                &child_cx,
                                &tx,
                                &mut seq,
                                &mut next_emit_at,
                                min_interval,
                                0,
                                "event_replay_storage_error",
                            )
                            .await;
                            return;
                        }
                    },
                };
                let cursor = EventReplayCursor::new(initial);
                let check = match storage
                    .check_event_retention_in_epoch_with_cx(
                        &child_cx,
                        cursor.scan_after_id,
                        cursor.scan_after_id,
                        &cursor.epoch,
                    )
                    .await
                {
                    Ok(check) => check,
                    Err(_) => {
                        emit_event_replay_error(
                            &child_cx,
                            &tx,
                            &mut seq,
                            &mut next_emit_at,
                            min_interval,
                            cursor.admitted_after_id,
                            "event_replay_storage_error",
                        )
                        .await;
                        return;
                    }
                };
                if !matches!(check.status, EventRetentionStatus::CompleteNoPruning) {
                    emit_event_cursor_discontinuity(
                        &child_cx,
                        &tx,
                        &mut seq,
                        &mut next_emit_at,
                        min_interval,
                        &cursor,
                        pane_filter,
                        &check,
                    )
                    .await;
                    return;
                }
                Some(cursor)
            } else {
                None
            };

            if !advance_stream_sequence(&mut seq) {
                return;
            }
            let ready_cursor = cursor
                .as_ref()
                .map(|cursor| cursor.token_at(pane_filter, cursor.admitted_after_id));
            let ready = make_stream_frame(
                "events",
                "ready",
                seq,
                json!({
                    "channel": format!("{channel:?}").to_lowercase(),
                    "max_hz": max_hz,
                    "pane_id": pane_filter,
                    "cursor": ready_cursor,
                    "durable_detections": durable_storage.is_some(),
                }),
            );
            // This is a deliberate durable checkpoint, including on a fresh
            // connection with no detections yet. A reconnect must reuse this
            // boundary instead of silently taking a later high-water mark.
            if let Some(event) = frame_to_sse_with_event_id("ready", ready_cursor, ready) {
                if !send_replay_sse(&child_cx, &tx, event, &mut next_emit_at, min_interval).await {
                    return;
                }
            }

            let keepalive_interval =
                Duration::from_secs(runtime_limits.stream_keepalive_secs.max(1));
            let mut last_catchup = Instant::now();
            let mut catchup_pending = cursor.is_some();
            let mut bus_closed = false;
            loop {
                if child_cx.checkpoint().is_err() {
                    break;
                }
                // A lost persistence wakeup must also be repaired while
                // unrelated bus traffic is continuous, not only when the
                // subscriber becomes idle. Do not reset this deadline for
                // ephemeral events or already-replayed notifications.
                if (catchup_pending || last_catchup.elapsed() >= keepalive_interval)
                    && let (Some(storage), Some(cursor)) = (durable_storage, cursor.as_mut())
                {
                    let epoch = cursor.epoch.clone();
                    match replay_event_frames(
                        &child_cx,
                        |query| read_event_replay_page(&child_cx, storage, query, &epoch),
                        pane_filter,
                        cursor,
                        &redactor,
                        &tx,
                        &mut seq,
                        &mut next_emit_at,
                        min_interval,
                    )
                    .await
                    {
                        EventReplayProgress::CaughtUp => catchup_pending = false,
                        EventReplayProgress::More => catchup_pending = true,
                        EventReplayProgress::Stopped => break,
                    }
                    last_catchup = Instant::now();
                }
                if bus_closed {
                    if !catchup_pending {
                        break;
                    }
                    task::yield_now().await;
                    continue;
                }
                let wait = if durable_storage.is_some() {
                    keepalive_interval.saturating_sub(last_catchup.elapsed())
                } else {
                    keepalive_interval
                };
                // A bounded replay turn yields to one live-only signal, but
                // never sleeps waiting for the bus while history remains.
                let recv_result = if catchup_pending {
                    match subscriber.try_recv() {
                        Some(result) => Some(result),
                        None => {
                            task::yield_now().await;
                            continue;
                        }
                    }
                } else {
                    crate::runtime_async::timeout_with_cx(
                        &child_cx,
                        wait,
                        subscriber.recv_cx(&child_cx),
                    )
                    .await
                    .ok()
                };

                match recv_result {
                    Some(Ok(event)) => {
                        // Publication can race across persistence callers.
                        // Persisted notifications only wake the ordered
                        // storage scan, even when their pane is filtered out.
                        if let Some(event_id) = persisted_event_id(&event)
                            && let Some(cursor) = cursor.as_ref()
                        {
                            if event_id > cursor.scan_after_id {
                                catchup_pending = true;
                            }
                            continue;
                        }
                        if !event_matches_pane(&event, pane_filter) {
                            continue;
                        }

                        let mut event_json = serde_json::to_value(&event).unwrap_or_else(|_| {
                            json!({
                                "error": "event_serialization_failed"
                            })
                        });
                        redact_json_value(&mut event_json, &redactor);

                        if !advance_stream_sequence(&mut seq) {
                            break;
                        }
                        let frame = make_stream_frame(
                            "events",
                            "event",
                            seq,
                            json!({ "event": event_json, "delivery": "live_only" }),
                        );
                        if let Some(event) = frame_to_sse_with_event_id("event", None, frame) {
                            if !send_replay_sse(
                                &child_cx,
                                &tx,
                                event,
                                &mut next_emit_at,
                                min_interval,
                            )
                            .await
                            {
                                break;
                            }
                        }
                    }
                    Some(Err(RecvError::Lagged { missed_count })) => {
                        if !advance_stream_sequence(&mut seq) {
                            break;
                        }
                        let frame = make_stream_frame(
                            "events",
                            "lag",
                            seq,
                            json!({ "missed_count": missed_count, "live_only": true }),
                        );
                        if let Some(event) = frame_to_sse_with_event_id("lag", None, frame) {
                            if !send_replay_sse(
                                &child_cx,
                                &tx,
                                event,
                                &mut next_emit_at,
                                min_interval,
                            )
                            .await
                            {
                                break;
                            }
                        }
                        catchup_pending = cursor.is_some();
                    }
                    Some(Err(RecvError::Cancelled)) => break,
                    Some(Err(RecvError::Closed)) => {
                        if !advance_stream_sequence(&mut seq) {
                            break;
                        }
                        let frame = make_stream_frame(
                            "events",
                            "lag",
                            seq,
                            json!({ "reason": "event_bus_closed", "live_only": true }),
                        );
                        if let Some(event) = frame_to_sse_with_event_id("lag", None, frame)
                            && !send_replay_sse(
                                &child_cx,
                                &tx,
                                event,
                                &mut next_emit_at,
                                min_interval,
                            )
                            .await
                        {
                            break;
                        }
                        // Finish bounded durable catchup before closing;
                        // no ephemeral frame replaces the durable cursor.
                        bus_closed = true;
                        catchup_pending = cursor.is_some();
                    }
                    None => {
                        // The due scan at the top of the loop repairs a
                        // committed row even if its publisher died before
                        // the final bus wakeup. Keepalives carry no cursor.
                        match tx
                            .try_send(SseEvent::comment("keepalive"))
                            .map_err(mpsc::TrySendError::from)
                        {
                            Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                            Err(mpsc::TrySendError::Closed(_)) => break,
                        }
                    }
                }
            }
        };

        SseResponse::new(SseByteStream::new(
            recv_cx,
            rx,
            Box::pin(producer),
            shutdown,
        ))
        .into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Event, EventStreamChannel, STREAM_MAX_CONSECUTIVE_DROPS, SseEvent,
        frame_to_sse_with_event_id, mpsc, parse_event_stream_channel, persisted_event_id,
        stream_min_interval,
    };
    use crate::runtime_async::CompatRuntime;
    use crate::web_framework::QueryString;
    use serde_json::json;

    struct ProducerDropProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for ProducerDropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn owned_stream_fixture(
        shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ) -> (
        super::SseByteStream,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (tx, rx) = mpsc::channel(2);
        tx.try_send(SseEvent::comment("synthetic queued item"))
            .expect("queue fixture item");
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe = ProducerDropProbe(std::sync::Arc::clone(&drops));
        let producer = Box::pin(async move {
            let _owned = (tx, probe);
            std::future::pending::<()>().await;
        });
        (
            super::SseByteStream::new(crate::cx::for_request(), rx, producer, shutdown),
            drops,
        )
    }

    #[test]
    fn stream_shutdown_before_first_poll_discards_queue_and_drops_producer_once() {
        use crate::runtime_async::stream::Stream;
        use std::pin::Pin;
        use std::sync::atomic::Ordering;
        use std::task::{Context, Poll, Waker};

        let (mut stream, drops) = owned_stream_fixture(Box::pin(std::future::ready(())));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(None));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(None));
        drop(stream);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn response_drop_disposes_parked_producer_without_a_detached_task() {
        use crate::runtime_async::stream::Stream;
        use std::pin::Pin;
        use std::sync::atomic::Ordering;
        use std::task::{Context, Poll, Waker};

        let (mut stream, drops) = owned_stream_fixture(Box::pin(std::future::pending()));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(Some(_))
        ));
        assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(stream);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn real_lifecycle_signal_wakes_and_ends_parked_body() {
        use crate::runtime_async::stream::Stream;
        use crate::web_framework::WebStreamLifecycle;
        use std::pin::Pin;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll, Wake, Waker};

        struct WakeCount(AtomicUsize);
        impl Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let lifecycle = WebStreamLifecycle::new();
        let waiter = lifecycle.shutdown_waiter(&crate::cx::for_request());
        let (mut stream, drops) = owned_stream_fixture(waiter);
        let notifications = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&notifications));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(Some(_))
        ));
        assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let before_signal = notifications.0.load(Ordering::SeqCst);
        lifecycle.signal_shutdown();
        assert!(
            notifications.0.load(Ordering::SeqCst) > before_signal,
            "shutdown must wake the response's registered task"
        );
        assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(None));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        lifecycle.signal_shutdown();
        assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn lifecycle_signal_during_producer_poll_wins_over_queued_output() {
        use crate::runtime_async::stream::Stream;
        use crate::web_framework::WebStreamLifecycle;
        use std::pin::Pin;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll, Waker};

        let lifecycle = WebStreamLifecycle::new();
        let cx = crate::cx::for_request();
        let waiter = lifecycle.shutdown_waiter(&cx);
        let (tx, rx) = mpsc::channel(2);
        let drops = Arc::new(AtomicUsize::new(0));
        let probe = ProducerDropProbe(Arc::clone(&drops));
        let producer = Box::pin(async move {
            let _probe = probe;
            tx.try_send(SseEvent::comment("must not escape shutdown"))
                .expect("queue available");
            lifecycle.signal_shutdown();
            std::future::pending::<()>().await;
        });
        let mut stream = super::SseByteStream::new(cx, rx, producer, waiter);
        let mut poll_cx = Context::from_waker(Waker::noop());
        assert_eq!(
            Pin::new(&mut stream).poll_next(&mut poll_cx),
            Poll::Ready(None)
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            Pin::new(&mut stream).poll_next(&mut poll_cx),
            Poll::Ready(None)
        );
    }

    fn default_limits() -> super::super::WebRuntimeLimits {
        super::super::resolve_runtime_limits(None)
    }

    fn replay_fixture(id: i64) -> crate::storage::StoredEvent {
        crate::storage::StoredEvent {
            id,
            pane_id: 7,
            rule_id: "codex.usage.reached".into(),
            agent_type: "codex".into(),
            event_type: "usage.reached".into(),
            severity: "warning".into(),
            confidence: 1.0,
            extracted: None,
            matched_text: Some("captured detection".into()),
            segment_id: None,
            detected_at: 1,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }
    }

    const TEST_EVENT_EPOCH: &str = "0123456789abcdef0123456789abcdef";

    fn replay_cursor(after_id: i64) -> super::EventReplayCursor {
        super::EventReplayCursor::new(super::EventResumeCursor {
            epoch: TEST_EVENT_EPOCH.to_owned(),
            after_id,
        })
    }

    fn replay_token(after_id: i64, pane_filter: Option<u64>) -> String {
        replay_cursor(after_id).token_at(pane_filter, after_id)
    }

    fn checked_fixture_page(
        rows: Vec<crate::storage::StoredEvent>,
        after_id: i64,
        high_water: i64,
    ) -> super::CheckedEventPage {
        let snapshot = crate::storage::EventRetentionSnapshot {
            cursor_epoch: TEST_EVENT_EPOCH.to_owned(),
            legacy_history_complete: true,
            generation: 0,
            evidence_from_event_id: 0,
            max_event_id: high_water,
            deleted_event_count: 0,
            last_deleted_at: None,
        };
        let page = crate::storage::EventStreamPage {
            events: rows,
            retention: snapshot.clone(),
        };
        let through_id = super::event_page_checked_through(&page).max(after_id);
        super::CheckedEventPage {
            page,
            check: crate::storage::EventRetentionCheck {
                requested_after_id: after_id,
                requested_through_id: through_id,
                checked_through_id: through_id,
                snapshot,
                status: crate::storage::EventRetentionStatus::CompleteNoPruning,
            },
        }
    }

    /// Framing/backpressure fixtures model an unpruned store. Supply its
    /// retention evidence and drive all bounded turns; retention failures
    /// and scan/admission separation are exercised directly below.
    #[allow(clippy::too_many_arguments)]
    async fn replay_fixture_frames<F, Fut>(
        cx: &crate::cx::Cx,
        mut fetch: F,
        pane_filter: Option<u64>,
        after_id: &mut i64,
        redactor: &crate::policy::Redactor,
        tx: &mpsc::Sender<SseEvent>,
        seq: &mut u64,
        next_emit_at: &mut std::time::Instant,
        min_interval: std::time::Duration,
    ) -> bool
    where
        F: FnMut(crate::storage::EventStreamQuery) -> Fut,
        Fut: std::future::Future<Output = crate::Result<Vec<crate::storage::StoredEvent>>>,
    {
        let mut cursor = replay_cursor(*after_id);
        loop {
            let progress = super::replay_event_frames(
                cx,
                |query| {
                    let after_id = query.after_id.unwrap();
                    let rows = fetch(query);
                    async move {
                        let rows = rows.await?;
                        let high_water = rows.iter().map(|row| row.id).max().unwrap_or(after_id);
                        Ok(checked_fixture_page(rows, after_id, high_water))
                    }
                },
                pane_filter,
                &mut cursor,
                redactor,
                tx,
                seq,
                next_emit_at,
                min_interval,
            )
            .await;
            *after_id = cursor.admitted_after_id;
            match progress {
                super::EventReplayProgress::CaughtUp => return true,
                super::EventReplayProgress::Stopped => return false,
                super::EventReplayProgress::More => {}
            }
        }
    }

    fn drain_replay_frames(mut rx: mpsc::Receiver<SseEvent>) -> Vec<SseEvent> {
        let cx = crate::cx::for_request();
        let mut poll_cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut frames = Vec::new();
        loop {
            match rx.poll_recv(&cx, &mut poll_cx) {
                std::task::Poll::Ready(Ok(frame)) => frames.push(frame),
                std::task::Poll::Ready(Err(_)) => return frames,
                std::task::Poll::Pending => panic!("sender must be dropped before draining"),
            }
        }
    }

    #[test]
    fn event_resume_requires_epoch_scope_and_an_authoritative_valid_header() {
        for pane in [None, Some(0), Some(7), Some(u64::MAX)] {
            for id in [0, 42, i64::MAX] {
                let token = replay_token(id, pane);
                let parsed = super::parse_event_cursor(&token, pane).unwrap();
                assert_eq!(parsed.epoch, TEST_EVENT_EPOCH);
                assert_eq!(parsed.after_id, id);
                assert_eq!(parsed.encode(pane), token);
            }
        }
        let bootstrap = replay_token(3, Some(7));
        let reconnect = replay_token(9, Some(7));
        let padded = format!(" \t{reconnect}\t ");
        let parsed = super::parse_event_resume(
            Some(padded.as_bytes()),
            Some(&bootstrap),
            Some("3"),
            Some(7),
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.after_id, 9, "valid header overrides the bootstrap URL");
        assert_eq!(
            super::parse_event_resume(Some(b" \t "), Some(&bootstrap), None, Some(7))
                .unwrap()
                .unwrap()
                .after_id,
            3
        );
        assert_eq!(
            super::parse_event_resume(None, None, None, None).unwrap(),
            None
        );
        assert_eq!(
            super::parse_event_cursor(&bootstrap, None),
            Err("event_cursor_scope_mismatch")
        );
        assert_eq!(
            super::parse_event_cursor(&bootstrap, Some(8)),
            Err("event_cursor_scope_mismatch")
        );
        for malformed in [
            "9".to_owned(),
            format!("event-v1:{TEST_EVENT_EPOCH}:all:01"),
            format!("event-v1:{TEST_EVENT_EPOCH}:all:-1"),
            format!("event-v1:{TEST_EVENT_EPOCH}:all:9223372036854775808"),
            format!("event-v1:{TEST_EVENT_EPOCH}:pane:18446744073709551616:1"),
            format!("event-v1:{TEST_EVENT_EPOCH}:all:1:2"),
            format!("event-v1:{}:all:1", TEST_EVENT_EPOCH.to_uppercase()),
            format!("delta-v1:{TEST_EVENT_EPOCH}:all:1"),
            format!("{reconnect}\n"),
            format!("\0{reconnect}"),
            "x".repeat(129),
        ] {
            assert_eq!(
                super::parse_event_resume(
                    Some(malformed.as_bytes()),
                    Some(&bootstrap),
                    None,
                    Some(7),
                ),
                Err(super::INVALID_EVENT_CURSOR),
                "malformed header must never fall back: {malformed:?}"
            );
        }
        assert_eq!(
            super::parse_event_resume(Some(&[0xff]), Some(&bootstrap), None, Some(7)),
            Err(super::INVALID_EVENT_CURSOR)
        );
        assert_eq!(
            super::parse_event_resume(None, Some(&bootstrap), Some("3"), Some(7)),
            Err(super::INVALID_EVENT_CURSOR)
        );
        assert_eq!(
            super::parse_event_resume(None, Some("9"), None, None),
            Err(super::INVALID_EVENT_CURSOR)
        );
    }

    #[test]
    fn event_replay_filtered_scan_does_not_acknowledge_unemitted_rows() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let (tx, rx) = mpsc::channel(1);
            let mut cursor = replay_cursor(10);
            let mut seq = 0;
            let mut next_emit_at = std::time::Instant::now();
            let first = super::replay_event_frames(
                &cx,
                |query| {
                    assert_eq!(query.after_id, Some(10));
                    assert_eq!(query.pane_id, Some(7));
                    std::future::ready(Ok(checked_fixture_page(Vec::new(), 10, 90)))
                },
                Some(7),
                &mut cursor,
                &redactor,
                &tx,
                &mut seq,
                &mut next_emit_at,
                std::time::Duration::ZERO,
            )
            .await;
            assert_eq!(first, super::EventReplayProgress::CaughtUp);
            assert_eq!(cursor.scan_after_id, 90);
            assert_eq!(cursor.admitted_after_id, 10);
            assert_eq!(seq, 0, "filtered rows must not manufacture SSE checkpoints");
            let second = super::replay_event_frames(
                &cx,
                |query| {
                    assert_eq!(query.after_id, Some(90), "scan must not repeat excluded rows");
                    std::future::ready(Ok(checked_fixture_page(vec![replay_fixture(95)], 90, 100)))
                },
                Some(7),
                &mut cursor,
                &redactor,
                &tx,
                &mut seq,
                &mut next_emit_at,
                std::time::Duration::ZERO,
            )
            .await;
            assert_eq!(second, super::EventReplayProgress::CaughtUp);
            assert_eq!(cursor.scan_after_id, 100);
            assert_eq!(cursor.admitted_after_id, 95);
            drop(tx);
            let frames = drain_replay_frames(rx);
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].id, Some(replay_token(95, Some(7))));
        });
    }

    #[test]
    fn event_replay_yields_after_a_bounded_turn_without_truncating_history() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let (tx, rx) = mpsc::channel(1100);
            let mut cursor = replay_cursor(0);
            let mut seq = 0;
            let mut next_emit_at = std::time::Instant::now();
            let mut calls = 0;
            let mut fetch = |query: crate::storage::EventStreamQuery| {
                calls += 1;
                let after_id = query.after_id.unwrap();
                let rows = (1..=1100)
                    .filter(|id| *id > after_id)
                    .take(query.limit.unwrap())
                    .map(replay_fixture)
                    .collect();
                std::future::ready(Ok(checked_fixture_page(rows, after_id, 1100)))
            };
            let first = super::replay_event_frames(
                &cx,
                &mut fetch,
                None,
                &mut cursor,
                &redactor,
                &tx,
                &mut seq,
                &mut next_emit_at,
                std::time::Duration::ZERO,
            )
            .await;
            assert_eq!(first, super::EventReplayProgress::More);
            assert_eq!(cursor.scan_after_id, 1024);
            assert_eq!(cursor.admitted_after_id, 1024);
            let second = super::replay_event_frames(
                &cx,
                &mut fetch,
                None,
                &mut cursor,
                &redactor,
                &tx,
                &mut seq,
                &mut next_emit_at,
                std::time::Duration::ZERO,
            )
            .await;
            assert_eq!(second, super::EventReplayProgress::CaughtUp);
            assert_eq!(calls, 5);
            assert_eq!(cursor.admitted_after_id, 1100);
            drop(tx);
            let frames = drain_replay_frames(rx);
            assert_eq!(frames.len(), 1100);
            assert_eq!(frames.last().unwrap().id, Some(replay_token(1100, None)));
        });
    }

    #[test]
    fn event_replay_discontinuities_are_terminal_content_free_and_do_not_advance() {
        run_async_test_isolated(|| async {
            use crate::storage::{EventRetentionInterval, EventRetentionStatus};

            for (status, reason) in [
                (
                    EventRetentionStatus::Pruned {
                        interval: EventRetentionInterval {
                            start_id: 11,
                            end_id: 12,
                            first_generation: 1,
                            last_generation: 1,
                            first_deleted_at: 100,
                            last_deleted_at: 100,
                        },
                    },
                    "retention_pruned",
                ),
                (
                    EventRetentionStatus::LegacyHistoryUnavailable {
                        evidence_from_event_id: 20,
                    },
                    "legacy_history_unavailable",
                ),
                (EventRetentionStatus::CursorEpochMismatch, "cursor_epoch_mismatch"),
                (
                    EventRetentionStatus::CursorAheadOfHighWater { max_event_id: 5 },
                    "cursor_ahead_of_high_water",
                ),
            ] {
                let cx = crate::cx::for_request();
                let redactor = crate::policy::Redactor::new();
                let (tx, rx) = mpsc::channel(1);
                let mut cursor = replay_cursor(10);
                let mut seq = 0;
                let mut next_emit_at = std::time::Instant::now();
                let mut page = checked_fixture_page(vec![replay_fixture(99)], 10, 99);
                page.check.status = status;
                let mut page = Some(page);
                let progress = super::replay_event_frames(
                    &cx,
                    |_| std::future::ready(Ok(page.take().expect("terminal after one read"))),
                    Some(7),
                    &mut cursor,
                    &redactor,
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await;
                assert_eq!(progress, super::EventReplayProgress::Stopped);
                assert_eq!(cursor.scan_after_id, 10);
                assert_eq!(cursor.admitted_after_id, 10);
                drop(tx);
                let frames = drain_replay_frames(rx);
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].id, None);
                assert_eq!(frames[0].event_type.as_deref(), Some("cursor_discontinuity"));
                let payload = frames[0].data.as_deref().unwrap();
                assert!(!payload.contains("captured detection"));
                let frame: serde_json::Value = serde_json::from_str(payload).unwrap();
                assert_eq!(frame["data"]["terminal"], true);
                assert_eq!(frame["data"]["reason"], reason);
                assert_eq!(frame["data"]["resume_cursor"], replay_token(10, Some(7)));
            }
        });
    }

    #[test]
    fn event_replay_rejects_inconsistent_retention_evidence_before_delivery() {
        run_async_test_isolated(|| async {
            let mut wrong_epoch = checked_fixture_page(vec![replay_fixture(11)], 10, 11);
            wrong_epoch.page.retention.cursor_epoch = "f".repeat(32);
            let mut wrong_range = checked_fixture_page(vec![replay_fixture(11)], 10, 11);
            wrong_range.check.requested_after_id = 9;
            let mut incomplete = checked_fixture_page(vec![replay_fixture(11)], 10, 11);
            incomplete.check.checked_through_id = 10;
            // On a full page the checked range ends below the global
            // high-water mark. A later snapshot must still not regress it.
            let mut regressed_high_water =
                checked_fixture_page((11..=266).map(replay_fixture).collect(), 10, 300);
            regressed_high_water.check.snapshot.max_event_id = 290;
            let mut regressed_generation =
                checked_fixture_page(vec![replay_fixture(11)], 10, 11);
            regressed_generation.page.retention.generation = 2;
            regressed_generation.check.snapshot.generation = 1;
            for page in [
                wrong_epoch,
                wrong_range,
                incomplete,
                regressed_high_water,
                regressed_generation,
            ] {
                let cx = crate::cx::for_request();
                let redactor = crate::policy::Redactor::new();
                let (tx, rx) = mpsc::channel(super::EVENT_STREAM_REPLAY_PAGE_SIZE + 1);
                let mut cursor = replay_cursor(10);
                let mut seq = 0;
                let mut next_emit_at = std::time::Instant::now();
                let mut page = Some(page);
                let progress = super::replay_event_frames(
                    &cx,
                    |_| std::future::ready(Ok(page.take().unwrap())),
                    None,
                    &mut cursor,
                    &redactor,
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await;
                assert_eq!(progress, super::EventReplayProgress::Stopped);
                assert_eq!(cursor.scan_after_id, 10);
                assert_eq!(cursor.admitted_after_id, 10);
                drop(tx);
                let frames = drain_replay_frames(rx);
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].id, None);
                assert!(frames[0].data.as_deref().unwrap().contains("event_replay_invalid_page"));
            }
        });
    }

    #[test]
    fn event_replay_delivers_more_than_one_thousand_rows_in_bounded_pages() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let (tx, rx) = mpsc::channel(1101);
            let (mut after_id, mut seq) = (0, 0);
            let mut next_emit_at = std::time::Instant::now();
            let mut cursors = Vec::new();
            let fetch = |query: crate::storage::EventStreamQuery| {
                assert_eq!(query.pane_id, Some(7));
                assert_eq!(query.limit, Some(super::EVENT_STREAM_REPLAY_PAGE_SIZE));
                let cursor = query.after_id.expect("cursor is always explicit");
                cursors.push(cursor);
                // Non-contiguous ids model retained rows with holes.
                let rows = (1..=1100)
                    .map(|n| n * 2)
                    .filter(|id| *id > cursor)
                    .take(super::EVENT_STREAM_REPLAY_PAGE_SIZE)
                    .map(replay_fixture)
                    .collect();
                std::future::ready(Ok(rows))
            };
            assert!(
                replay_fixture_frames(
                    &cx,
                    fetch,
                    Some(7),
                    &mut after_id,
                    &redactor,
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await
            );
            assert_eq!(cursors, [0, 512, 1024, 1536, 2048]);
            assert_eq!(after_id, 2200);
            assert_eq!(seq, 1100);
            drop(tx);
            let frames = drain_replay_frames(rx);
            assert_eq!(frames.len(), 1100);
            for (index, frame) in frames.iter().enumerate() {
                assert_eq!(
                    frame.id,
                    Some(replay_token(((index + 1) * 2) as i64, Some(7)))
                );
                assert_eq!(frame.event_type.as_deref(), Some("event"));
                let data: serde_json::Value =
                    serde_json::from_str(frame.data.as_deref().unwrap()).unwrap();
                assert_eq!(data["data"]["replayed"], true);
            }
        });
    }

    #[test]
    fn event_replay_storage_failure_keeps_cursor_and_emits_no_new_id() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let (tx, rx) = mpsc::channel(super::EVENT_STREAM_REPLAY_PAGE_SIZE + 1);
            let (mut after_id, mut seq) = (0, 0);
            let mut next_emit_at = std::time::Instant::now();
            let mut calls = 0;
            let fetch = |_: crate::storage::EventStreamQuery| {
                calls += 1;
                std::future::ready(if calls == 1 {
                    Ok((1..=256).map(replay_fixture).collect())
                } else {
                    Err(crate::Error::runtime_backend(
                        "replay test",
                        "private backend detail",
                    ))
                })
            };
            assert!(
                !replay_fixture_frames(
                    &cx,
                    fetch,
                    None,
                    &mut after_id,
                    &redactor,
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await
            );
            assert_eq!(calls, 2);
            assert_eq!(after_id, 256);
            drop(tx);
            let frames = drain_replay_frames(rx);
            assert_eq!(frames.len(), 257);
            let error = frames.last().unwrap();
            assert_eq!(error.id, None);
            assert_eq!(error.event_type.as_deref(), Some("error"));
            let data = error.data.as_deref().unwrap();
            assert!(data.contains("event_replay_storage_error"));
            assert!(!data.contains("private backend detail"));
        });
    }

    #[test]
    fn event_replay_rejects_invalid_pages_before_advancing() {
        run_async_test_isolated(|| async {
            let mut wrong_pane = replay_fixture(1);
            wrong_pane.pane_id = 8;
            for rows in [
                vec![replay_fixture(2), replay_fixture(1)],
                vec![replay_fixture(1), replay_fixture(1)],
                vec![replay_fixture(0)],
                vec![wrong_pane],
                (1..=257).map(replay_fixture).collect(),
            ] {
                let cx = crate::cx::for_request();
                let redactor = crate::policy::Redactor::new();
                let (tx, rx) = mpsc::channel(3);
                let (mut after_id, mut seq) = (0, 0);
                let mut next_emit_at = std::time::Instant::now();
                let mut rows = Some(rows);
                assert!(
                    !replay_fixture_frames(
                        &cx,
                        |_| std::future::ready(Ok(rows.take().expect("only one fetch"))),
                        Some(7),
                        &mut after_id,
                        &redactor,
                        &tx,
                        &mut seq,
                        &mut next_emit_at,
                        std::time::Duration::ZERO,
                    )
                    .await
                );
                assert_eq!(after_id, 0);
                drop(tx);
                let frames = drain_replay_frames(rx);
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].id, None);
                assert!(
                    frames[0]
                        .data
                        .as_deref()
                        .unwrap()
                        .contains("event_replay_invalid_page")
                );
            }
        });
    }

    #[test]
    fn event_replay_empty_page_preserves_cursor_without_emitting() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let (tx, rx) = mpsc::channel(1);
            let (mut after_id, mut seq) = (91, 0);
            let mut next_emit_at = std::time::Instant::now();
            assert!(
                replay_fixture_frames(
                    &cx,
                    |_| std::future::ready(Ok(Vec::new())),
                    None,
                    &mut after_id,
                    &redactor,
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await
            );
            assert_eq!(after_id, 91);
            assert_eq!(seq, 0);
            drop(tx);
            assert!(drain_replay_frames(rx).is_empty());
        });
    }

    #[test]
    fn event_replay_closed_receiver_does_not_advance_cursor() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let (tx, rx) = mpsc::channel(1);
            drop(rx);
            let (mut after_id, mut seq) = (0, 0);
            let mut next_emit_at = std::time::Instant::now();
            assert!(
                !replay_fixture_frames(
                    &cx,
                    |_| std::future::ready(Ok(vec![replay_fixture(1)])),
                    None,
                    &mut after_id,
                    &redactor,
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await
            );
            assert_eq!(after_id, 0);
        });
    }

    #[test]
    fn replay_sender_waits_for_capacity_instead_of_dropping_the_event() {
        run_async_test_isolated(|| async {
            use std::future::Future;
            use std::task::{Context, Poll, Waker};

            let cx = crate::cx::for_request();
            let (tx, mut rx) = mpsc::channel(1);
            tx.try_send(SseEvent::comment("occupied")).unwrap();
            let mut next_emit_at = std::time::Instant::now();
            let mut send = Box::pin(super::send_replay_sse(
                &cx,
                &tx,
                SseEvent::new("retained").id("17"),
                &mut next_emit_at,
                std::time::Duration::ZERO,
            ));
            let mut poll_cx = Context::from_waker(Waker::noop());
            assert!(send.as_mut().poll(&mut poll_cx).is_pending());
            assert!(matches!(
                rx.poll_recv(&cx, &mut poll_cx),
                Poll::Ready(Ok(_))
            ));
            assert_eq!(send.as_mut().poll(&mut poll_cx), Poll::Ready(true));
            let Poll::Ready(Ok(delivered)) = rx.poll_recv(&cx, &mut poll_cx) else {
                panic!("retained frame must be delivered after capacity is available");
            };
            assert_eq!(delivered.id.as_deref(), Some("17"));
            assert_eq!(delivered.data.as_deref(), Some("retained"));
        });
    }

    #[test]
    fn event_replay_repeated_catchup_preserves_order_and_suppresses_duplicates() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let (tx, rx) = mpsc::channel(8);
            let (mut after_id, mut seq) = (0, 0);
            let mut next_emit_at = std::time::Instant::now();
            let mut cursors = Vec::new();
            // The second catchup models an id-5 notification arriving before
            // id 4. Storage already contains both; the last pass represents
            // an overlapping wakeup or a keepalive scan with no new rows.
            for ids in [vec![1, 3], vec![1, 3, 4, 5], vec![1, 3, 4, 5]] {
                assert!(
                    replay_fixture_frames(
                        &cx,
                        |query| {
                            let cursor = query.after_id.unwrap();
                            cursors.push(cursor);
                            std::future::ready(Ok(ids
                                .iter()
                                .copied()
                                .filter(|id| *id > cursor)
                                .map(replay_fixture)
                                .collect()))
                        },
                        Some(7),
                        &mut after_id,
                        &redactor,
                        &tx,
                        &mut seq,
                        &mut next_emit_at,
                        std::time::Duration::ZERO,
                    )
                    .await
                );
            }
            assert_eq!(cursors, [0, 3, 5]);
            assert_eq!(after_id, 5);
            drop(tx);
            let frames = drain_replay_frames(rx);
            let delivered: Vec<_> = frames.iter().map(|frame| frame.id.clone()).collect();
            let expected = [1, 3, 4, 5].map(|id| Some(replay_token(id, Some(7))));
            assert_eq!(delivered, expected);
        });
    }

    #[test]
    fn event_replay_body_drains_a_capacity_one_channel_without_losing_frames() {
        run_async_test_isolated(|| async {
            use crate::runtime_async::stream::Stream;
            use std::pin::Pin;
            use std::sync::Arc;
            use std::sync::atomic::{AtomicI64, Ordering};

            let cx = crate::cx::for_request();
            let producer_cx = cx.clone();
            let (tx, rx) = mpsc::channel(1);
            let cursor = Arc::new(AtomicI64::new(0));
            let producer_cursor = Arc::clone(&cursor);
            let producer = Box::pin(async move {
                let mut after_id = 0;
                let mut seq = 0;
                let mut next_emit_at = std::time::Instant::now();
                assert!(
                    replay_fixture_frames(
                        &producer_cx,
                        |query| {
                            std::future::ready(Ok((1..=65)
                                .filter(|id| *id > query.after_id.unwrap())
                                .map(replay_fixture)
                                .collect()))
                        },
                        None,
                        &mut after_id,
                        &crate::policy::Redactor::new(),
                        &tx,
                        &mut seq,
                        &mut next_emit_at,
                        std::time::Duration::ZERO,
                    )
                    .await
                );
                producer_cursor.store(after_id, Ordering::SeqCst);
            });
            let mut stream = super::SseByteStream::new(
                cx.clone(),
                rx,
                producer,
                Box::pin(std::future::pending()),
            );
            let frames = crate::runtime_async::timeout_with_cx(
                &cx,
                std::time::Duration::from_secs(5),
                async {
                    let mut frames = Vec::new();
                    while let Some(frame) =
                        std::future::poll_fn(|poll_cx| Pin::new(&mut stream).poll_next(poll_cx))
                            .await
                    {
                        frames.push(String::from_utf8(frame).unwrap());
                    }
                    frames
                },
            )
            .await
            .expect("finite replay must finish even with a capacity-one channel");
            assert_eq!(frames.len(), 65);
            for (index, frame) in frames.iter().enumerate() {
                let expected = format!("id: {}", replay_token((index + 1) as i64, None));
                assert!(frame.lines().any(|line| line == expected), "{frame}");
            }
            assert_eq!(cursor.load(Ordering::SeqCst), 65);
        });
    }

    #[test]
    fn event_replay_shutdown_drops_a_producer_waiting_for_capacity() {
        run_async_test_isolated(|| async {
            use crate::runtime_async::stream::Stream;
            use crate::web_framework::WebStreamLifecycle;
            use std::pin::Pin;
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::task::{Context, Poll, Waker};

            let cx = crate::cx::for_request();
            let producer_cx = cx.clone();
            let lifecycle = WebStreamLifecycle::new();
            let waiter = lifecycle.shutdown_waiter(&cx);
            let (tx, rx) = mpsc::channel(1);
            let drops = Arc::new(AtomicUsize::new(0));
            let probe = ProducerDropProbe(Arc::clone(&drops));
            let completed = Arc::new(AtomicBool::new(false));
            let producer_completed = Arc::clone(&completed);
            let producer = Box::pin(async move {
                let _probe = probe;
                let mut after_id = 0;
                let mut seq = 0;
                let mut next_emit_at = std::time::Instant::now();
                let result = replay_fixture_frames(
                    &producer_cx,
                    |_| std::future::ready(Ok((1..=3).map(replay_fixture).collect())),
                    None,
                    &mut after_id,
                    &crate::policy::Redactor::new(),
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await;
                producer_completed.store(result, Ordering::SeqCst);
            });
            let mut stream = super::SseByteStream::new(cx, rx, producer, waiter);
            let mut poll_cx = Context::from_waker(Waker::noop());
            let Poll::Ready(Some(first)) = Pin::new(&mut stream).poll_next(&mut poll_cx) else {
                panic!("the first frame should be queued before the producer parks");
            };
            assert!(
                String::from_utf8(first)
                    .unwrap()
                    .lines()
                    .any(|line| line == format!("id: {}", replay_token(1, None)))
            );
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert!(!completed.load(Ordering::SeqCst));
            lifecycle.signal_shutdown();
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut poll_cx),
                Poll::Ready(None)
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(!completed.load(Ordering::SeqCst));
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut poll_cx),
                Poll::Ready(None)
            );
        });
    }

    #[test]
    fn event_replay_redacts_matched_text_and_nested_payloads() {
        run_async_test_isolated(|| async {
            let cx = crate::cx::for_request();
            let redactor = crate::policy::Redactor::new();
            let secret = format!("ghp_{}", "a".repeat(36));
            assert_ne!(
                redactor.redact(&secret),
                secret,
                "fixture must match a redaction rule"
            );
            let mut row = replay_fixture(1);
            row.matched_text = Some(format!("visible_marker {secret}"));
            row.extracted = Some(json!({ "nested": [{ "value": secret.clone() }] }));
            let mut row = Some(row);
            let (tx, rx) = mpsc::channel(1);
            let (mut after_id, mut seq) = (0, 0);
            let mut next_emit_at = std::time::Instant::now();
            assert!(
                replay_fixture_frames(
                    &cx,
                    |_| std::future::ready(Ok(vec![row.take().unwrap()])),
                    None,
                    &mut after_id,
                    &redactor,
                    &tx,
                    &mut seq,
                    &mut next_emit_at,
                    std::time::Duration::ZERO,
                )
                .await
            );
            drop(tx);
            let frames = drain_replay_frames(rx);
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].id, Some(replay_token(1, None)));
            let data = frames[0].data.as_deref().unwrap();
            assert!(!data.contains(&secret));
            assert!(data.contains("visible_marker"));
            assert!(data.contains("nested"));
            assert!(data.contains("codex.usage.reached"));
        });
    }

    #[test]
    fn lifecycle_shutdown_drops_producer_parked_in_real_rate_limit_sleep() {
        run_async_test_isolated(|| async {
            use crate::runtime_async::stream::Stream;
            use crate::web_framework::WebStreamLifecycle;
            use std::pin::Pin;
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::task::{Context, Poll, Waker};
            use std::time::{Duration, Instant};

            let lifecycle = WebStreamLifecycle::new();
            let cx = crate::cx::for_request();
            let waiter = lifecycle.shutdown_waiter(&cx);
            let (tx, rx) = mpsc::channel(2);
            let drops = Arc::new(AtomicUsize::new(0));
            let probe = ProducerDropProbe(Arc::clone(&drops));
            let sent = Arc::new(AtomicBool::new(false));
            let producer_sent = Arc::clone(&sent);
            let producer = Box::pin(async move {
                let _probe = probe;
                let mut next_emit_at = Instant::now() + Duration::from_secs(3600);
                let mut consecutive_drops = 0;
                let did_send = super::send_rate_limited_sse(
                    &tx,
                    SseEvent::comment("must not outlive the response"),
                    &mut next_emit_at,
                    Duration::from_secs(1),
                    &mut consecutive_drops,
                )
                .await;
                producer_sent.store(did_send, Ordering::SeqCst);
            });
            let mut stream = super::SseByteStream::new(cx, rx, producer, waiter);
            let mut poll_cx = Context::from_waker(Waker::noop());
            assert!(Pin::new(&mut stream).poll_next(&mut poll_cx).is_pending());
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert!(!sent.load(Ordering::SeqCst));

            lifecycle.signal_shutdown();
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut poll_cx),
                Poll::Ready(None)
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(!sent.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn stream_sequence_ends_before_overflow_without_reusing_an_id() {
        let mut first = 0;
        assert!(super::advance_stream_sequence(&mut first));
        assert_eq!(first, 1);
        let mut last = u64::MAX - 1;
        assert!(super::advance_stream_sequence(&mut last));
        assert_eq!(last, u64::MAX);
        assert!(!super::advance_stream_sequence(&mut last));
        assert_eq!(last, u64::MAX);
    }

    /// Per ft-22x4r: every async test in supported paths must drive the
    /// project asupersync runtime instead of `#[tokio::test]`. This
    /// helper builds a thread-isolated current-thread runtime, runs the
    /// future to completion, and clears the retained runtime handle after
    /// teardown. A teardown panic fails the test; it is not passing evidence.
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

                let teardown_result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drop(runtime);
                    }));
                crate::runtime_async::clear_runtime_handle();

                if let Err(payload) = test_result {
                    std::panic::resume_unwind(payload);
                }
                if let Err(payload) = teardown_result {
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
    fn stream_min_interval_never_permits_more_than_max_hz() {
        use std::time::Duration;

        assert_eq!(stream_min_interval(3), Duration::from_nanos(333_333_334));
        assert_eq!(stream_min_interval(50), Duration::from_millis(20));
        assert_eq!(stream_min_interval(0), Duration::from_secs(1));
        assert_eq!(stream_min_interval(u64::MAX), Duration::from_nanos(1));
        for max_hz in [1, 3, 7, 50, 333, 500, 1001, 1_000_000, u64::MAX] {
            let interval = stream_min_interval(max_hz);
            assert!(interval > Duration::ZERO, "max_hz {max_hz}");
            assert!(
                interval.as_nanos() * u128::from(max_hz) >= 1_000_000_000,
                "max_hz {max_hz} interval {interval:?}"
            );
        }
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
    fn event_frames_carry_the_persisted_event_id_or_none() {
        // A reconnecting client retains epoch and pane scope along with
        // its durable boundary; live-only frames carry no id.
        let token = replay_token(1234, Some(7));
        let with_id =
            frame_to_sse_with_event_id("event", Some(token.clone()), json!({"k": 1})).unwrap();
        let text = String::from_utf8(with_id.to_bytes()).unwrap();
        assert!(text.contains(&format!("id: {token}\n")), "{text}");
        let without = frame_to_sse_with_event_id("event", None, json!({"k": 1})).unwrap();
        let text = String::from_utf8(without.to_bytes()).unwrap();
        assert!(!text.contains("id:"), "{text}");

        let detected = Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection: crate::patterns::Detection {
                rule_id: "codex.usage.reached".to_string(),
                agent_type: crate::patterns::AgentType::Codex,
                event_type: "usage.reached".to_string(),
                severity: crate::patterns::Severity::Warning,
                confidence: 1.0,
                extracted: serde_json::Value::Null,
                matched_text: String::new(),
                span: (0, 0),
            },
            event_id: Some(77),
        };
        assert_eq!(persisted_event_id(&detected), Some(77));
        assert_eq!(
            persisted_event_id(&Event::SegmentCaptured {
                pane_id: 1,
                seq: 2,
                content_len: 3
            }),
            None
        );
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

    fn run_drop_counter_async_test_isolated<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: std::future::Future<Output = ()>,
    {
        let _guard = drop_counter_test_lock();
        run_async_test_isolated(f);
    }

    /// br-ft-95fd3: a successful send does NOT bump either drop
    /// counter. Vacuous-regression guard against the bumpers
    /// false-positiving on the happy path.
    #[test]
    fn send_rate_limited_sse_ok_path_does_not_bump_drop_counter_ft_95fd3() {
        run_drop_counter_async_test_isolated(|| async {
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
        run_drop_counter_async_test_isolated(|| async {
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
        run_drop_counter_async_test_isolated(|| async {
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
                let expected_ms: i64 = offset_secs * 1000;
                proptest::prop_assert_eq!(
                    ms, expected_ms,
                    "br-ft-bn6qi: post-epoch ms must equal offset_secs*1000"
                );
            }
        }
    }
}
