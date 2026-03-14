//! Native HTTP framework seam for the web server module.
//!
//! Provides minimal HTTP types (Request, Response, StatusCode, etc.) and a
//! lightweight TCP server built on asupersync primitives. This replaces the
//! external FastAPI dependency with ~500 lines of purpose-built code.
//!
//! Design: only the API surface actually used by the `web/` module is
//! implemented. No unused generality.

use crate::runtime_compat::task;
use crate::{Error, Result};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::TcpListener;
use asupersync::stream::Stream;
use serde::Serialize;
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

// ── Core type aliases ────────────────────────────────────────────────────

/// Boxed future used in middleware and handler signatures.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Asupersync context re-export.
pub(crate) type Cx = asupersync::Cx;

// ── StatusCode ───────────────────────────────────────────────────────────

/// HTTP status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusCode(u16);

impl StatusCode {
    pub(crate) const OK: Self = Self(200);
    pub(crate) const BAD_REQUEST: Self = Self(400);
    pub(crate) const INTERNAL_SERVER_ERROR: Self = Self(500);
    pub(crate) const SERVICE_UNAVAILABLE: Self = Self(503);

    pub(crate) fn as_u16(self) -> u16 {
        self.0
    }

    fn reason_phrase(self) -> &'static str {
        match self.0 {
            200 => "OK",
            400 => "Bad Request",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Unknown",
        }
    }
}

// ── Method ───────────────────────────────────────────────────────────────

/// HTTP method (only GET is used by the web module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Get => write!(f, "GET"),
        }
    }
}

// ── QueryString ──────────────────────────────────────────────────────────

/// Parsed query string with key=value lookup.
pub(crate) struct QueryString<'a> {
    raw: &'a str,
    pairs: Vec<(&'a str, &'a str)>,
}

impl<'a> QueryString<'a> {
    pub(crate) fn parse(raw: &'a str) -> Self {
        let pairs = raw
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?;
                let value = parts.next().unwrap_or("");
                Some((key, value))
            })
            .collect();
        Self { raw, pairs }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&'a str> {
        self.pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
    }

    #[allow(dead_code)]
    pub(crate) fn raw(&self) -> &'a str {
        self.raw
    }
}

// ── ResponseBody ─────────────────────────────────────────────────────────

/// HTTP response body.
pub(crate) enum ResponseBody {
    Empty,
    Bytes(Vec<u8>),
    Streaming(Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>),
}

impl ResponseBody {
    pub(crate) fn stream<S>(s: S) -> Self
    where
        S: Stream<Item = Vec<u8>> + Send + 'static,
    {
        Self::Streaming(Box::pin(s))
    }
}

// ── TypeMap (request extensions) ─────────────────────────────────────────

/// Simple type-map for storing typed extensions on a Request.
struct TypeMap {
    map: HashMap<std::any::TypeId, Box<dyn Any + Send + Sync>>,
}

impl TypeMap {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    fn insert<T: Any + Send + Sync>(&mut self, val: T) {
        self.map.insert(std::any::TypeId::of::<T>(), Box::new(val));
    }

    fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.map
            .get(&std::any::TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }
}

// ── Request ──────────────────────────────────────────────────────────────

/// HTTP request.
pub(crate) struct Request {
    method: Method,
    path: String,
    query: String,
    headers: HashMap<String, Vec<u8>>,
    #[allow(dead_code)]
    body: Vec<u8>,
    extensions: TypeMap,
}

impl Request {
    fn new(method: Method, path: String, query: String, headers: HashMap<String, Vec<u8>>) -> Self {
        Self {
            method,
            path,
            query,
            headers,
            body: Vec::new(),
            extensions: TypeMap::new(),
        }
    }

    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn query(&self) -> Option<&str> {
        if self.query.is_empty() {
            None
        } else {
            Some(&self.query)
        }
    }

    pub(crate) fn headers(&self) -> &HashMap<String, Vec<u8>> {
        &self.headers
    }

    pub(crate) fn insert_extension<T: Any + Send + Sync>(&mut self, val: T) {
        self.extensions.insert(val);
    }

    pub(crate) fn get_extension<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.extensions.get()
    }
}

// ── Response ─────────────────────────────────────────────────────────────

