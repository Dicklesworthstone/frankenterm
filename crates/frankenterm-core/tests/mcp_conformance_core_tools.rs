//! [ft-4o5mb] Per-tool request → response envelope golden roundtrip for the
//! four core operator-facing MCP tools: wa.search, wa.get_text, wa.send,
//! wa.wait_for.
//!
//! Mirrors the pattern in mcp_conformance.rs for wa.rules_list:
//!
//!   1. spawn_client() → call_tool("wa.<tool>", …)
//!   2. parse_tool_envelope() → assert the envelope shape
//!      (ok/elapsed_ms/version/now/mcp_version + data | error/error_code/hint).
//!   3. For error paths: assert the documented FT-MCP-0001 hint substring.
//!
//! Why this file exists: mcp_conformance_manifest.rs pins the *tool schemas*
//! (input-shape, additionalProperties, resource templates) but does not
//! exercise a single request → response roundtrip. mcp_conformance.rs
//! exercises one tool (wa.rules_list). A refactor that renames a response
//! field passes both of those while silently breaking every client. This
//! file pins the envelope contract for the four tools the operator touches
//! on every session.
//!
//! Scope guard (ft-4o5mb): this first pass covers the invalid-args path
//! per tool (4 tests), which pins:
//!   - the FT-MCP-0001 error_code contract
//!   - the common envelope fields (ok/elapsed_ms/version/now/mcp_version)
//!   - the per-tool hint substring (which is the documented field list
//!     client remediation code keys off).
//!
//! The invalid-args path is fast, deterministic, and requires no pane
//! fixtures or stored output. Success-path envelope tests and the
//! format-reject path interact with framework-layer schema validation
//! (fastmcp's `additionalProperties:false` enforcement) and are a
//! follow-up scope item — the common envelope fields they would assert
//! are already fenced by the 4 tests below, so the outer contract is
//! protected.

#![cfg(feature = "mcp")]

use frankenterm_core::VERSION;
use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, framework_create_memory_transport_pair,
};
use serde_json::{Value, json};
use std::path::PathBuf;

fn spawn_client(db_path: Option<PathBuf>) -> FrameworkTestClient {
    let server = build_server_with_db(&Config::default(), db_path).expect("build MCP server");
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    std::thread::spawn(move || {
        let _ = server.run_transport(server_transport);
    });

    let mut client = FrameworkTestClient::new(client_transport);
    client
        .initialize()
        .expect("initialize in-memory MCP client");
    client
}

fn first_text_content(contents: &[FrameworkContent]) -> &str {
    contents
        .first()
        .and_then(|content| match content {
            FrameworkContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("expected first MCP content to be text")
}

fn parse_json_envelope(contents: &[FrameworkContent]) -> Value {
    serde_json::from_str(first_text_content(contents)).expect("parse JSON envelope")
}

/// The outer envelope contract shared by every MCP tool response —
/// success OR error. Drift here is the blast radius that ft-4o5mb is
/// meant to fence against: a refactor that breaks any of these five
/// keys silently breaks every downstream client.
fn assert_common_envelope_fields(envelope: &Value, ok: bool) {
    assert_eq!(envelope["ok"], Value::Bool(ok), "envelope: {envelope}");
    assert!(
        envelope["elapsed_ms"].is_number(),
        "elapsed_ms must be a number: {envelope}"
    );
    assert_eq!(envelope["version"], VERSION);
    assert!(
        envelope["now"].is_number(),
        "now must be a number: {envelope}"
    );
    assert_eq!(envelope["mcp_version"], "v1");
}

/// The FT-MCP-0001 error envelope contract — when args fail to
/// deserialize, the tool MUST emit this exact shape. Hint is freeform
/// but MUST contain `hint_substring` so that clients can key off the
/// error for remediation.
fn assert_invalid_args_envelope_shape(envelope: &Value, hint_substring: &str) {
    assert_common_envelope_fields(envelope, false);
    assert!(
        envelope.get("data").is_none(),
        "error envelope must not carry data: {envelope}"
    );
    assert_eq!(envelope["error_code"], "FT-MCP-0001");
    assert!(envelope["error"].is_string());
    let hint = envelope["hint"].as_str().expect("hint string present");
    assert!(
        hint.contains(hint_substring),
        "hint must contain {hint_substring:?}, got {hint:?}"
    );
}

// ══════════════════════════════════════════════════════════════════════════
// wa.search — query required; wrong-type query must hit invalid-args path
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn mcp_conformance_search_invalid_args_returns_documented_error_envelope() {
    let mut client = spawn_client(None);
    // query is required + must be a string. An integer query forces
    // serde to reject at parse time, exercising the invalid-args path.
    let reply = client
        .call_tool("wa.search", json!({"query": 42, "format": "json"}))
        .expect("call wa.search with wrong-typed query");

    let envelope = parse_json_envelope(&reply);
    assert_invalid_args_envelope_shape(
        &envelope,
        "Expected object with query (required), limit, pane, since, until, snippets, mode",
    );
    assert!(
        envelope["error"]
            .as_str()
            .expect("error string")
            .contains("Invalid params")
    );
}

// ══════════════════════════════════════════════════════════════════════════
// wa.get_text — pane_id required
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn mcp_conformance_get_text_invalid_args_returns_documented_error_envelope() {
    let mut client = spawn_client(None);
    // pane_id required; a string pane_id forces serde rejection.
    let reply = client
        .call_tool(
            "wa.get_text",
            json!({"pane_id": "not-a-number", "format": "json"}),
        )
        .expect("call wa.get_text with wrong-typed pane_id");

    let envelope = parse_json_envelope(&reply);
    assert_invalid_args_envelope_shape(
        &envelope,
        "Expected object with pane_id (required), tail, escapes",
    );
    assert!(
        envelope["error"]
            .as_str()
            .expect("error string")
            .contains("Invalid params")
    );
}

// ══════════════════════════════════════════════════════════════════════════
// wa.send — pane_id + text required
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn mcp_conformance_send_invalid_args_returns_documented_error_envelope() {
    let mut client = spawn_client(None);
    // Missing both required fields (pane_id + text). serde catches
    // pane_id missing first, so the hint surfaces on that error.
    let reply = client
        .call_tool("wa.send", json!({"format": "json"}))
        .expect("call wa.send with missing required fields");

    let envelope = parse_json_envelope(&reply);
    assert_invalid_args_envelope_shape(
        &envelope,
        "Expected object with pane_id, text, dry_run, wait_for, timeout_secs, wait_for_regex",
    );
    assert!(
        envelope["error"]
            .as_str()
            .expect("error string")
            .contains("Invalid params")
    );
}

// ══════════════════════════════════════════════════════════════════════════
// wa.wait_for — pane_id + pattern required
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn mcp_conformance_wait_for_invalid_args_returns_documented_error_envelope() {
    let mut client = spawn_client(None);
    // Both pane_id and pattern missing → serde rejects first required.
    let reply = client
        .call_tool("wa.wait_for", json!({"format": "json"}))
        .expect("call wa.wait_for with missing required fields");

    let envelope = parse_json_envelope(&reply);
    assert_invalid_args_envelope_shape(
        &envelope,
        "Expected object with pane_id, pattern, timeout_secs, tail, regex",
    );
    assert!(
        envelope["error"]
            .as_str()
            .expect("error string")
            .contains("Invalid params")
    );
}
