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
/// The root is depth zero. At the cap, replace the entire subtree with an
/// explicit redaction marker: stopping traversal must never pass secrets
/// through unchanged. Dispose of that subtree iteratively to retain the stack
/// bound even for programmatically constructed trees beyond the parser limit.
pub const MAX_REDACT_RECURSION_DEPTH: usize = 64;

const REDACT_DEPTH_LIMIT_MARKER: &str = "[REDACTED: depth limit]";
const REDACT_OBJECT_KEY_MARKER: &str = "[REDACTED: object key]";
const REDACT_BASE64_LIMIT_MARKER: &str = "[REDACTED: encoded inspection limit]";
const REDACT_BASE64_JSON_MARKER: &str = "[REDACTED: encoded JSON inspection limit]";

/// br-ft-10i8s: cumulative count of times [`redact_json_value`] hit
/// the depth cap and stopped descending. A non-zero value means a
/// hostile or malformed JSON payload tried to drive the walker past
/// the safe recursion limit; its remaining subtree was replaced
/// by a redaction marker. Same observability-counter pattern as
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
fn record_redact_depth_limit_hits(hits: u64) {
    let _ =
        REDACT_DEPTH_LIMIT_HIT_COUNT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            Some(count.saturating_add(hits))
        });
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

/// Largest base64 string this module will decode while looking for secrets.
///
/// Preserve the entire supported IPC envelope, including operator-configured
/// limits above the 128 KiB default. The old 16 KiB inspection shortcut leaked
/// larger encoded secrets; replacing everything above that shortcut would
/// instead destroy legitimate screenshots and capture data. Only values above
/// the absolute IPC message ceiling get an explicit encoded omission marker.
/// This per-value bound is not an aggregate event/response memory budget;
/// upstream ownership and aggregate bounds are tracked by ft-xxfwy.55.22.
const MAX_BASE64_REDACT_INPUT_BYTES: usize = crate::tuning_config::IpcTuning::MAX_MAX_MESSAGE_SIZE;
// Nested base64 expands by 4/3: charging each encoded layer against four
// external envelopes bounds aggregate decoding work without resetting the
// allowance for siblings or for recursively encoded JSON.
const MAX_BASE64_INSPECTION_BYTES: usize = MAX_BASE64_REDACT_INPUT_BYTES * 4;

fn encoded_json_marker(marker: &str) -> String {
    use base64::Engine as _;
    // Callers supply only the fixed, quote-free markers above. Keep the decoded
    // omission a valid JSON string, as well as preserving base64 decodability.
    base64::engine::general_purpose::STANDARD.encode(format!("\"{marker}\""))
}

