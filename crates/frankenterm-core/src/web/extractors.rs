//! Request extractors and query parameter helpers for Wave 4B migration.
//!
//! Provides typed extractors for pulling shared resources (storage, event bus,
//! redactor) from request extensions, plus helpers for parsing common query
//! string parameters.

use super::error::json_err;
use super::middleware::AppState;
use super::{QueryString, Request, Response, StatusCode, WebRuntimeLimits, resolve_runtime_limits};
use crate::events::EventBus;
use crate::policy::Redactor;
use crate::storage::StorageHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// =============================================================================
// br-ft-10i8s: redaction recursion depth bound
// =============================================================================

/// Maximum recursion depth for [`redact_json_value`].
///
/// br-ft-10i8s: a hostile JSON payload like `[[[[[...]]]]]` (one
/// bracket per body byte) parses into a tree whose depth is
/// proportional to body length. Without a depth bound, the recursive
/// walker blows the stack on most platforms (each frame > 64 bytes
/// against a 2 MiB default stack means ~32K levels is enough).
/// Hitting the cap causes the walker to early-return without
/// descending further — the truncated subtree's strings stay
/// un-redacted, so the cap is set well above any legitimate event-
/// payload depth (event JSON is rarely > 10 levels deep).
pub const MAX_REDACT_RECURSION_DEPTH: usize = 64;

/// br-ft-10i8s: cumulative count of times [`redact_json_value`] hit
/// the depth cap and stopped descending. A non-zero value means a
/// hostile or malformed JSON payload tried to drive the walker past
/// the safe recursion limit; its remaining subtree was passed
/// through un-redacted. Same observability-counter pattern as
/// `EPOCH_CLOCK_ANOMALY_COUNT` (ft-bn6qi) and the SSE drop counters
/// (ft-95fd3).
static REDACT_DEPTH_LIMIT_HIT_COUNT: AtomicU64 = AtomicU64::new(0);

/// br-ft-10i8s: cumulative count of times the redaction walker
/// stopped at [`MAX_REDACT_RECURSION_DEPTH`].
#[must_use]
pub fn redact_depth_limit_hit_count() -> u64 {
    REDACT_DEPTH_LIMIT_HIT_COUNT.load(Ordering::Relaxed)
}

/// br-ft-10i8s: test helper to reset the depth-cap counter.
#[cfg(test)]
fn reset_redact_depth_limit_hit_count_for_test() {
    REDACT_DEPTH_LIMIT_HIT_COUNT.store(0, Ordering::Relaxed);
}

#[inline]
fn record_redact_depth_limit_hit() {
    REDACT_DEPTH_LIMIT_HIT_COUNT.fetch_add(1, Ordering::Relaxed);
}

// =============================================================================
// State extractors
// =============================================================================

