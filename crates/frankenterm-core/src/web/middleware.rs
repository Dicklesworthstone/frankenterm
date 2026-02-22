//! HTTP middleware surface for Wave 4B migration.
//!
//! Middleware/state extraction from `web.rs` while preserving behavior.

use super::*;

#[derive(Debug, Clone, Copy)]
struct RequestStart(Instant);

#[derive(Debug, Clone, Default)]
pub(super) struct RequestSpanLogger;

impl Middleware for RequestSpanLogger {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> BoxFuture<'a, ControlFlow> {
        req.insert_extension(RequestStart(Instant::now()));
        Box::pin(async { ControlFlow::Continue })
    }

    fn after<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a Request,
        response: Response,
    ) -> BoxFuture<'a, Response> {
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

/// Shared application state available to all handlers.
#[derive(Clone)]
pub(super) struct AppState {
    pub(super) storage: Option<StorageHandle>,
    pub(super) event_bus: Option<Arc<EventBus>>,
    pub(super) redactor: Arc<Redactor>,
}

/// Middleware that injects [`AppState`] into every request.
#[derive(Clone)]
pub(super) struct StateInjector {
    pub(super) state: AppState,
}

impl Middleware for StateInjector {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> BoxFuture<'a, ControlFlow> {
        req.insert_extension(self.state.clone());
        Box::pin(async { ControlFlow::Continue })
    }

    fn name(&self) -> &'static str {
        "StateInjector"
    }
}

/// Rejects requests whose Content-Length exceeds [`MAX_REQUEST_BODY_BYTES`].
#[derive(Clone, Default)]
pub(super) struct BodySizeGuard;

impl Middleware for BodySizeGuard {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> BoxFuture<'a, ControlFlow> {
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
