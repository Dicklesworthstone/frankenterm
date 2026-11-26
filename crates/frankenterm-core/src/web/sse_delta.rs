//! Storage-backed terminal-output streaming. The response owns every wait;
//! cursor advancement means a frame was accepted, never merely attempted.
//! Reconnect uses an epoch- and pane-scoped persisted segment id. The ready
//! frame establishes the initial tail boundary; later control frames never
//! overwrite the client's last received segment cursor.

use super::super::error::json_err;
use super::super::extractors::{
    parse_pane_filter, query_text, request_runtime_limits, require_storage_and_event_bus,
};
use super::super::{STREAM_CHANNEL_BUFFER, WebRuntimeLimits};
use super::{
    SseByteStream, SseEvent, SseResponse, advance_stream_sequence, frame_to_sse, make_stream_frame,
    parse_stream_max_hz, send_rate_limited_sse, send_replay_sse, stream_min_interval,
};
use crate::events::{Event, RecvError};
use crate::policy::Redactor;
use crate::runtime_async::{mpsc, task};
use crate::storage::{
    SEGMENT_STREAM_PAGE_MAX, SegmentScanQuery, SegmentStreamPage, SegmentStreamSnapshot,
};
use crate::web_framework::{QueryString, Request, Response, StatusCode, WebStreamLifecycle};
use serde_json::json;
use std::future::Future;
use std::time::{Duration, Instant};

const DELTA_CURSOR_PREFIX: &str = "delta-v2:";
const INVALID_CURSOR: &str = "invalid_delta_cursor";
const CURSOR_SCOPE_MISMATCH: &str = "delta_cursor_scope_mismatch";

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeltaResumeCursor {
    epoch: String,
    after_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeltaResume {
    Cursor(DeltaResumeCursor),
    /// A deliberate query against the current database, never a reconnect.
    Bootstrap(i64),
}

fn decimal_u64(value: &str) -> Option<u64> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn segment_id(value: &str) -> Option<i64> {
    decimal_u64(value).and_then(|id| i64::try_from(id).ok())
}

fn encode_delta_cursor(epoch: &str, pane_filter: Option<u64>, id: i64) -> String {
    match pane_filter {
        Some(pane) => format!("{DELTA_CURSOR_PREFIX}{epoch}:pane:{pane}:{id}"),
        None => format!("{DELTA_CURSOR_PREFIX}{epoch}:all:{id}"),
    }
}

/// Legacy cursors do not identify a capture database. Never infer an epoch
/// from today's database or reuse a cursor with a different pane selection.
fn parse_delta_cursor(
    value: &str,
    pane_filter: Option<u64>,
) -> Result<DeltaResumeCursor, &'static str> {
    if value.len() > 128 {
        return Err(INVALID_CURSOR);
    }
    let suffix = value
        .strip_prefix(DELTA_CURSOR_PREFIX)
        .ok_or(INVALID_CURSOR)?;
    let (epoch, scoped_id) = suffix.split_once(':').ok_or(INVALID_CURSOR)?;
    if epoch.len() != 32
        || !epoch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(INVALID_CURSOR);
    }
    let (scope, id) = if let Some(id) = scoped_id.strip_prefix("all:") {
        (None, id)
    } else if let Some(pane_and_id) = scoped_id.strip_prefix("pane:") {
        let (pane, id) = pane_and_id.split_once(':').ok_or(INVALID_CURSOR)?;
        (Some(decimal_u64(pane).ok_or(INVALID_CURSOR)?), id)
    } else {
        return Err(INVALID_CURSOR);
    };
    let id = segment_id(id).ok_or(INVALID_CURSOR)?;
    if scope != pane_filter {
        return Err(CURSOR_SCOPE_MISMATCH);
    }
    Ok(DeltaResumeCursor {
        epoch: epoch.to_owned(),
        after_id: id,
    })
}

/// A browser's reconnect header supersedes the original URL's bootstrap id.
/// An empty header means no retained cursor; malformed nonempty headers must
/// fail instead of silently downgrading the connection to fresh/live-only.
fn parse_delta_resume(
    last_event_id: Option<&[u8]>,
    cursor: Option<&str>,
    since_id: Option<&str>,
    pane_filter: Option<u64>,
) -> Result<Option<DeltaResume>, &'static str> {
    if let Some(bytes) = last_event_id {
        if bytes.len() > 128 || bytes.iter().any(|&byte| matches!(byte, b'\r' | b'\n' | 0)) {
            return Err(INVALID_CURSOR);
        }
        let value = std::str::from_utf8(bytes)
            .map_err(|_| INVALID_CURSOR)?
            .trim_matches([' ', '\t']);
        if !value.is_empty() {
            return parse_delta_cursor(value, pane_filter)
                .map(DeltaResume::Cursor)
                .map(Some);
        }
    }
    if let Some(cursor) = cursor {
        return parse_delta_cursor(cursor, pane_filter)
            .map(DeltaResume::Cursor)
            .map(Some);
    }
    since_id
        .map(|value| {
            segment_id(value)
                .map(DeltaResume::Bootstrap)
                .ok_or(INVALID_CURSOR)
        })
        .transpose()
}