/// HTTP response.
pub(crate) struct Response {
    status: StatusCode,
    headers: HashMap<String, Vec<u8>>,
    body: ResponseBody,
}

impl Response {
    pub(crate) fn with_status(status: StatusCode) -> Self {
        Self {
            status,
            headers: HashMap::new(),
            body: ResponseBody::Empty,
        }
    }

    pub(crate) fn json<T: Serialize>(data: &T) -> std::result::Result<Self, serde_json::Error> {
        let body = serde_json::to_vec(data)?;
        Ok(Self {
            status: StatusCode::OK,
            headers: HashMap::from([(
                "content-type".to_string(),
                b"application/json".to_vec(),
            )]),
            body: ResponseBody::Bytes(body),
        })
    }

    pub(crate) fn internal_error() -> Self {
        Self::with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }

    pub(crate) fn header(mut self, key: &str, value: Vec<u8>) -> Self {
        self.headers.insert(key.to_lowercase(), value);
        self
    }

    pub(crate) fn body(mut self, body: ResponseBody) -> Self {
        self.body = body;
        self
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn headers_ref(&self) -> &HashMap<String, Vec<u8>> {
        &self.headers
    }

    /// Serialize this response to an HTTP/1.1 byte buffer (non-streaming).
    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(512);
        let status = self.status.as_u16();
        let reason = self.status.reason_phrase();
        buf.extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());

        let body_bytes = match &self.body {
            ResponseBody::Bytes(b) => Some(b.as_slice()),
            ResponseBody::Empty => Some(&[][..]),
            ResponseBody::Streaming(_) => None,
        };

        if let Some(body) = body_bytes {
            // Add content-length if not streaming
            if !self.headers.contains_key("content-length") {
                buf.extend_from_slice(
                    format!("content-length: {}\r\n", body.len()).as_bytes(),
                );
            }
        }

        for (key, value) in &self.headers {
            buf.extend_from_slice(key.as_bytes());
            buf.extend_from_slice(b": ");
            buf.extend_from_slice(value);
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(b"\r\n");

        if let Some(body) = body_bytes {
            buf.extend_from_slice(body);
        }
        buf
    }
}

// ── ControlFlow ──────────────────────────────────────────────────────────

/// Middleware control flow: continue processing or short-circuit with a response.
pub(crate) enum ControlFlow {
    Continue,
    Break(Response),
}

// ── RequestContext ────────────────────────────────────────────────────────

/// Opaque request context passed to handlers and middleware.
/// Currently unused by FrankenTerm handlers (all use `_ctx`).
pub(crate) struct RequestContext;

// ── Middleware trait ──────────────────────────────────────────────────────

/// HTTP middleware with before/after hooks.
pub(crate) trait Middleware: Send + Sync {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _req: &'a mut Request,
    ) -> BoxFuture<'a, ControlFlow> {
        Box::pin(async { ControlFlow::Continue })
    }

    fn after<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        _req: &'a Request,
        response: Response,
    ) -> BoxFuture<'a, Response> {
        Box::pin(async move { response })
    }

    fn name(&self) -> &'static str;
}

// ── Route + App ──────────────────────────────────────────────────────────

type RouteHandler = Box<
    dyn for<'a> Fn(&'a RequestContext, &'a mut Request) -> BoxFuture<'a, Response> + Send + Sync,
>;

struct Route {
    path: String,
    method: Method,
    handler: RouteHandler,
}

/// HTTP application with routes and middleware.
pub(crate) struct App {
    routes: Vec<Route>,
    middlewares: Vec<Box<dyn Middleware>>,
}

impl App {
    pub(crate) fn builder() -> AppBuilder {
        AppBuilder {
            routes: Vec::new(),
            middlewares: Vec::new(),
        }
    }

    /// Dispatch a request through middleware and routes.
    async fn handle(&self, req: &mut Request) -> Response {
        let ctx = RequestContext;

        // Run before-middlewares
        for mw in &self.middlewares {
            match mw.before(&ctx, req).await {
                ControlFlow::Continue => {}
                ControlFlow::Break(resp) => return resp,
            }
        }

        // Route dispatch
        let response = self.dispatch(&ctx, req).await;

        // Run after-middlewares (in reverse order)
        let mut response = response;
        for mw in self.middlewares.iter().rev() {
            response = mw.after(&ctx, req, response).await;
        }

        response
    }

