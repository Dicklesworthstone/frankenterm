//! Golden-artifact tests for `PaneStateData` JSON ⇄ TOON roundtrip.
//!
//! `PaneStateData` is the shape returned by `ft robot state` and the MCP
//! `wa.state` tool; it is part of the robot envelope conformance surface the
//! operator UI and MCP clients depend on. Because it carries volatile fields
//! (`pane_uuid`, `pane_id`, `tab_id`, `window_id`, `cwd`), golden tests use
//! `insta` with field redactions / scrubbers so snapshots are stable across
//! runs and hosts.
//!
//! Each snapshot freezes:
//!   * JSON serialization ordering (fields, optional presence)
//!   * TOON encoder output for the same struct (so any drift is caught)
//!   * Roundtrip fidelity (encode → decode → re-encode preserves semantics)
//!
//! The scrubbers replace volatile values with stable placeholders:
//!   * `pane_id`/`tab_id`/`window_id` → `"[counter]"`
//!   * `pane_uuid` → `"[uuid]"`
//!   * `cwd` → `"[cwd]"`
//! leaving the structural shape and the deterministic fields visible.

use frankenterm_core::robot_types::{PaneStateData, PaneTextResult, StateWithTextData};
use insta::{assert_json_snapshot, assert_snapshot};
use std::collections::BTreeMap;

fn sample_pane(pane_id: u64, title: &str) -> PaneStateData {
    PaneStateData {
        pane_id,
        pane_uuid: Some(format!("uuid-{pane_id:08x}-abcd-1234-5678-900000000000")),
        tab_id: pane_id * 10,
        window_id: 1000 + pane_id,
        domain: "local".to_string(),
        title: Some(title.to_string()),
        cwd: Some(format!("/tmp/workspace-{pane_id}")),
        observed: true,
        ignore_reason: None,
    }
}

fn sample_ignored_pane() -> PaneStateData {
    PaneStateData {
        pane_id: 99,
        pane_uuid: None,
        tab_id: 990,
        window_id: 1099,
        domain: "ssh:build-host".to_string(),
        title: Some("ignored tmux pane".to_string()),
        cwd: None,
        observed: false,
        ignore_reason: Some("tmux_passthrough".to_string()),
    }
}

#[test]
fn pane_state_single_pane_json_matches_golden() {
    let pane = sample_pane(42, "builder");
    assert_json_snapshot!(
        "pane_state_single_pane_json",
        pane,
        {
            ".pane_id" => "[pane_id]",
            ".pane_uuid" => "[uuid]",
            ".tab_id" => "[tab_id]",
            ".window_id" => "[window_id]",
            ".cwd" => "[cwd]",
        }
    );
}

#[test]
fn pane_state_ignored_pane_omits_uuid_and_cwd() {
    let pane = sample_ignored_pane();
    assert_json_snapshot!(
        "pane_state_ignored_pane_json",
        pane,
        {
            ".pane_id" => "[pane_id]",
            ".tab_id" => "[tab_id]",
            ".window_id" => "[window_id]",
        }
    );
}

#[test]
fn pane_state_with_text_envelope_json_matches_golden() {
    let panes = vec![sample_pane(42, "builder"), sample_ignored_pane()];
    let mut pane_text: BTreeMap<u64, PaneTextResult> = BTreeMap::new();
    pane_text.insert(
        42,
        PaneTextResult::Ok {
            text: "hello\nworld\n".to_string(),
            truncated: false,
            truncation_info: None,
        },
    );
    pane_text.insert(
        99,
        PaneTextResult::Error {
            code: "pane_ignored".to_string(),
            message: "tmux passthrough pane is not readable".to_string(),
            hint: Some("attach via tmux directly".to_string()),
        },
    );

    let envelope = StateWithTextData {
        panes,
        tail_lines: 200,
        escapes_included: false,
        pane_text,
    };

    assert_json_snapshot!(
        "pane_state_with_text_envelope_json",
        envelope,
        {
            ".panes[].pane_id" => "[pane_id]",
            ".panes[].pane_uuid" => "[uuid]",
            ".panes[].tab_id" => "[tab_id]",
            ".panes[].window_id" => "[window_id]",
            ".panes[].cwd" => "[cwd]",
        }
    );
}

#[test]
fn pane_state_toon_roundtrip_preserves_json_semantics() {
    let panes = vec![sample_pane(42, "builder"), sample_ignored_pane()];
    let json_value = serde_json::to_value(&panes).expect("serialize panes to JSON");

    // Encode to TOON and decode back — parity must hold for the operator
    // format-negotiation contract.
    let toon_text = toon_rust::cli::json_stringify::json_stringify_lines(&json_value, 0).join("\n");
    let decoded = toon_rust::try_decode(&toon_text, None).expect("TOON decode");
    let decoded_json_text =
        toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let roundtripped: serde_json::Value =
        serde_json::from_str(&decoded_json_text).expect("roundtrip JSON parse");

    assert_eq!(
        json_value, roundtripped,
        "PaneStateData TOON roundtrip must preserve JSON semantics"
    );

    // Freeze the canonical TOON shape so we catch encoder drift.
    assert_snapshot!("pane_state_toon_canonical", toon_text);
}

#[test]
fn pane_text_result_ok_and_error_variants_match_golden() {
    let ok = PaneTextResult::Ok {
        text: "alpha\nbeta\n".to_string(),
        truncated: true,
        truncation_info: Some(frankenterm_core::robot_types::TruncationInfo {
            original_bytes: 4096,
            returned_bytes: 512,
            original_lines: 200,
            returned_lines: 20,
        }),
    };
    let err = PaneTextResult::Error {
        code: "pane_not_found".to_string(),
        message: "pane 7 is not registered with the mux".to_string(),
        hint: Some("run 'ft robot state' to list active panes".to_string()),
    };

    assert_json_snapshot!("pane_text_result_ok_truncated", ok);
    assert_json_snapshot!("pane_text_result_error", err);
}