fn delta_frame_to_sse(
    kind: &'static str,
    seq: u64,
    epoch: &str,
    pane_filter: Option<u64>,
    persisted_segment_id: Option<i64>,
    frame: serde_json::Value,
) -> Option<SseEvent> {
    frame_to_sse(kind, seq, frame).map(|mut event| {
        event.id = persisted_segment_id.map(|id| encode_delta_cursor(epoch, pane_filter, id));
        event
    })
}

struct DeltaStreamState {
    pane_filter: Option<u64>,
    epoch: String,
    after_id: i64,
    observed_high_water: i64,
    scan_limit: usize,
    pages_per_yield: usize,
    seq: u64,
    next_emit_at: Instant,
    min_interval: Duration,
}

impl DeltaStreamState {
    fn new(
        pane_filter: Option<u64>,
        cursor: DeltaResumeCursor,
        limits: WebRuntimeLimits,
        max_hz: u64,
    ) -> Self {
        Self {
            pane_filter,
            epoch: cursor.epoch,
            // Both fresh and resumed streams use a committed ID boundary.
            // Capture timestamps can move backwards while IDs advance.
            after_id: cursor.after_id,
            observed_high_water: cursor.after_id,
            // Defend direct/internal construction as well as validated config.
            scan_limit: limits.stream_scan_limit.clamp(1, SEGMENT_STREAM_PAGE_MAX),
            pages_per_yield: limits.stream_scan_max_pages.max(1),
            seq: 0,
            next_emit_at: Instant::now(),
            min_interval: stream_min_interval(max_hz),
        }
    }

