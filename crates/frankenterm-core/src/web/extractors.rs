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