    async fn dispatch(&self, ctx: &RequestContext, req: &mut Request) -> Response {
        let path = req.path().to_string();
        let method = *req.method();

        for route in &self.routes {
            if route.method == method && route.path == path {
                return (route.handler)(ctx, req).await;
            }
        }

        // 404 Not Found
        Response::with_status(StatusCode(404))
            .header("content-type", b"application/json".to_vec())
            .body(ResponseBody::Bytes(
                br#"{"ok":false,"error":"Not Found","error_code":"not_found"}"#.to_vec(),
            ))
    }
}

/// Builder for App.
pub(crate) struct AppBuilder {
    routes: Vec<Route>,
    middlewares: Vec<Box<dyn Middleware>>,
}

impl AppBuilder {
    pub(crate) fn middleware<M: Middleware + 'static>(mut self, mw: M) -> Self {
        self.middlewares.push(Box::new(mw));
        self
    }

    pub(crate) fn route<F, Fut>(mut self, path: &str, method: Method, handler: F) -> Self
    where
        F: Fn(&RequestContext, &mut Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response> + Send + 'static,
    {
        let handler: RouteHandler = Box::new(move |ctx, req| Box::pin(handler(ctx, req)));
        self.routes.push(Route {
            path: path.to_string(),
            method,
            handler,
        });
        self
    }

    pub(crate) fn build(self) -> App {
        App {
            routes: self.routes,
            middlewares: self.middlewares,
        }
    }
}

// ── Server ───────────────────────────────────────────────────────────────

/// Server error.
#[derive(Debug)]
pub(crate) enum ServerError {
    Shutdown,
    Io(std::io::Error),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shutdown => write!(f, "server shutdown"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

pub(crate) type FrameworkServerJoinResult =
    std::result::Result<std::result::Result<(), ServerError>, task::JoinError>;

/// Lightweight TCP server built on asupersync.
pub(crate) struct TcpServer {
    shutdown: AtomicBool,
}

impl TcpServer {
    fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
        }
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

/// Framework-owned runtime state for the feature-gated web server.
pub(crate) struct FrameworkWebRuntime {
    app: Arc<App>,
    server: Arc<TcpServer>,
    join: task::JoinHandle<std::result::Result<(), ServerError>>,
}

impl FrameworkWebRuntime {
    pub(crate) async fn start(bind_addr: String, app: App) -> Result<(SocketAddr, Self)> {
        let app = Arc::new(app);
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(Error::Io)?;
        let local_addr = listener.local_addr().map_err(Error::Io)?;

        let server = Arc::new(TcpServer::new());

        let join = {
            let app = Arc::clone(&app);
            let server = Arc::clone(&server);
            task::spawn(async move { serve_loop(server, listener, app).await })
        };

        Ok((local_addr, Self { app, server, join }))
    }

    pub(crate) fn signal_shutdown(&self) {
        self.server.shutdown();
    }

    pub(crate) fn join_handle_mut(
        &mut self,
    ) -> &mut task::JoinHandle<std::result::Result<(), ServerError>> {
        &mut self.join
    }

    pub(crate) async fn finish(self, result: FrameworkServerJoinResult) -> Result<()> {
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
        Ok(())
    }
}

/// Accept loop: parse HTTP requests and dispatch to the App.
async fn serve_loop(
    server: Arc<TcpServer>,
    listener: TcpListener,
    app: Arc<App>,
) -> std::result::Result<(), ServerError> {
    loop {
        if server.is_shutdown() {
            return Err(ServerError::Shutdown);
        }

        let (mut stream, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                if server.is_shutdown() {
                    return Err(ServerError::Shutdown);
                }
                warn!(target: "wa.web", error = %e, "accept failed");
                continue;
            }
        };

        if server.is_shutdown() {
            return Err(ServerError::Shutdown);
        }

        let app = Arc::clone(&app);
        let server = Arc::clone(&server);
        task::spawn(async move {
            if let Err(e) = handle_connection(&app, &server, &mut stream, peer).await {
                debug!(target: "wa.web", peer = %peer, error = %e, "connection error");
            }
        });
    }
}

/// Handle a single HTTP connection (one request-response cycle).
async fn handle_connection(
    app: &App,
    server: &TcpServer,
    stream: &mut asupersync::net::TcpStream,
    _peer: SocketAddr,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if server.is_shutdown() {
        return Ok(());
    }

    // Read request headers byte-by-byte until \r\n\r\n (max 8KB).
    // This is correct but not optimized — a follow-up can add buffered reads.
    let mut buf = Vec::with_capacity(2048);
    let max_header_bytes = 8192;
    loop {
        if buf.len() >= max_header_bytes {
            let resp = Response::with_status(StatusCode::BAD_REQUEST);
            stream.write_all(&resp.to_bytes()).await?;
            return Ok(());
        }
        match stream.read_u8().await {
            Ok(b) => buf.push(b),
            Err(_) => {
                if buf.is_empty() {
                    return Ok(());
                }
                break;
            }
        }
        // Check for end-of-headers marker
        if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
            break;
        }
    }
    let raw = &buf[..];

