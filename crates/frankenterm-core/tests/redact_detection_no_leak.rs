//! INV-RED-1 — detection `matched_text` / `extracted` must be redacted before a
//! `Detection` is persisted or emitted on the event bus.
//!
//! Gauntlet FND-010 (frankenterm__gauntlet_workspace, Round 1): `matched_text` is
//! the full regex match span (`patterns.rs` `m.as_str()`), so a rule whose pattern
//! reaches past its anchor can capture adjacent secret bytes. The local *persist*
//! path already redacted (`detection_to_stored_event`), but the local event-bus
//! *emit* published the raw in-memory detection, and the distributed ingest path
//! persisted + emitted raw — three leak vectors to live subscribers (web SSE),
//! `ft robot events` read-out (via the stored row), and distributed aggregators.
//!
//! The fix routes every persist/emit through the shared `runtime::redact_detection`
//! choke point. This test pins that helper: (1) it delegates to the canonical
//! `Redactor` (contract), (2) it removes a planted secret from `matched_text` and
//! from nested `extracted` string leaves (non-vacuity), and (3) it is identity for
//! clean text (no over-redaction surprise).

use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::redactor::Redactor;
use frankenterm_core::runtime::redact_detection;

fn detection_with(matched: &str, extracted: serde_json::Value) -> Detection {
    Detection {
        rule_id: "test.rule".to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type: "error".to_string(),
        severity: Severity::Warning,
        confidence: 1.0,
        extracted,
        matched_text: matched.to_string(),
        span: (0, 0),
    }
}

#[test]
fn redact_detection_removes_planted_secret_from_matched_text() {
    // OpenAI-style key: matches `sk-(?:proj-|...)?[a-zA-Z0-9_-]{20,}`.
    let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz012345";
    let raw = format!("Error: leaked key {secret} in agent output");
    let d = detection_with(&raw, serde_json::json!({}));
    let r = redact_detection(&d);
    assert!(
        !r.matched_text.contains(secret),
        "raw secret must not survive in matched_text: {}",
        r.matched_text
    );
    assert!(
        r.matched_text.contains("[REDACTED"),
        "expected a redaction marker, got: {}",
        r.matched_text
    );
    // Non-content fields are preserved.
    assert_eq!(r.rule_id, d.rule_id);
    assert_eq!(r.event_type, d.event_type);
}

#[test]
fn redact_detection_redacts_nested_extracted_string_leaves() {
    // OpenAI-style key (same pattern proven to redact in the matched_text test).
    let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz012345";
    let d = detection_with(
        "clean anchor",
        serde_json::json!({
            "token": secret,
            "nested": { "k": format!("value {secret}") },
            "num": 42
        }),
    );
    let r = redact_detection(&d);
    let serialized = serde_json::to_string(&r.extracted).expect("serialize extracted");
    assert!(
        !serialized.contains(secret),
        "raw secret must not survive in nested extracted leaves: {serialized}"
    );
    // Non-string leaves are untouched.
    assert_eq!(r.extracted.get("num").and_then(|v| v.as_i64()), Some(42));
}

#[test]
fn redact_detection_matches_canonical_redactor_contract() {
    // Delegation contract: redact_detection's matched_text == Redactor::redact().
    // Also non-vacuous: this input contains a redactable key, so both sides change.
    let raw = "leaked sk-proj-abcdefghijklmnopqrstuvwxyz012345 in trace";
    let d = detection_with(raw, serde_json::json!({}));
    let r = redact_detection(&d);
    let canonical = Redactor::new().redact(raw);
    assert_eq!(
        r.matched_text, canonical,
        "must delegate to the canonical Redactor"
    );
    assert_ne!(
        r.matched_text, raw,
        "delegation must be non-vacuous on a redactable input"
    );
}

#[test]
fn redact_detection_is_identity_for_non_secret_text() {
    // Non-vacuity floor in the other direction: clean text is unchanged.
    let clean = "compilation failed: type mismatch at line 42";
    let d = detection_with(clean, serde_json::json!({ "file": "main.rs" }));
    let r = redact_detection(&d);
    assert_eq!(r.matched_text, clean);
    assert_eq!(
        r.extracted.get("file").and_then(|v| v.as_str()),
        Some("main.rs")
    );
}
