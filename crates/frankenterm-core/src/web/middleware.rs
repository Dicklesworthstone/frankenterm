//! HTTP middleware surface for Wave 4B migration.
//!
//! Middleware/state extraction from `web.rs` while preserving behavior.

use super::error::json_err;
use super::{WebRuntimeLimits, resolve_runtime_limits};
use crate::events::EventBus;
use crate::policy::Redactor;
use crate::storage::StorageHandle;
use crate::web_framework::{
    BoxFuture, ControlFlow, FrameworkRequestBody, Middleware, PreBodyValidator, Request,
    RequestContext, Response, StatusCode,
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

/// Rejects decoded bodies and declared lengths above the configured body cap.
///
/// The pinned framework buffers bodies before its admission hook, and its
/// parser's total-request allowance includes space for headers. That allowance
/// must not become extra body capacity for chunked requests. Checking the
/// actual body also protects routes that never invoke a body extractor.
#[derive(Clone)]
pub struct BodySizeGuard {
    max_request_body_bytes: usize,
}

impl BodySizeGuard {
    pub(crate) fn new(max_request_body_bytes: usize) -> Self {
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

impl PreBodyValidator for BodySizeGuard {
    fn validate(&self, req: &Request) -> std::result::Result<(), Response> {
        let declared_length = req
            .headers()
            .get("content-length")
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|v| v.parse::<usize>().ok());
        let body_length = match req.body() {
            FrameworkRequestBody::Empty => Some(0),
            FrameworkRequestBody::Bytes(bytes) => Some(bytes.len()),
            FrameworkRequestBody::Stream { content_length, .. } => *content_length,
        };
        if let Some(length) = declared_length
            .into_iter()
            .chain(body_length)
            .find(|length| *length > self.max_request_body_bytes)
        {
            let max_request_body_bytes = self.max_request_body_bytes;
            return Err(json_err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                format!("Request body too large ({length} bytes); max is {max_request_body_bytes}"),
            )
            .header("connection", b"close".to_vec()));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "BodySizeGuard"
    }
}

impl Middleware for BodySizeGuard {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> BoxFuture<'a, ControlFlow> {
        let decision = match self.validate(req) {
            Ok(()) => ControlFlow::Continue,
            Err(response) => ControlFlow::Break(response),
        };
        Box::pin(async move { decision })
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
        ControlFlow, FrameworkRequestBody, Method, Middleware, PreBodyValidator, Request,
        RequestContext, ResponseBody, StatusCode,
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
                    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
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

    #[test]
    fn body_size_guard_checks_decoded_bytes_without_consuming_them() {
        let guard = BodySizeGuard::new(8);
        for size in [0, 7, 8, 9] {
            for content_length in [None, Some("0"), Some("8")] {
                let mut request = Request::new(Method::Get, "/health");
                if let Some(value) = content_length {
                    request
                        .headers_mut()
                        .insert("content-length", value.as_bytes().to_vec());
                }
                request.set_body(FrameworkRequestBody::Bytes(vec![b'x'; size]));
                let result = guard.validate(&request);
                if size > 8 {
                    let response = result.expect_err("decoded bytes must enforce the cap");
                    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
                    assert!(response.headers().iter().any(|(name, value)| {
                        name.eq_ignore_ascii_case("connection") && value == b"close"
                    }));
                } else {
                    result.expect("an in-bounds body must remain accepted");
                }
                let FrameworkRequestBody::Bytes(bytes) = request.body() else {
                    panic!("admission must preserve the body for its handler");
                };
                assert_eq!(bytes, &vec![b'x'; size]);
            }
        }
    }

    #[test]
    fn body_size_guard_zero_cap_allows_only_empty_bodies() {
        let guard = BodySizeGuard::new(0);
        let mut request = Request::new(Method::Get, "/health");
        guard
            .validate(&request)
            .expect("empty requests are allowed");
        request.set_body(FrameworkRequestBody::Bytes(vec![b'x']));
        assert_eq!(
            guard.validate(&request).unwrap_err().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