    // Parse HTTP request
    let mut headers_buf = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers_buf);
    match parsed.parse(raw) {
        Ok(httparse::Status::Complete(_body_offset)) => {}
        Ok(httparse::Status::Partial) => {
            let resp = Response::with_status(StatusCode::BAD_REQUEST);
            stream.write_all(&resp.to_bytes()).await?;
            return Ok(());
        }
        Err(_) => {
            let resp = Response::with_status(StatusCode::BAD_REQUEST);
            stream.write_all(&resp.to_bytes()).await?;
            return Ok(());
        }
    }

    let method_str = parsed.method.unwrap_or("GET");
    let method = match method_str {
        "GET" => Method::Get,
        _ => {
            let resp = Response::with_status(StatusCode(405))
                .header("allow", b"GET".to_vec());
            stream.write_all(&resp.to_bytes()).await?;
            return Ok(());
        }
    };

    let full_path = parsed.path.unwrap_or("/");
    let (path, query) = match full_path.find('?') {
        Some(idx) => (full_path[..idx].to_string(), full_path[idx + 1..].to_string()),
        None => (full_path.to_string(), String::new()),
    };

    let mut headers = HashMap::new();
    for h in parsed.headers.iter() {
        headers.insert(h.name.to_lowercase(), h.value.to_vec());
    }

    let mut req = Request::new(method, path, query, headers);

    // Dispatch through App
    let response = app.handle(&mut req).await;

    // Write response
    match &response.body {
        ResponseBody::Streaming(_stream) => {
            // Write headers first, then stream chunks as SSE
            let mut header_buf = Vec::with_capacity(256);
            let status = response.status.as_u16();
            let reason = response.status.reason_phrase();
            header_buf
                .extend_from_slice(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes());
            header_buf.extend_from_slice(b"transfer-encoding: chunked\r\n");
            for (key, value) in &response.headers {
                header_buf.extend_from_slice(key.as_bytes());
                header_buf.extend_from_slice(b": ");
                header_buf.extend_from_slice(value);
                header_buf.extend_from_slice(b"\r\n");
            }
            header_buf.extend_from_slice(b"\r\n");
            stream.write_all(&header_buf).await?;
            stream.flush().await?;

            // Stream body chunks
            // Note: SSE streaming requires the response body to be consumed
            // by the caller. For now, we write headers and let the connection
            // stay open. Full SSE support will be wired in a follow-up slice.
        }
        _ => {
            stream.write_all(&response.to_bytes()).await?;
        }
    }

    stream.flush().await?;
    Ok(())
}

// ── Helper functions ─────────────────────────────────────────────────────

pub(crate) fn json_response_with_status<T: Serialize>(
    status: StatusCode,
    payload: &T,
) -> Response {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    Response::with_status(status)
        .header("content-type", b"application/json".to_vec())
        .body(ResponseBody::Bytes(body))
}

