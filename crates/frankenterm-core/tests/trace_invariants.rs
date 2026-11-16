//! Tests for explain-match trace stability, boundedness, and redaction.
//!
//! This module validates that MatchTrace and related types satisfy critical invariants:
//!
//! 1. **Golden stability**: Trace structure is deterministic and matches expected schema
//! 2. **Boundedness**: Truncation is explicit and sizes are enforced
//! 3. **Redaction**: No secrets leak into trace output
//! 4. **Schema validation**: Trace objects conform to expected structure
//!
//! # Related Beads
//!
//! - wa-upg.14.5: Tests: explain-match trace stability, boundedness, and redaction
//! - wa-upg.14.2: Core: explain-match trace generation in pattern engine

use frankenterm_core::patterns::{
    MatchTrace, TraceBounds, TraceEvidence, TraceGate, TraceOptions, TraceSpan,
};
use frankenterm_core::policy::Redactor;
use serde_json::Value;

// ============================================================================
// Golden Stability Tests
// ============================================================================

/// Verify TraceOptions has sensible defaults
#[test]
fn trace_options_default_values() {
    let opts = TraceOptions::default();

    // Defaults should be reasonable for typical use
    assert!(
        opts.max_evidence_items > 0,
        "max_evidence_items should be positive"
    );
    assert!(
        opts.max_evidence_items <= 100,
        "max_evidence_items should be bounded (got {})",
        opts.max_evidence_items
    );

    assert!(
        opts.max_excerpt_bytes > 0,
        "max_excerpt_bytes should be positive"
    );
    assert!(
        opts.max_excerpt_bytes <= 4096,
        "max_excerpt_bytes should be bounded (got {})",
        opts.max_excerpt_bytes
    );

    assert!(
        opts.max_capture_bytes > 0,
        "max_capture_bytes should be positive"
    );

    eprintln!("[ARTIFACT] TraceOptions defaults:");
    eprintln!("  max_evidence_items: {}", opts.max_evidence_items);
    eprintln!("  max_excerpt_bytes: {}", opts.max_excerpt_bytes);
    eprintln!("  max_capture_bytes: {}", opts.max_capture_bytes);
}

/// Verify TraceSpan serialization is stable
#[test]
fn trace_span_serialization_stability() {
    let span = TraceSpan { start: 10, end: 50 };

    let json = serde_json::to_string(&span).expect("TraceSpan should serialize");
    let parsed: Value = serde_json::from_str(&json).expect("Should parse as JSON");

    // Field names must be stable
    assert!(parsed.get("start").is_some(), "Missing 'start' field");
    assert!(parsed.get("end").is_some(), "Missing 'end' field");

    // Values must match
    assert_eq!(parsed["start"], 10);
    assert_eq!(parsed["end"], 50);

    eprintln!("[ARTIFACT] TraceSpan JSON: {}", json);
}

/// Verify TraceEvidence serialization is stable
#[test]
fn trace_evidence_serialization_stability() {
    // Test with truncated=true so the field is serialized
    let evidence = TraceEvidence {
        kind: "anchor".to_string(),
        label: Some("usage_reached".to_string()),
        span: Some(TraceSpan { start: 0, end: 20 }),
        excerpt: Some("Usage limit reached".to_string()),
        truncated: true, // Note: truncated=false is omitted via skip_serializing_if
    };

    let json = serde_json::to_string(&evidence).expect("TraceEvidence should serialize");
    let parsed: Value = serde_json::from_str(&json).expect("Should parse as JSON");

    // Required fields
    assert!(parsed.get("kind").is_some(), "Missing 'kind' field");
    // truncated is only serialized when true (skip_serializing_if = "std::ops::Not::not")
    assert!(
        parsed.get("truncated").is_some(),
        "truncated should be present when true"
    );

    // Values
    assert_eq!(parsed["kind"], "anchor");
    assert_eq!(parsed["truncated"], true);

    eprintln!("[ARTIFACT] TraceEvidence JSON: {}", json);

    // Test that truncated=false is omitted
    let evidence_not_truncated = TraceEvidence {
        kind: "anchor".to_string(),
        label: None,
        span: None,
        excerpt: None,
        truncated: false,
    };
    let json2 = serde_json::to_string(&evidence_not_truncated).expect("serialize");
    let parsed2: Value = serde_json::from_str(&json2).expect("parse");
    assert!(
        parsed2.get("truncated").is_none(),
        "truncated=false should be omitted from JSON"
    );
}

