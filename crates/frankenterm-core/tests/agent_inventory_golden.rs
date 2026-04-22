//! Golden freeze for the core agent-inventory robot payload contract (ft-bs0ec).
//!
//! Pins the canonical JSON serialization of [`AgentInventoryData`], including:
//! - mixed installed-agent rows with both present and omitted optional fields
//! - running-agent inventory keyed by pane id strings
//! - aggregate summary counts
//! - the `filesystem_detection_available` feature-availability bit
//!
//! Regenerate the golden with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test agent_inventory_golden
//! ```

use frankenterm_core::robot_types::{
    AgentInventoryData, AgentInventorySummary, InstalledAgentInfo, RunningAgentInfo,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn mock_agent_inventory() -> AgentInventoryData {
    let installed = vec![
        InstalledAgentInfo {
            slug: "claude".to_string(),
            display_name: Some("Claude Code".to_string()),
            detected: true,
            evidence: vec![
                "found ~/.claude".to_string(),
                "binary: /usr/local/bin/claude".to_string(),
            ],
            root_paths: vec!["/Users/demo/.claude".to_string()],
            config_path: Some("/Users/demo/.claude/settings.json".to_string()),
            binary_path: Some("/usr/local/bin/claude".to_string()),
            version: Some("1.2.3".to_string()),
        },
        InstalledAgentInfo {
            slug: "codex".to_string(),
            display_name: Some("Codex".to_string()),
            detected: true,
            evidence: Vec::new(),
            root_paths: vec!["/Users/demo/.codex".to_string()],
            config_path: None,
            binary_path: Some("/usr/local/bin/codex".to_string()),
            version: Some("0.9.0".to_string()),
        },
        InstalledAgentInfo {
            slug: "aider".to_string(),
            display_name: None,
            detected: true,
            evidence: vec!["detected via ~/.config/aider".to_string()],
            root_paths: Vec::new(),
            config_path: Some("/Users/demo/.config/aider/aider.conf.yml".to_string()),
            binary_path: None,
            version: None,
        },
        InstalledAgentInfo {
            slug: "gemini".to_string(),
            display_name: Some("Gemini CLI".to_string()),
            detected: false,
            evidence: vec!["not installed".to_string()],
            root_paths: Vec::new(),
            config_path: None,
            binary_path: None,
            version: None,
        },
    ];

    let mut running = BTreeMap::new();
    running.insert(
        7,
        RunningAgentInfo {
            slug: "claude".to_string(),
            display_name: None,
            state: "waiting_approval".to_string(),
            session_id: None,
            source: "pane_title".to_string(),
            pane_id: 7,
        },
    );
    running.insert(
        42,
        RunningAgentInfo {
            slug: "codex".to_string(),
            display_name: Some("Codex".to_string()),
            state: "working".to_string(),
            session_id: Some("sess-codex-42".to_string()),
            source: "pattern_engine".to_string(),
            pane_id: 42,
        },
    );

    AgentInventoryData {
        installed,
        running,
        summary: AgentInventorySummary {
            installed_count: 3,
            running_count: 2,
            configured_count: 2,
            installed_but_idle_count: 1,
        },
        filesystem_detection_available: true,
    }
}

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
    serde_json::to_string_pretty(&canonicalize(value)).expect("serialize inventory")
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("agent_inventory_contract.json")
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
            "missing agent inventory golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test agent_inventory_golden",
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
            "agent inventory golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test agent_inventory_golden",
            golden.display(),
            actual_path.display()
        );
    }
}

#[test]
fn agent_inventory_contract_matches_golden() {
    let payload = mock_agent_inventory();
    let json = serde_json::to_value(&payload).expect("serialize AgentInventoryData");
    let actual = pretty_canonical(&json);
    assert_matches_golden(&actual, &golden_path());
}

#[test]
fn agent_inventory_contract_is_deterministic() {
    let payload = mock_agent_inventory();
    let first = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    let second = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    let third = pretty_canonical(&serde_json::to_value(&payload).unwrap());
    assert_eq!(
        first, second,
        "golden must be deterministic across captures"
    );
    assert_eq!(
        second, third,
        "golden must stay deterministic across captures"
    );
}

#[test]
fn agent_inventory_running_map_uses_stringified_pane_keys() {
    let payload = mock_agent_inventory();
    let json = serde_json::to_value(&payload).expect("serialize AgentInventoryData");
    let running = json["running"]
        .as_object()
        .expect("running inventory should serialize as an object");
    assert!(running.contains_key("7"));
    assert!(running.contains_key("42"));
}