    fn discontinuity(&self, snapshot: &SegmentStreamSnapshot) -> Option<&'static str> {
        if snapshot.cursor_epoch != self.epoch {
            Some("cursor_epoch_mismatch")
        } else if self.after_id > snapshot.max_segment_id {
            Some("cursor_ahead_of_high_water")
        } else if snapshot.max_segment_id < self.observed_high_water {
            Some("high_water_regressed")
        } else {
            None
        }
    }

    async fn emit_discontinuity(
        &mut self,
        cx: &crate::cx::Cx,
        tx: &mpsc::Sender<SseEvent>,
        reason: &'static str,
    ) {
        let resume_cursor = encode_delta_cursor(&self.epoch, self.pane_filter, self.after_id);
        let _ = self
            .emit(
                cx,
                tx,
                "cursor_discontinuity",
                None,
                json!({
                    "code": "delta_cursor_discontinuity",
                    "terminal": true,
                    "reason": reason,
                    "resume_cursor": resume_cursor,
                }),
            )
            .await;
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
            kind,
            self.seq,
            &self.epoch,
            self.pane_filter,
            persisted_segment_id,
            frame,
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
        Fut: Future<Output = crate::Result<SegmentStreamPage>>,
    {
        let mut pages = 0;
        loop {
            if cx.checkpoint().is_err() {
                return false;
            }
            let query = SegmentScanQuery {
                after_id: Some(self.after_id),
                pane_id: self.pane_filter,
                since: None,
                until: None,
                limit: self.scan_limit,
            };
            let page = match fetch(query).await {
                Ok(page) => page,
                Err(_) => {
                    let _ = self
                        .emit(cx, tx, "error", None, json!({"code": "storage_error"}))
                        .await;
                    return false;
                }
            };
            // The page and authority are read together, including empty
            // pages. A new database cannot supply rows under an old cursor.
            if let Some(reason) = self.discontinuity(&page.snapshot) {
                self.emit_discontinuity(cx, tx, reason).await;
                return false;
            }
            let segments = page.segments;
            let page_len = segments.len();
            let mut previous = self.after_id;
            // Reject a broken query contract before any row in the page is
            // admitted. In particular, never skip unseen rows by accepting
            // an out-of-order, duplicate, or wrong-pane result.
            let valid = page_len <= self.scan_limit
                && segments.iter().all(|segment| {
                    let valid = segment.id > previous
                        && segment.id <= page.snapshot.max_segment_id
                        && self.pane_filter.is_none_or(|pane| segment.pane_id == pane);
                    previous = segment.id;
                    valid
                });
            if !valid {
                let _ = self
                    .emit(
                        cx,
                        tx,
                        "error",
                        None,
                        json!({"code": "invalid_segment_page"}),
                    )
                    .await;
                return false;
            }
            self.observed_high_water = page.snapshot.max_segment_id;
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
                self.after_id = segment.id;
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

pub fn handle_stream_deltas(
    req: &Request,
) -> std::pin::Pin<Box<dyn Future<Output = Response> + Send>> {
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);
    let pane_filter = match parse_pane_filter(&qs) {
        Ok(pane_id) => pane_id,
        Err(response) => return Box::pin(async move { response }),
    };
    // Browser reconnects send the cursor through URLSearchParams, encoded.
    let cursor = query_text(&qs, "cursor");
    let resume_after = match parse_delta_resume(
        req.headers().get("last-event-id"),
        cursor.as_deref(),
        qs.get("since_id"),
        pane_filter,
    ) {
        Ok(cursor) => cursor,
        Err(code) => {
            return Box::pin(async move {
                json_err(
                    StatusCode::BAD_REQUEST,
                    code,
                    "Use a complete delta-v2 cursor for this pane selection or a deliberate since_id bootstrap",
                )
            });
        }
    };
    let limits = request_runtime_limits(req);
    let max_hz = parse_stream_max_hz(&qs, limits);
    let result = require_storage_and_event_bus(req);
    let lifecycle = req.get_extension::<WebStreamLifecycle>().cloned();
    let session = req
        .get_extension::<super::super::auth::SessionGrant>()
        .cloned();
    let admission = super::super::auth::reserve_stream(req);

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
        let permit = match admission {
            Ok(permit) => permit,
            Err(response) => return response,
        };
        let shutdown = super::super::auth::stream_shutdown(
            lifecycle.shutdown_waiter(&stream_cx),
            session,
            &stream_cx,
        );
        let recv_cx = stream_cx.clone();
        // Subscribe before capturing the tail boundary so a committed row
        // after that read cannot fall between the snapshot and subscription.
        // Only the scoped tail is exposed as the fresh cursor; the global
        // allocation watermark stays internal. Periodic scans also recover
        // missing bus notifications and fence a changed capture database.
        let mut subscriber = event_bus.subscribe_deltas();
        let snapshot = match storage
            .get_segment_stream_snapshot_with_cx(&stream_cx, pane_filter)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(_) => {
                return json_err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_error",
                    "Unable to establish the terminal output stream boundary",
                );
            }
        };
        let cursor = match resume_after {
            Some(DeltaResume::Cursor(cursor)) => cursor,
            Some(DeltaResume::Bootstrap(after_id)) => DeltaResumeCursor {
                epoch: snapshot.cursor_epoch.clone(),
                after_id,
            },
            None => DeltaResumeCursor {
                epoch: snapshot.cursor_epoch.clone(),
                after_id: snapshot.tail_segment_id,
            },
        };
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_BUFFER);
        let producer = async move {
            let child_cx = stream_cx;
            let mut state = DeltaStreamState::new(pane_filter, cursor, limits, max_hz);
            // A named terminal frame is observable by EventSource, unlike
            // an HTTP error body. It never adopts or acknowledges a new epoch.
            if let Some(reason) = state.discontinuity(&snapshot) {
                state.emit_discontinuity(&child_cx, &tx, reason).await;
                return;
            }
            state.observed_high_water = snapshot.max_segment_id;
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
                    "resume_after_id": state.after_id,
                    "cursor": encode_delta_cursor(&state.epoch, pane_filter, state.after_id),
                    "cursor_format": "delta-v2",
                    "recovery": "retained_segments_only",
                }),
            );
            // Even a client that disconnects before its first delta must
            // retain this boundary. Otherwise reconnect silently adopts a
            // later tail and loses output captured during the disconnect.
            let Some(ready) = delta_frame_to_sse(
                "ready",
                state.seq,
                &state.epoch,
                pane_filter,
                Some(state.after_id),
                ready,
            ) else {
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
                    |query| storage.get_segments_stream_page_with_cx(&child_cx, query),
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
                            |query| storage.get_segments_stream_page_with_cx(&child_cx, query),
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
                                |query| storage.get_segments_stream_page_with_cx(&child_cx, query),
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
                            .emit(
                                &child_cx,
                                &tx,
                                "lag",
                                None,
                                json!({"missed_count": missed_count}),
                            )
                            .await
                        {
                            break;
                        }
                        if !state
                            .catch_up(
                                &child_cx,
                                |query| storage.get_segments_stream_page_with_cx(&child_cx, query),
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
            permit,
        ))
        .into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::stream_permit;
    use super::*;
    use crate::runtime_async::CompatRuntime;
    use crate::storage::Segment;

    const TEST_EPOCH: &str = "0123456789abcdef0123456789abcdef";

    fn cursor(after_id: i64) -> DeltaResumeCursor {
        DeltaResumeCursor {
            epoch: TEST_EPOCH.to_owned(),
            after_id,
        }
    }

    fn token(pane: Option<u64>, id: i64) -> String {
        encode_delta_cursor(TEST_EPOCH, pane, id)
    }

    fn page(segments: Vec<Segment>) -> SegmentStreamPage {
        SegmentStreamPage {
            snapshot: SegmentStreamSnapshot {
                cursor_epoch: TEST_EPOCH.to_owned(),
                max_segment_id: i64::MAX,
                tail_segment_id: segments.iter().map(|segment| segment.id).max().unwrap_or(0),
            },
            segments,
        }
    }

    fn ready_page(segments: Vec<Segment>) -> std::future::Ready<crate::Result<SegmentStreamPage>> {
        std::future::ready(Ok(page(segments)))
    }

    fn run_async<F>(f: impl FnOnce() -> F + Send + 'static)
    where
        F: Future<Output = ()>,
    {
        let result = std::thread::spawn(move || {
            let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
                .build()
                .expect("test runtime");
            let test =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(f())));
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
        let mut state = DeltaStreamState::new(Some(7), cursor(0), limits, 50);
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
        let value: serde_json::Value =
            serde_json::from_str(frame.data.as_deref().unwrap()).unwrap();
        value["data"]["segment_id"].as_i64().unwrap()
    }

    #[test]
    fn delta_catchup_drains_multiple_windows_and_sparse_ids_in_order() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut state = state();
            let (tx, rx) = mpsc::channel(8);
            let mut queries = Vec::new();
            assert!(
                state
                    .catch_up(
                        &cx,
                        |query| {
                            assert_eq!(query.limit, 2);
                            assert_eq!(query.pane_id, Some(7));
                            assert_eq!(query.since, None);
                            queries.push(query.after_id);
                            ready_page(
                                [2, 4, 9, 11, 20]
                                    .into_iter()
                                    .filter(|id| *id > query.after_id.unwrap_or(0))
                                    .take(query.limit)
                                    .map(segment)
                                    .collect(),
                            )
                        },
                        &Redactor::new(),
                        &tx
                    )
                    .await
            );
            assert_eq!(queries, [Some(0), Some(4), Some(11)]);
            assert_eq!(state.after_id, 20);
            drop(tx);
            let frames = drain(rx);
            assert_eq!(
                frames.iter().map(frame_segment_id).collect::<Vec<_>>(),
                [2, 4, 9, 11, 20]
            );
        });
    }

    #[test]
    fn delta_storage_failure_preserves_the_last_admitted_cursor() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut state = state();
            let (tx, rx) = mpsc::channel(4);
            let mut calls = 0;
            assert!(
                !state
                    .catch_up(
                        &cx,
                        |_| {
                            calls += 1;
                            std::future::ready(if calls == 1 {
                                Ok(page(vec![segment(1), segment(2)]))
                            } else {
                                Err(crate::Error::runtime_backend(
                                    "fixture",
                                    "private storage detail",
                                ))
                            })
                        },
                        &Redactor::new(),
                        &tx
                    )
                    .await
            );
            assert_eq!(state.after_id, 2);
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.len(), 3);
            assert_eq!(frames[2].event_type.as_deref(), Some("error"));
            assert_eq!(frames[2].id, None);
            assert!(frames[2].data.as_deref().unwrap().contains("storage_error"));
            assert!(
                !frames[2]
                    .data
                    .as_deref()
                    .unwrap()
                    .contains("private storage detail")
            );
        });
    }

    #[test]
    fn delta_changed_epoch_and_rollback_stop_before_empty_or_populated_pages() {
        run_async(|| async {
            for reason in [
                "cursor_epoch_mismatch",
                "cursor_ahead_of_high_water",
                "high_water_regressed",
            ] {
                for rows in [Vec::new(), vec![segment(11)]] {
                    let cx = crate::cx::for_request();
                    let mut state = state();
                    state.after_id = 10;
                    state.observed_high_water = 20;
                    let mut result = page(rows);
                    match reason {
                        "cursor_epoch_mismatch" => result.snapshot.cursor_epoch = "f".repeat(32),
                        "cursor_ahead_of_high_water" => result.snapshot.max_segment_id = 9,
                        "high_water_regressed" => result.snapshot.max_segment_id = 15,
                        _ => unreachable!(),
                    }
                    let mut result = Some(result);
                    let (tx, rx) = mpsc::channel(1);
                    assert!(
                        !state
                            .catch_up(
                                &cx,
                                |_| std::future::ready(Ok(result.take().unwrap())),
                                &Redactor::new(),
                                &tx,
                            )
                            .await
                    );
                    assert_eq!(state.after_id, 10);
                    assert_eq!(state.epoch, TEST_EPOCH);
                    assert_eq!(state.observed_high_water, 20);
                    drop(tx);
                    let frames = drain(rx);
                    assert_eq!(frames.len(), 1);
                    assert_eq!(
                        frames[0].event_type.as_deref(),
                        Some("cursor_discontinuity")
                    );
                    assert_eq!(frames[0].id, None);
                    let frame: serde_json::Value =
                        serde_json::from_str(frames[0].data.as_deref().unwrap()).unwrap();
                    assert_eq!(frame["data"]["code"], "delta_cursor_discontinuity");
                    assert_eq!(frame["data"]["terminal"], true);
                    assert_eq!(frame["data"]["reason"], reason);
                    assert_eq!(frame["data"]["resume_cursor"], token(Some(7), 10));
                }
            }
        });
    }

    #[test]
    fn delta_epoch_change_between_pages_preserves_the_last_admitted_cursor() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let mut state = state();
            let mut calls = 0;
            let (tx, rx) = mpsc::channel(3);
            assert!(
                !state
                    .catch_up(
                        &cx,
                        |_| {
                            calls += 1;
                            let mut result = page(if calls == 1 {
                                vec![segment(1), segment(2)]
                            } else {
                                vec![segment(3)]
                            });
                            if calls > 1 {
                                result.snapshot.cursor_epoch = "f".repeat(32);
                            }
                            std::future::ready(Ok(result))
                        },
                        &Redactor::new(),
                        &tx,
                    )
                    .await
            );
            assert_eq!(calls, 2);
            assert_eq!(state.after_id, 2);
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.len(), 3);
            assert_eq!(frame_segment_id(&frames[0]), 1);
            assert_eq!(frame_segment_id(&frames[1]), 2);
            assert_eq!(frames[2].id, None);
            let frame: serde_json::Value =
                serde_json::from_str(frames[2].data.as_deref().unwrap()).unwrap();
            assert_eq!(frame["data"]["resume_cursor"], token(Some(7), 2));
            assert_eq!(frame["data"]["reason"], "cursor_epoch_mismatch");
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
                assert!(
                    !state
                        .catch_up(
                            &cx,
                            |_| ready_page(rows.take().unwrap()),
                            &Redactor::new(),
                            &tx
                        )
                        .await
                );
                assert_eq!(state.after_id, 0);
                drop(tx);
                let frames = drain(rx);
                assert_eq!(frames.len(), 1);
                assert!(
                    frames[0]
                        .data
                        .as_deref()
                        .unwrap()
                        .contains("invalid_segment_page")
                );
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
            assert!(
                !closed
                    .catch_up(&cx, |_| ready_page(vec![segment(1)]), &Redactor::new(), &tx)
                    .await
            );
            assert_eq!(closed.after_id, 0);

            let mut exhausted = state();
            exhausted.seq = u64::MAX;
            let (tx, rx) = mpsc::channel(1);
            assert!(
                !exhausted
                    .catch_up(&cx, |_| ready_page(vec![segment(1)]), &Redactor::new(), &tx)
                    .await
            );
            assert_eq!(exhausted.after_id, 0);
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
                assert!(
                    state
                        .catch_up(
                            &producer_cx,
                            |query| {
                                ready_page(
                                    (1..=65)
                                        .filter(|id| *id > query.after_id.unwrap_or(0))
                                        .take(query.limit)
                                        .map(segment)
                                        .collect(),
                                )
                            },
                            &Redactor::new(),
                            &tx
                        )
                        .await
                );
                producer_cursor.store(state.after_id, Ordering::SeqCst);
            });
            let mut stream = SseByteStream::new(
                cx.clone(),
                rx,
                producer,
                Box::pin(std::future::pending()),
                stream_permit(),
            );
            let frames =
                crate::runtime_async::timeout_with_cx(&cx, Duration::from_secs(5), async {
                    let mut frames = Vec::new();
                    while let Some(bytes) =
                        std::future::poll_fn(|poll| Pin::new(&mut stream).poll_next(poll)).await
                    {
                        let text = String::from_utf8(bytes).unwrap();
                        let payload = text
                            .lines()
                            .find_map(|line| line.strip_prefix("data: "))
                            .unwrap();
                        let value: serde_json::Value = serde_json::from_str(payload).unwrap();
                        frames.push(value["data"]["segment_id"].as_i64().unwrap());
                    }
                    frames
                })
                .await
                .expect("bounded replay must finish through the actual body");
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
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
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
                let _ = state
                    .catch_up(
                        &producer_cx,
                        |_| ready_page(vec![segment(1), segment(2)]),
                        &Redactor::new(),
                        &tx,
                    )
                    .await;
            });
            let mut stream = SseByteStream::new(cx, rx, producer, shutdown, stream_permit());
            let mut poll = Context::from_waker(Waker::noop());
            assert!(matches!(
                Pin::new(&mut stream).poll_next(&mut poll),
                Poll::Ready(Some(_))
            ));
            assert!(!dropped.load(Ordering::SeqCst));
            lifecycle.signal_shutdown();
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut poll),
                Poll::Ready(None)
            );
            assert!(dropped.load(Ordering::SeqCst));
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut poll),
                Poll::Ready(None)
            );
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
            assert!(
                state
                    .catch_up(
                        &cx,
                        |_| ready_page(vec![row.take().unwrap()]),
                        &redactor,
                        &tx
                    )
                    .await
            );
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.len(), 1);
            let payload = frames[0].data.as_deref().unwrap();
            assert!(payload.contains("visible-marker"));
            assert!(!payload.contains(&secret));
        });
    }

    #[test]
    fn delta_cursor_has_a_versioned_epoch_and_pane_scoped_wire_format() {
        assert_eq!(token(None, 9001), format!("delta-v2:{TEST_EPOCH}:all:9001"));
        assert_eq!(
            token(Some(7), 9001),
            format!("delta-v2:{TEST_EPOCH}:pane:7:9001")
        );
        for pane in [None, Some(0), Some(7), Some(u64::MAX)] {
            for id in [0, 1, 9001, i64::MAX] {
                assert_eq!(parse_delta_cursor(&token(pane, id), pane), Ok(cursor(id)));
            }
        }
    }

    #[test]
    fn delta_resume_header_supersedes_bootstrap_query_but_empty_header_does_not() {
        assert_eq!(parse_delta_resume(None, None, None, None), Ok(None));
        assert_eq!(
            parse_delta_resume(None, None, Some("0"), None),
            Ok(Some(DeltaResume::Bootstrap(0)))
        );
        assert_eq!(
            parse_delta_resume(Some(b""), None, Some("91"), Some(7)),
            Ok(Some(DeltaResume::Bootstrap(91)))
        );
        assert_eq!(parse_delta_resume(Some(b" \t"), None, None, None), Ok(None));
        let query = token(Some(7), 91);
        let header = format!(" {} \t", token(Some(7), 95));
        assert_eq!(
            parse_delta_resume(Some(header.as_bytes()), Some(&query), Some("0"), Some(7)),
            Ok(Some(DeltaResume::Cursor(cursor(95))))
        );
        assert_eq!(
            parse_delta_resume(Some(b""), Some(&query), Some("0"), Some(7)),
            Ok(Some(DeltaResume::Cursor(cursor(91))))
        );
        assert_eq!(
            parse_delta_resume(None, Some("invalid"), Some("0"), Some(7)),
            Err(INVALID_CURSOR)
        );
        // The browser retains its original URL; a valid received header is
        // authoritative even if the bootstrap parameter is stale or invalid.
        assert_eq!(
            parse_delta_resume(
                Some(header.as_bytes()),
                Some("invalid"),
                Some("invalid"),
                Some(7)
            ),
            Ok(Some(DeltaResume::Cursor(cursor(95)))),
        );
    }

    #[test]
    fn delta_resume_rejects_legacy_malformed_and_cross_scope_cursors() {
        for value in [
            "17".to_owned(),
            "delta-v1:all:17".to_owned(),
            "delta-v1:pane:7:17".to_owned(),
            format!("delta-v2:{TEST_EPOCH}:all:"),
            format!("delta-v2:{TEST_EPOCH}:all:-1"),
            format!("delta-v2:{TEST_EPOCH}:all:+1"),
            format!("delta-v2:{TEST_EPOCH}:all:01"),
            format!("delta-v2:{TEST_EPOCH}:all:1.0"),
            format!("delta-v2:{TEST_EPOCH}:all: 1"),
            format!("delta-v2:{TEST_EPOCH}:all:9223372036854775808"),
            format!("delta-v2:{TEST_EPOCH}:all:1:2"),
            format!("delta-v2:{TEST_EPOCH}:pane:7"),
            format!("delta-v2:{TEST_EPOCH}:pane:07:1"),
            format!("delta-v2:{TEST_EPOCH}:pane:18446744073709551616:1"),
            format!("delta-v2:{TEST_EPOCH}:unknown:1"),
            format!("delta-v2:{}:all:1", TEST_EPOCH.to_uppercase()),
            format!("{}\n", token(None, 1)),
            format!("\r{}", token(None, 1)),
            format!("\0{}", token(None, 1)),
            "x".repeat(129),
        ] {
            assert_eq!(
                parse_delta_resume(
                    Some(value.as_bytes()),
                    Some(&token(None, 1)),
                    Some("0"),
                    None
                ),
                Err(INVALID_CURSOR),
                "{value}"
            );
        }
        for value in [
            "",
            "-1",
            "+1",
            " 1",
            "1 ",
            "1.0",
            "1e3",
            "١",
            "9223372036854775808",
        ] {
            assert_eq!(
                parse_delta_resume(None, None, Some(value), None),
                Err(INVALID_CURSOR),
                "{value}"
            );
        }
        assert_eq!(
            parse_delta_resume(Some(&[0xff]), None, Some("0"), None),
            Err(INVALID_CURSOR)
        );
        assert_eq!(
            parse_delta_resume(Some(&[b'1'; 129]), None, None, None),
            Err(INVALID_CURSOR)
        );
        assert_eq!(
            parse_delta_cursor(&token(Some(7), 17), Some(8)),
            Err(CURSOR_SCOPE_MISMATCH)
        );
        assert_eq!(
            parse_delta_cursor(&token(Some(7), 17), None),
            Err(CURSOR_SCOPE_MISMATCH)
        );
        assert_eq!(
            parse_delta_cursor(&token(None, 17), Some(7)),
            Err(CURSOR_SCOPE_MISMATCH)
        );
    }

    #[test]
    fn delta_ready_and_data_use_persisted_ids_and_other_frames_never_reset_them() {
        let ready =
            delta_frame_to_sse("ready", 1, TEST_EPOCH, Some(7), Some(9000), json!({})).unwrap();
        assert_eq!(ready.id.as_deref(), Some(token(Some(7), 9000).as_str()));
        let delta = delta_frame_to_sse(
            "delta",
            2,
            TEST_EPOCH,
            Some(7),
            Some(9001),
            json!({"seq": 2}),
        )
        .unwrap();
        let bytes = String::from_utf8(delta.to_bytes()).unwrap();
        assert!(
            bytes
                .lines()
                .any(|line| line == format!("id: {}", token(Some(7), 9001)))
        );
        assert!(!bytes.lines().any(|line| line == "id: 2"));
        for kind in ["gap", "lag", "error", "cursor_discontinuity"] {
            let control =
                delta_frame_to_sse(kind, 9002, TEST_EPOCH, Some(7), None, json!({})).unwrap();
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
            let mut resumed = DeltaStreamState::new(Some(7), cursor(4), limits, 50);
            resumed.min_interval = Duration::ZERO;
            let (tx, rx) = mpsc::channel(8);
            let mut queries = Vec::new();
            assert!(
                resumed
                    .catch_up(
                        &cx,
                        |query| {
                            assert_eq!(
                                query.since, None,
                                "reconnect must not use its new wall clock as a cutoff"
                            );
                            assert_eq!(query.pane_id, Some(7));
                            queries.push(query.after_id);
                            ready_page(
                                [1, 4, 9, 11]
                                    .into_iter()
                                    .filter(|id| *id > query.after_id.unwrap())
                                    .take(query.limit)
                                    .map(segment)
                                    .collect(),
                            )
                        },
                        &Redactor::new(),
                        &tx
                    )
                    .await
            );
            // Exactly-full final pages require an empty followup read. A
            // subsequent notification must not replay the recovered suffix.
            assert_eq!(queries, [Some(4), Some(11)]);
            assert!(
                resumed
                    .catch_up(
                        &cx,
                        |query| {
                            assert_eq!(query.after_id, Some(11));
                            ready_page(Vec::new())
                        },
                        &Redactor::new(),
                        &tx
                    )
                    .await
            );
            drop(tx);
            let frames = drain(rx);
            assert_eq!(
                frames.iter().map(frame_segment_id).collect::<Vec<_>>(),
                [9, 11]
            );
            assert_eq!(frames[0].id.as_deref(), Some(token(Some(7), 9).as_str()));
            assert_eq!(frames[1].id.as_deref(), Some(token(Some(7), 11).as_str()));
            assert_eq!(resumed.after_id, 11);
        });
    }

    #[test]
    fn delta_fresh_tail_uses_ids_and_keeps_new_rows_with_old_timestamps() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let limits = super::super::super::resolve_runtime_limits(None);
            let mut fresh = DeltaStreamState::new(Some(7), cursor(4), limits, 50);
            fresh.min_interval = Duration::ZERO;
            let (tx, rx) = mpsc::channel(2);
            assert!(
                fresh
                    .catch_up(
                        &cx,
                        |query| {
                            assert_eq!(query.after_id, Some(4));
                            assert_eq!(query.since, None);
                            assert_eq!(query.pane_id, Some(7));
                            ready_page(
                                [1, 4, 9, 11]
                                    .into_iter()
                                    .filter(|id| *id > query.after_id.unwrap())
                                    .map(|id| {
                                        let mut row = segment(id);
                                        row.captured_at = if id == 9 { i64::MIN } else { 0 };
                                        row
                                    })
                                    .collect(),
                            )
                        },
                        &Redactor::new(),
                        &tx
                    )
                    .await
            );
            assert_eq!(fresh.after_id, 11);
            drop(tx);
            assert_eq!(
                drain(rx).iter().map(frame_segment_id).collect::<Vec<_>>(),
                [9, 11]
            );
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
                let mut initial = DeltaStreamState::new(Some(7), cursor(0), limits, 50);
                initial.min_interval = Duration::ZERO;
                assert!(
                    initial
                        .catch_up(
                            &producer_cx,
                            |_| ready_page((1..=3).map(segment).collect()),
                            &Redactor::new(),
                            &tx
                        )
                        .await
                );
                producer_admitted.store(initial.after_id, Ordering::SeqCst);
            });
            let mut stream = SseByteStream::new(
                cx.clone(),
                rx,
                producer,
                Box::pin(std::future::pending()),
                stream_permit(),
            );
            let first = crate::runtime_async::timeout_with_cx(
                &cx,
                Duration::from_secs(5),
                std::future::poll_fn(|poll| Pin::new(&mut stream).poll_next(poll)),
            )
            .await
            .expect("first frame must arrive")
            .expect("first delta");
            let first = String::from_utf8(first).unwrap();
            let received_id = first
                .lines()
                .find_map(|line| line.strip_prefix("id: "))
                .unwrap()
                .to_owned();
            assert_eq!(received_id, token(Some(7), 1));
            assert_eq!(
                admitted.load(Ordering::SeqCst),
                3,
                "the producer queued more than the client read"
            );
            drop(stream); // The unread frames 2 and 3 disappear with this body.

            let resume =
                parse_delta_resume(Some(received_id.as_bytes()), None, None, Some(7)).unwrap();
            let Some(DeltaResume::Cursor(resume)) = resume else {
                panic!("received cursor must stay epoch-bound");
            };
            let mut resumed = DeltaStreamState::new(Some(7), resume, limits, 50);
            resumed.min_interval = Duration::ZERO;
            let (tx, rx) = mpsc::channel(8);
            assert!(
                resumed
                    .catch_up(
                        &cx,
                        |query| {
                            assert_eq!(query.after_id, Some(1));
                            assert_eq!(query.since, None);
                            ready_page(
                                (1..=4)
                                    .filter(|id| *id > query.after_id.unwrap())
                                    .map(segment)
                                    .collect(),
                            )
                        },
                        &Redactor::new(),
                        &tx
                    )
                    .await
            );
            drop(tx);
            let frames = drain(rx);
            assert_eq!(
                frames.iter().map(frame_segment_id).collect::<Vec<_>>(),
                [2, 3, 4]
            );
        });
    }

    #[test]
    fn delta_resume_failure_does_not_replace_the_received_cursor() {
        run_async(|| async {
            let cx = crate::cx::for_request();
            let limits = super::super::super::resolve_runtime_limits(None);
            let mut resumed = DeltaStreamState::new(None, cursor(91), limits, 50);
            resumed.min_interval = Duration::ZERO;
            let (tx, rx) = mpsc::channel(1);
            assert!(
                !resumed
                    .catch_up(
                        &cx,
                        |query| {
                            assert_eq!(query.after_id, Some(91));
                            assert_eq!(query.since, None);
                            std::future::ready(Err(crate::Error::runtime_backend(
                                "fixture",
                                "secret detail",
                            )))
                        },
                        &Redactor::new(),
                        &tx
                    )
                    .await
            );
            assert_eq!(resumed.after_id, 91);
            drop(tx);
            let frames = drain(rx);
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].id, None);
            assert_eq!(frames[0].event_type.as_deref(), Some("error"));
        });
    }
}