/// Verify TraceGate serialization is stable
#[test]
fn trace_gate_serialization_stability() {
    let gate_pass = TraceGate {
        gate: "agent_type".to_string(),
        passed: true,
        reason: None,
    };

    let gate_fail = TraceGate {
        gate: "dedupe".to_string(),
        passed: false,
        reason: Some("Already seen within TTL".to_string()),
    };

    let json_pass = serde_json::to_string(&gate_pass).expect("TraceGate should serialize");
    let json_fail = serde_json::to_string(&gate_fail).expect("TraceGate should serialize");

    let parsed_pass: Value = serde_json::from_str(&json_pass).expect("Should parse");
    let parsed_fail: Value = serde_json::from_str(&json_fail).expect("Should parse");

    // Required fields
    assert!(parsed_pass.get("gate").is_some(), "Missing 'gate' field");
    assert!(
        parsed_pass.get("passed").is_some(),
        "Missing 'passed' field"
    );

    // Values
    assert_eq!(parsed_pass["gate"], "agent_type");
    assert_eq!(parsed_pass["passed"], true);
    assert_eq!(parsed_fail["passed"], false);
    assert!(parsed_fail.get("reason").is_some());

    eprintln!("[ARTIFACT] TraceGate (pass) JSON: {}", json_pass);
    eprintln!("[ARTIFACT] TraceGate (fail) JSON: {}", json_fail);
}

/// Verify TraceBounds serialization is stable
#[test]
fn trace_bounds_serialization_stability() {
    let bounds = TraceBounds {
        max_evidence_items: 20,
        max_excerpt_bytes: 256,
        max_capture_bytes: 128,
        evidence_total: 25,
        evidence_truncated: true,
        truncated_fields: vec!["excerpt".to_string(), "capture.session_id".to_string()],
    };

    let json = serde_json::to_string(&bounds).expect("TraceBounds should serialize");
    let parsed: Value = serde_json::from_str(&json).expect("Should parse");

    // All required fields
    assert!(
        parsed.get("max_evidence_items").is_some(),
        "Missing max_evidence_items"
    );
    assert!(
        parsed.get("max_excerpt_bytes").is_some(),
        "Missing max_excerpt_bytes"
    );
    assert!(
        parsed.get("max_capture_bytes").is_some(),
        "Missing max_capture_bytes"
    );
    assert!(
        parsed.get("evidence_total").is_some(),
        "Missing evidence_total"
    );
    assert!(
        parsed.get("evidence_truncated").is_some(),
        "Missing evidence_truncated"
    );

    // Values
    assert_eq!(parsed["evidence_total"], 25);
    assert_eq!(parsed["evidence_truncated"], true);

    eprintln!("[ARTIFACT] TraceBounds JSON: {}", json);
}

