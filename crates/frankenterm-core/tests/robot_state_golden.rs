//! Golden snapshot for the `ft robot state` JSON wire shape.
//!
//! Pins the canonical JSON serialization of [`StateWithTextData`] —
//! the response envelope returned by `ft robot state --include-text`
//! — for a deterministic 2-pane mock wezterm. Any change to field
//! names, field order under canonicalization, `#[serde(default)]`
//! attributes, or the `PaneInfo → PaneStateData` field-mapping
//! contract will diverge from the committed golden and force a
//! deliberate update.
//!
//! Regenerate the golden with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p frankenterm-core \
//!     --no-default-features --features tui \
//!     --test robot_state_golden
//! ```
//!
//! The golden lives at
//! `tests/fixtures/robot_state_two_pane.json` relative to the crate
//! manifest directory.
//!
//! Domain: snapshot golden (pane 5).

use frankenterm_core::robot_types::{PaneStateData, PaneTextResult, StateWithTextData};
use frankenterm_core::wezterm::{PaneInfo, PaneSize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Build a deterministic 2-pane mock wezterm:
/// - pane 1: active local bash at `/home/agent`, title "claude"
/// - pane 2: inactive ssh-remote vim at `/srv/repo`, title "editor"
fn mock_two_pane_wezterm() -> Vec<PaneInfo> {
    vec![
        PaneInfo {
            pane_id: 1,
            tab_id: 10,
            window_id: 100,
            domain_id: Some(1),
            domain_name: Some("local".to_string()),
            workspace: Some("default".to_string()),
            size: Some(PaneSize {
                rows: 24,
                cols: 80,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some("claude".to_string()),
            cwd: Some("file:///home/agent".to_string()),
            tty_name: Some("/dev/pts/0".to_string()),
            cursor_x: Some(0),
            cursor_y: Some(0),
            cursor_visibility: None,
            left_col: Some(0),
            top_row: Some(0),
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        },
        PaneInfo {
            pane_id: 2,
            tab_id: 10,
            window_id: 100,
            domain_id: Some(2),
            domain_name: Some("ssh:remote".to_string()),
            workspace: Some("default".to_string()),
            size: Some(PaneSize {
                rows: 24,
                cols: 80,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some("editor".to_string()),
            cwd: Some("file://remote/srv/repo".to_string()),
            tty_name: Some("/dev/pts/1".to_string()),
            cursor_x: Some(0),
            cursor_y: Some(0),
            cursor_visibility: None,
            left_col: Some(0),
            top_row: Some(0),
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        },
    ]
}

/// Convert a [`PaneInfo`] to [`PaneStateData`] using the same field
/// mapping the MCP and IPC layers apply. Kept local to the test so
/// the golden fails loudly if a refactor drifts either the mapping
/// or the wire shape.
fn pane_state_from_info(info: &PaneInfo) -> PaneStateData {
    PaneStateData {
        pane_id: info.pane_id,
        pane_uuid: None,
        tab_id: info.tab_id,
        window_id: info.window_id,
        domain: info.inferred_domain(),
        title: info.title.clone(),
        cwd: info.cwd.clone(),
        observed: true,
        ignore_reason: None,
    }
}

/// Build a deterministic tail-text fixture so the `pane_text` map's
/// `PaneTextResult` variants are exercised by the golden too.
fn mock_pane_text() -> BTreeMap<u64, PaneTextResult> {
    let mut text = BTreeMap::new();
    text.insert(
        1,
        PaneTextResult::Ok {
            text: "$ echo hello\nhello\n$ ".to_string(),
            truncated: false,
            truncation_info: None,
        },
    );
    text.insert(
        2,
        PaneTextResult::Error {
            code: "pane_unreachable".to_string(),
            message: "ssh domain unreachable".to_string(),
            hint: Some("check network connectivity to remote host".to_string()),
        },
    );
    text
}

/// Canonicalize a JSON value by recursively sorting object keys so
/// serialization order never affects the golden. Matches the pattern
/// used by `mcp_manifest_golden.rs`.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn pretty_canonical(value: &Value) -> String {
    let canonical = canonicalize(value);
    serde_json::to_string_pretty(&canonical).expect("serialize state payload")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("robot_state_two_pane.json")
}

fn read_or_update_golden(path: &PathBuf, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixtures dir");
        }
        std::fs::write(path, format!("{actual}\n")).expect("write golden");
        return actual.to_string();
    }

    std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing robot state golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test robot_state_golden",
            path.display()
        )
    })
}

fn assert_matches_golden(actual: &str, golden: &PathBuf) {
    let expected = read_or_update_golden(golden, actual);
    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');

    if expected_trimmed != actual_trimmed {
        let actual_path = golden.with_extension("actual.json");
        let _ = std::fs::write(&actual_path, format!("{actual}\n"));
        panic!(
            "robot state golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test robot_state_golden",
            golden.display(),
            actual_path.display()
        );
    }
}

#[test]
fn robot_state_two_pane_matches_golden() {
    let panes_info = mock_two_pane_wezterm();
    let panes: Vec<PaneStateData> = panes_info.iter().map(pane_state_from_info).collect();

    let payload = StateWithTextData {
        panes,
        tail_lines: 20,
        escapes_included: false,
        pane_text: mock_pane_text(),
    };

    let json =
        serde_json::to_value(&payload).expect("serialize StateWithTextData for golden snapshot");
    let actual = pretty_canonical(&json);
    assert_matches_golden(&actual, &golden_path());
}

#[test]
fn robot_state_golden_is_deterministic() {
    // Re-serialize twice and confirm byte-identical output so the
    // golden can never drift due to HashMap iteration order or
    // other non-determinism in the serializer path.
    let panes_info = mock_two_pane_wezterm();
    let panes: Vec<PaneStateData> = panes_info.iter().map(pane_state_from_info).collect();
    let payload = StateWithTextData {
        panes,
        tail_lines: 20,
        escapes_included: false,
        pane_text: mock_pane_text(),
    };

    let first = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    let second = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    let third = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    assert_eq!(first, second, "golden must be deterministic across captures");
    assert_eq!(
        second, third,
        "golden must remain deterministic across repeated captures"
    );
}

#[test]
fn robot_state_pane_state_mapping_preserves_domain_override() {
    // Regression guard on the PaneInfo → PaneStateData mapping:
    // explicit `domain_name = "ssh:remote"` must surface as the
    // `domain` field on PaneStateData via `inferred_domain()`.
    let panes = mock_two_pane_wezterm();
    let p0 = pane_state_from_info(&panes[0]);
    let p1 = pane_state_from_info(&panes[1]);
    assert_eq!(p0.domain, "local");
    assert_eq!(p1.domain, "ssh:remote");
}

#[test]
fn robot_state_pane_text_variants_serialize_distinct_tags() {
    // Regression guard on the PaneTextResult enum tag encoding:
    // Ok variant must serialize with "status":"ok" and Error with
    // "status":"error". The golden already pins this, but we also
    // assert it inline so diff reviewers can eyeball the contract.
    let text = mock_pane_text();
    let ok_value = serde_json::to_value(&text[&1]).unwrap();
    let err_value = serde_json::to_value(&text[&2]).unwrap();
    assert_eq!(
        ok_value.get("status").and_then(Value::as_str),
        Some("ok"),
        "Ok variant must use status=\"ok\""
    );
    assert_eq!(
        err_value.get("status").and_then(Value::as_str),
        Some("error"),
        "Error variant must use status=\"error\""
    );
}
