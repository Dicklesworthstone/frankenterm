//! Web server scaffolding for wa (feature-gated: web).
//!
//! Provides a minimal `wa web` HTTP server with /health and read-only
//! data endpoints (/panes, /events, /search, /bookmarks, /ruleset-profile, /saved-searches)
//! backed by shared query helpers.

use crate::policy::Redactor;
use crate::storage::{EventQuery, PaneRecord, SearchOptions, SearchResult, StorageHandle};
use crate::ui_query;
use crate::{Error, Result, VERSION};
use asupersync::net::TcpListener;
use fastapi::ResponseBody;
use fastapi::core::{ControlFlow, Cx, Handler, Middleware, StartupOutcome};
use fastapi::http::QueryString;
use fastapi::prelude::{App, Method, Request, RequestContext, Response, StatusCode};
use fastapi::{ServerConfig, ServerError, TcpServer};
use serde::Serialize;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8000;

/// Hard ceiling on list endpoint results.
const MAX_LIMIT: usize = 500;
/// Default limit when none is provided.
const DEFAULT_LIMIT: usize = 50;

/// Maximum request body size (64 KB). Rejects oversized requests early.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;

/// Configuration for the web server.
#[derive(Clone)]
pub struct WebServerConfig {
    host: String,
    port: u16,
    storage: Option<StorageHandle>,
    /// Must be set to `true` to bind on a non-localhost address.
    allow_public_bind: bool,
}

impl std::fmt::Debug for WebServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebServerConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("storage", &self.storage.is_some())
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
    join: tokio::task::JoinHandle<std::result::Result<(), ServerError>>,
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

fn build_app(storage: Option<StorageHandle>) -> App {
    let state = AppState {
        storage,
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
    let app = build_app(config.storage);

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
        tokio::spawn(async move {
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

    println!("wa web listening on http://{bound_addr}");

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