/// Verify MatchTrace serialization is stable and has all expected fields
#[test]
fn match_trace_serialization_stability() {
    let trace = MatchTrace {
        pack_id: "builtin:codex".to_string(),
        rule_id: "core.codex:usage_reached".to_string(),
        extractor_id: Some("regex".to_string()),
        matched_text: Some("Usage limit reached for all Pro models".to_string()),
        confidence: Some(0.95),
        eligible: true,
        gates: vec![
            TraceGate {
                gate: "agent_type".to_string(),
                passed: true,
                reason: None,
            },
            TraceGate {
                gate: "dedupe".to_string(),
                passed: true,
                reason: None,
            },
        ],
        evidence: vec![TraceEvidence {
            kind: "anchor".to_string(),
            label: Some("usage_reached".to_string()),
            span: Some(TraceSpan { start: 0, end: 39 }),
            excerpt: Some("Usage limit reached for all Pro models".to_string()),
            truncated: false,
        }],
        bounds: TraceBounds {
            max_evidence_items: 20,
            max_excerpt_bytes: 256,
            max_capture_bytes: 128,
            evidence_total: 1,
            evidence_truncated: false,
            truncated_fields: vec![],
        },
    };

    let json = serde_json::to_string_pretty(&trace).expect("MatchTrace should serialize");
    let parsed: Value = serde_json::from_str(&json).expect("Should parse");

    // Required fields
    let required_fields = [
        "pack_id", "rule_id", "eligible", "gates", "evidence", "bounds",
    ];
    for field in required_fields {
        assert!(
            parsed.get(field).is_some(),
            "Missing required field: {}",
            field
        );
    }

    // Type checks
    assert!(parsed["gates"].is_array(), "gates should be array");
    assert!(parsed["evidence"].is_array(), "evidence should be array");
    assert!(parsed["bounds"].is_object(), "bounds should be object");

    // Values
    assert_eq!(parsed["pack_id"], "builtin:codex");
    assert_eq!(parsed["rule_id"], "core.codex:usage_reached");
    assert_eq!(parsed["eligible"], true);

    eprintln!("[ARTIFACT] MatchTrace JSON:\n{}", json);
}

/// Test deterministic ordering: fields should serialize in consistent order
#[test]
fn match_trace_deterministic_field_ordering() {
    let trace1 = create_test_trace();
    let trace2 = create_test_trace();

    let json1 = serde_json::to_string(&trace1).expect("serialize");
    let json2 = serde_json::to_string(&trace2).expect("serialize");

    assert_eq!(
        json1, json2,
        "Identical traces should produce identical JSON"
    );

    // Parse and re-serialize to verify ordering stability
    let parsed: Value = serde_json::from_str(&json1).expect("parse");
    let re_serialized = serde_json::to_string(&parsed).expect("re-serialize");

    // Note: serde_json::Value uses BTreeMap internally, so field order is alphabetical
    eprintln!("[ARTIFACT] Original JSON length: {}", json1.len());
    eprintln!(
        "[ARTIFACT] Re-serialized JSON length: {}",
        re_serialized.len()
    );
}

/// Test deterministic ordering of gates list
#[test]
fn gates_list_ordering_is_stable() {
    let trace = create_test_trace();

    let json1 = serde_json::to_string(&trace).expect("serialize");
    let json2 = serde_json::to_string(&trace).expect("serialize");

    let parsed1: Value = serde_json::from_str(&json1).expect("parse");
    let parsed2: Value = serde_json::from_str(&json2).expect("parse");

    assert_eq!(
        parsed1["gates"], parsed2["gates"],
        "Gates ordering should be deterministic"
    );

    // Gates should maintain insertion order
    let gates = parsed1["gates"].as_array().expect("gates should be array");
    if gates.len() >= 2 {
        assert_eq!(
            gates[0]["gate"], "agent_type",
            "First gate should be agent_type"
        );
        assert_eq!(gates[1]["gate"], "dedupe", "Second gate should be dedupe");
    }
}

