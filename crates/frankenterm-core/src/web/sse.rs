//! Server-Sent Events endpoint surface for Wave 4B migration.

#[allow(clippy::wildcard_imports)]
use super::*;

#[derive(Debug, Clone, Copy)]
pub(super) enum EventStreamChannel {
    All,
    Deltas,
    Detections,
    Signals,
}

pub(super) fn parse_stream_max_hz(qs: &QueryString<'_>) -> u64 {
    qs.get("max_hz")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|hz| *hz > 0)
        .unwrap_or(STREAM_DEFAULT_MAX_HZ)
        .min(STREAM_MAX_MAX_HZ)
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

fn epoch_ms_now() -> i64 {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(ts.as_millis()).unwrap_or(0)
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
        Response::with_status(StatusCode::OK)
            .header("content-type", b"text/event-stream".to_vec())
            .header("cache-control", b"no-cache".to_vec())
            .header("connection", b"keep-alive".to_vec())
            .header("x-accel-buffering", b"no".to_vec())
            .body(ResponseBody::stream(self.stream))
    }
}

/// Returns a future that completes when the receiver side of the channel is
/// dropped (i.e. the client disconnected). This bridges the tokio
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
    #[cfg(feature = "asupersync-runtime")]
    cx: asupersync::Cx,
}

impl SseByteStream {
    #[cfg(feature = "asupersync-runtime")]
    fn new(rx: mpsc::Receiver<SseEvent>) -> Self {
        Self {
            rx,
            cx: asupersync::Cx::for_testing(),
        }
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    fn new(rx: mpsc::Receiver<SseEvent>) -> Self {
        Self { rx }
    }
}

impl Stream for SseByteStream {
    type Item = Vec<u8>;

    #[cfg(feature = "asupersync-runtime")]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(&this.cx, cx) {
            Poll::Ready(Ok(event)) => Poll::Ready(Some(event.to_bytes())),
            Poll::Ready(Err(_)) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
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
    *next_emit_at = Instant::now() + min_interval;

    match tx.try_send(event).map_err(mpsc::TrySendError::from) {
        Ok(()) => {
            *consecutive_drops = 0;
            true
        }
        Err(mpsc::TrySendError::Full(_)) => {
            *consecutive_drops += 1;
            *consecutive_drops < STREAM_MAX_CONSECUTIVE_DROPS
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
    redactor: &Redactor,
    tx: &mpsc::Sender<SseEvent>,
    seq: &mut u64,
    next_emit_at: &mut Instant,
    min_interval: Duration,
    consecutive_drops: &mut u64,
) -> bool {
    for _ in 0..STREAM_SCAN_MAX_PAGES {
        let query = SegmentScanQuery {
            after_id: *after_id,
            pane_id: pane_filter,
            since: Some(started_at_ms),
            until: None,
            limit: STREAM_SCAN_LIMIT,
        };

        let segments = match storage.scan_segments(query).await {
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
                return false;
            }
        };

        if segments.is_empty() {
            break;
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
                    return false;
                }
            }
        }

        if page_len < STREAM_SCAN_LIMIT {
            break;
        }
    }

    true
}

pub(super) fn handle_stream_events(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);
    let pane_filter = parse_u64(&qs, "pane_id");
    let max_hz = parse_stream_max_hz(&qs);
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
        task::spawn(async move {
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
                    recv = timeout(
                        Duration::from_secs(STREAM_KEEPALIVE_SECS),
                        subscriber.recv(),
                    ) => recv,
                };

                match recv_result {
                    Ok(Ok(event)) => {
                        if !event_matches_pane(&event, pane_filter) {
                            continue;
                        }

                        let mut event_json = serde_json::to_value(&event).unwrap_or_else(|_| {
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
                    Ok(Err(RecvError::Closed)) => break,
                    Err(_) => {
                        if tx.try_send(SseEvent::comment("keepalive")).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        SseResponse::new(SseByteStream::new(rx)).into_response()
    })
}

pub(super) fn handle_stream_deltas(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);
    let pane_filter = parse_u64(&qs, "pane_id");
    let max_hz = parse_stream_max_hz(&qs);
    let result = require_storage_and_event_bus(req);

    Box::pin(async move {
        let (storage, event_bus, redactor) = match result {
            Ok(r) => r,
            Err(resp) => return resp,
        };

        let mut subscriber = event_bus.subscribe_deltas();
        let (tx, rx) = mpsc::channel(STREAM_CHANNEL_BUFFER);
        let started_at_ms = epoch_ms_now();
        task::spawn(async move {
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
                    recv = timeout(
                        Duration::from_secs(STREAM_KEEPALIVE_SECS),
                        subscriber.recv(),
                    ) => recv,
                };

                match recv_result {
                    Ok(Ok(Event::SegmentCaptured { pane_id, .. })) => {
                        if pane_filter.is_some_and(|pid| pid != pane_id) {
                            continue;
                        }
                        if !emit_new_segment_frames(
                            &storage,
                            pane_filter,
                            started_at_ms,
                            &mut after_id,
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
                    Ok(Ok(Event::GapDetected { pane_id, reason })) => {
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
                                "reason": redactor.redact(&reason),
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

                        if !emit_new_segment_frames(
                            &storage,
                            pane_filter,
                            started_at_ms,
                            &mut after_id,
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
                    Ok(Err(RecvError::Closed)) => break,
                    Err(_) => {
                        let _ = tx.try_send(SseEvent::comment("keepalive"));
                    }
                }
            }
        });

        SseResponse::new(SseByteStream::new(rx)).into_response()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
