//! Web server scaffolding for wa (feature-gated: web).
//!
//! Provides a minimal `ft web` HTTP server with /health and read-only
//! data endpoints (/panes, /events, /search, /bookmarks, /ruleset-profile, /saved-searches)
//! backed by shared query helpers, plus SSE subscriptions for live event/delta streams.

use crate::events::{Event, EventBus, RecvError};
use crate::policy::Redactor;
use crate::runtime_compat::{mpsc, select, signal, sleep, task, timeout};
use crate::storage::{
    EventQuery, PaneRecord, SearchOptions, SearchResult, SegmentScanQuery, StorageHandle,
};
use crate::ui_query;
use crate::{Error, Result, VERSION};
use asupersync::net::TcpListener;
use asupersync::stream::Stream;
#[cfg(test)]
use serde::Serialize;
use serde_json::json;
use std::net::{SocketAddr, TcpStream};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

mod web_framework {
    pub use fastapi::ResponseBody;
    pub use fastapi::core::{
        BoxFuture, ControlFlow, Cx, Handler, Middleware, StartupOutcome,
    };
    pub use fastapi::http::QueryString;
    pub use fastapi::prelude::{App, Method, Request, RequestContext, Response, StatusCode};
    pub use fastapi::{ServerConfig, ServerError, TcpServer};
}

use web_framework::{
    App, BoxFuture, ControlFlow, Cx, Handler, Method, Middleware, QueryString, Request,
    RequestContext, Response, ResponseBody, ServerConfig, ServerError, StartupOutcome, StatusCode,
    TcpServer,
};

mod error;
mod extractors;
mod handlers;
mod middleware;
mod openapi;
mod router;
mod server;
mod sse;
mod websocket;

use error::{json_err, json_ok};
use extractors::{
    parse_bool, parse_i64, parse_limit, parse_u64, redact_json_value, require_event_bus,
    require_storage, require_storage_and_event_bus,
};
#[cfg(test)]
use handlers::{
    BookmarkView, BookmarksResponse, EventAnnotationsView, EventView, EventsResponse,
    HealthResponse, PaneView, PanesResponse, RulesetProfileResponse, RulesetProfileView,
    SavedSearchView, SavedSearchesResponse, SearchHit, SearchResponse,
};
use handlers::{
    handle_bookmarks, handle_events, handle_panes, handle_ruleset_profile, handle_saved_searches,
    handle_search, health_response,
};
use middleware::{AppState, BodySizeGuard, RequestSpanLogger, StateInjector};
use server::{handle_server_exit, poke_listener};
pub use server::{run_web_server, start_web_server};

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

#[cfg(test)]
fn parse_stream_max_hz(qs: &QueryString<'_>) -> u64 {
    sse::parse_stream_max_hz(qs)
}

#[cfg(test)]
type EventStreamChannel = sse::EventStreamChannel;

#[cfg(test)]
fn parse_event_stream_channel(
    qs: &QueryString<'_>,
) -> std::result::Result<EventStreamChannel, Response> {
    sse::parse_event_stream_channel(qs)
}

#[cfg(test)]
fn epoch_ms_now() -> i64 {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(ts.as_millis()).unwrap_or(0)
}

#[cfg(test)]
fn make_stream_frame(
    stream: &'static str,
    kind: &'static str,
    seq: u64,
    data: serde_json::Value,
) -> serde_json::Value {
    sse::make_stream_frame(stream, kind, seq, data)
}

#[cfg(test)]
fn frame_to_sse(
    event_type: &'static str,
    seq: u64,
    frame: serde_json::Value,
) -> Option<sse::SseEvent> {
    sse::frame_to_sse(event_type, seq, frame)
}

#[cfg(test)]
fn event_matches_pane(event: &Event, pane_filter: Option<u64>) -> bool {
    sse::event_matches_pane(event, pane_filter)
}

fn handle_stream_events(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    sse::handle_stream_events(req)
}

fn handle_stream_deltas(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    sse::handle_stream_deltas(req)
}

// =============================================================================
// App builder
// =============================================================================

fn build_app(storage: Option<StorageHandle>, event_bus: Option<Arc<EventBus>>) -> App {
    router::build_app(storage, event_bus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::error::ApiResponse;

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
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"], serde_json::json!("hello"));
        assert!(json.get("error").is_none());
        assert!(json.get("error_code").is_none());
        assert_eq!(json["version"], VERSION);
    }

    #[test]
    fn api_response_error_has_ok_false() {
        let resp = ApiResponse::<()>::error("test_code", "something went wrong");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert!(json.get("data").is_none());
        assert_eq!(json["error"], "something went wrong");
        assert_eq!(json["error_code"], "test_code");
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
        let payload = String::from_utf8(sse.to_bytes()).expect("valid UTF-8 SSE payload");
        assert!(payload.contains("id: 7\n"));
        assert!(payload.contains("event: delta\n"));
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
            first_seen_at: 1000,
            last_seen_at: 2000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
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
            first_seen_at: 500,
            last_seen_at: 600,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
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
            first_seen_at: 0,
            last_seen_at: 0,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
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
            segment_id: None,
            detected_at: 5000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
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
            segment_id: None,
            detected_at: 0,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
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