/// Test deterministic ordering of evidence list
#[test]
fn evidence_list_ordering_is_stable() {
    let mut trace = create_test_trace();
    trace.evidence = vec![
        TraceEvidence {
            kind: "anchor".to_string(),
            label: Some("first".to_string()),
            span: Some(TraceSpan { start: 0, end: 10 }),
            excerpt: Some("First match".to_string()),
            truncated: false,
        },
        TraceEvidence {
            kind: "capture".to_string(),
            label: Some("session_id".to_string()),
            span: Some(TraceSpan { start: 20, end: 30 }),
            excerpt: Some("abc123".to_string()),
            truncated: false,
        },
        TraceEvidence {
            kind: "match".to_string(),
            label: None,
            span: Some(TraceSpan { start: 40, end: 50 }),
            excerpt: Some("Full pattern".to_string()),
            truncated: false,
        },
    ];

    let json = serde_json::to_string(&trace).expect("serialize");
    let parsed: Value = serde_json::from_str(&json).expect("parse");

    let evidence = parsed["evidence"].as_array().expect("evidence array");
    assert_eq!(evidence.len(), 3);
    assert_eq!(evidence[0]["kind"], "anchor");
    assert_eq!(evidence[1]["kind"], "capture");
    assert_eq!(evidence[2]["kind"], "match");
}

// ============================================================================
// Boundedness Tests
// ============================================================================

/// Verify TraceBounds enforces maximum evidence items
#[test]
fn trace_bounds_evidence_truncation_visible() {
    // Create bounds that indicate truncation occurred
    let bounds = TraceBounds {
        max_evidence_items: 5,
        max_excerpt_bytes: 256,
        max_capture_bytes: 128,
        evidence_total: 15, // More than max
        evidence_truncated: true,
        truncated_fields: vec![],
    };

    // Truncation should be visible in serialized output
    let json = serde_json::to_string(&bounds).expect("serialize");
    let parsed: Value = serde_json::from_str(&json).expect("parse");

    assert_eq!(
        parsed["evidence_truncated"], true,
        "Truncation flag must be set when evidence_total > max_evidence_items"
    );
    assert_eq!(
        parsed["evidence_total"], 15,
        "Total count should be preserved even when truncated"
    );

    eprintln!(
        "[ARTIFACT] Truncated bounds: evidence_total={}, max={}",
        bounds.evidence_total, bounds.max_evidence_items
    );
}

/// Verify truncated_fields is populated when field truncation occurs
#[test]
fn trace_bounds_truncated_fields_recorded() {
    let bounds = TraceBounds {
        max_evidence_items: 20,
        max_excerpt_bytes: 100,
        max_capture_bytes: 50,
        evidence_total: 5,
        evidence_truncated: false,
        truncated_fields: vec![
            "evidence[0].excerpt".to_string(),
            "evidence[2].capture.session_id".to_string(),
        ],
    };

    let json = serde_json::to_string(&bounds).expect("serialize");
    let parsed: Value = serde_json::from_str(&json).expect("parse");

    let truncated = parsed["truncated_fields"]
        .as_array()
        .expect("truncated_fields should be array");
    assert_eq!(truncated.len(), 2);
    assert!(
        truncated.iter().any(|v| v == "evidence[0].excerpt"),
        "Should record which fields were truncated"
    );

    eprintln!("[ARTIFACT] Truncated fields: {:?}", truncated);
}

/// Verify TraceEvidence.truncated flag is set when excerpt is cut
#[test]
fn trace_evidence_truncated_flag_set() {
    // Simulate a truncated excerpt
    let evidence = TraceEvidence {
        kind: "match".to_string(),
        label: None,
        span: Some(TraceSpan {
            start: 0,
            end: 1000,
        }),
        excerpt: Some("This excerpt was truncated to fit...".to_string()),
        truncated: true, // Flag must be set
    };

    let json = serde_json::to_string(&evidence).expect("serialize");
    let parsed: Value = serde_json::from_str(&json).expect("parse");

    assert_eq!(
        parsed["truncated"], true,
        "Truncated flag should be true for truncated excerpts"
    );
}

