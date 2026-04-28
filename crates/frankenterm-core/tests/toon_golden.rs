#![cfg(feature = "mcp")]

//! Exact-byte TOON output goldens for robot and MCP envelopes.
//!
//! Regenerate intentionally with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test toon_golden
//! ```
//!
//! Robot goldens include the trailing newline emitted by the CLI
//! `print_robot_response(..., RobotOutputFormat::Toon, ...)` path. MCP goldens
//! do not add a synthetic newline because MCP returns the encoded text as a
//! content item.

use frankenterm_core::robot_types::{
    GetTextData, PaneStateData, RobotResponse, SearchData, SearchHit,
};
use serde::Serialize;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

const VERSION: &str = "0.1.0";
const MCP_VERSION: &str = "v1";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn golden_dir() -> PathBuf {
    workspace_root().join("tests").join("goldens").join("toon")
}

fn golden_path(name: &str) -> PathBuf {
    golden_dir().join(format!("{name}.toon"))
}

fn robot_success<T: Serialize>(data: T, elapsed_ms: u64, now: u64) -> RobotResponse<T> {
    RobotResponse {
        ok: true,
        data: Some(data),
        error: None,
        error_code: None,
        hint: None,
        elapsed_ms,
        version: VERSION.to_string(),
        now,
    }
}

#[derive(Serialize)]
struct McpGoldenEnvelope<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    elapsed_ms: u64,
    version: String,
    now: u64,
    mcp_version: &'static str,
}

fn mcp_success<T: Serialize>(data: T, elapsed_ms: u64, now: u64) -> McpGoldenEnvelope<T> {
    McpGoldenEnvelope {
        ok: true,
        data: Some(data),
        error: None,
        error_code: None,
        hint: None,
        elapsed_ms,
        version: VERSION.to_string(),
        now,
        mcp_version: MCP_VERSION,
    }
}

fn mcp_error(
    code: &str,
    message: &str,
    hint: Option<&str>,
    elapsed_ms: u64,
    now: u64,
) -> McpGoldenEnvelope<()> {
    McpGoldenEnvelope {
        ok: false,
        data: None,
        error: Some(message.to_string()),
        error_code: Some(code.to_string()),
        hint: hint.map(str::to_string),
        elapsed_ms,
        version: VERSION.to_string(),
        now,
        mcp_version: MCP_VERSION,
    }
}

fn encode_toon<T: Serialize>(value: &T, trailing_newline: bool) -> Vec<u8> {
    let mut encoded = toon_rust::encode(serde_json::to_value(value).expect("serialize"), None);
    if trailing_newline {
        encoded.push('\n');
    }
    encoded.into_bytes()
}

fn read_or_update_golden(path: &Path, actual: &[u8]) -> Vec<u8> {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create TOON golden dir");
        }
        fs::write(path, actual).expect("write TOON golden");
        return actual.to_vec();
    }

    fs::read(path).unwrap_or_else(|err| {
        panic!(
            "missing TOON golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test toon_golden",
            path.display()
        )
    })
}

fn assert_toon_matches_golden(name: &str, actual: &[u8]) {
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, actual);
    if expected != actual {
        let actual_path = path.with_extension("actual.toon");
        let _ = fs::write(&actual_path, actual);
        panic!(
            "TOON golden drift detected for {name}. Review the byte diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test toon_golden",
            path.display(),
            actual_path.display()
        );
    }
}

fn robot_state_fixture() -> RobotResponse<Vec<PaneStateData>> {
    robot_success(
        vec![
            PaneStateData {
                pane_id: 7,
                pane_uuid: Some("pane-uuid-7".to_string()),
                tab_id: 2,
                window_id: 1,
                domain: "local".to_string(),
                title: Some("cod_4".to_string()),
                cwd: Some("/Users/jemanuel/projects/frankenterm".to_string()),
                observed: true,
                ignore_reason: None,
            },
            PaneStateData {
                pane_id: 8,
                pane_uuid: None,
                tab_id: 2,
                window_id: 1,
                domain: "ssh:builder".to_string(),
                title: Some("rch-worker".to_string()),
                cwd: None,
                observed: false,
                ignore_reason: Some("domain filtered".to_string()),
            },
        ],
        4,
        1_777_000_000_001,
    )
}

fn robot_get_text_fixture() -> RobotResponse<GetTextData> {
    robot_success(
        GetTextData {
            pane_id: 7,
            text: "$ cargo test toon\nrunning 5 tests\nok\n".to_string(),
            tail_lines: 40,
            escapes_included: false,
            truncated: true,
            truncation_info: Some(frankenterm_core::robot_types::TruncationInfo {
                original_bytes: 4096,
                returned_bytes: 128,
                original_lines: 120,
                returned_lines: 40,
            }),
        },
        9,
        1_777_000_000_002,
    )
}

fn robot_search_fixture() -> RobotResponse<SearchData> {
    robot_success(
        SearchData {
            query: "golden toon".to_string(),
            results: vec![
                SearchHit {
                    segment_id: 42,
                    pane_id: 7,
                    seq: 9001,
                    captured_at: 1_777_000_000_003,
                    score: 12.5,
                    snippet: Some("...exact-byte <mark>TOON</mark> output...".to_string()),
                    content: None,
                    semantic_score: Some(0.875),
                    fusion_rank: Some(1),
                },
                SearchHit {
                    segment_id: 43,
                    pane_id: 8,
                    seq: 9002,
                    captured_at: 1_777_000_000_004,
                    score: 8.25,
                    snippet: None,
                    content: Some("MCP envelope encoded as TOON".to_string()),
                    semantic_score: None,
                    fusion_rank: Some(2),
                },
            ],
            total_hits: 2,
            limit: 5,
            pane_filter: Some(7),
            since_filter: Some(1_777_000_000_000),
            until_filter: None,
            mode: Some("hybrid".to_string()),
            metrics: Some(json!({
                "lexical_candidates": 3,
                "semantic_candidates": 2,
                "fusion_backend": "frankensearch_rrf"
            })),
        },
        14,
        1_777_000_000_005,
    )
}

#[test]
fn toon_robot_state_matches_golden() {
    let actual = encode_toon(&robot_state_fixture(), true);
    assert_toon_matches_golden("robot_state", &actual);
}

#[test]
fn toon_robot_get_text_matches_golden() {
    let actual = encode_toon(&robot_get_text_fixture(), true);
    assert_toon_matches_golden("robot_get_text", &actual);
}

#[test]
fn toon_robot_search_results_matches_golden() {
    let actual = encode_toon(&robot_search_fixture(), true);
    assert_toon_matches_golden("robot_search_results", &actual);
}

#[test]
fn toon_mcp_state_envelope_matches_golden() {
    let envelope = mcp_success(
        json!({
            "panes": [
                {
                    "pane_id": 7,
                    "domain": "local",
                    "title": "cod_4",
                    "capabilities": {
                        "can_read": true,
                        "can_send": true,
                        "reserved_by": null
                    }
                }
            ],
            "total": 1
        }),
        6,
        1_777_000_000_006,
    );
    let actual = encode_toon(&envelope, false);
    assert_toon_matches_golden("mcp_state_envelope", &actual);
}

#[test]
fn toon_mcp_error_envelope_matches_golden() {
    let envelope = mcp_error(
        "FT-MCP-0008",
        "pane 99 not found",
        Some("Use wa.state to list available panes"),
        3,
        1_777_000_000_007,
    );
    let actual = encode_toon(&envelope, false);
    assert_toon_matches_golden("mcp_error_envelope", &actual);
}