/// If `candidate` is base64 whose decoded text carries a secret, return the
/// re-encoded redacted text. An over-budget base64-looking string gets an
/// encoded omission marker; an unrecognized or safely inspected string is None.
///
/// The field keeps its shape -- still base64, still decodable by the same
/// consumer -- but what it decodes to is the redacted view. Within the budget,
/// non-UTF-8 binary data and text with nothing to redact are left unchanged.
fn redact_base64_payload(
    candidate: &str,
    redactor: &Redactor,
    depth: usize,
    omissions: &mut RedactionOmissions,
) -> Option<String> {
    use base64::Engine as _;

    if candidate.len() < 8 {
        return None;
    }
    // Cheap rejection before any allocation: standard base64 is a multiple of
    // four characters drawn from a fixed alphabet.
    if !candidate.len().is_multiple_of(4) {
        return None;
    }
    if !candidate
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return None;
    }
    if candidate.len() > MAX_BASE64_REDACT_INPUT_BYTES
        || candidate.len() > MAX_BASE64_INSPECTION_BYTES - omissions.inspected_base64_bytes
    {
        omissions.encoded_limit_hits = omissions.encoded_limit_hits.saturating_add(1);
        return Some(encoded_json_marker(REDACT_BASE64_LIMIT_MARKER));
    }
    omissions.inspected_base64_bytes += candidate.len();

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(candidate)
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(mut decoded_json) => {
            // A JSON consumer resolves Unicode escapes before interpreting
            // secrets. Inspect that same representation, with the SAME depth
            // and work allowance; entering another encoding is not a new root.
            if redact_json_value_with_depth(&mut decoded_json, redactor, depth + 1, omissions) {
                let serialized = match serde_json::to_vec(&decoded_json) {
                    Ok(serialized) => serialized,
                    Err(_) => {
                        omissions.encoded_json_omissions =
                            omissions.encoded_json_omissions.saturating_add(1);
                        return Some(encoded_json_marker(REDACT_BASE64_JSON_MARKER));
                    }
                };
                return Some(base64::engine::general_purpose::STANDARD.encode(serialized));
            }
        }
        Err(_) if serde_json::from_str::<serde::de::IgnoredAny>(&text).is_ok() => {
            // Value's bounded parser can reject valid, deeply nested JSON that
            // a browser would accept. IgnoredAny validates iteratively in the
            // pinned serde_json implementation, without constructing that tree.
            // Never fall back to raw-text matching for such an opaque document.
            omissions.encoded_json_omissions = omissions.encoded_json_omissions.saturating_add(1);
            return Some(encoded_json_marker(REDACT_BASE64_JSON_MARKER));
        }
        Err(_) => {}
    }
    // Preserve safe encoded JSON byte-for-byte, including whitespace/escapes.
    // Non-JSON text retains the existing raw-text policy; binary returned above.
    let redacted = redactor.redact(&text);
    if redacted == text {
        return None;
    }
    Some(base64::engine::general_purpose::STANDARD.encode(redacted))
}

/// Redact JSON strings and omit objects with secret-bearing keys.
///
/// br-ft-10i8s: descent is bounded by [`MAX_REDACT_RECURSION_DEPTH`]
/// to prevent stack overflow on hostile / malformed JSON. A subtree at the
/// cap is replaced, never passed through un-redacted;
/// [`REDACT_DEPTH_LIMIT_HIT_COUNT`] bumps once per replaced subtree.
pub(super) fn redact_json_value(value: &mut serde_json::Value, redactor: &Redactor) {
    let mut omissions = RedactionOmissions::default();
    redact_json_value_with_depth(value, redactor, 0, &mut omissions);
    if omissions.depth_hits > 0
        || omissions.secret_key_objects > 0
        || omissions.encoded_limit_hits > 0
        || omissions.encoded_json_omissions > 0
    {
        record_redact_depth_limit_hits(omissions.depth_hits);
        // One bounded warning per invocation, not one per hostile child.
        tracing::warn!(
            target: "ft.web.redact",
            event = "redact_subtrees_omitted",
            max_depth = MAX_REDACT_RECURSION_DEPTH,
            depth_hits = omissions.depth_hits,
            secret_key_objects = omissions.secret_key_objects,
            encoded_limit_hits = omissions.encoded_limit_hits,
            encoded_json_omissions = omissions.encoded_json_omissions,
            "JSON subtrees omitted by web redaction"
        );
    }
}

#[derive(Default)]
struct RedactionOmissions {
    depth_hits: u64,
    secret_key_objects: u64,
    encoded_limit_hits: u64,
    encoded_json_omissions: u64,
    inspected_base64_bytes: usize,
}