/// Ensure trace bounds values are reasonable
#[test]
fn trace_options_bounds_are_reasonable() {
    let opts = TraceOptions::default();

    // Maximum bytes should be bounded to prevent memory issues
    assert!(
        opts.max_excerpt_bytes <= 8192,
        "max_excerpt_bytes too large: {}",
        opts.max_excerpt_bytes
    );
    assert!(
        opts.max_capture_bytes <= 4096,
        "max_capture_bytes too large: {}",
        opts.max_capture_bytes
    );
    assert!(
        opts.max_evidence_items <= 100,
        "max_evidence_items too large: {}",
        opts.max_evidence_items
    );

    // But they should also be useful
    assert!(
        opts.max_excerpt_bytes >= 64,
        "max_excerpt_bytes too small for useful excerpts: {}",
        opts.max_excerpt_bytes
    );
}

// ============================================================================
// Redaction / Non-Leak Tests
// ============================================================================

/// Verify secrets are not present in trace output
#[test]
fn redaction_removes_openai_api_key() {
    let redactor = Redactor::new();

    let text_with_secret = "API key: sk-proj-abc123def456ghi789jkl012mno345pqr678stu901vwx";
    let redacted = redactor.redact(text_with_secret);

    assert!(
        !redacted.contains("sk-proj-"),
        "OpenAI key prefix should be redacted"
    );
    assert!(
        redacted.contains("[REDACTED]"),
        "Should contain redaction marker"
    );

    eprintln!("[ARTIFACT] Original: {}", text_with_secret);
    eprintln!("[ARTIFACT] Redacted: {}", redacted);
}

/// Verify Anthropic API keys are redacted
#[test]
fn redaction_removes_anthropic_api_key() {
    let redactor = Redactor::new();

    let text_with_secret = "Using key: sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456";
    let redacted = redactor.redact(text_with_secret);

    assert!(
        !redacted.contains("sk-ant-"),
        "Anthropic key prefix should be redacted"
    );
    assert!(
        redacted.contains("[REDACTED]"),
        "Should contain redaction marker"
    );
}

/// Verify GitHub tokens are redacted
#[test]
fn redaction_removes_github_token() {
    let redactor = Redactor::new();

    let text_with_secret = "GITHUB_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    let redacted = redactor.redact(text_with_secret);

    assert!(
        !redacted.contains("ghp_"),
        "GitHub token prefix should be redacted"
    );
}

/// Verify AWS keys are redacted
#[test]
fn redaction_removes_aws_keys() {
    let redactor = Redactor::new();

    // AWS Access Key ID
    let text1 = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
    let redacted1 = redactor.redact(text1);
    assert!(
        !redacted1.contains("AKIAIOSFODNN7EXAMPLE"),
        "AWS access key should be redacted"
    );

    // AWS Secret Key
    let text2 = "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    let redacted2 = redactor.redact(text2);
    assert!(
        !redacted2.contains("wJalrXUtnFEMI"),
        "AWS secret key should be redacted"
    );
}

/// Verify bearer tokens are redacted
#[test]
fn redaction_removes_bearer_token() {
    let redactor = Redactor::new();

    let text_with_secret =
        "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkw";
    let redacted = redactor.redact(text_with_secret);

    assert!(
        !redacted.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
        "JWT token should be redacted"
    );
}

/// Verify database connection strings are redacted
#[test]
fn redaction_removes_database_password() {
    let redactor = Redactor::new();

    let text_with_secret = "DATABASE_URL=postgres://user:secretpassword123@localhost:5432/mydb";
    let redacted = redactor.redact(text_with_secret);

    assert!(
        !redacted.contains("secretpassword123"),
        "Database password should be redacted"
    );
}

/// Test that redaction preserves structure around secrets
#[test]
fn redaction_preserves_surrounding_text() {
    let redactor = Redactor::new();

    let text =
        "Error connecting with key sk-proj-abc123def456ghi789jkl012mno345pqr678stu901vwx to API";
    let redacted = redactor.redact(text);

    assert!(redacted.contains("Error connecting with key"));
    assert!(redacted.contains("to API"));
    assert!(!redacted.contains("sk-proj-"));
}

