//! Integration tests for WezTerm CLI JSON parsing
//!
//! These tests verify that our PaneInfo models correctly parse
//! various WezTerm CLI output formats.

use std::fs;
use std::path::PathBuf;
use wa_core::wezterm::PaneInfo;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("wezterm_cli")
}

fn load_fixture(name: &str) -> Vec<PaneInfo> {
    let path = fixtures_dir().join(name);
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", path.display(), e));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {}", path.display(), e))
}

#[test]
fn parse_local_single_pane() {
    let panes = load_fixture("local_single_pane.json");
    assert_eq!(panes.len(), 1);

    let pane = &panes[0];
    assert_eq!(pane.pane_id, 0);
    assert_eq!(pane.effective_domain(), "local");
    assert_eq!(pane.effective_title(), "zsh");
    assert_eq!(pane.effective_rows(), 24);
    assert_eq!(pane.effective_cols(), 80);
    assert!(pane.is_active);
    assert!(!pane.is_zoomed);

    // Check size details
    let size = pane.size.as_ref().unwrap();
    assert_eq!(size.pixel_width, Some(640));
    assert_eq!(size.dpi, Some(96));

    // Check cwd parsing
    let cwd = pane.parsed_cwd();
    assert!(!cwd.is_remote);
    assert_eq!(cwd.path, "/home/user");
}

#[test]
fn parse_multi_pane_split() {
    let panes = load_fixture("multi_pane_split.json");
    assert_eq!(panes.len(), 2);

    // First pane is active
    assert!(panes[0].is_active);
    assert!(!panes[1].is_active);

    // Both in same tab
    assert_eq!(panes[0].tab_id, panes[1].tab_id);

    // Different pane IDs
    assert_ne!(panes[0].pane_id, panes[1].pane_id);
}

#[test]
fn parse_ssh_multiplexed() {
    let panes = load_fixture("ssh_multiplexed.json");
    assert_eq!(panes.len(), 3);

    // First pane is local
    assert_eq!(panes[0].effective_domain(), "local");
    assert!(!panes[0].parsed_cwd().is_remote);

    // Second pane is on prod-server
    assert_eq!(panes[1].effective_domain(), "ssh:prod-server");
    let cwd = panes[1].parsed_cwd();
    assert!(cwd.is_remote);
    assert_eq!(cwd.host, "prod-server");
    assert_eq!(cwd.path, "/var/log");

    // Third pane is on staging-server
    assert_eq!(panes[2].effective_domain(), "ssh:staging-server");
    let cwd = panes[2].parsed_cwd();
    assert!(cwd.is_remote);
    assert_eq!(cwd.host, "staging-server");

    // Only second pane is active
    assert!(!panes[0].is_active);
    assert!(panes[1].is_active);
    assert!(!panes[2].is_active);
}

#[test]
fn parse_minimal_fields() {
    let panes = load_fixture("minimal_fields.json");
    assert_eq!(panes.len(), 1);

    let pane = &panes[0];
    assert_eq!(pane.pane_id, 0);
    assert_eq!(pane.tab_id, 0);
    assert_eq!(pane.window_id, 0);

    // All optional fields should have sensible defaults
    assert_eq!(pane.effective_domain(), "local");
    assert_eq!(pane.effective_title(), "");
    assert_eq!(pane.effective_rows(), 24);
    assert_eq!(pane.effective_cols(), 80);
    assert!(!pane.is_active);
    assert!(!pane.is_zoomed);
}

#[test]
fn parse_future_compat() {
    let panes = load_fixture("future_compat.json");
    assert_eq!(panes.len(), 1);

    let pane = &panes[0];

    // Known fields parse correctly
    assert_eq!(pane.pane_id, 0);
    assert_eq!(pane.effective_domain(), "local");
    assert_eq!(pane.effective_title(), "shell");

    // Unknown fields are captured in extra
    assert!(pane.extra.contains_key("some_new_field_v2"));
    assert!(pane.extra.contains_key("another_field_v3"));
    assert!(pane.extra.contains_key("numeric_future_field"));

    // Can access unknown field values
    assert_eq!(pane.extra.get("numeric_future_field").unwrap(), &42);
}

#[test]
fn parse_unicode_fields() {
    let panes = load_fixture("unicode_fields.json");
    assert_eq!(panes.len(), 1);

    let pane = &panes[0];
    assert_eq!(pane.effective_title(), "\u{7f16}\u{7a0b}\u{7ec8}\u{7aef}");

    let cwd = pane.parsed_cwd();
    assert!(!cwd.is_remote);
    assert_eq!(cwd.path, "/home/\u{7528}\u{6237}/\u{9879}\u{76ee}");
}

#[test]
fn inferred_domain_from_cwd() {
    // Test domain inference when domain_name is missing but cwd is remote
    let json = r#"[{
        "pane_id": 0,
        "tab_id": 0,
        "window_id": 0,
        "cwd": "file://my-remote-host/home/user"
    }]"#;

    let panes: Vec<PaneInfo> = serde_json::from_str(json).unwrap();
    let pane = &panes[0];

    // Domain should be inferred from cwd
    assert_eq!(pane.inferred_domain(), "ssh:my-remote-host");

    // effective_domain returns "local" (the default) since domain_name is None
    assert_eq!(pane.effective_domain(), "local");
}

#[test]
fn all_fixtures_parse_successfully() {
    // Meta-test: ensure all fixtures in the directory parse
    let dir = fixtures_dir();
    for entry in fs::read_dir(&dir).expect("Failed to read fixtures directory") {
        let entry = entry.expect("Failed to read directory entry");
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "json") {
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));
            let _panes: Vec<PaneInfo> = serde_json::from_str(&content)
                .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path.display(), e));
        }
    }
}