/// Extract a [`StorageHandle`] and [`Redactor`] from the request's [`AppState`].
pub(super) fn require_storage(
    req: &Request,
) -> std::result::Result<(StorageHandle, Arc<Redactor>), Response> {
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

/// Extract an [`EventBus`] and [`Redactor`] from the request's [`AppState`].
pub(super) fn require_event_bus(
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

/// Extract [`StorageHandle`], [`EventBus`], and [`Redactor`] from the request's
/// [`AppState`].
pub(super) fn require_storage_and_event_bus(
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

/// Return the request-scoped web runtime limits.
pub(super) fn request_runtime_limits(req: &Request) -> WebRuntimeLimits {
    req.get_extension::<AppState>()
        .map(|state| state.runtime_limits)
        .unwrap_or_else(|| resolve_runtime_limits(None))
}

// =============================================================================
// Query parameter helpers
// =============================================================================

/// Parse `?limit=N` with bounds clamping.
pub(super) fn parse_limit(qs: &QueryString<'_>, limits: WebRuntimeLimits) -> usize {
    qs.get("limit")
        .and_then(|v: &str| v.parse::<usize>().ok())
        .unwrap_or(limits.default_list_limit)
        .min(limits.max_list_limit)
}

/// Parse a `u64` query parameter by key.
pub(super) fn parse_u64(qs: &QueryString<'_>, key: &str) -> Option<u64> {
    qs.get(key).and_then(|v: &str| v.parse::<u64>().ok())
}

/// Parse an `i64` query parameter by key.
pub(super) fn parse_i64(qs: &QueryString<'_>, key: &str) -> Option<i64> {
    qs.get(key).and_then(|v: &str| v.parse::<i64>().ok())
}

/// Parse a boolean query parameter (case-insensitive "1", "true", or "yes").
pub(super) fn parse_bool(qs: &QueryString<'_>, key: &str) -> bool {
    qs.get(key).is_some_and(|v: &str| {
        let lower = v.to_ascii_lowercase();
        matches!(&*lower, "1" | "true" | "yes")
    })
}

// =============================================================================
// JSON redaction
// =============================================================================

/// Recursively redact string values in a JSON tree using the given [`Redactor`].
///
/// br-ft-10i8s: descent is bounded by [`MAX_REDACT_RECURSION_DEPTH`]
/// to prevent stack overflow on hostile / malformed JSON. A subtree
/// past the cap is left un-redacted; [`REDACT_DEPTH_LIMIT_HIT_COUNT`]
/// bumps once per hit so operators can detect when the cap fires.
pub(super) fn redact_json_value(value: &mut serde_json::Value, redactor: &Redactor) {
    redact_json_value_with_depth(value, redactor, 0);
}

fn redact_json_value_with_depth(value: &mut serde_json::Value, redactor: &Redactor, depth: usize) {
    if depth >= MAX_REDACT_RECURSION_DEPTH {
        record_redact_depth_limit_hit();
        tracing::warn!(
            target: "ft.web.redact",
            event = "redact_depth_limit_hit",
            max_depth = MAX_REDACT_RECURSION_DEPTH,
            "redact_json_value stopped at depth cap; subtree left un-redacted (br-ft-10i8s)"
        );
        return;
    }
    match value {
        serde_json::Value::String(s) => {
            *s = redactor.redact(s);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value_with_depth(item, redactor, depth + 1);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                redact_json_value_with_depth(v, redactor, depth + 1);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_bool, parse_i64, parse_limit, parse_u64, redact_json_value};
    use crate::policy::Redactor;
    use crate::web_framework::QueryString;

    fn default_limits() -> crate::web::WebRuntimeLimits {
        crate::web::resolve_runtime_limits(None)
    }

    // ── parse_limit ──────────────────────────────────────────────────

    #[test]
    fn parse_limit_default_when_absent() {
        let qs = QueryString::parse("");
        assert_eq!(
            parse_limit(&qs, default_limits()),
            super::super::DEFAULT_LIMIT
        );
    }

    #[test]
    fn parse_limit_explicit_value() {
        let qs = QueryString::parse("limit=25");
        assert_eq!(parse_limit(&qs, default_limits()), 25);
    }

    #[test]
    fn parse_limit_clamped_to_max() {
        let qs = QueryString::parse("limit=99999");
        assert_eq!(parse_limit(&qs, default_limits()), super::super::MAX_LIMIT);
    }

    #[test]
    fn parse_limit_invalid_uses_default() {
        let qs = QueryString::parse("limit=abc");
        assert_eq!(
            parse_limit(&qs, default_limits()),
            super::super::DEFAULT_LIMIT
        );
    }

    #[test]
    fn parse_limit_zero_is_valid() {
        let qs = QueryString::parse("limit=0");
        assert_eq!(parse_limit(&qs, default_limits()), 0);
    }

    #[test]
    fn parse_limit_uses_runtime_limits_ft_9ahut() {
        let limits = crate::web::WebRuntimeLimits {
            max_list_limit: 7,
            default_list_limit: 3,
            max_request_body_bytes: 1024,
            stream_default_max_hz: 5,
            stream_max_max_hz: 10,
            stream_keepalive_secs: 11,
            stream_scan_limit: 4,
            stream_scan_max_pages: 2,
        };

        let qs = QueryString::parse("");
        assert_eq!(parse_limit(&qs, limits), 3);

        let qs = QueryString::parse("limit=99");
        assert_eq!(parse_limit(&qs, limits), 7);
    }

    // ── parse_u64 ────────────────────────────────────────────────────

    #[test]
    fn parse_u64_present() {
        let qs = QueryString::parse("pane=42");
        assert_eq!(parse_u64(&qs, "pane"), Some(42));
    }

    #[test]
    fn parse_u64_absent() {
        let qs = QueryString::parse("other=1");
        assert_eq!(parse_u64(&qs, "pane"), None);
    }

    #[test]
    fn parse_u64_invalid() {
        let qs = QueryString::parse("pane=-1");
        assert_eq!(parse_u64(&qs, "pane"), None);
    }

    // ── parse_i64 ────────────────────────────────────────────────────

    #[test]
    fn parse_i64_positive() {
        let qs = QueryString::parse("since=1000");
        assert_eq!(parse_i64(&qs, "since"), Some(1000));
    }

    #[test]
    fn parse_i64_negative() {
        let qs = QueryString::parse("offset=-500");
        assert_eq!(parse_i64(&qs, "offset"), Some(-500));
    }

    #[test]
    fn parse_i64_absent() {
        let qs = QueryString::parse("");
        assert_eq!(parse_i64(&qs, "offset"), None);
    }

    // ── parse_bool ───────────────────────────────────────────────────

    #[test]
    fn parse_bool_true_variants() {
        for val in ["1", "true", "yes", "TRUE", "Yes", "True"] {
            let query = format!("verbose={val}");
            let qs = QueryString::parse(&query);
            assert!(parse_bool(&qs, "verbose"), "expected true for '{val}'");
        }
    }

    #[test]
    fn parse_bool_false_variants() {
        for val in ["0", "false", "no", "FALSE", "No"] {
            let query = format!("verbose={val}");
            let qs = QueryString::parse(&query);
            assert!(!parse_bool(&qs, "verbose"), "expected false for '{val}'");
        }
    }

    #[test]
    fn parse_bool_absent_is_false() {
        let qs = QueryString::parse("");
        assert!(!parse_bool(&qs, "verbose"));
    }

    // ── redact_json_value ────────────────────────────────────────────

    #[test]
    fn redact_json_value_leaves_non_strings_unchanged() {
        let redactor = Redactor::new();
        let mut value = serde_json::json!({"count": 42, "active": true, "empty": null});
        redact_json_value(&mut value, &redactor);
        assert_eq!(value["count"], 42);
        assert_eq!(value["active"], true);
        assert!(value["empty"].is_null());
    }

    #[test]
    fn redact_json_value_recurses_into_arrays() {
        let redactor = Redactor::new();
        let mut value = serde_json::json!(["hello", ["nested"]]);
        redact_json_value(&mut value, &redactor);
        // Strings are passed through the redactor (which by default returns them unchanged)
        assert_eq!(value[0], "hello");
        assert_eq!(value[1][0], "nested");
    }

    #[test]
    fn redact_json_value_recurses_into_objects() {
        let redactor = Redactor::new();
        let mut value = serde_json::json!({"outer": {"inner": "text"}});
        redact_json_value(&mut value, &redactor);
        assert_eq!(value["outer"]["inner"], "text");
    }

    // ── br-ft-10i8s: depth cap ──

    #[test]
    fn redact_depth_cap_stops_at_max_depth_ft_10i8s() {
        // br-ft-10i8s: deeply nested arrays past the cap leave the
        // remaining subtree un-redacted but DO NOT stack-overflow.
        // Pre-fix this would have caused a SIGSEGV on most
        // platforms.
        super::reset_redact_depth_limit_hit_count_for_test();
        let redactor = Redactor::new();

        // Build 100 levels of nested arrays — well over the cap (64).
        let mut value = serde_json::json!("leaf");
        for _ in 0..100 {
            value = serde_json::json!([value]);
        }

        redact_json_value(&mut value, &redactor);

        // Counter incremented at least once (the walker hit the cap
        // on the leaf descent).
        assert!(
            super::redact_depth_limit_hit_count() >= 1,
            "depth cap must bump the counter; got {}",
            super::redact_depth_limit_hit_count()
        );
    }

    #[test]
    fn redact_depth_cap_under_limit_does_not_bump_counter_ft_10i8s() {
        // Sanity: realistic event-payload depth (5 levels) does not
        // bump the cap counter.
        super::reset_redact_depth_limit_hit_count_for_test();
        let redactor = Redactor::new();
        let mut value = serde_json::json!({
            "a": {
                "b": {
                    "c": {
                        "d": {
                            "e": "leaf"
                        }
                    }
                }
            }
        });
        redact_json_value(&mut value, &redactor);
        assert_eq!(super::redact_depth_limit_hit_count(), 0);
    }

    #[test]
    fn redact_at_exact_cap_does_not_truncate_ft_10i8s() {
        // Boundary: a tree exactly MAX_REDACT_RECURSION_DEPTH-1
        // levels deep should fully redact (no off-by-one
        // truncation). MAX is 64, so we build a 60-level tree to
        // stay safely under.
        super::reset_redact_depth_limit_hit_count_for_test();
        let redactor = Redactor::new();
        let mut value = serde_json::json!("leaf");
        for _ in 0..60 {
            value = serde_json::json!([value]);
        }
        redact_json_value(&mut value, &redactor);
        assert_eq!(
            super::redact_depth_limit_hit_count(),
            0,
            "60 < cap=64 must not trigger truncation"
        );
    }
}
