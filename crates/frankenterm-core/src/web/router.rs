//! Router assembly for the web server.
//!
//! This is the first strangler-fig extraction from `web.rs`, keeping
//! behavior identical while moving route wiring into `web/` modules.

use super::handlers::{
    handle_bookmarks, handle_events, handle_panes, handle_ruleset_profile, handle_saved_searches,
    handle_search, health_response,
};
use super::middleware::{AppState, BodySizeGuard, RequestSpanLogger, StateInjector};
use super::sse::{handle_stream_deltas, handle_stream_events};
use crate::events::EventBus;
use crate::policy::Redactor;
use crate::storage::StorageHandle;
use crate::web_framework::{App, Method, Request, RequestContext};
use std::sync::Arc;

pub(super) fn build_app(storage: Option<StorageHandle>, event_bus: Option<Arc<EventBus>>) -> App {
    let state = AppState {
        storage,
        event_bus,
        redactor: Arc::new(Redactor::new()),
    };

    App::builder()
        .middleware(BodySizeGuard)
        .middleware(RequestSpanLogger)
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