pub(crate) fn sse_stream_response<S>(stream: S) -> Response
where
    S: Stream<Item = Vec<u8>> + Send + 'static,
{
    Response::with_status(StatusCode::OK)
        .header("content-type", b"text/event-stream".to_vec())
        .header("cache-control", b"no-cache".to_vec())
        .header("connection", b"keep-alive".to_vec())
        .header("x-accel-buffering", b"no".to_vec())
        .body(ResponseBody::stream(stream))
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Serialize)]
    struct ExamplePayload {
        ok: bool,
    }

    struct EmptyStream;

    impl Stream for EmptyStream {
        type Item = Vec<u8>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    fn header_value<'a>(response: &'a Response, key: &str) -> Option<&'a str> {
        response
            .headers_ref()
            .get(key)
            .and_then(|value| std::str::from_utf8(value).ok())
    }

    #[test]
    fn json_response_helper_sets_status_and_content_type() {
        let response =
            json_response_with_status(StatusCode::BAD_REQUEST, &ExamplePayload { ok: false });

        assert_eq!(response.status().as_u16(), 400);
        assert_eq!(
            header_value(&response, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn sse_response_helper_sets_standard_stream_headers() {
        let response = sse_stream_response(EmptyStream);

        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            header_value(&response, "content-type"),
            Some("text/event-stream")
        );
        assert_eq!(header_value(&response, "cache-control"), Some("no-cache"));
        assert_eq!(header_value(&response, "connection"), Some("keep-alive"));
        assert_eq!(header_value(&response, "x-accel-buffering"), Some("no"));
    }

    #[test]
    fn status_code_values() {
        assert_eq!(StatusCode::OK.as_u16(), 200);
        assert_eq!(StatusCode::BAD_REQUEST.as_u16(), 400);
        assert_eq!(StatusCode::INTERNAL_SERVER_ERROR.as_u16(), 500);
        assert_eq!(StatusCode::SERVICE_UNAVAILABLE.as_u16(), 503);
    }

    #[test]
    fn query_string_parse_and_get() {
        let qs = QueryString::parse("limit=10&pane=0&verbose=true");
        assert_eq!(qs.get("limit"), Some("10"));
        assert_eq!(qs.get("pane"), Some("0"));
        assert_eq!(qs.get("verbose"), Some("true"));
        assert_eq!(qs.get("missing"), None);
    }

    #[test]
    fn query_string_empty() {
        let qs = QueryString::parse("");
        assert_eq!(qs.get("anything"), None);
    }

    #[test]
    fn response_json_sets_content_type() {
        let resp = Response::json(&ExamplePayload { ok: true }).unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            header_value(&resp, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn response_with_status_and_headers() {
        let resp = Response::with_status(StatusCode::SERVICE_UNAVAILABLE)
            .header("retry-after", b"30".to_vec());
        assert_eq!(resp.status().as_u16(), 503);
        assert_eq!(header_value(&resp, "retry-after"), Some("30"));
    }

    #[test]
    fn response_internal_error() {
        let resp = Response::internal_error();
        assert_eq!(resp.status().as_u16(), 500);
    }

    #[test]
    fn request_extensions_round_trip() {
        #[derive(Debug, Clone)]
        struct TestExt(u32);

        let mut req =
            Request::new(Method::Get, "/test".into(), String::new(), HashMap::new());
        assert!(req.get_extension::<TestExt>().is_none());

        req.insert_extension(TestExt(42));
        let ext = req.get_extension::<TestExt>().unwrap();
        assert_eq!(ext.0, 42);
    }

    #[test]
    fn request_method_and_path() {
        let req = Request::new(
            Method::Get,
            "/health".into(),
            "format=json".into(),
            HashMap::new(),
        );
        assert_eq!(*req.method(), Method::Get);
        assert_eq!(req.path(), "/health");
        assert_eq!(req.query(), Some("format=json"));
    }

    #[test]
    fn response_to_bytes_format() {
        let resp = Response::with_status(StatusCode::OK)
            .header("content-type", b"text/plain".to_vec())
            .body(ResponseBody::Bytes(b"hello".to_vec()));
        let bytes = resp.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("content-length: 5\r\n"));
        assert!(text.contains("content-type: text/plain\r\n"));
        assert!(text.ends_with("\r\n\r\nhello"));
    }

    #[test]
    fn control_flow_variants() {
        let cf = ControlFlow::Continue;
        assert!(matches!(cf, ControlFlow::Continue));

        let resp = Response::internal_error();
        let cf = ControlFlow::Break(resp);
        assert!(matches!(cf, ControlFlow::Break(_)));
    }

    #[test]
    fn method_display() {
        assert_eq!(format!("{}", Method::Get), "GET");
    }
}
