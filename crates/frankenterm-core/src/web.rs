//! Web server scaffolding for wa (feature-gated: web).
//!
//! Provides a minimal `ft web` HTTP server with /health and read-only
//! data endpoints (/panes, /events, /search, /bookmarks, /ruleset-profile, /saved-searches)
//! backed by shared query helpers, plus SSE subscriptions for live event/delta streams.

use crate::events::{Event, EventBus, RecvError};
use crate::policy::Redactor;
use crate::runtime_compat::{mpsc, sleep, task, timeout};
use crate::storage::{
    EventQuery, PaneRecord, SearchOptions, SearchResult, SegmentScanQuery, StorageHandle,
};
use crate::ui_query;
use crate::{Error, Result, VERSION};
use asupersync::net::TcpListener;
use asupersync::stream::Stream;
use fastapi::ResponseBody;
use fastapi::core::{ControlFlow, Cx, Handler, Middleware, SseEvent, SseResponse, StartupOutcome};
use fastapi::http::QueryString;
use fastapi::prelude::{App, Method, Request, RequestContext, Response, StatusCode};
use fastapi::{ServerConfig, ServerError, TcpServer};
use serde::Serialize;
use serde_json::json;
use std::net::{SocketAddr, TcpStream};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8000;

/// Hard ceiling on list endpoint results.
const MAX_LIMIT: usize = 500;
/// Default limit when none is provided.
const DEFAULT_LIMIT: usize = 50;

/// Maximum request body size (64 KB). Rejects oversized requests early.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
const STREAM_SCHEMA_VERSION: &str = "ft.stream.v1";
const STREAM_DEFAULT_MAX_HZ: u64 = 50;
const STREAM_MAX_MAX_HZ: u64 = 500;
const STREAM_CHANNEL_BUFFER: usize = 256;
const STREAM_KEEPALIVE_SECS: u64 = 15;
const STREAM_SCAN_LIMIT: usize = 256;
const STREAM_SCAN_MAX_PAGES: usize = 8;
const STREAM_MAX_CONSECUTIVE_DROPS: u64 = 64;

/// Configuration for the web server.
#[derive(Clone)]
pub struct WebServerConfig {
    host: String,
    port: u16,
    storage: Option<StorageHandle>,
    event_bus: Option<Arc<EventBus>>,
    /// Must be set to `true` to bind on a non-localhost address.
    allow_public_bind: bool,
}

impl std::fmt::Debug for WebServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebServerConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("storage", &self.storage.is_some())
            .field("event_bus", &self.event_bus.is_some())
            .field("allow_public_bind", &self.allow_public_bind)
            .finish()
    }
}

impl WebServerConfig {
    /// Create a new config with the default localhost host.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port,
            storage: None,
            event_bus: None,
            allow_public_bind: false,
        }
    }

    /// Override the port.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Override the bind host.
    ///
    /// Non-localhost addresses require [`Self::with_dangerous_public_bind`].
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Attach a storage handle for data endpoints.
    #[must_use]
    pub fn with_storage(mut self, storage: StorageHandle) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Attach an event bus for live stream endpoints.
    #[must_use]
    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    /// Explicitly opt in to binding on a non-localhost address.
    ///
    /// Without this, [`start_web_server`] refuses to bind publicly.
    #[must_use]
    pub fn with_dangerous_public_bind(mut self) -> Self {
        self.allow_public_bind = true;
        self
    }

    #[must_use]
    fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Returns `true` when the configured host is a loopback address.
    fn is_localhost(&self) -> bool {
        matches!(
            self.host.as_str(),
            "127.0.0.1" | "::1" | "localhost" | "[::1]"
        )
    }
}

impl Default for WebServerConfig {
    fn default() -> Self {
        Self::new(DEFAULT_PORT)
    }
}

/// Handle to a running web server.
pub struct WebServerHandle {
    bound_addr: SocketAddr,
    server: Arc<TcpServer>,
    app: Arc<App>,
    join: task::JoinHandle<std::result::Result<(), ServerError>>,
}

impl WebServerHandle {
    /// The address the server actually bound to.
    #[must_use]
    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }

    /// Trigger graceful shutdown and wait for completion.
    pub async fn shutdown(self) -> Result<()> {
        self.server.shutdown();
        poke_listener(self.bound_addr);
        handle_server_exit(self.join.await, &self.server, &self.app).await
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestStart(Instant);

#[derive(Debug, Clone, Default)]
struct RequestSpanLogger;

impl Middleware for RequestSpanLogger {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> fastapi::core::BoxFuture<'a, ControlFlow> {
        req.insert_extension(RequestStart(Instant::now()));
        Box::pin(async { ControlFlow::Continue })
    }

    fn after<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a Request,
        response: Response,
    ) -> fastapi::core::BoxFuture<'a, Response> {
        let start = req
            .get_extension::<RequestStart>()
            .map(|s| s.0)
            .unwrap_or_else(Instant::now);
        let duration = start.elapsed();
        let method = req.method();
        let path = req.path();
        let status = response.status().as_u16();

        info!(
            target: "wa.web",
            method = %method,
            path = %path,
            status,
            duration_ms = duration.as_millis(),
            "web request"
        );

        Box::pin(async move { response })
    }

    fn name(&self) -> &'static str {
        "RequestSpanLogger"
    }
}

// =============================================================================
// Shared state injected via request extensions
// =============================================================================

/// Shared application state available to all handlers.
#[derive(Clone)]
struct AppState {
    storage: Option<StorageHandle>,
    event_bus: Option<Arc<EventBus>>,
    redactor: Arc<Redactor>,
}

/// Middleware that injects [`AppState`] into every request.
#[derive(Clone)]
struct StateInjector {
    state: AppState,
}

impl Middleware for StateInjector {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> fastapi::core::BoxFuture<'a, ControlFlow> {
        req.insert_extension(self.state.clone());
        Box::pin(async { ControlFlow::Continue })
    }

    fn name(&self) -> &'static str {
        "StateInjector"
    }
}

/// Rejects requests whose Content-Length exceeds [`MAX_REQUEST_BODY_BYTES`].
#[derive(Clone, Default)]
struct BodySizeGuard;

impl Middleware for BodySizeGuard {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> fastapi::core::BoxFuture<'a, ControlFlow> {
        if let Some(cl) = req
            .headers()
            .get("content-length")
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|v| v.parse::<usize>().ok())
        {
            if cl > MAX_REQUEST_BODY_BYTES {
                let resp = json_err(
                    StatusCode::BAD_REQUEST,
                    "body_too_large",
                    format!("Request body too large ({cl} bytes); max is {MAX_REQUEST_BODY_BYTES}"),
                );
                return Box::pin(async move { ControlFlow::Break(resp) });
            }
        }
        Box::pin(async { ControlFlow::Continue })
    }

    fn name(&self) -> &'static str {
        "BodySizeGuard"
    }
}

// =============================================================================
// Response envelope
// =============================================================================

#[derive(Serialize)]
struct ApiResponse<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    version: &'static str,
}

impl<T: Serialize> ApiResponse<T> {
    fn success(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            version: VERSION,
        }
    }
}