fn replace_json_subtree(value: &mut serde_json::Value, marker: &str) {
    // Assignment alone would recursively drop the old Value and reintroduce
    // the overflow the traversal bound prevents. Consume owned children.
    let omitted = std::mem::replace(value, serde_json::Value::String(marker.to_owned()));
    let mut pending = vec![omitted];
    while let Some(omitted) = pending.pop() {
        match omitted {
            serde_json::Value::Array(mut children) => pending.append(&mut children),
            serde_json::Value::Object(fields) => pending.extend(fields.into_values()),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }
}

fn redact_json_value_with_depth(
    value: &mut serde_json::Value,
    redactor: &Redactor,
    depth: usize,
    omissions: &mut RedactionOmissions,
) -> bool {
    if depth >= MAX_REDACT_RECURSION_DEPTH {
        replace_json_subtree(value, REDACT_DEPTH_LIMIT_MARKER);
        omissions.depth_hits = omissions.depth_hits.saturating_add(1);
        return true;
    }
    match value {
        serde_json::Value::String(s) => {
            let redacted = redactor.redact(s);
            if redacted == *s {
                // The pattern matcher only sees the encoded form of a base64
                // field, so a secret inside one is served verbatim: a reader
                // decodes it and the redaction never happened. A user-var
                // event carries exactly this shape -- `event_data` redacted
                // beside a raw `value` that decodes to the same secret
                // (ft-xxfwy.19 probe).
                if let Some(reencoded) = redact_base64_payload(s, redactor, depth, omissions) {
                    *s = reencoded;
                    true
                } else {
                    false
                }
            } else {
                *s = redacted;
                true
            }
        }
        serde_json::Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= redact_json_value_with_depth(item, redactor, depth + 1, omissions);
            }
            changed
        }
        serde_json::Value::Object(map) => {
            // Keys are output too. Renaming secret keys could collide and
            // silently merge unrelated fields, so omit the containing object
            // explicitly instead. Its siblings remain intact.
            if map.keys().any(|key| {
                redactor.redact(key) != *key
                    || redact_base64_payload(key, redactor, depth + 1, omissions).is_some()
            }) {
                replace_json_subtree(value, REDACT_OBJECT_KEY_MARKER);
                omissions.secret_key_objects = omissions.secret_key_objects.saturating_add(1);
                return true;
            }
            let mut changed = false;
            for v in map.values_mut() {
                changed |= redact_json_value_with_depth(v, redactor, depth + 1, omissions);
            }
            changed
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
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

    // ── ft-xxfwy.19: base64 fields cannot smuggle a secret past redaction ──

    #[test]
    fn a_secret_inside_a_base64_field_is_redacted_in_place() {
        use base64::Engine as _;

        // The shape a user-var event actually streams: the decoded view beside
        // the raw encoded value it came from.
        let plaintext =
            r#"{"kind":"note","message":"a token sk-livetest1234567890abcdefghij trails here"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(plaintext);
        let redactor = Redactor::new();
        let mut value = serde_json::json!({
            "payload": {
                "value": encoded,
                "event_data": serde_json::from_str::<serde_json::Value>(plaintext).unwrap(),
            }
        });

        redact_json_value(&mut value, &redactor);

        let served = value["payload"]["value"]
            .as_str()
            .expect("value stays a string");
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(served)
                .expect("the field is still base64 a consumer can decode"),
        )
        .expect("still UTF-8");
        assert!(
            !decoded.contains("sk-livetest1234567890abcdefghij"),
            "a reader who decodes the field must not get the secret back: {decoded}"
        );
        assert!(
            !value["payload"]["event_data"]["message"]
                .as_str()
                .unwrap()
                .contains("sk-livetest1234567890abcdefghij"),
            "the decoded view must stay redacted too"
        );
    }

    #[test]
    fn base64_without_a_secret_is_left_exactly_as_it_was() {
        use base64::Engine as _;

        let encoded = base64::engine::general_purpose::STANDARD.encode(r#"{"kind":"note"}"#);
        let redactor = Redactor::new();
        let mut value = serde_json::json!({ "value": encoded.clone() });
        redact_json_value(&mut value, &redactor);
        assert_eq!(value["value"], encoded);
    }

    #[test]
    fn base64_lookalikes_and_binary_blobs_are_not_decoded() {
        let redactor = Redactor::new();
        // Not base64: wrong length, and characters outside the alphabet.
        for candidate in ["hello world", "abc", "not-base64-!!", "AAAA AAAA"] {
            let mut value = serde_json::json!({ "field": candidate });
            redact_json_value(&mut value, &redactor);
            assert_eq!(value["field"], candidate, "{candidate} must pass through");
        }
        // Valid base64 of non-UTF-8 bytes is left alone rather than mangled.
        use base64::Engine as _;
        let binary = base64::engine::general_purpose::STANDARD.encode([0xff_u8, 0xfe, 0xfd, 0xfc]);
        let mut value = serde_json::json!({ "field": binary.clone() });
        redact_json_value(&mut value, &redactor);
        assert_eq!(value["field"], binary);
    }

    #[test]
    fn base64_inspection_limit_never_passes_opaque_secrets_through() {
        use base64::Engine as _;
        let redactor = Redactor::new();
        let canary = "sk-livetest1234567890abcdefghij";
        // Exercise the old shortcut and both sides of the actual production
        // ceiling. No test-only budget substitutes for the shipped boundary.
        let max_encoded = super::MAX_BASE64_REDACT_INPUT_BYTES;
        assert_eq!(
            max_encoded,
            crate::tuning_config::IpcTuning::MAX_MAX_MESSAGE_SIZE
        );
        assert!(max_encoded > crate::ipc::MAX_MESSAGE_SIZE);
        for decoded_len in [12_288, 12_289, max_encoded / 4 * 3, max_encoded / 4 * 3 + 1] {
            let plaintext = format!("{canary} {}", " ".repeat(decoded_len - canary.len() - 1));
            let encoded = base64::engine::general_purpose::STANDARD.encode(plaintext);
            let encoded_len = encoded.len();
            assert_eq!(encoded_len, decoded_len.div_ceil(3) * 4);
            let mut value = serde_json::Value::String(encoded);
            redact_json_value(&mut value, &redactor);
            let served = base64::engine::general_purpose::STANDARD
                .decode(value.as_str().expect("encoded result remains a string"))
                .expect("served result remains canonical base64");
            let served = String::from_utf8(served).expect("text payload");
            assert!(!served.contains(canary), "encoded canary must not escape");
            if encoded_len > max_encoded {
                assert_eq!(
                    serde_json::from_str::<String>(&served).expect("valid JSON marker"),
                    "[REDACTED: encoded inspection limit]"
                );
            } else {
                assert_eq!(
                    served,
                    format!("[REDACTED] {}", " ".repeat(decoded_len - canary.len() - 1))
                );
            }
        }
    }

    #[test]
    fn large_safe_base64_text_and_binary_are_preserved_exactly() {
        use base64::Engine as _;

        let redactor = Redactor::new();
        // Above both the old 16 KiB shortcut and the default 128 KiB IPC
        // envelope: configured larger payloads must retain their data too.
        for payload in [vec![b' '; 192 * 1024], vec![0xff; 192 * 1024]] {
            let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
            assert!(encoded.len() > crate::ipc::MAX_MESSAGE_SIZE);
            assert!(encoded.len() < super::MAX_BASE64_REDACT_INPUT_BYTES);
            let mut value = serde_json::Value::String(encoded.clone());
            redact_json_value(&mut value, &redactor);
            assert_eq!(value.as_str(), Some(encoded.as_str()));
        }
    }

    #[test]
    fn encoded_json_unicode_values_and_keys_cannot_bypass_redaction() {
        use base64::Engine as _;

        let redactor = Redactor::new();
        let canary = "sk-livetest1234567890abcdefghij";
        for (source, key_case) in [
            (
                r#"{"safe":"keep","secret":"\u0073\u006b-livetest1234567890abcdefghij"}"#,
                false,
            ),
            (
                r#"{"\u0073\u006b-livetest1234567890abcdefghij":"keep"}"#,
                true,
            ),
        ] {
            assert!(
                !source.contains(canary),
                "raw-text matching cannot see the token"
            );
            let parsed: serde_json::Value =
                serde_json::from_str(source).expect("valid JSON source");
            assert!(
                serde_json::to_string(&parsed)
                    .expect("JSON")
                    .contains(canary)
            );
            let mut value =
                serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(source));
            redact_json_value(&mut value, &redactor);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(value.as_str().expect("encoded string"))
                .expect("base64");
            let served: serde_json::Value = serde_json::from_slice(&decoded).expect("served JSON");
            assert!(
                !serde_json::to_string(&served)
                    .expect("JSON")
                    .contains(canary)
            );
            if key_case {
                assert_eq!(served, "[REDACTED: object key]");
            } else {
                assert_eq!(served["safe"], "keep");
                assert_eq!(served["secret"], "[REDACTED]");
            }
        }
    }

    #[test]
    fn safe_encoded_json_escapes_whitespace_and_non_json_text_are_preserved() {
        use base64::Engine as _;

        for source in [r#"{ "safe" : "\u0061", "n" : 3 }"#, "[notes] ordinary text"] {
            let encoded = base64::engine::general_purpose::STANDARD.encode(source);
            let mut value = serde_json::Value::String(encoded.clone());
            redact_json_value(&mut value, &Redactor::new());
            assert_eq!(value.as_str(), Some(encoded.as_str()));
        }
    }

    #[test]
    fn valid_encoded_json_beyond_parser_depth_is_not_raw_text_fallback() {
        use base64::Engine as _;

        let source = format!(
            "{}{}{}",
            "[".repeat(200),
            r#""\u0073\u006b-livetest1234567890abcdefghij""#,
            "]".repeat(200)
        );
        assert!(serde_json::from_str::<serde_json::Value>(&source).is_err());
        assert!(serde_json::from_str::<serde::de::IgnoredAny>(&source).is_ok());
        let mut value =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(source));
        redact_json_value(&mut value, &Redactor::new());
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value.as_str().expect("encoded string"))
            .expect("base64");
        assert_eq!(
            serde_json::from_slice::<String>(&decoded).expect("valid JSON marker"),
            "[REDACTED: encoded JSON inspection limit]"
        );
    }

    #[test]
    fn encoded_json_layers_share_logical_depth_and_emit_valid_json() {
        use base64::Engine as _;

        let _serialized = lock_depth_counter();
        let canary_json = r#""sk-livetest1234567890abcdefghij""#;
        for depth in [62, 63] {
            super::reset_redact_depth_limit_hit_count_for_test();
            let mut value = serde_json::Value::String(
                base64::engine::general_purpose::STANDARD.encode(canary_json),
            );
            for _ in 0..depth {
                value = serde_json::Value::Array(vec![value]);
            }
            redact_json_value(&mut value, &Redactor::new());
            let mut leaf = &value;
            for _ in 0..depth {
                leaf = &leaf[0];
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(leaf.as_str().expect("encoded string"))
                .expect("base64");
            assert_eq!(
                serde_json::from_slice::<String>(&decoded).expect("valid JSON"),
                if depth == 62 {
                    "[REDACTED]"
                } else {
                    "[REDACTED: depth limit]"
                }
            );
            assert_eq!(
                super::redact_depth_limit_hit_count(),
                u64::from(depth == 63)
            );
        }
    }

    #[test]
    fn encoded_json_siblings_share_the_inspection_byte_allowance() {
        use base64::Engine as _;

        let candidate = base64::engine::general_purpose::STANDARD
            .encode(r#""sk-livetest1234567890abcdefghij""#);
        let mut state = super::RedactionOmissions {
            inspected_base64_bytes: super::MAX_BASE64_INSPECTION_BYTES - candidate.len(),
            ..Default::default()
        };
        let first = super::redact_base64_payload(&candidate, &Redactor::new(), 0, &mut state)
            .expect("first redaction");
        assert_eq!(
            serde_json::from_slice::<String>(
                &base64::engine::general_purpose::STANDARD
                    .decode(first)
                    .expect("base64")
            )
            .expect("JSON"),
            "[REDACTED]"
        );
        assert_eq!(
            state.inspected_base64_bytes,
            super::MAX_BASE64_INSPECTION_BYTES
        );
        let second = super::redact_base64_payload(&candidate, &Redactor::new(), 0, &mut state)
            .expect("second is omitted");
        assert_eq!(
            serde_json::from_slice::<String>(
                &base64::engine::general_purpose::STANDARD
                    .decode(second)
                    .expect("base64")
            )
            .expect("valid JSON marker"),
            "[REDACTED: encoded inspection limit]"
        );
        assert_eq!(state.encoded_limit_hits, 1);
    }

    #[test]
    fn secret_json_keys_are_omitted_without_key_collisions() {
        let _serialized = lock_depth_counter();
        let redactor = Redactor::new();
        let canary = "sk-livetest1234567890abcdefghij";
        for depth in [0, 63] {
            let mut fields = serde_json::Map::new();
            fields.insert(canary.to_owned(), serde_json::Value::Null);
            fields.insert("[REDACTED]".to_owned(), serde_json::json!("must not merge"));
            let mut value = serde_json::Value::Object(fields);
            for _ in 0..depth {
                value = serde_json::Value::Array(vec![value]);
            }
            redact_json_value(&mut value, &redactor);
            let serialized = serde_json::to_string(&value).expect("bounded JSON");
            assert!(!serialized.contains(canary));
            assert!(serialized.contains("[REDACTED: object key]"));
            assert!(!serialized.contains("must not merge"));
        }
    }

    #[test]
    fn wide_depth_cutoff_counts_subtrees_once_per_invocation() {
        #[derive(Clone)]
        struct LogBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for LogBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("log buffer lock")
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let _serialized = lock_depth_counter();
        super::reset_redact_depth_limit_hit_count_for_test();
        let canary = "sk-livetest1234567890abcdefghij";
        let mut value =
            serde_json::Value::Array(vec![serde_json::Value::String(canary.to_owned()); 1024]);
        for _ in 0..63 {
            value = serde_json::Value::Array(vec![value]);
        }
        let log = LogBuffer(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            redact_json_value(&mut value, &Redactor::new());
        });
        assert_eq!(super::redact_depth_limit_hit_count(), 1024);
        let serialized = serde_json::to_string(&value).expect("bounded JSON");
        assert_eq!(serialized.matches("[REDACTED: depth limit]").count(), 1024);
        assert!(!serialized.contains(canary));
        let bytes = log.0.lock().expect("log buffer lock");
        let logged = std::str::from_utf8(&bytes).expect("structured warning text");
        assert_eq!(logged.lines().count(), 1, "one warning, not 1024 warnings");
        assert!(logged.contains("redact_subtrees_omitted"));
        assert!(logged.contains("depth_hits=1024"));
        assert!(!logged.contains(canary), "warning must be content-free");
    }

    // ── br-ft-10i8s: depth cap ──

    /// The depth-cap tests reset and then read a process-wide counter, so they
    /// cannot run beside each other: one test's bump lands after another's
    /// reset and the assertion reads a stranger's count. That was latent until
    /// this module gained more tests and the scheduling changed.
    static DEPTH_COUNTER_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_depth_counter() -> std::sync::MutexGuard<'static, ()> {
        DEPTH_COUNTER_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn redact_depth_cap_stops_at_max_depth_ft_10i8s() {
        let _serialized = lock_depth_counter();
        // A stack bound must not become a redaction bypass (ft-xxfwy.55.21).
        super::reset_redact_depth_limit_hit_count_for_test();
        let redactor = Redactor::new();

        // Build 100 levels of nested arrays — well over the cap (64).
        let canary = "sk-livetest1234567890abcdefghij";
        let mut value = serde_json::Value::String(canary.to_owned());
        for _ in 0..100 {
            value = serde_json::Value::Array(vec![value]);
        }

        redact_json_value(&mut value, &redactor);

        assert_eq!(super::redact_depth_limit_hit_count(), 1);
        let serialized = serde_json::to_string(&value).expect("bounded result serializes");
        assert!(!serialized.contains(canary), "deep secret must not escape");
        assert!(serialized.contains("[REDACTED: depth limit]"));
        let mut leaf = &value;
        for _ in 0..super::MAX_REDACT_RECURSION_DEPTH {
            leaf = &leaf[0];
        }
        assert_eq!(leaf, "[REDACTED: depth limit]");
    }

    #[test]
    fn redact_depth_cap_under_limit_does_not_bump_counter_ft_10i8s() {
        // Sanity: realistic event-payload depth (5 levels) does not
        // bump the cap counter.
        let _serialized = lock_depth_counter();
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
    fn redact_just_below_cap_preserves_shape_ft_10i8s() {
        // Boundary: a tree exactly MAX_REDACT_RECURSION_DEPTH-1
        // levels deep should fully redact without truncation.
        let _serialized = lock_depth_counter();
        super::reset_redact_depth_limit_hit_count_for_test();
        let redactor = Redactor::new();
        let canary = "sk-livetest1234567890abcdefghij";
        let mut value = serde_json::Value::String(canary.to_owned());
        for _ in 0..super::MAX_REDACT_RECURSION_DEPTH - 1 {
            value = serde_json::Value::Array(vec![value]);
        }
        redact_json_value(&mut value, &redactor);
        assert_eq!(super::redact_depth_limit_hit_count(), 0);
        let serialized = serde_json::to_string(&value).expect("under-cap result serializes");
        assert!(!serialized.contains(canary));
        assert!(!serialized.contains("[REDACTED: depth limit]"));
        let mut leaf = &value;
        for _ in 0..super::MAX_REDACT_RECURSION_DEPTH - 1 {
            leaf = &leaf[0];
        }
        assert_eq!(leaf, "[REDACTED]");
    }

    #[test]
    fn redact_exact_cap_replaces_object_subtree() {
        let _serialized = lock_depth_counter();
        super::reset_redact_depth_limit_hit_count_for_test();
        let redactor = Redactor::new();
        let canary = "sk-livetest1234567890abcdefghij";
        let mut value = serde_json::json!({ "secret": canary, "ordinary": 42 });
        for _ in 0..super::MAX_REDACT_RECURSION_DEPTH {
            let mut object = serde_json::Map::new();
            object.insert("child".to_owned(), value);
            value = serde_json::Value::Object(object);
        }
        redact_json_value(&mut value, &redactor);
        assert_eq!(super::redact_depth_limit_hit_count(), 1);
        let serialized = serde_json::to_string(&value).expect("bounded result serializes");
        assert!(!serialized.contains(canary));
        let mut leaf = &value;
        for _ in 0..super::MAX_REDACT_RECURSION_DEPTH {
            leaf = &leaf["child"];
        }
        assert_eq!(leaf, "[REDACTED: depth limit]");
    }

    #[test]
    fn redact_depth_cap_disposes_deep_tree_without_recursive_drop() {
        let _serialized = lock_depth_counter();
        // An in-process event can exceed serde's parser limit. Construction
        // uses moves; json! would itself recursively walk the growing tree.
        std::thread::Builder::new()
            .name("web-redaction-depth-disposal".to_owned())
            .stack_size(512 * 1024)
            .spawn(|| {
                let mut value =
                    serde_json::Value::String("sk-livetest1234567890abcdefghij".to_owned());
                for _ in 0..20_000 {
                    value = serde_json::Value::Array(vec![value]);
                }
                redact_json_value(&mut value, &Redactor::new());
                let serialized = serde_json::to_string(&value).expect("bounded JSON");
                assert!(!serialized.contains("sk-livetest1234567890abcdefghij"));
                assert!(serialized.contains("[REDACTED: depth limit]"));
                // Both omitted children and retained result must drop safely.
                drop(value);
            })
            .expect("owned bounded-stack thread starts")
            .join()
            .expect("deep redaction and destruction complete");
    }
}