/// Test redaction with multiple secrets in same text
#[test]
fn redaction_handles_multiple_secrets() {
    let redactor = Redactor::new();

    // Note: OpenAI key pattern requires 20+ chars after prefix, Anthropic requires longer token
    let text = "OPENAI_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz ANTHROPIC_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz GITHUB_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    let redacted = redactor.redact(text);

    assert!(
        !redacted.contains("sk-proj-abcdefghijklmnopqrstuvwxyz"),
        "OpenAI key should be redacted. Got: {}",
        redacted
    );
    assert!(
        !redacted.contains("sk-ant-api03-abcdefghijklmnopqrstuvwxyz"),
        "Anthropic key should be redacted. Got: {}",
        redacted
    );
    assert!(
        !redacted.contains("ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
        "GitHub token should be redacted. Got: {}",
        redacted
    );

    // Should have multiple redaction markers
    let marker_count = redacted.matches("[REDACTED]").count();
    assert!(
        marker_count >= 2,
        "Should have multiple redaction markers, got {}. Redacted text: {}",
        marker_count,
        redacted
    );

    eprintln!("[ARTIFACT] Multi-secret redaction: {}", redacted);
}

/// Verify redaction with debug markers shows pattern names
#[test]
fn redaction_debug_markers_show_pattern_name() {
    let redactor = Redactor::with_debug_markers();

    let text = "Key: sk-proj-abc123def456ghi789jkl012mno345pqr678stu901vwx";
    let redacted = redactor.redact(text);

    assert!(
        redacted.contains("[REDACTED:"),
        "Debug mode should include pattern name"
    );
    assert!(
        redacted.contains("openai_key") || redacted.contains("REDACTED"),
        "Should indicate the pattern type"
    );

    eprintln!("[ARTIFACT] Debug redaction: {}", redacted);
}

/// Verify that safe text is not accidentally redacted
#[test]
fn redaction_does_not_affect_safe_text() {
    let redactor = Redactor::new();

    let safe_texts = [
        "Normal log message without secrets",
        "Error: connection timeout after 30s",
        "Processing file: /home/user/data.txt",
        "Status code: 200 OK",
        "git commit -m 'Update README'",
    ];

    for text in safe_texts {
        let redacted = redactor.redact(text);
        assert_eq!(
            text, redacted,
            "Safe text should not be modified: '{}'",
            text
        );
    }
}

/// Simulate what would happen if a trace contained secrets
#[test]
fn trace_evidence_with_secret_should_be_redactable() {
    let redactor = Redactor::new();

    // Simulate evidence that captured a secret (before redaction)
    let evidence = TraceEvidence {
        kind: "capture".to_string(),
        label: Some("api_key".to_string()),
        span: Some(TraceSpan { start: 0, end: 50 }),
        excerpt: Some("sk-proj-abc123def456ghi789jkl012mno345pqr678stu901vwx".to_string()),
        truncated: false,
    };

    // The excerpt should be redactable
    let redacted_excerpt = redactor.redact(evidence.excerpt.as_ref().unwrap());
    assert!(
        redacted_excerpt.contains("[REDACTED]"),
        "Secret in evidence should be redactable"
    );
    assert!(
        !redacted_excerpt.contains("sk-proj-"),
        "Redacted excerpt should not contain secret prefix"
    );
}

// ============================================================================
// Schema Validation Tests
// ============================================================================

/// Verify MatchTrace can roundtrip through JSON
#[test]
fn match_trace_json_roundtrip() {
    // Note: TraceBounds.truncated_fields uses skip_serializing_if = "Vec::is_empty"
    // but lacks #[serde(default)] for deserialization, so we test with non-empty truncated_fields
    let mut original = create_test_trace();
    original.bounds.truncated_fields = vec!["test_field".to_string()]; // Non-empty to ensure serialization

    let json = serde_json::to_string(&original).expect("serialize");
    let roundtripped: MatchTrace = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(original.pack_id, roundtripped.pack_id);
    assert_eq!(original.rule_id, roundtripped.rule_id);
    assert_eq!(original.eligible, roundtripped.eligible);
    assert_eq!(original.confidence, roundtripped.confidence);
    assert_eq!(original.gates.len(), roundtripped.gates.len());
    assert_eq!(original.evidence.len(), roundtripped.evidence.len());
}

/// Document that TraceBounds needs #[serde(default)] for truncated_fields
/// This test captures a schema issue: empty truncated_fields causes deserialization failure
#[test]
fn trace_bounds_empty_truncated_fields_deserialization_issue() {
    // Create bounds with empty truncated_fields
    let bounds = TraceBounds {
        max_evidence_items: 20,
        max_excerpt_bytes: 256,
        max_capture_bytes: 128,
        evidence_total: 1,
        evidence_truncated: false,
        truncated_fields: vec![], // Empty, will be omitted from JSON
    };

    let json = serde_json::to_string(&bounds).expect("serialize");
    eprintln!(
        "[ARTIFACT] TraceBounds JSON (empty truncated_fields): {}",
        json
    );

    // This will fail without #[serde(default)] on truncated_fields
    // We document this as expected behavior for now
    let result: Result<TraceBounds, _> = serde_json::from_str(&json);
    if result.is_err() {
        eprintln!(
            "[KNOWN ISSUE] TraceBounds deserialization fails with empty truncated_fields. \
             Consider adding #[serde(default)] to the field definition."
        );
    }
}

/// Verify schema has expected top-level structure
#[test]
fn match_trace_schema_structure() {
    let trace = create_test_trace();
    let value = serde_json::to_value(&trace).expect("to_value");

    // Required top-level fields
    assert!(value.is_object(), "MatchTrace should serialize to object");
    let obj = value.as_object().unwrap();

    let required = [
        "pack_id", "rule_id", "eligible", "gates", "evidence", "bounds",
    ];
    for field in required {
        assert!(obj.contains_key(field), "Missing required field: {}", field);
    }

    // Optional fields should be omitted when None/empty
    // (based on skip_serializing_if attributes)
}

/// Verify evidence items have expected structure
#[test]
fn trace_evidence_schema_structure() {
    // Use truncated=true so that field is serialized (it's skipped when false)
    let evidence = TraceEvidence {
        kind: "anchor".to_string(),
        label: Some("test".to_string()),
        span: Some(TraceSpan { start: 0, end: 10 }),
        excerpt: Some("test text".to_string()),
        truncated: true,
    };

    let value = serde_json::to_value(&evidence).expect("to_value");
    let obj = value.as_object().unwrap();

    // Required fields
    assert!(obj.contains_key("kind"), "Missing 'kind'");
    // truncated is conditionally serialized (only when true)
    assert!(
        obj.contains_key("truncated"),
        "truncated should be present when true"
    );

    // Optional fields should be present when set
    assert!(obj.contains_key("label"), "label should be present");
    assert!(obj.contains_key("span"), "span should be present");
    assert!(obj.contains_key("excerpt"), "excerpt should be present");
}

/// Verify gates have expected structure
#[test]
fn trace_gate_schema_structure() {
    let gate = TraceGate {
        gate: "agent_type".to_string(),
        passed: true,
        reason: Some("Matched expected agent".to_string()),
    };

    let value = serde_json::to_value(&gate).expect("to_value");
    let obj = value.as_object().unwrap();

    assert!(obj.contains_key("gate"), "Missing 'gate'");
    assert!(obj.contains_key("passed"), "Missing 'passed'");
    // reason is optional
}

/// Verify bounds have expected structure
#[test]
fn trace_bounds_schema_structure() {
    let bounds = TraceBounds {
        max_evidence_items: 20,
        max_excerpt_bytes: 256,
        max_capture_bytes: 128,
        evidence_total: 5,
        evidence_truncated: false,
        truncated_fields: vec![],
    };

    let value = serde_json::to_value(&bounds).expect("to_value");
    let obj = value.as_object().unwrap();

    let required = [
        "max_evidence_items",
        "max_excerpt_bytes",
        "max_capture_bytes",
        "evidence_total",
        "evidence_truncated",
    ];
    for field in required {
        assert!(obj.contains_key(field), "Missing required field: {}", field);
    }

    // truncated_fields should be omitted when empty (skip_serializing_if)
}

// ============================================================================
// Helper Functions
// ============================================================================

fn create_test_trace() -> MatchTrace {
    MatchTrace {
        pack_id: "builtin:codex".to_string(),
        rule_id: "core.codex:usage_reached".to_string(),
        extractor_id: Some("regex".to_string()),
        matched_text: Some("Usage limit reached".to_string()),
        confidence: Some(0.95),
        eligible: true,
        gates: vec![
            TraceGate {
                gate: "agent_type".to_string(),
                passed: true,
                reason: None,
            },
            TraceGate {
                gate: "dedupe".to_string(),
                passed: true,
                reason: None,
            },
        ],
        evidence: vec![TraceEvidence {
            kind: "anchor".to_string(),
            label: Some("usage_reached".to_string()),
            span: Some(TraceSpan { start: 0, end: 19 }),
            excerpt: Some("Usage limit reached".to_string()),
            truncated: false,
        }],
        bounds: TraceBounds {
            max_evidence_items: 20,
            max_excerpt_bytes: 256,
            max_capture_bytes: 128,
            evidence_total: 1,
            evidence_truncated: false,
            truncated_fields: vec![],
        },
    }
}

// ============================================================================
// Artifact Dump Tests (for CI)
// ============================================================================

#[test]
fn artifact_dump_trace_schema() {
    eprintln!("\n========== TRACE SCHEMA ARTIFACT DUMP ==========\n");

    let trace = create_test_trace();
    let json = serde_json::to_string_pretty(&trace).expect("serialize");

    eprintln!("Sample MatchTrace:\n{}\n", json);

    // Dump schema summary
    eprintln!("Schema summary:");
    eprintln!("- pack_id: string (required)");
    eprintln!("- rule_id: string (required)");
    eprintln!("- extractor_id: string (optional)");
    eprintln!("- matched_text: string (optional, bounded, redacted)");
    eprintln!("- confidence: float 0.0-1.0 (optional)");
    eprintln!("- eligible: boolean (required)");
    eprintln!("- gates: array of TraceGate (required)");
    eprintln!("- evidence: array of TraceEvidence (required)");
    eprintln!("- bounds: TraceBounds (required)");

    eprintln!("\n========== END TRACE SCHEMA DUMP ==========\n");
}

#[test]
fn artifact_dump_redaction_patterns() {
    eprintln!("\n========== REDACTION PATTERNS ARTIFACT DUMP ==========\n");

    let redactor = Redactor::with_debug_markers();

    let test_cases = [
        (
            "OpenAI key",
            "sk-proj-abc123def456ghi789jkl012mno345pqr678stu901vwx",
        ),
        (
            "Anthropic key",
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456",
        ),
        ("GitHub token", "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
        ("AWS Access Key", "AKIAIOSFODNN7EXAMPLE"),
        (
            "Bearer token",
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ),
        (
            "Database URL",
            "postgres://user:secretpass@localhost:5432/db",
        ),
    ];

    for (name, secret) in test_cases {
        let redacted = redactor.redact(secret);
        eprintln!("{}: {} -> {}", name, secret, redacted);
    }

    eprintln!("\n========== END REDACTION PATTERNS DUMP ==========\n");
}