impl ApiResponse<()> {
    fn error(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
            error_code: Some(code.to_string()),
            version: VERSION,
        }
    }
}

fn json_ok<T: Serialize>(data: T) -> Response {
    let resp = ApiResponse::success(data);
    Response::json(&resp).unwrap_or_else(|_| Response::internal_error())
}

fn json_err(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let resp = ApiResponse::<()>::error(code, message);
    let body = serde_json::to_vec(&resp).unwrap_or_default();
    Response::with_status(status)
        .header("content-type", b"application/json".to_vec())
        .body(ResponseBody::Bytes(body))
}

fn require_storage(req: &Request) -> std::result::Result<(StorageHandle, Arc<Redactor>), Response> {
    let state = req.get_extension::<AppState>().ok_or_else(|| {
        json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "App state not configured",
        )
    })?;
    let storage = state.storage.clone().ok_or_else(|| {
        json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_storage",
            "No database connected",
        )
    })?;
    Ok((storage, Arc::clone(&state.redactor)))
}

fn require_event_bus(
    req: &Request,
) -> std::result::Result<(Arc<EventBus>, Arc<Redactor>), Response> {
    let state = req.get_extension::<AppState>().ok_or_else(|| {
        json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "App state not configured",
        )
    })?;
    let event_bus = state.event_bus.clone().ok_or_else(|| {
        json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_event_bus",
            "No event bus configured",
        )
    })?;
    Ok((event_bus, Arc::clone(&state.redactor)))
}

fn require_storage_and_event_bus(
    req: &Request,
) -> std::result::Result<(StorageHandle, Arc<EventBus>, Arc<Redactor>), Response> {
    let state = req.get_extension::<AppState>().ok_or_else(|| {
        json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "App state not configured",
        )
    })?;
    let storage = state.storage.clone().ok_or_else(|| {
        json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_storage",
            "No database connected",
        )
    })?;
    let event_bus = state.event_bus.clone().ok_or_else(|| {
        json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_event_bus",
            "No event bus configured",
        )
    })?;
    Ok((storage, event_bus, Arc::clone(&state.redactor)))
}

// =============================================================================
// Query parameter helpers
// =============================================================================

fn parse_limit(qs: &QueryString<'_>) -> usize {
    qs.get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT)
}

fn parse_u64(qs: &QueryString<'_>, key: &str) -> Option<u64> {
    qs.get(key).and_then(|v| v.parse::<u64>().ok())
}

fn parse_i64(qs: &QueryString<'_>, key: &str) -> Option<i64> {
    qs.get(key).and_then(|v| v.parse::<i64>().ok())
}

fn parse_bool(qs: &QueryString<'_>, key: &str) -> bool {
    matches!(qs.get(key), Some("1" | "true" | "yes"))
}

fn parse_stream_max_hz(qs: &QueryString<'_>) -> u64 {
    qs.get("max_hz")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|hz| *hz > 0)
        .unwrap_or(STREAM_DEFAULT_MAX_HZ)
        .min(STREAM_MAX_MAX_HZ)
}

#[derive(Debug, Clone, Copy)]
enum EventStreamChannel {
    All,
    Deltas,
    Detections,
    Signals,
}

