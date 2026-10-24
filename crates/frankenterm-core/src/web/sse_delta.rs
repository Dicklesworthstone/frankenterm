//! Storage-backed terminal-output streaming. The response owns every wait;
//! cursor advancement means a frame was accepted, never merely attempted.
//! Reconnect uses a namespaced, pane-scoped persisted segment id; control
//! frames never overwrite the client's last received segment cursor.

use super::super::error::json_err;
use super::super::extractors::{request_runtime_limits, require_storage_and_event_bus};
use super::super::{STREAM_CHANNEL_BUFFER, WebRuntimeLimits};
use super::{
    SseByteStream, SseEvent, SseResponse, advance_stream_sequence, epoch_ms_now, frame_to_sse,
    make_stream_frame, parse_stream_max_hz, send_rate_limited_sse, send_replay_sse,
    stream_min_interval,
};
use crate::events::{Event, RecvError};
use crate::policy::Redactor;
use crate::runtime_async::{mpsc, task};
use crate::storage::{Segment, SegmentScanQuery};
use crate::web_framework::{QueryString, Request, Response, StatusCode, WebStreamLifecycle};
use serde_json::json;
use std::future::Future;
use std::time::{Duration, Instant};

const DELTA_CURSOR_PREFIX: &str = "delta-v1:";
const INVALID_CURSOR: &str = "invalid_delta_cursor";
const CURSOR_SCOPE_MISMATCH: &str = "delta_cursor_scope_mismatch";

fn decimal_u64(value: &str) -> Option<u64> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn segment_id(value: &str) -> Option<i64> {
    decimal_u64(value).and_then(|id| i64::try_from(id).ok())
}

fn encode_delta_cursor(pane_filter: Option<u64>, id: i64) -> String {
    match pane_filter {
        Some(pane) => format!("{DELTA_CURSOR_PREFIX}pane:{pane}:{id}"),
        None => format!("{DELTA_CURSOR_PREFIX}all:{id}"),
    }
}

/// A numeric legacy SSE id was a connection sequence, not a storage position.
/// Never guess its meaning. A cursor also belongs to the original pane filter;
/// changing that filter requires a deliberate new bootstrap cursor.
fn parse_delta_cursor(value: &str, pane_filter: Option<u64>) -> Result<i64, &'static str> {
    let suffix = value.strip_prefix(DELTA_CURSOR_PREFIX).ok_or(INVALID_CURSOR)?;
    let (scope, id) = if let Some(id) = suffix.strip_prefix("all:") {
        (None, id)
    } else if let Some(pane_and_id) = suffix.strip_prefix("pane:") {
        let (pane, id) = pane_and_id.split_once(':').ok_or(INVALID_CURSOR)?;
        (Some(decimal_u64(pane).ok_or(INVALID_CURSOR)?), id)
    } else {
        return Err(INVALID_CURSOR);
    };
    let id = segment_id(id).ok_or(INVALID_CURSOR)?;
    if scope != pane_filter {
        return Err(CURSOR_SCOPE_MISMATCH);
    }
    Ok(id)
}

/// A browser's reconnect header supersedes the original URL's bootstrap id.
/// An empty header means no retained cursor; malformed nonempty headers must
/// fail instead of silently downgrading the connection to fresh/live-only.
fn parse_delta_resume(
    last_event_id: Option<&[u8]>,
    since_id: Option<&str>,
    pane_filter: Option<u64>,
) -> Result<Option<i64>, &'static str> {
    if let Some(bytes) = last_event_id {
        if bytes.len() > 128 {
            return Err(INVALID_CURSOR);
        }
        let value = std::str::from_utf8(bytes).map_err(|_| INVALID_CURSOR)?.trim();
        if !value.is_empty() {
            return parse_delta_cursor(value, pane_filter).map(Some);
        }
    }
    since_id.map(|value| segment_id(value).ok_or(INVALID_CURSOR)).transpose()
}

fn delta_frame_to_sse(
    kind: &'static str,
    seq: u64,
    pane_filter: Option<u64>,
    persisted_segment_id: Option<i64>,
    frame: serde_json::Value,
) -> Option<SseEvent> {
    frame_to_sse(kind, seq, frame).map(|mut event| {
        event.id = persisted_segment_id.map(|id| encode_delta_cursor(pane_filter, id));
        event
    })
}

