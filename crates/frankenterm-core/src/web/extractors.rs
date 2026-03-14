//! Request extractors and query parameter helpers for Wave 4B migration.
//!
//! Provides typed extractors for pulling shared resources (storage, event bus,
//! redactor) from request extensions, plus helpers for parsing common query
//! string parameters.

use super::error::json_err;
use super::middleware::AppState;
use super::{DEFAULT_LIMIT, MAX_LIMIT, QueryString, Request, Response, StatusCode};
use crate::events::EventBus;
use crate::policy::Redactor;
use crate::storage::StorageHandle;
use std::sync::Arc;

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

// =============================================================================
// Query parameter helpers
// =============================================================================

/// Parse `?limit=N` with bounds clamping.
pub(super) fn parse_limit(qs: &QueryString<'_>) -> usize {
    qs.get("limit")
        .and_then(|v: &str| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT)
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
pub(super) fn redact_json_value(value: &mut serde_json::Value, redactor: &Redactor) {
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

#[cfg(test)]
mod tests {
    use super::{parse_bool, parse_i64, parse_limit, parse_u64, redact_json_value};
    use crate::policy::Redactor;
    use crate::web_framework::QueryString;

    // ── parse_limit ──────────────────────────────────────────────────

    #[test]
    fn parse_limit_default_when_absent() {
        let qs = QueryString::parse("");
        assert_eq!(parse_limit(&qs), super::DEFAULT_LIMIT);
    }

    #[test]
    fn parse_limit_explicit_value() {
        let qs = QueryString::parse("limit=25");
        assert_eq!(parse_limit(&qs), 25);
    }

    #[test]
    fn parse_limit_clamped_to_max() {
        let qs = QueryString::parse("limit=99999");
        assert_eq!(parse_limit(&qs), super::MAX_LIMIT);
    }

    #[test]
    fn parse_limit_invalid_uses_default() {
        let qs = QueryString::parse("limit=abc");
        assert_eq!(parse_limit(&qs), super::DEFAULT_LIMIT);
    }

    #[test]
    fn parse_limit_zero_is_valid() {
        let qs = QueryString::parse("limit=0");
        assert_eq!(parse_limit(&qs), 0);
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
            let qs = QueryString::parse(&format!("verbose={val}"));
            assert!(parse_bool(&qs, "verbose"), "expected true for '{val}'");
        }
    }

    #[test]
    fn parse_bool_false_variants() {
        for val in ["0", "false", "no", "FALSE", "No"] {
            let qs = QueryString::parse(&format!("verbose={val}"));
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
}
