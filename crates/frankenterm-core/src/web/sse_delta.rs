//! Storage-backed terminal-output streaming. The response owns every wait;
//! cursor advancement means a frame was accepted, never merely attempted.

use super::super::error::json_err;
use super::super::extractors::{
    parse_u64, request_runtime_limits, require_storage_and_event_bus,
};
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

struct DeltaStreamState {
    pane_filter: Option<u64>,
    since_ms: i64,
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
        since_ms: i64,
        limits: WebRuntimeLimits,
        max_hz: u64,
    ) -> Self {
        Self {
            pane_filter,
            since_ms,
            after_id: None,
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
        data: serde_json::Value,
    ) -> bool {
        if !advance_stream_sequence(&mut self.seq) {
            return false;
        }
        let frame = make_stream_frame("deltas", kind, self.seq, data);
        let Some(event) = frame_to_sse(kind, self.seq, frame) else {
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
                since: Some(self.since_ms),
                until: None,
                limit: self.scan_limit,
            };
            let segments = match fetch(query).await {
                Ok(segments) => segments,
                Err(_) => {
                    let _ = self
                        .emit(cx, tx, "error", json!({"code": "storage_error"}))
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
                        && self.pane_filter.is_none_or(|pane| segment.pane_id == pane);
                    previous = segment.id;
                    valid
                });
            if !valid {
                let _ = self
                    .emit(cx, tx, "error", json!({"code": "invalid_segment_page"}))
                    .await;
                return false;
            }
            for segment in segments {
                if !self
                    .emit(
                        cx,
                        tx,
                        "delta",
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
    let pane_filter = parse_u64(&qs, "pane_id");
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
            let mut state = DeltaStreamState::new(pane_filter, started_at_ms, limits, max_hz);
            if !advance_stream_sequence(&mut state.seq) {
                return;
            }
            let ready = make_stream_frame(
                "deltas",
                "ready",
                state.seq,
                json!({"max_hz": max_hz, "pane_id": pane_filter}),
            );
            let Some(ready) = frame_to_sse("ready", state.seq, ready) else {
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
                            .emit(&child_cx, &tx, "lag", json!({"missed_count": missed_count}))
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
        let mut state = DeltaStreamState::new(Some(7), 123, limits, 50);
        state.min_interval = Duration::ZERO;
        state
    }

    fn segment(id: i64) -> Segment {
        Segment {
            id,
            pane_id: 7,
            seq: u64::try_from(id).unwrap(),
            content: format!("segment {id}"),
            content_len: 9,
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
}