struct DeltaStreamState {
    pane_filter: Option<u64>,
    since_ms: Option<i64>,
    after_id: Option<i64>,
    scan_limit: usize,
    pages_per_yield: usize,
    seq: u64,
    next_emit_at: Instant,
    min_interval: Duration,
}

impl DeltaStreamState {
    fn new(
        pane_filter: Option<u64>,
        started_at_ms: i64,
        resume_after: Option<i64>,
        limits: WebRuntimeLimits,
        max_hz: u64,
    ) -> Self {
        Self {
            pane_filter,
            // Replayed rows predate this connection. Keeping the fresh-stream
            // timestamp filter during resume would silently discard them.
            since_ms: resume_after.is_none().then_some(started_at_ms),
            after_id: resume_after,
            // Defend direct/internal construction as well as validated config.
            scan_limit: limits.stream_scan_limit.max(1),
            pages_per_yield: limits.stream_scan_max_pages.max(1),
            seq: 0,
            next_emit_at: Instant::now(),
            min_interval: stream_min_interval(max_hz),
        }
    }

    async fn emit(
        &mut self,
        cx: &crate::cx::Cx,
        tx: &mpsc::Sender<SseEvent>,
        kind: &'static str,
        persisted_segment_id: Option<i64>,
        data: serde_json::Value,
    ) -> bool {
        if !advance_stream_sequence(&mut self.seq) {
            return false;
        }
        let frame = make_stream_frame("deltas", kind, self.seq, data);
        let Some(event) = delta_frame_to_sse(
            kind, self.seq, self.pane_filter, persisted_segment_id, frame,
        ) else {
            return false;
        };
        send_replay_sse(cx, tx, event, &mut self.next_emit_at, self.min_interval).await
    }

    /// Fetch one bounded page at a time, yielding between scan windows. Tests
    /// inject storage outcomes here but use the real framing and send path.
    async fn catch_up<F, Fut>(
        &mut self,
        cx: &crate::cx::Cx,
        mut fetch: F,
        redactor: &Redactor,
        tx: &mpsc::Sender<SseEvent>,
    ) -> bool
    where
        F: FnMut(SegmentScanQuery) -> Fut,
        Fut: Future<Output = crate::Result<Vec<Segment>>>,
    {
        let mut pages = 0;
        loop {
            if cx.checkpoint().is_err() {
                return false;
            }
            let query = SegmentScanQuery {
                after_id: self.after_id,
                pane_id: self.pane_filter,
                since: self.since_ms,
                until: None,
                limit: self.scan_limit,
            };
            let segments = match fetch(query).await {
                Ok(segments) => segments,
                Err(_) => {
                    let _ = self
                        .emit(cx, tx, "error", None, json!({"code": "storage_error"}))
                        .await;
                    return false;
                }
            };
            let page_len = segments.len();
            let mut previous = self.after_id.unwrap_or(0);
            // Reject a broken query contract before any row in the page is
            // admitted. In particular, never skip unseen rows by accepting
            // an out-of-order, duplicate, or wrong-pane result.
            let valid = page_len <= self.scan_limit
                && segments.iter().all(|segment| {
                    let valid = segment.id > previous
                        && self.pane_filter.is_none_or(|pane| segment.pane_id == pane)
                        && self.since_ms.is_none_or(|since| segment.captured_at >= since);
                    previous = segment.id;
                    valid
                });
            if !valid {
                let _ = self
                    .emit(cx, tx, "error", None, json!({"code": "invalid_segment_page"}))
                    .await;
                return false;
            }
            for segment in segments {
                if !self
                    .emit(
                        cx,
                        tx,
                        "delta",
                        Some(segment.id),
                        json!({
                            "segment_id": segment.id,
                            "pane_id": segment.pane_id,
                            "seq": segment.seq,
                            "captured_at": segment.captured_at,
                            "content_len": segment.content_len,
                            "content": redactor.redact(&segment.content),
                        }),
                    )
                    .await
                {
                    return false;
                }
                // send_replay_sse waits for capacity; it never reports a
                // channel-full drop as success. Only admitted frames move
                // this connection's cursor. Socket delivery is not implied.
                self.after_id = Some(segment.id);
            }
            if page_len < self.scan_limit {
                return true;
            }
            pages += 1;
            if pages == self.pages_per_yield {
                task::yield_now().await;
                pages = 0;
            }
        }
    }
}

