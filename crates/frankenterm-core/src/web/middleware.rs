//! HTTP middleware surface for Wave 4B migration.
//!
//! Middleware/state extraction from `web.rs` while preserving behavior.

use super::error::json_err;
use super::{WebRuntimeLimits, resolve_runtime_limits};
use crate::events::EventBus;
use crate::policy::Redactor;
use crate::storage::StorageHandle;
use crate::web_framework::{
    BoxFuture, ControlFlow, Middleware, Request, RequestContext, Response, StatusCode,
};
use std::sync::Arc;
use std::time::Instant;
use tracing::info;

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
    pub(super) runtime_limits: WebRuntimeLimits,
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

/// Rejects requests whose Content-Length exceeds the configured web body limit.
#[derive(Clone)]
pub(super) struct BodySizeGuard {
    max_request_body_bytes: usize,
}

impl BodySizeGuard {
    pub(super) fn new(max_request_body_bytes: usize) -> Self {
        Self {
            max_request_body_bytes,
        }
    }
}

impl Default for BodySizeGuard {
    fn default() -> Self {
        Self::new(resolve_runtime_limits(None).max_request_body_bytes)
    }
}

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
            if cl > self.max_request_body_bytes {
                let max_request_body_bytes = self.max_request_body_bytes;
                let resp = json_err(
                    StatusCode::BAD_REQUEST,
                    "body_too_large",
                    format!("Request body too large ({cl} bytes); max is {max_request_body_bytes}"),
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

#[cfg(test)]
mod tests {
    use super::BodySizeGuard;
    use crate::runtime_async::CompatRuntime;
    use crate::web_framework::{
        ControlFlow, Method, Middleware, Request, RequestContext, ResponseBody, StatusCode,
    };

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build web middleware test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn body_size_guard_uses_configured_limit_ft_9ahut() {
        run_async_test(async {
            let guard = BodySizeGuard::new(8);
            let ctx = RequestContext::new(crate::cx::for_request(), 1);
            let mut req = Request::new(Method::Post, "/events");
            req.headers_mut().insert("content-length", b"9".to_vec());

            match guard.before(&ctx, &mut req).await {
                ControlFlow::Break(response) => {
                    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                    let (_, _, body) = response.into_parts();
                    let ResponseBody::Bytes(bytes) = body else {
                        panic!("expected JSON response body");
                    };
                    let json: serde_json::Value =
                        serde_json::from_slice(&bytes).expect("valid JSON");
                    assert_eq!(json["error_code"], "body_too_large");
                    assert!(
                        json["error"]
                            .as_str()
                            .expect("error message")
                            .contains("max is 8")
                    );
                }
                ControlFlow::Continue => panic!("request should have been rejected"),
            }
        });
    }
}