fn parse_event_stream_channel(
    qs: &QueryString<'_>,
) -> std::result::Result<EventStreamChannel, Response> {
    match qs.get("channel") {
        None | Some("all") => Ok(EventStreamChannel::All),
        Some("deltas" | "delta") => Ok(EventStreamChannel::Deltas),
        Some("detections" | "detection") => Ok(EventStreamChannel::Detections),
        Some("signals" | "signal") => Ok(EventStreamChannel::Signals),
        Some(other) => Err(json_err(
            StatusCode::BAD_REQUEST,
            "invalid_channel",
            format!(
                "Invalid stream channel '{other}'. Expected one of: all, deltas, detections, signals"
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

fn redact_json_value(value: &mut serde_json::Value, redactor: &Redactor) {
    match value {
        serde_json::Value::String(s) => {
            *s = redactor.redact(s);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value(item, redactor);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                redact_json_value(v, redactor);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

struct TokioSseStream {
    rx: mpsc::Receiver<SseEvent>,
}

impl TokioSseStream {
    fn new(rx: mpsc::Receiver<SseEvent>) -> Self {
        Self { rx }
    }
}

impl Stream for TokioSseStream {
    type Item = SseEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

fn make_stream_frame(
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

fn frame_to_sse(event_type: &'static str, seq: u64, frame: serde_json::Value) -> Option<SseEvent> {
    serde_json::to_string(&frame).ok().map(|body| {
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

    match tx.try_send(event) {
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

fn event_matches_pane(event: &Event, pane_filter: Option<u64>) -> bool {
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

fn handle_stream_events(
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
                let recv_result = tokio::select! {
                    () = tx.closed() => break,
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

        SseResponse::new(TokioSseStream::new(rx)).into_response()
    })
}

fn handle_stream_deltas(
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
                let recv_result = tokio::select! {
                    () = tx.closed() => break,
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

        SseResponse::new(TokioSseStream::new(rx)).into_response()
    })
}

// =============================================================================
// /health
// =============================================================================

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
    version: &'static str,
}

fn health_response() -> Response {
    let payload = HealthResponse {
        ok: true,
        version: VERSION,
    };
    Response::json(&payload).unwrap_or_else(|_| Response::internal_error())
}

// =============================================================================
// /panes
// =============================================================================

#[derive(Serialize)]
struct PanesResponse {
    panes: Vec<PaneView>,
    total: usize,
}

#[derive(Serialize)]
struct PaneView {
    pane_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_uuid: Option<String>,
    domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tab_id: Option<u64>,
    first_seen_at: i64,
    last_seen_at: i64,
}

impl PaneView {
    fn from_record(r: PaneRecord, redactor: &Redactor) -> Self {
        Self {
            pane_id: r.pane_id,
            pane_uuid: r.pane_uuid,
            domain: r.domain,
            title: r.title.map(|t| redactor.redact(&t)),
            cwd: r.cwd.map(|c| redactor.redact(&c)),
            window_id: r.window_id,
            tab_id: r.tab_id,
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
        }
    }
}

fn handle_panes(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match storage.get_panes().await {
            Ok(panes) => {
                let total = panes.len();
                let views: Vec<PaneView> = panes
                    .into_iter()
                    .map(|p| PaneView::from_record(p, &redactor))
                    .collect();
                json_ok(PanesResponse {
                    panes: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query panes: {e}"),
            ),
        }
    })
}

// =============================================================================
// /events
// =============================================================================

#[derive(Serialize)]
struct EventsResponse {
    events: Vec<EventView>,
    total: usize,
}

#[derive(Serialize)]
struct EventView {
    id: i64,
    pane_id: u64,
    rule_id: String,
    event_type: String,
    severity: String,
    confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    extracted: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<EventAnnotationsView>,
    detected_at: i64,
}

#[derive(Serialize)]
struct EventAnnotationsView {
    #[serde(skip_serializing_if = "Option::is_none")]
    triage_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    labels: Vec<String>,
}

impl EventAnnotationsView {
    fn from_stored(annotations: crate::storage::EventAnnotations, redactor: &Redactor) -> Self {
        Self {
            triage_state: annotations.triage_state.map(|v| redactor.redact(&v)),
            note: annotations.note.map(|v| redactor.redact(&v)),
            labels: annotations
                .labels
                .into_iter()
                .map(|label| redactor.redact(&label))
                .collect(),
        }
    }
}

impl EventView {
    fn from_stored(
        e: crate::storage::StoredEvent,
        redactor: &Redactor,
        annotations: Option<EventAnnotationsView>,
    ) -> Self {
        Self {
            id: e.id,
            pane_id: e.pane_id,
            rule_id: e.rule_id,
            event_type: e.event_type,
            severity: e.severity,
            confidence: e.confidence,
            extracted: e.extracted,
            matched_text: e.matched_text.map(|t| redactor.redact(&t)),
            annotations,
            detected_at: e.detected_at,
        }
    }
}

fn handle_events(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);

    let query = EventQuery {
        limit: Some(parse_limit(&qs)),
        pane_id: parse_u64(&qs, "pane_id"),
        rule_id: qs.get("rule_id").map(String::from),
        event_type: qs.get("event_type").map(String::from),
        triage_state: qs.get("triage_state").map(String::from),
        label: qs.get("label").map(String::from),
        unhandled_only: parse_bool(&qs, "unhandled"),
        since: parse_i64(&qs, "since"),
        until: parse_i64(&qs, "until"),
    };

    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match storage.get_events(query).await {
            Ok(events) => {
                let total = events.len();
                let mut views: Vec<EventView> = Vec::with_capacity(total);
                for event in events {
                    let annotations = match storage.get_event_annotations(event.id).await {
                        Ok(Some(annotations)) => {
                            Some(EventAnnotationsView::from_stored(annotations, &redactor))
                        }
                        Ok(None) => None,
                        Err(err) => {
                            warn!(target: "wa.web", error = %err, event_id = event.id, "failed to load event annotations");
                            None
                        }
                    };
                    views.push(EventView::from_stored(event, &redactor, annotations));
                }
                json_ok(EventsResponse {
                    events: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query events: {e}"),
            ),
        }
    })
}

// =============================================================================
// /search
// =============================================================================

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<SearchHit>,
    total: usize,
}

#[derive(Serialize)]
struct SearchHit {
    segment_id: i64,
    pane_id: u64,
    score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
    captured_at: i64,
    content_len: usize,
}

impl SearchHit {
    fn from_result(r: SearchResult, redactor: &Redactor) -> Self {
        Self {
            segment_id: r.segment.id,
            pane_id: r.segment.pane_id,
            score: r.score,
            snippet: r.snippet.map(|s| redactor.redact(&s)),
            captured_at: r.segment.captured_at,
            content_len: r.segment.content_len,
        }
    }
}

fn handle_search(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);

    let query_str = qs.get("q").map(String::from);
    let options = SearchOptions {
        limit: Some(parse_limit(&qs)),
        pane_id: parse_u64(&qs, "pane_id"),
        since: parse_i64(&qs, "since"),
        until: parse_i64(&qs, "until"),
        include_snippets: Some(true),
        snippet_max_tokens: Some(64),
        highlight_prefix: None,
        highlight_suffix: None,
    };

    Box::pin(async move {
        let query = match query_str {
            Some(q) if !q.is_empty() => q,
            _ => {
                return json_err(
                    StatusCode::BAD_REQUEST,
                    "missing_query",
                    "Query parameter 'q' is required",
                );
            }
        };
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match storage.search_with_results(&query, options).await {
            Ok(results) => {
                let total = results.len();
                let hits: Vec<SearchHit> = results
                    .into_iter()
                    .map(|r| SearchHit::from_result(r, &redactor))
                    .collect();
                json_ok(SearchResponse {
                    results: hits,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Search failed: {e}"),
            ),
        }
    })
}

// =============================================================================
// /bookmarks
// =============================================================================

#[derive(Serialize)]
struct BookmarksResponse {
    bookmarks: Vec<BookmarkView>,
    total: usize,
}

#[derive(Serialize)]
struct BookmarkView {
    pane_id: u64,
    alias: String,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl BookmarkView {
    fn from_query(bookmark: ui_query::PaneBookmarkView, redactor: &Redactor) -> Self {
        Self {
            pane_id: bookmark.pane_id,
            alias: redactor.redact(&bookmark.alias),
            tags: bookmark
                .tags
                .iter()
                .map(|tag| redactor.redact(tag))
                .collect(),
            description: bookmark.description.map(|desc| redactor.redact(&desc)),
            created_at: bookmark.created_at,
            updated_at: bookmark.updated_at,
        }
    }
}

fn handle_bookmarks(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match ui_query::list_pane_bookmarks(&storage).await {
            Ok(bookmarks) => {
                let total = bookmarks.len();
                let views: Vec<BookmarkView> = bookmarks
                    .into_iter()
                    .map(|bookmark| BookmarkView::from_query(bookmark, &redactor))
                    .collect();
                json_ok(BookmarksResponse {
                    bookmarks: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query bookmarks: {e}"),
            ),
        }
    })
}

// =============================================================================
// /ruleset-profile
// =============================================================================

#[derive(Serialize)]
struct RulesetProfileResponse {
    active_profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_last_applied_at: Option<u64>,
    profiles: Vec<RulesetProfileView>,
    total: usize,
}

#[derive(Serialize)]
struct RulesetProfileView {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_applied_at: Option<u64>,
    implicit: bool,
}

impl RulesetProfileView {
    fn from_summary(summary: crate::rulesets::RulesetProfileSummary, redactor: &Redactor) -> Self {
        Self {
            name: summary.name,
            description: summary.description.map(|d| redactor.redact(&d)),
            path: summary.path.map(|p| redactor.redact(&p)),
            last_applied_at: summary.last_applied_at,
            implicit: summary.implicit,
        }
    }
}

fn handle_ruleset_profile(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let redactor = req
        .get_extension::<AppState>()
        .map(|s| Arc::clone(&s.redactor))
        .unwrap_or_else(|| Arc::new(Redactor::new()));
    Box::pin(async move {
        let config_path = crate::config::resolve_config_path(None);
        match ui_query::resolve_ruleset_profile_state(config_path.as_deref()) {
            Ok(state) => {
                let total = state.profiles.len();
                let profiles = state
                    .profiles
                    .into_iter()
                    .map(|profile| RulesetProfileView::from_summary(profile, &redactor))
                    .collect();
                json_ok(RulesetProfileResponse {
                    active_profile: state.active_profile,
                    active_last_applied_at: state.active_last_applied_at,
                    profiles,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ruleset_profile_error",
                format!("Failed to resolve ruleset profile state: {e}"),
            ),
        }
    })
}

// =============================================================================
// /saved-searches
// =============================================================================

#[derive(Serialize)]
struct SavedSearchesResponse {
    saved_searches: Vec<SavedSearchView>,
    total: usize,
}

#[derive(Serialize)]
struct SavedSearchView {
    id: String,
    name: String,
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_id: Option<u64>,
    limit: i64,
    since_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_interval_ms: Option<i64>,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_result_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl SavedSearchView {
    fn from_query(saved: ui_query::SavedSearchView, redactor: &Redactor) -> Self {
        Self {
            id: saved.id,
            name: redactor.redact(&saved.name),
            query: redactor.redact(&saved.query),
            pane_id: saved.pane_id,
            limit: saved.limit,
            since_mode: saved.since_mode,
            since_ms: saved.since_ms,
            schedule_interval_ms: saved.schedule_interval_ms,
            enabled: saved.enabled,
            last_run_at: saved.last_run_at,
            last_result_count: saved.last_result_count,
            last_error: saved.last_error.map(|e| redactor.redact(&e)),
            created_at: saved.created_at,
            updated_at: saved.updated_at,
        }
    }
}

fn handle_saved_searches(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match ui_query::list_saved_searches(&storage).await {
            Ok(saved_searches) => {
                let total = saved_searches.len();
                let views = saved_searches
                    .into_iter()
                    .map(|saved| SavedSearchView::from_query(saved, &redactor))
                    .collect();
                json_ok(SavedSearchesResponse {
                    saved_searches: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query saved searches: {e}"),
            ),
        }
    })
}

// =============================================================================
// App builder
// =============================================================================

fn build_app(storage: Option<StorageHandle>, event_bus: Option<Arc<EventBus>>) -> App {
    let state = AppState {
        storage,
        event_bus,
        redactor: Arc::new(Redactor::new()),
    };

    App::builder()
        .middleware(BodySizeGuard)
        .middleware(RequestSpanLogger::default())
        .middleware(StateInjector { state })
        .route(
            "/health",
            Method::Get,
            |_ctx: &RequestContext, _req: &mut Request| async { health_response() },
        )
        .route(
            "/panes",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_panes(req),
        )
        .route(
            "/events",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_events(req),
        )
        .route(
            "/search",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_search(req),
        )
        .route(
            "/bookmarks",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_bookmarks(req),
        )
        .route(
            "/ruleset-profile",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_ruleset_profile(req),
        )
        .route(
            "/saved-searches",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_saved_searches(req),
        )
        .route(
            "/stream/events",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_stream_events(req),
        )
        .route(
            "/stream/deltas",
            Method::Get,
            |_ctx: &RequestContext, req: &mut Request| handle_stream_deltas(req),
        )
        .build()
}

/// Start the web server and return a handle for shutdown.
///
/// Refuses to bind on non-localhost addresses unless the config was
/// created with [`WebServerConfig::with_dangerous_public_bind`].
pub async fn start_web_server(config: WebServerConfig) -> Result<WebServerHandle> {
    if !config.is_localhost() && !config.allow_public_bind {
        return Err(Error::Runtime(format!(
            "refusing to bind on public address '{}' — \
             use --dangerous-bind-any or with_dangerous_public_bind() to override",
            config.host
        )));
    }
    if !config.is_localhost() {
        warn!(
            target: "wa.web",
            host = %config.host,
            "binding web server on non-localhost address — endpoints may be remotely reachable"
        );
    }
    let bind_addr = config.bind_addr();
    let app = build_app(config.storage, config.event_bus);

    match app.run_startup_hooks().await {
        StartupOutcome::Success => {}
        StartupOutcome::PartialSuccess { warnings } => {
            warn!(target: "wa.web", warnings, "web startup hooks had warnings");
        }
        StartupOutcome::Aborted(err) => {
            return Err(Error::Runtime(format!(
                "web startup aborted: {}",
                err.message
            )));
        }
    }

    let app = Arc::new(app);
    let listener = TcpListener::bind(bind_addr.clone())
        .await
        .map_err(Error::Io)?;
    let local_addr = listener.local_addr().map_err(Error::Io)?;

    let server = Arc::new(TcpServer::new(ServerConfig::new(bind_addr)));
    let handler: Arc<dyn Handler> = Arc::clone(&app) as Arc<dyn Handler>;

    let server_task = {
        let server = Arc::clone(&server);
        task::spawn(async move {
            let cx = Cx::for_testing();
            server.serve_on_handler(&cx, listener, handler).await
        })
    };

    info!(
        target: "wa.web",
        bound_addr = %local_addr,
        "web server listening"
    );

    Ok(WebServerHandle {
        bound_addr: local_addr,
        server,
        app,
        join: server_task,
    })
}

/// Run the web server until Ctrl+C, then shut down gracefully.
pub async fn run_web_server(config: WebServerConfig) -> Result<()> {
    let WebServerHandle {
        bound_addr,
        server,
        app,
        mut join,
    } = start_web_server(config).await?;

    println!("ft web listening on http://{bound_addr}");

    tokio::select! {
        result = &mut join => {
            handle_server_exit(result, &server, &app).await?;
        }
        shutdown = wait_for_shutdown_signal() => {
            shutdown?;
            server.shutdown();
            poke_listener(bound_addr);
            handle_server_exit(join.await, &server, &app).await?;
        }
    }

    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term = signal(SignalKind::terminate())
            .map_err(|e| Error::Runtime(format!("SIGTERM handler failed: {e}")))?;

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| Error::Runtime(format!("Ctrl+C handler failed: {e}")))?;
        Ok(())
    }
}

async fn handle_server_exit(
    result: std::result::Result<std::result::Result<(), ServerError>, tokio::task::JoinError>,
    server: &Arc<TcpServer>,
    app: &Arc<App>,
) -> Result<()> {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(ServerError::Shutdown)) => {}
        Ok(Err(err)) => {
            return Err(Error::Runtime(format!("web server error: {err}")));
        }
        Err(err) => {
            return Err(Error::Runtime(format!("web server join error: {err}")));
        }
    }

    let forced = server.drain().await;
    if forced > 0 {
        warn!(target: "wa.web", forced, "web server forced closed connections");
    }
    app.run_shutdown_hooks().await;
    Ok(())
}

fn poke_listener(addr: SocketAddr) {
    let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // =========================================================================
    // WebServerConfig tests
    // =========================================================================

    #[test]
    fn config_default_uses_localhost_and_default_port() {
        let cfg = WebServerConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(cfg.storage.is_none());
        assert!(cfg.event_bus.is_none());
        assert!(!cfg.allow_public_bind);
    }

    #[test]
    fn config_new_sets_custom_port() {
        let cfg = WebServerConfig::new(9999);
        assert_eq!(cfg.port, 9999);
        assert_eq!(cfg.host, DEFAULT_HOST);
    }

    #[test]
    fn config_with_port_overrides() {
        let cfg = WebServerConfig::new(8000).with_port(3000);
        assert_eq!(cfg.port, 3000);
    }

    #[test]
    fn config_with_host_overrides() {
        let cfg = WebServerConfig::new(8000).with_host("0.0.0.0");
        assert_eq!(cfg.host, "0.0.0.0");
    }

    #[test]
    fn config_bind_addr_formats_correctly() {
        let cfg = WebServerConfig::new(8080).with_host("10.0.0.1");
        assert_eq!(cfg.bind_addr(), "10.0.0.1:8080");
    }

    #[test]
    fn config_is_localhost_ipv4_loopback() {
        let cfg = WebServerConfig::new(8000);
        assert!(cfg.is_localhost());
    }

    #[test]
    fn config_is_localhost_ipv6_loopback() {
        let cfg = WebServerConfig::new(8000).with_host("::1");
        assert!(cfg.is_localhost());
    }

    #[test]
    fn config_is_localhost_bracketed_ipv6() {
        let cfg = WebServerConfig::new(8000).with_host("[::1]");
        assert!(cfg.is_localhost());
    }

    #[test]
    fn config_is_localhost_name() {
        let cfg = WebServerConfig::new(8000).with_host("localhost");
        assert!(cfg.is_localhost());
    }

    #[test]
    fn config_is_not_localhost_public_ip() {
        let cfg = WebServerConfig::new(8000).with_host("0.0.0.0");
        assert!(!cfg.is_localhost());
    }

    #[test]
    fn config_is_not_localhost_external_ip() {
        let cfg = WebServerConfig::new(8000).with_host("192.168.1.1");
        assert!(!cfg.is_localhost());
    }

    #[test]
    fn config_dangerous_public_bind_flag() {
        let cfg = WebServerConfig::new(8000).with_dangerous_public_bind();
        assert!(cfg.allow_public_bind);
    }

    #[test]
    fn config_debug_format_hides_storage_details() {
        let cfg = WebServerConfig::default();
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("storage: false"));
        assert!(dbg.contains("event_bus: false"));
    }

    #[test]
    fn config_builder_chaining() {
        let cfg = WebServerConfig::new(3000)
            .with_host("10.0.0.1")
            .with_port(4000)
            .with_dangerous_public_bind();
        assert_eq!(cfg.host, "10.0.0.1");
        assert_eq!(cfg.port, 4000);
        assert!(cfg.allow_public_bind);
    }

    // =========================================================================
    // ApiResponse envelope tests
    // =========================================================================

    #[test]
    fn api_response_success_has_ok_true() {
        let resp = ApiResponse::success("hello");
        assert!(resp.ok);
        assert_eq!(resp.data.as_deref(), Some("hello"));
        assert!(resp.error.is_none());
        assert!(resp.error_code.is_none());
        assert_eq!(resp.version, VERSION);
    }

    #[test]
    fn api_response_error_has_ok_false() {
        let resp = ApiResponse::<()>::error("test_code", "something went wrong");
        assert!(!resp.ok);
        assert!(resp.data.is_none());
        assert_eq!(resp.error.as_deref(), Some("something went wrong"));
        assert_eq!(resp.error_code.as_deref(), Some("test_code"));
    }

    #[test]
    fn api_response_success_serialization() {
        let resp = ApiResponse::success(vec![1, 2, 3]);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"], serde_json::json!([1, 2, 3]));
        assert!(json.get("error").is_none());
        assert!(json.get("error_code").is_none());
        assert_eq!(json["version"], VERSION);
    }

    #[test]
    fn api_response_error_serialization() {
        let resp = ApiResponse::<()>::error("no_storage", "DB not connected");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert!(json.get("data").is_none());
        assert_eq!(json["error"], "DB not connected");
        assert_eq!(json["error_code"], "no_storage");
    }

    #[test]
    fn api_response_success_with_struct_data() {
        #[derive(Serialize)]
        struct Inner {
            count: usize,
        }
        let resp = ApiResponse::success(Inner { count: 42 });
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"]["count"], 42);
    }

    // =========================================================================
    // Query parameter parsing tests
    // =========================================================================

    #[test]
    fn parse_limit_default_when_absent() {
        let qs = QueryString::parse("");
        assert_eq!(parse_limit(&qs), DEFAULT_LIMIT);
    }

    #[test]
    fn parse_limit_uses_provided_value() {
        let qs = QueryString::parse("limit=10");
        assert_eq!(parse_limit(&qs), 10);
    }

    #[test]
    fn parse_limit_caps_at_max() {
        let qs = QueryString::parse("limit=999999");
        assert_eq!(parse_limit(&qs), MAX_LIMIT);
    }

    #[test]
    fn parse_limit_ignores_non_numeric() {
        let qs = QueryString::parse("limit=abc");
        assert_eq!(parse_limit(&qs), DEFAULT_LIMIT);
    }

    #[test]
    fn parse_limit_zero_is_valid() {
        let qs = QueryString::parse("limit=0");
        assert_eq!(parse_limit(&qs), 0);
    }

    #[test]
    fn parse_u64_returns_some_on_valid() {
        let qs = QueryString::parse("pane_id=42");
        assert_eq!(parse_u64(&qs, "pane_id"), Some(42));
    }

    #[test]
    fn parse_u64_returns_none_on_missing() {
        let qs = QueryString::parse("");
        assert_eq!(parse_u64(&qs, "pane_id"), None);
    }

    #[test]
    fn parse_u64_returns_none_on_invalid() {
        let qs = QueryString::parse("pane_id=-1");
        assert_eq!(parse_u64(&qs, "pane_id"), None);
    }

    #[test]
    fn parse_u64_returns_none_on_non_numeric() {
        let qs = QueryString::parse("pane_id=hello");
        assert_eq!(parse_u64(&qs, "pane_id"), None);
    }

    #[test]
    fn parse_i64_returns_some_on_valid() {
        let qs = QueryString::parse("since=-500");
        assert_eq!(parse_i64(&qs, "since"), Some(-500));
    }

    #[test]
    fn parse_i64_returns_none_on_missing() {
        let qs = QueryString::parse("");
        assert_eq!(parse_i64(&qs, "since"), None);
    }

    #[test]
    fn parse_i64_returns_none_on_non_numeric() {
        let qs = QueryString::parse("since=foo");
        assert_eq!(parse_i64(&qs, "since"), None);
    }

    #[test]
    fn parse_bool_true_values() {
        for val in &["1", "true", "yes"] {
            let raw = format!("flag={val}");
            let qs = QueryString::parse(&raw);
            assert!(parse_bool(&qs, "flag"), "Expected true for '{val}'");
        }
    }

    #[test]
    fn parse_bool_false_values() {
        for val in &["0", "false", "no", ""] {
            let raw = format!("flag={val}");
            let qs = QueryString::parse(&raw);
            assert!(!parse_bool(&qs, "flag"), "Expected false for '{val}'");
        }
    }

    #[test]
    fn parse_bool_missing_is_false() {
        let qs = QueryString::parse("");
        assert!(!parse_bool(&qs, "flag"));
    }

    #[test]
    fn parse_stream_max_hz_default() {
        let qs = QueryString::parse("");
        assert_eq!(parse_stream_max_hz(&qs), STREAM_DEFAULT_MAX_HZ);
    }

    #[test]
    fn parse_stream_max_hz_custom() {
        let qs = QueryString::parse("max_hz=100");
        assert_eq!(parse_stream_max_hz(&qs), 100);
    }

    #[test]
    fn parse_stream_max_hz_capped_at_max() {
        let qs = QueryString::parse("max_hz=99999");
        assert_eq!(parse_stream_max_hz(&qs), STREAM_MAX_MAX_HZ);
    }

    #[test]
    fn parse_stream_max_hz_zero_uses_default() {
        let qs = QueryString::parse("max_hz=0");
        assert_eq!(parse_stream_max_hz(&qs), STREAM_DEFAULT_MAX_HZ);
    }

    #[test]
    fn parse_stream_max_hz_non_numeric_uses_default() {
        let qs = QueryString::parse("max_hz=fast");
        assert_eq!(parse_stream_max_hz(&qs), STREAM_DEFAULT_MAX_HZ);
    }

    // =========================================================================
    // EventStreamChannel parsing tests
    // =========================================================================

    #[test]
    fn parse_event_stream_channel_default_is_all() {
        let qs = QueryString::parse("");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::All));
    }

    #[test]
    fn parse_event_stream_channel_explicit_all() {
        let qs = QueryString::parse("channel=all");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::All));
    }

    #[test]
    fn parse_event_stream_channel_deltas() {
        let qs = QueryString::parse("channel=deltas");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::Deltas));
    }

    #[test]
    fn parse_event_stream_channel_delta_singular() {
        let qs = QueryString::parse("channel=delta");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::Deltas));
    }

    #[test]
    fn parse_event_stream_channel_detections() {
        let qs = QueryString::parse("channel=detections");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::Detections));
    }

    #[test]
    fn parse_event_stream_channel_detection_singular() {
        let qs = QueryString::parse("channel=detection");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::Detections));
    }

    #[test]
    fn parse_event_stream_channel_signals() {
        let qs = QueryString::parse("channel=signals");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::Signals));
    }

    #[test]
    fn parse_event_stream_channel_signal_singular() {
        let qs = QueryString::parse("channel=signal");
        let ch = parse_event_stream_channel(&qs).unwrap();
        assert!(matches!(ch, EventStreamChannel::Signals));
    }

    #[test]
    fn parse_event_stream_channel_invalid_returns_err() {
        let qs = QueryString::parse("channel=foobar");
        assert!(parse_event_stream_channel(&qs).is_err());
    }

    // =========================================================================
    // Stream frame helpers
    // =========================================================================

    #[test]
    fn make_stream_frame_structure() {
        let frame = make_stream_frame("events", "ready", 1, json!({"test": true}));
        assert_eq!(frame["schema"], STREAM_SCHEMA_VERSION);
        assert_eq!(frame["stream"], "events");
        assert_eq!(frame["kind"], "ready");
        assert_eq!(frame["seq"], 1);
        assert!(frame["ts_ms"].is_number());
        assert_eq!(frame["data"]["test"], true);
    }

    #[test]
    fn make_stream_frame_different_streams() {
        let f1 = make_stream_frame("deltas", "delta", 5, json!(null));
        let f2 = make_stream_frame("events", "event", 6, json!(null));
        assert_eq!(f1["stream"], "deltas");
        assert_eq!(f2["stream"], "events");
    }

    #[test]
    fn make_stream_frame_seq_preserved() {
        let frame = make_stream_frame("events", "lag", 999, json!({}));
        assert_eq!(frame["seq"], 999);
    }

    #[test]
    fn frame_to_sse_produces_event() {
        let frame = json!({"schema": "ft.stream.v1", "data": "test"});
        let sse = frame_to_sse("event", 42, frame);
        assert!(sse.is_some());
    }

    #[test]
    fn frame_to_sse_sets_event_type_and_id() {
        let frame = json!({"test": true});
        let sse = frame_to_sse("delta", 7, frame).unwrap();
        assert_eq!(sse.id(), Some("7"));
        assert_eq!(sse.event_type(), Some("delta"));
    }

    // =========================================================================
    // redact_json_value tests
    // =========================================================================

    #[test]
    fn redact_json_value_string() {
        let redactor = Redactor::new();
        let mut val = json!("no secrets here");
        redact_json_value(&mut val, &redactor);
        assert_eq!(val.as_str().unwrap(), "no secrets here");
    }

    #[test]
    fn redact_json_value_nested_object() {
        let redactor = Redactor::new();
        let mut val = json!({"a": {"b": "inner"}});
        redact_json_value(&mut val, &redactor);
        assert_eq!(val["a"]["b"], "inner");
    }

    #[test]
    fn redact_json_value_array() {
        let redactor = Redactor::new();
        let mut val = json!(["first", "second"]);
        redact_json_value(&mut val, &redactor);
        assert_eq!(val[0], "first");
        assert_eq!(val[1], "second");
    }

    #[test]
    fn redact_json_value_leaves_non_strings_unchanged() {
        let redactor = Redactor::new();
        let mut val = json!({"num": 42, "bool": true, "null": null});
        redact_json_value(&mut val, &redactor);
        assert_eq!(val["num"], 42);
        assert_eq!(val["bool"], true);
        assert!(val["null"].is_null());
    }

    #[test]
    fn redact_json_value_empty_object() {
        let redactor = Redactor::new();
        let mut val = json!({});
        redact_json_value(&mut val, &redactor);
        assert!(val.as_object().unwrap().is_empty());
    }

    #[test]
    fn redact_json_value_empty_array() {
        let redactor = Redactor::new();
        let mut val = json!([]);
        redact_json_value(&mut val, &redactor);
        assert!(val.as_array().unwrap().is_empty());
    }

    #[test]
    fn redact_json_value_deeply_nested() {
        let redactor = Redactor::new();
        let mut val = json!({"a": [{"b": [{"c": "deep"}]}]});
        redact_json_value(&mut val, &redactor);
        assert_eq!(val["a"][0]["b"][0]["c"], "deep");
    }

    // =========================================================================
    // event_matches_pane tests
    // =========================================================================

    #[test]
    fn event_matches_pane_no_filter_always_matches() {
        let event = Event::SegmentCaptured {
            pane_id: 42,
            seq: 1,
            content_len: 10,
        };
        assert!(event_matches_pane(&event, None));
    }

    #[test]
    fn event_matches_pane_matching_id() {
        let event = Event::SegmentCaptured {
            pane_id: 5,
            seq: 1,
            content_len: 10,
        };
        assert!(event_matches_pane(&event, Some(5)));
    }

    #[test]
    fn event_matches_pane_non_matching_id() {
        let event = Event::SegmentCaptured {
            pane_id: 5,
            seq: 1,
            content_len: 10,
        };
        assert!(!event_matches_pane(&event, Some(99)));
    }

    #[test]
    fn event_matches_pane_gap_event() {
        let event = Event::GapDetected {
            pane_id: 3,
            reason: "timeout".into(),
        };
        assert!(event_matches_pane(&event, Some(3)));
        assert!(!event_matches_pane(&event, Some(4)));
    }

    #[test]
    fn event_matches_pane_discovered_event() {
        let event = Event::PaneDiscovered {
            pane_id: 7,
            domain: "local".into(),
            title: "test".into(),
        };
        assert!(event_matches_pane(&event, Some(7)));
        assert!(!event_matches_pane(&event, Some(8)));
    }

    #[test]
    fn event_matches_pane_disappeared_event() {
        let event = Event::PaneDisappeared { pane_id: 10 };
        assert!(event_matches_pane(&event, Some(10)));
        assert!(!event_matches_pane(&event, Some(11)));
    }

    // =========================================================================
    // epoch_ms_now tests
    // =========================================================================

    #[test]
    fn epoch_ms_now_returns_positive() {
        let ms = epoch_ms_now();
        assert!(
            ms > 0,
            "epoch_ms_now should return positive value, got {ms}"
        );
    }

    #[test]
    fn epoch_ms_now_is_recent() {
        let ms = epoch_ms_now();
        // Should be after 2020-01-01 in epoch ms
        let jan_2020 = 1_577_836_800_000_i64;
        assert!(ms > jan_2020, "epoch_ms_now should be after 2020, got {ms}");
    }

    // =========================================================================
    // View type conversion tests
    // =========================================================================

    #[test]
    fn pane_view_from_record_basic() {
        let redactor = Redactor::new();
        let record = PaneRecord {
            pane_id: 1,
            pane_uuid: Some("uuid-123".into()),
            domain: "local".into(),
            window_id: Some(0),
            tab_id: Some(0),
            title: Some("test pane".into()),
            cwd: Some("/home/user".into()),
            tty_name: None,
            pid: None,
            first_seen_at: 1000,
            last_seen_at: 2000,
        };
        let view = PaneView::from_record(record, &redactor);
        assert_eq!(view.pane_id, 1);
        assert_eq!(view.pane_uuid.as_deref(), Some("uuid-123"));
        assert_eq!(view.domain, "local");
        assert_eq!(view.title.as_deref(), Some("test pane"));
        assert_eq!(view.cwd.as_deref(), Some("/home/user"));
        assert_eq!(view.first_seen_at, 1000);
        assert_eq!(view.last_seen_at, 2000);
    }

    #[test]
    fn pane_view_from_record_optional_fields_none() {
        let redactor = Redactor::new();
        let record = PaneRecord {
            pane_id: 99,
            pane_uuid: None,
            domain: "remote".into(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            pid: None,
            first_seen_at: 500,
            last_seen_at: 600,
        };
        let view = PaneView::from_record(record, &redactor);
        assert_eq!(view.pane_id, 99);
        assert!(view.pane_uuid.is_none());
        assert!(view.title.is_none());
        assert!(view.cwd.is_none());
        assert!(view.window_id.is_none());
        assert!(view.tab_id.is_none());
    }

    #[test]
    fn pane_view_serialization_skips_none() {
        let redactor = Redactor::new();
        let record = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".into(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            pid: None,
            first_seen_at: 0,
            last_seen_at: 0,
        };
        let view = PaneView::from_record(record, &redactor);
        let json = serde_json::to_value(&view).unwrap();
        assert!(!json.as_object().unwrap().contains_key("pane_uuid"));
        assert!(!json.as_object().unwrap().contains_key("title"));
        assert!(!json.as_object().unwrap().contains_key("cwd"));
    }

    #[test]
    fn event_view_from_stored_basic() {
        let redactor = Redactor::new();
        let stored = crate::storage::StoredEvent {
            id: 10,
            pane_id: 1,
            rule_id: "core.codex:usage_reached".into(),
            agent_type: "codex".into(),
            event_type: "usage_limit".into(),
            severity: "warning".into(),
            confidence: 0.95,
            extracted: Some(json!({"wait_until": "2026-01-20"})),
            matched_text: Some("Usage limit reached".into()),
            detected_at: 5000,
        };
        let view = EventView::from_stored(stored, &redactor, None);
        assert_eq!(view.id, 10);
        assert_eq!(view.pane_id, 1);
        assert_eq!(view.rule_id, "core.codex:usage_reached");
        assert_eq!(view.event_type, "usage_limit");
        assert_eq!(view.severity, "warning");
        assert!((view.confidence - 0.95).abs() < f64::EPSILON);
        assert!(view.extracted.is_some());
        assert!(view.matched_text.is_some());
        assert!(view.annotations.is_none());
    }

    #[test]
    fn event_view_with_annotations() {
        let redactor = Redactor::new();
        let stored = crate::storage::StoredEvent {
            id: 1,
            pane_id: 0,
            rule_id: "test".into(),
            agent_type: "test".into(),
            event_type: "test".into(),
            severity: "info".into(),
            confidence: 1.0,
            extracted: None,
            matched_text: None,
            detected_at: 0,
        };
        let annotations = crate::storage::EventAnnotations {
            triage_state: Some("investigating".into()),
            triage_updated_at: Some(1000),
            triage_updated_by: Some("operator".into()),
            note: Some("checking this".into()),
            note_updated_at: Some(1000),
            note_updated_by: None,
            labels: vec!["urgent".into(), "regression".into()],
        };
        let ann_view = EventAnnotationsView::from_stored(annotations, &redactor);
        let view = EventView::from_stored(stored, &redactor, Some(ann_view));
        assert!(view.annotations.is_some());
        let ann = view.annotations.unwrap();
        assert_eq!(ann.triage_state.as_deref(), Some("investigating"));
        assert_eq!(ann.note.as_deref(), Some("checking this"));
        assert_eq!(ann.labels.len(), 2);
        assert_eq!(ann.labels[0], "urgent");
    }

    #[test]
    fn event_annotations_view_empty_labels() {
        let redactor = Redactor::new();
        let annotations = crate::storage::EventAnnotations {
            triage_state: None,
            triage_updated_at: None,
            triage_updated_by: None,
            note: None,
            note_updated_at: None,
            note_updated_by: None,
            labels: vec![],
        };
        let view = EventAnnotationsView::from_stored(annotations, &redactor);
        assert!(view.triage_state.is_none());
        assert!(view.note.is_none());
        assert!(view.labels.is_empty());
    }

    #[test]
    fn search_hit_from_result_basic() {
        let redactor = Redactor::new();
        let result = SearchResult {
            segment: crate::storage::Segment {
                id: 42,
                pane_id: 3,
                seq: 10,
                content: "test content".into(),
                content_len: 12,
                content_hash: None,
                captured_at: 9999,
            },
            snippet: Some("...test...".into()),
            highlight: None,
            score: 1.5,
        };
        let hit = SearchHit::from_result(result, &redactor);
        assert_eq!(hit.segment_id, 42);
        assert_eq!(hit.pane_id, 3);
        assert!((hit.score - 1.5).abs() < f64::EPSILON);
        assert_eq!(hit.snippet.as_deref(), Some("...test..."));
        assert_eq!(hit.captured_at, 9999);
        assert_eq!(hit.content_len, 12);
    }

    #[test]
    fn search_hit_from_result_no_snippet() {
        let redactor = Redactor::new();
        let result = SearchResult {
            segment: crate::storage::Segment {
                id: 1,
                pane_id: 0,
                seq: 0,
                content: String::new(),
                content_len: 0,
                content_hash: None,
                captured_at: 0,
            },
            snippet: None,
            highlight: None,
            score: 0.0,
        };
        let hit = SearchHit::from_result(result, &redactor);
        assert!(hit.snippet.is_none());
    }

    #[test]
    fn search_hit_serialization_skips_none_snippet() {
        let redactor = Redactor::new();
        let result = SearchResult {
            segment: crate::storage::Segment {
                id: 1,
                pane_id: 0,
                seq: 0,
                content: String::new(),
                content_len: 0,
                content_hash: None,
                captured_at: 0,
            },
            snippet: None,
            highlight: None,
            score: 0.0,
        };
        let hit = SearchHit::from_result(result, &redactor);
        let json = serde_json::to_value(&hit).unwrap();
        assert!(!json.as_object().unwrap().contains_key("snippet"));
    }

    #[test]
    fn bookmark_view_from_query() {
        let redactor = Redactor::new();
        let bookmark = ui_query::PaneBookmarkView {
            pane_id: 5,
            alias: "my-agent".into(),
            tags: vec!["codex".into(), "prod".into()],
            description: Some("Production agent".into()),
            created_at: 1000,
            updated_at: 2000,
        };
        let view = BookmarkView::from_query(bookmark, &redactor);
        assert_eq!(view.pane_id, 5);
        assert_eq!(view.alias, "my-agent");
        assert_eq!(view.tags.len(), 2);
        assert_eq!(view.tags[0], "codex");
        assert_eq!(view.description.as_deref(), Some("Production agent"));
    }

    #[test]
    fn bookmark_view_no_description() {
        let redactor = Redactor::new();
        let bookmark = ui_query::PaneBookmarkView {
            pane_id: 1,
            alias: "test".into(),
            tags: vec![],
            description: None,
            created_at: 0,
            updated_at: 0,
        };
        let view = BookmarkView::from_query(bookmark, &redactor);
        assert!(view.description.is_none());
        assert!(view.tags.is_empty());
    }

    #[test]
    fn saved_search_view_from_query() {
        let redactor = Redactor::new();
        let saved = ui_query::SavedSearchView {
            id: "ss-001".into(),
            name: "Error search".into(),
            query: "error OR fatal".into(),
            pane_id: Some(3),
            limit: 100,
            since_mode: "relative".into(),
            since_ms: Some(-3600000),
            schedule_interval_ms: Some(60000),
            enabled: true,
            last_run_at: Some(5000),
            last_result_count: Some(7),
            last_error: None,
            created_at: 1000,
            updated_at: 2000,
        };
        let view = SavedSearchView::from_query(saved, &redactor);
        assert_eq!(view.id, "ss-001");
        assert_eq!(view.name, "Error search");
        assert_eq!(view.query, "error OR fatal");
        assert_eq!(view.pane_id, Some(3));
        assert_eq!(view.limit, 100);
        assert!(view.enabled);
        assert_eq!(view.last_result_count, Some(7));
        assert!(view.last_error.is_none());
    }

    #[test]
    fn saved_search_view_minimal() {
        let redactor = Redactor::new();
        let saved = ui_query::SavedSearchView {
            id: "ss-min".into(),
            name: "minimal".into(),
            query: "test".into(),
            pane_id: None,
            limit: 10,
            since_mode: "absolute".into(),
            since_ms: None,
            schedule_interval_ms: None,
            enabled: false,
            last_run_at: None,
            last_result_count: None,
            last_error: Some("timeout".into()),
            created_at: 0,
            updated_at: 0,
        };
        let view = SavedSearchView::from_query(saved, &redactor);
        assert!(!view.enabled);
        assert!(view.pane_id.is_none());
        assert_eq!(view.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn ruleset_profile_view_from_summary() {
        let redactor = Redactor::new();
        let summary = crate::rulesets::RulesetProfileSummary {
            name: "custom".into(),
            description: Some("Custom rules".into()),
            path: Some("/etc/ft/rules".into()),
            last_applied_at: Some(12345),
            implicit: false,
        };
        let view = RulesetProfileView::from_summary(summary, &redactor);
        assert_eq!(view.name, "custom");
        assert_eq!(view.description.as_deref(), Some("Custom rules"));
        assert_eq!(view.path.as_deref(), Some("/etc/ft/rules"));
        assert_eq!(view.last_applied_at, Some(12345));
        assert!(!view.implicit);
    }

    #[test]
    fn ruleset_profile_view_implicit_default() {
        let redactor = Redactor::new();
        let summary = crate::rulesets::RulesetProfileSummary {
            name: "default".into(),
            description: None,
            path: None,
            last_applied_at: None,
            implicit: true,
        };
        let view = RulesetProfileView::from_summary(summary, &redactor);
        assert!(view.implicit);
        assert!(view.description.is_none());
        assert!(view.path.is_none());
    }

    // =========================================================================
    // HealthResponse tests
    // =========================================================================

    #[test]
    fn health_response_serialization() {
        let payload = HealthResponse {
            ok: true,
            version: VERSION,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["version"], VERSION);
    }

    // =========================================================================
    // Constants sanity tests
    // =========================================================================

    #[test]
    fn stream_constants_are_sensible() {
        assert!(STREAM_DEFAULT_MAX_HZ > 0);
        assert!(STREAM_MAX_MAX_HZ >= STREAM_DEFAULT_MAX_HZ);
        assert!(STREAM_CHANNEL_BUFFER > 0);
        assert!(STREAM_KEEPALIVE_SECS > 0);
        assert!(STREAM_SCAN_LIMIT > 0);
        assert!(STREAM_SCAN_MAX_PAGES > 0);
        assert!(STREAM_MAX_CONSECUTIVE_DROPS > 0);
        assert!(MAX_REQUEST_BODY_BYTES > 0);
        assert!(MAX_LIMIT > 0);
        assert!(DEFAULT_LIMIT > 0);
        assert!(DEFAULT_LIMIT <= MAX_LIMIT);
    }

    #[test]
    fn default_host_is_loopback() {
        assert_eq!(DEFAULT_HOST, "127.0.0.1");
    }

    // =========================================================================
    // Serialization roundtrip tests
    // =========================================================================

    #[test]
    fn panes_response_serialization() {
        let resp = PanesResponse {
            panes: vec![],
            total: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 0);
        assert!(json["panes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn events_response_serialization() {
        let resp = EventsResponse {
            events: vec![],
            total: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 0);
        assert!(json["events"].as_array().unwrap().is_empty());
    }

    #[test]
    fn search_response_serialization() {
        let resp = SearchResponse {
            results: vec![],
            total: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 0);
        assert!(json["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn bookmarks_response_serialization() {
        let resp = BookmarksResponse {
            bookmarks: vec![],
            total: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 0);
    }

    #[test]
    fn saved_searches_response_serialization() {
        let resp = SavedSearchesResponse {
            saved_searches: vec![],
            total: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["total"], 0);
    }

    #[test]
    fn ruleset_profile_response_serialization() {
        let resp = RulesetProfileResponse {
            active_profile: "default".into(),
            active_last_applied_at: None,
            profiles: vec![],
            total: 0,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["active_profile"], "default");
        assert!(
            !json
                .as_object()
                .unwrap()
                .contains_key("active_last_applied_at")
        );
    }
}