pub(crate) fn handle_stream_deltas(
    req: &Request,
) -> std::pin::Pin<Box<dyn Future<Output = Response> + Send>> {
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);
    // Invalid filter text must not broaden a pane-scoped stream to all panes.
    let pane_filter = match qs.get("pane_id") {
        None => None,
        Some(value) => match decimal_u64(value) {
            Some(pane) => Some(pane),
            None => return Box::pin(async {
                json_err(StatusCode::BAD_REQUEST, "invalid_pane_id", "pane_id must be a nonnegative integer")
            }),
        },
    };
    let resume_after = match parse_delta_resume(
        req.headers().get("last-event-id").map(|value| value.as_ref()),
        qs.get("since_id"),
        pane_filter,
    ) {
        Ok(cursor) => cursor,
        Err(code) => return Box::pin(async move {
            json_err(
                StatusCode::BAD_REQUEST,
                code,
                "Use a delta-v1 cursor for this pane selection or an explicit nonnegative since_id",
            )
        }),
    };
    let limits = request_runtime_limits(req);
    let max_hz = parse_stream_max_hz(&qs, limits);
    let result = require_storage_and_event_bus(req);
    let lifecycle = req.get_extension::<WebStreamLifecycle>().cloned();

    Box::pin(async move {
        let (storage, event_bus, redactor) = match result {
            Ok(result) => result,
            Err(response) => return response,
        };
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
        let started_at_ms = epoch_ms_now();
        let mut subscriber = event_bus.subscribe_deltas();
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_BUFFER);
        let producer = async move {
            let child_cx = stream_cx;
            let mut state = DeltaStreamState::new(pane_filter, started_at_ms, resume_after, limits, max_hz);
            if !advance_stream_sequence(&mut state.seq) {
                return;
            }
            let ready = make_stream_frame(
                "deltas",
                "ready",
                state.seq,
                json!({
                    "max_hz": max_hz,
                    "pane_id": pane_filter,
                    "resume_after_id": resume_after,
                    "cursor_format": "delta-v1",
                    "recovery": "retained_segments_only",
                }),
            );
            let Some(ready) = delta_frame_to_sse("ready", state.seq, pane_filter, None, ready) else {
                return;
            };
            // This is the first and only queued frame at admission, so the
            // existing ready-frame helper cannot encounter a full channel.
            let mut consecutive_drops = 0;
            if !send_rate_limited_sse(
                &tx,
                ready,
                &mut state.next_emit_at,
                state.min_interval,
                &mut consecutive_drops,
            )
            .await
            {
                return;
            }
            // Start draining immediately, even without a bus notification.
            if !state
                .catch_up(
                    &child_cx,
                    |query| storage.scan_segments_with_cx(&child_cx, query),
                    &redactor,
                    &tx,
                )
                .await
            {
                return;
            }
            let keepalive_interval = Duration::from_secs(limits.stream_keepalive_secs.max(1));
            let mut last_scan = Instant::now();
            loop {
                if child_cx.checkpoint().is_err() {
                    break;
                }
                // Unrelated-pane traffic must not postpone recovery of a
                // committed segment whose final notification was lost.
                if last_scan.elapsed() >= keepalive_interval {
                    if !state
                        .catch_up(
                            &child_cx,
                            |query| storage.scan_segments_with_cx(&child_cx, query),
                            &redactor,
                            &tx,
                        )
                        .await
                    {
                        break;
                    }
                    last_scan = Instant::now();
                }
                let received = crate::runtime_async::timeout_with_cx(
                    &child_cx,
                    keepalive_interval.saturating_sub(last_scan.elapsed()),
                    subscriber.recv_cx(&child_cx),
                )
                .await;
                match received {
                    Ok(Ok(Event::SegmentCaptured { pane_id, .. })) => {
                        if pane_filter.is_some_and(|pane| pane != pane_id) {
                            task::yield_now().await;
                            continue;
                        }
                        if !state
                            .catch_up(
                                &child_cx,
                                |query| storage.scan_segments_with_cx(&child_cx, query),
                                &redactor,
                                &tx,
                            )
                            .await
                        {
                            break;
                        }
                        last_scan = Instant::now();
                    }
                    Ok(Ok(Event::GapDetected {
                        pane_id,
                        seq_before,
                        seq_after,
                        reason,
                        detected_at_ms,
                    })) => {
                        if pane_filter.is_some_and(|pane| pane != pane_id) {
                            task::yield_now().await;
                            continue;
                        }
                        if !state
                            .emit(
                                &child_cx,
                                &tx,
                                "gap",
                                None,
                                json!({
                                    "pane_id": pane_id,
                                    "seq_before": seq_before,
                                    "seq_after": seq_after,
                                    "reason": redactor.redact(&reason),
                                    "detected_at_ms": detected_at_ms,
                                }),
                            )
                            .await
                        {
                            break;
                        }
                    }
                    Ok(Ok(_)) => task::yield_now().await,
                    Ok(Err(RecvError::Lagged { missed_count })) => {
                        // A bus lag can include non-replayable capture-gap
                        // notifications, so report it even when stored text
                        // itself can be recovered in full.
                        if !state
                            .emit(&child_cx, &tx, "lag", None, json!({"missed_count": missed_count}))
                            .await
                        {
                            break;
                        }
                        if !state
                            .catch_up(
                                &child_cx,
                                |query| storage.scan_segments_with_cx(&child_cx, query),
                                &redactor,
                                &tx,
                            )
                            .await
                        {
                            break;
                        }
                        last_scan = Instant::now();
                    }
                    Ok(Err(RecvError::Cancelled | RecvError::Closed)) => break,
                    Err(_) => {
                        // Comments carry no cursor. A full queue already has
                        // output ready and can safely omit this keepalive.
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
    use super::*;
    use crate::runtime_async::CompatRuntime;

    fn run_async<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: Future<Output = ()>,
    {
        let result = std::thread::spawn(move || {
            let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
                .build()
                .expect("test runtime");
            let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(f())));
            let teardown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
            crate::runtime_async::clear_runtime_handle();
            if let Err(payload) = test {
                std::panic::resume_unwind(payload);
            }
            if let Err(payload) = teardown {
                std::panic::resume_unwind(payload);
            }
        })
        .join();
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn state() -> DeltaStreamState {
        let mut limits = super::super::super::resolve_runtime_limits(None);
        limits.stream_scan_limit = 2;
        limits.stream_scan_max_pages = 1;
        let mut state = DeltaStreamState::new(Some(7), 123, None, limits, 50);
        state.min_interval = Duration::ZERO;
        state
    }

    fn segment(id: i64) -> Segment {
        let content = format!("segment {id}");
        Segment {
            id,
            pane_id: 7,
            seq: u64::try_from(id).unwrap(),
            content_len: content.len(),
            content,
            content_hash: None,
            captured_at: 124,
        }
    }

    fn drain(mut rx: mpsc::Receiver<SseEvent>) -> Vec<SseEvent> {
        let cx = crate::cx::for_request();
        let mut poll = std::task::Context::from_waker(std::task::Waker::noop());
        let mut frames = Vec::new();
        loop {
            match rx.poll_recv(&cx, &mut poll) {
                std::task::Poll::Ready(Ok(frame)) => frames.push(frame),
                std::task::Poll::Ready(Err(_)) => return frames,
                std::task::Poll::Pending => panic!("sender must be dropped"),
            }
        }
    }

    fn frame_segment_id(frame: &SseEvent) -> i64 {
        let value: serde_json::Value = serde_json::from_str(frame.data.as_deref().unwrap()).unwrap();
        value["data"]["segment_id"].as_i64().unwrap()
    }

    #[test]
    fn delta_catchup_drains_multiple_windows_and_sparse_ids_in_order() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut state = state();
            let (tx, rx) = mpsc::channel(8);
            let mut queries = Vec::new();
            assert!(state.catch_up(&cx, |query| {
                assert_eq!(query.limit, 2);
                assert_eq!(query.pane_id, Some(7));
                assert_eq!(query.since, Some(123));
                queries.push(query.after_id);
                std::future::ready(Ok([2, 4, 9, 11, 20].into_iter()
                    .filter(|id| *id > query.after_id.unwrap_or(0))
                    .take(query.limit).map(segment).collect()))
            }, &Redactor::new(), &tx).await);
            assert_eq!(queries, [None, Some(4), Some(11)]);
            assert_eq!(state.after_id, Some(20));
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.iter().map(frame_segment_id).collect::<Vec<_>>(), [2, 4, 9, 11, 20]);
        });
    }

    #[test]
    fn delta_storage_failure_preserves_the_last_admitted_cursor() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut state = state();
            let (tx, rx) = mpsc::channel(4);
            let mut calls = 0;
            assert!(!state.catch_up(&cx, |_| {
                calls += 1;
                std::future::ready(if calls == 1 {
                    Ok(vec![segment(1), segment(2)])
                } else {
                    Err(crate::Error::runtime_backend("fixture", "private storage detail"))
                })
            }, &Redactor::new(), &tx).await);
            assert_eq!(state.after_id, Some(2));
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.len(), 3);
            assert_eq!(frames[2].event_type.as_deref(), Some("error"));
            assert_eq!(frames[2].id, None);
            assert!(frames[2].data.as_deref().unwrap().contains("storage_error"));
            assert!(!frames[2].data.as_deref().unwrap().contains("private storage detail"));
        });
    }

    #[test]
    fn delta_invalid_pages_never_advance_the_cursor() {
        run_async(|| async {
            let mut wrong_pane = segment(1);
            wrong_pane.pane_id = 8;
            for rows in [
                vec![segment(2), segment(1)],
                vec![segment(1), segment(1)],
                vec![segment(0)],
                vec![wrong_pane],
                vec![segment(1), segment(2), segment(3)],
            ] {
                let cx = crate::cx::for_request();
                let mut state = state();
                let mut rows = Some(rows);
                let (tx, rx) = mpsc::channel(1);
                assert!(!state.catch_up(&cx,
                    |_| std::future::ready(Ok(rows.take().unwrap())),
                    &Redactor::new(), &tx).await);
                assert_eq!(state.after_id, None);
                drop(tx);
                let frames = drain(rx);
                assert_eq!(frames.len(), 1);
                assert!(frames[0].data.as_deref().unwrap().contains("invalid_segment_page"));
            }
        });
    }

    #[test]
    fn delta_closed_receiver_and_sequence_exhaustion_do_not_skip_a_row() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut closed = state();
            let (tx, rx) = mpsc::channel(1);
            drop(rx);
            assert!(!closed.catch_up(&cx,
                |_| std::future::ready(Ok(vec![segment(1)])),
                &Redactor::new(), &tx).await);
            assert_eq!(closed.after_id, None);

            let mut exhausted = state();
            exhausted.seq = u64::MAX;
            let (tx, rx) = mpsc::channel(1);
            assert!(!exhausted.catch_up(&cx,
                |_| std::future::ready(Ok(vec![segment(1)])),
                &Redactor::new(), &tx).await);
            assert_eq!(exhausted.after_id, None);
            drop(tx);
            assert!(drain(rx).is_empty());
        });
    }

    #[test]
    fn delta_body_drains_through_capacity_one_without_losing_output() {
        run_async(|| async {
            use crate::runtime_async::stream::Stream;
            use std::pin::Pin;
            use std::sync::Arc;
            use std::sync::atomic::{AtomicI64, Ordering};

            let cx = crate::cx::for_request();
            let producer_cx = cx.clone();
            let cursor = Arc::new(AtomicI64::new(0));
            let producer_cursor = Arc::clone(&cursor);
            let (tx, rx) = mpsc::channel(1);
            let producer = Box::pin(async move {
                let mut state = state();
                assert!(state.catch_up(&producer_cx, |query| {
                    std::future::ready(Ok((1..=65)
                        .filter(|id| *id > query.after_id.unwrap_or(0))
                        .take(query.limit).map(segment).collect()))
                }, &Redactor::new(), &tx).await);
                producer_cursor.store(state.after_id.unwrap(), Ordering::SeqCst);
            });
            let mut stream = SseByteStream::new(cx.clone(), rx, producer, Box::pin(std::future::pending()));
            let frames = crate::runtime_async::timeout_with_cx(&cx, Duration::from_secs(5), async {
                let mut frames = Vec::new();
                while let Some(bytes) = std::future::poll_fn(|poll| Pin::new(&mut stream).poll_next(poll)).await {
                    let text = String::from_utf8(bytes).unwrap();
                    let payload = text.lines().find_map(|line| line.strip_prefix("data: ")).unwrap();
                    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
                    frames.push(value["data"]["segment_id"].as_i64().unwrap());
                }
                frames
            }).await.expect("bounded replay must finish through the actual body");
            assert_eq!(frames, (1..=65).collect::<Vec<_>>());
            assert_eq!(cursor.load(Ordering::SeqCst), 65);
        });
    }

    #[test]
    fn delta_shutdown_disposes_a_producer_waiting_for_channel_capacity() {
        run_async(|| async {
            use crate::runtime_async::stream::Stream;
            use std::pin::Pin;
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::task::{Context, Poll, Waker};

            struct Probe(Arc<AtomicBool>);
            impl Drop for Probe {
                fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
            }
            let cx = crate::cx::for_request();
            let producer_cx = cx.clone();
            let lifecycle = WebStreamLifecycle::new();
            let shutdown = lifecycle.shutdown_waiter(&cx);
            let dropped = Arc::new(AtomicBool::new(false));
            let probe = Probe(Arc::clone(&dropped));
            let (tx, rx) = mpsc::channel(1);
            let producer = Box::pin(async move {
                let _probe = probe;
                let mut state = state();
                let _ = state.catch_up(&producer_cx,
                    |_| std::future::ready(Ok(vec![segment(1), segment(2)])),
                    &Redactor::new(), &tx).await;
            });
            let mut stream = SseByteStream::new(cx, rx, producer, shutdown);
            let mut poll = Context::from_waker(Waker::noop());
            assert!(matches!(Pin::new(&mut stream).poll_next(&mut poll), Poll::Ready(Some(_))));
            assert!(!dropped.load(Ordering::SeqCst));
            lifecycle.signal_shutdown();
            assert_eq!(Pin::new(&mut stream).poll_next(&mut poll), Poll::Ready(None));
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(Pin::new(&mut stream).poll_next(&mut poll), Poll::Ready(None));
        });
    }

    #[test]
    fn delta_redaction_precedes_delivery_and_keeps_benign_output() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut state = state();
            let secret = format!("ghp_{}", "a".repeat(36));
            let redactor = Redactor::new();
            assert_ne!(redactor.redact(&secret), secret);
            let mut row = segment(1);
            row.content = format!("visible-marker {secret}");
            row.content_len = row.content.len();
            let mut row = Some(row);
            let (tx, rx) = mpsc::channel(1);
            assert!(state.catch_up(&cx,
                |_| std::future::ready(Ok(vec![row.take().unwrap()])),
                &redactor, &tx).await);
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.len(), 1);
            let payload = frames[0].data.as_deref().unwrap();
            assert!(payload.contains("visible-marker"));
            assert!(!payload.contains(&secret));
        });
    }

    #[test]
    fn delta_cursor_has_a_versioned_pane_scoped_wire_format() {
        assert_eq!(encode_delta_cursor(None, 9001), "delta-v1:all:9001");
        assert_eq!(encode_delta_cursor(Some(7), 9001), "delta-v1:pane:7:9001");
        assert_eq!(parse_delta_cursor("delta-v1:all:9001", None), Ok(9001));
        assert_eq!(parse_delta_cursor("delta-v1:pane:7:9001", Some(7)), Ok(9001));
        assert_eq!(
            parse_delta_cursor("delta-v1:pane:18446744073709551615:9223372036854775807", Some(u64::MAX)),
            Ok(i64::MAX),
        );
        assert_eq!(parse_delta_cursor("delta-v1:all:0", None), Ok(0));
        for pane in [None, Some(0), Some(7), Some(u64::MAX)] {
            for id in [0, 1, 9001, i64::MAX] {
                assert_eq!(parse_delta_cursor(&encode_delta_cursor(pane, id), pane), Ok(id));
            }
        }
    }

    #[test]
    fn delta_resume_header_supersedes_bootstrap_query_but_empty_header_does_not() {
        assert_eq!(parse_delta_resume(None, None, None), Ok(None));
        assert_eq!(parse_delta_resume(None, Some("0"), None), Ok(Some(0)));
        assert_eq!(parse_delta_resume(None, Some("91"), Some(7)), Ok(Some(91)));
        assert_eq!(parse_delta_resume(Some(b""), Some("91"), Some(7)), Ok(Some(91)));
        assert_eq!(parse_delta_resume(Some(b" \t"), None, None), Ok(None));
        assert_eq!(
            parse_delta_resume(Some(b" delta-v1:pane:7:95 \t"), Some("91"), Some(7)),
            Ok(Some(95)),
        );
        // The browser retains its original URL; a valid received header is
        // authoritative even if the bootstrap parameter is stale or invalid.
        assert_eq!(
            parse_delta_resume(Some(b"delta-v1:all:95"), Some("invalid"), None),
            Ok(Some(95)),
        );
    }

    #[test]
    fn delta_resume_rejects_legacy_malformed_and_cross_scope_cursors() {
        for value in [
            "17", "delta-v0:all:17", "delta-v1:all:", "delta-v1:all:-1",
            "delta-v1:all:+1", "delta-v1:all:1.0", "delta-v1:all: 1",
            "delta-v1:all:9223372036854775808", "delta-v1:all:1:2",
            "delta-v1:pane:7", "delta-v1:pane:7:1:2", "delta-v1:pane:+7:1",
            "delta-v1:pane:18446744073709551616:1", "delta-v1:unknown:1",
        ] {
            assert_eq!(parse_delta_resume(Some(value.as_bytes()), Some("0"), None), Err(INVALID_CURSOR), "{value}");
        }
        for value in ["", "-1", "+1", " 1", "1 ", "1.0", "1e3", "١", "9223372036854775808"] {
            assert_eq!(parse_delta_resume(None, Some(value), None), Err(INVALID_CURSOR), "{value}");
        }
        assert_eq!(parse_delta_resume(Some(&[0xff]), Some("0"), None), Err(INVALID_CURSOR));
        assert_eq!(parse_delta_resume(Some(&[b'1'; 129]), None, None), Err(INVALID_CURSOR));
        assert_eq!(parse_delta_cursor("delta-v1:pane:7:17", Some(8)), Err(CURSOR_SCOPE_MISMATCH));
        assert_eq!(parse_delta_cursor("delta-v1:pane:7:17", None), Err(CURSOR_SCOPE_MISMATCH));
        assert_eq!(parse_delta_cursor("delta-v1:all:17", Some(7)), Err(CURSOR_SCOPE_MISMATCH));
    }

    #[test]
    fn delta_data_uses_persisted_ids_and_control_frames_never_reset_them() {
        let delta = delta_frame_to_sse("delta", 2, Some(7), Some(9001), json!({"seq": 2})).unwrap();
        let bytes = String::from_utf8(delta.to_bytes()).unwrap();
        assert!(bytes.lines().any(|line| line == "id: delta-v1:pane:7:9001"));
        assert!(!bytes.lines().any(|line| line == "id: 2"));
        for kind in ["ready", "gap", "lag", "error"] {
            let control = delta_frame_to_sse(kind, 9002, Some(7), None, json!({})).unwrap();
            assert_eq!(control.id, None, "{kind}");
            let text = String::from_utf8(control.to_bytes()).unwrap();
            assert!(!text.lines().any(|line| line.starts_with("id:")), "{text}");
        }
    }

    #[test]
    fn delta_resume_drains_old_timestamps_without_replaying_the_delivered_prefix() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut limits = super::super::super::resolve_runtime_limits(None);
            limits.stream_scan_limit = 2;
            limits.stream_scan_max_pages = 1;
            let mut resumed = DeltaStreamState::new(Some(7), 10_000, Some(4), limits, 50);
            resumed.min_interval = Duration::ZERO;
            let (tx, rx) = mpsc::channel(8);
            let mut queries = Vec::new();
            assert!(resumed.catch_up(&cx, |query| {
                assert_eq!(query.since, None, "reconnect must not use its new wall clock as a cutoff");
                assert_eq!(query.pane_id, Some(7));
                queries.push(query.after_id);
                std::future::ready(Ok([1, 4, 9, 11].into_iter()
                    .filter(|id| *id > query.after_id.unwrap())
                    .take(query.limit).map(segment).collect()))
            }, &Redactor::new(), &tx).await);
            // Exactly-full final pages require an empty followup read. A
            // subsequent notification must not replay the recovered suffix.
            assert_eq!(queries, [Some(4), Some(11)]);
            assert!(resumed.catch_up(&cx, |query| {
                assert_eq!(query.after_id, Some(11));
                std::future::ready(Ok(Vec::new()))
            }, &Redactor::new(), &tx).await);
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.iter().map(frame_segment_id).collect::<Vec<_>>(), [9, 11]);
            assert_eq!(frames[0].id.as_deref(), Some("delta-v1:pane:7:9"));
            assert_eq!(frames[1].id.as_deref(), Some("delta-v1:pane:7:11"));
            assert_eq!(resumed.after_id, Some(11));
        });
    }

    #[test]
    fn delta_fresh_connection_keeps_its_no_history_timestamp_filter() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let limits = super::super::super::resolve_runtime_limits(None);
            let mut fresh = DeltaStreamState::new(Some(7), 10_000, None, limits, 50);
            let (tx, rx) = mpsc::channel(1);
            assert!(fresh.catch_up(&cx, |query| {
                assert_eq!(query.after_id, None);
                assert_eq!(query.since, Some(10_000));
                assert_eq!(query.pane_id, Some(7));
                std::future::ready(Ok(Vec::new()))
            }, &Redactor::new(), &tx).await);
            assert_eq!(fresh.after_id, None);
            drop(tx);
            assert!(drain(rx).is_empty());
        });
    }

    #[test]
    fn delta_reconnect_recovers_frames_queued_but_not_read_before_disconnect() {
        run_async(|| async {
            use crate::runtime_async::stream::Stream;
            use std::pin::Pin;
            use std::sync::Arc;
            use std::sync::atomic::{AtomicI64, Ordering};

            let cx = crate::cx::for_request();
            let producer_cx = cx.clone();
            let admitted = Arc::new(AtomicI64::new(0));
            let producer_admitted = Arc::clone(&admitted);
            let mut limits = super::super::super::resolve_runtime_limits(None);
            limits.stream_scan_limit = 8;
            let (tx, rx) = mpsc::channel(8);
            let producer = Box::pin(async move {
                let mut initial = DeltaStreamState::new(Some(7), 123, Some(0), limits, 50);
                initial.min_interval = Duration::ZERO;
                assert!(initial.catch_up(&producer_cx,
                    |_| std::future::ready(Ok((1..=3).map(segment).collect())),
                    &Redactor::new(), &tx).await);
                producer_admitted.store(initial.after_id.unwrap(), Ordering::SeqCst);
            });
            let mut stream = SseByteStream::new(cx.clone(), rx, producer, Box::pin(std::future::pending()));
            let first = crate::runtime_async::timeout_with_cx(&cx, Duration::from_secs(5),
                std::future::poll_fn(|poll| Pin::new(&mut stream).poll_next(poll)))
                .await.expect("first frame must arrive").expect("first delta");
            let first = String::from_utf8(first).unwrap();
            let received_id = first.lines().find_map(|line| line.strip_prefix("id: ")).unwrap().to_owned();
            assert_eq!(received_id, "delta-v1:pane:7:1");
            assert_eq!(admitted.load(Ordering::SeqCst), 3, "the producer queued more than the client read");
            drop(stream); // The unread frames 2 and 3 disappear with this body.

            let resume = parse_delta_resume(Some(received_id.as_bytes()), None, Some(7)).unwrap();
            let mut resumed = DeltaStreamState::new(Some(7), 10_000, resume, limits, 50);
            resumed.min_interval = Duration::ZERO;
            let (tx, rx) = mpsc::channel(8);
            assert!(resumed.catch_up(&cx, |query| {
                assert_eq!(query.after_id, Some(1));
                assert_eq!(query.since, None);
                std::future::ready(Ok((1..=4)
                    .filter(|id| *id > query.after_id.unwrap())
                    .map(segment).collect()))
            }, &Redactor::new(), &tx).await);
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.iter().map(frame_segment_id).collect::<Vec<_>>(), [2, 3, 4]);
        });
    }

    #[test]
    fn delta_resume_failure_does_not_replace_the_received_cursor() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let limits = super::super::super::resolve_runtime_limits(None);
            let mut resumed = DeltaStreamState::new(None, 10_000, Some(91), limits, 50);
            resumed.min_interval = Duration::ZERO;
            let (tx, rx) = mpsc::channel(1);
            assert!(!resumed.catch_up(&cx, |query| {
                assert_eq!(query.after_id, Some(91));
                assert_eq!(query.since, None);
                std::future::ready(Err(crate::Error::runtime_backend("fixture", "secret detail")))
            }, &Redactor::new(), &tx).await);
            assert_eq!(resumed.after_id, Some(91));
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].id, None);
            assert_eq!(frames[0].event_type.as_deref(), Some("error"));
        });
    }
}
