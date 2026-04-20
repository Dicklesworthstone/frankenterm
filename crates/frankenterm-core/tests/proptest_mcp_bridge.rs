//! Property-based tests for the public `mcp_bridge` server wiring surface.

#![cfg(feature = "mcp")]

use frankenterm_core::VERSION;
use frankenterm_core::config::Config;
use frankenterm_core::mcp::{build_server, build_server_with_db};
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn tool_names(server: &frankenterm_core::mcp_framework::FrameworkServer) -> BTreeSet<String> {
    server.tools().into_iter().map(|tool| tool.name).collect()
}

fn resource_uris(server: &frankenterm_core::mcp_framework::FrameworkServer) -> BTreeSet<String> {
    server
        .resources()
        .into_iter()
        .map(|resource| resource.uri)
        .collect()
}

fn template_uris(server: &frankenterm_core::mcp_framework::FrameworkServer) -> BTreeSet<String> {
    server
        .resource_templates()
        .into_iter()
        .map(|template| template.uri_template)
        .collect()
}

#[test]
fn build_server_reports_expected_identity() {
    let server = build_server(&Config::default()).expect("build default MCP server");
    let info = server.info();
    let tools = tool_names(&server);
    let resources = resource_uris(&server);

    assert_eq!(info.name, "wezterm-automata");
    assert_eq!(info.version, VERSION);
    assert!(tools.contains("wa.state"));
    assert!(tools.contains("wa.get_text"));
    assert!(tools.contains("wa.wait_for"));
    assert!(resources.contains("wa://panes"));
    assert!(resources.contains("wa://rules"));
    assert!(resources.contains("wa://workflows"));
}

proptest! {

    #[test]
    fn build_server_with_db_enables_db_gated_surface(suffix in "[a-z0-9_-]{1,24}") {
        let db_path = PathBuf::from(format!("/tmp/ft-cod1-mcp-{suffix}.sqlite3"));
        let server = build_server_with_db(&Config::default(), Some(db_path))
            .expect("build MCP server with DB");

        let tools = tool_names(&server);
        let resources = resource_uris(&server);
        let templates = template_uris(&server);
        let events_template = "wa://events/{limit}";
        let accounts_template = "wa://accounts/{service}";
        let reservations_template = "wa://reservations/{pane_id}";

        prop_assert!(tools.contains("wa.search"));
        prop_assert!(tools.contains("wa.events"));
        prop_assert!(tools.contains("wa.events_annotate"));
        prop_assert!(tools.contains("wa.reservations"));
        prop_assert!(tools.contains("wa.accounts"));
        prop_assert!(tools.contains("wa.accounts_refresh"));

        prop_assert!(resources.contains("wa://events"));
        prop_assert!(resources.contains("wa://accounts"));
        prop_assert!(resources.contains("wa://reservations"));

        prop_assert!(templates.contains(events_template));
        prop_assert!(templates.contains(accounts_template));
        prop_assert!(templates.contains(reservations_template));
    }

    #[test]
    fn build_server_db_toggle_changes_registration_surface(suffix in "[a-z0-9_-]{1,24}") {
        let no_db = build_server(&Config::default()).expect("build MCP server without DB");
        let with_db = build_server_with_db(
            &Config::default(),
            Some(PathBuf::from(format!("/tmp/ft-cod1-mcp-toggle-{suffix}.sqlite3"))),
        )
        .expect("build MCP server with DB");

        let no_db_tools = tool_names(&no_db);
        let with_db_tools = tool_names(&with_db);
        let no_db_resources = resource_uris(&no_db);
        let with_db_resources = resource_uris(&with_db);
        let no_db_templates = template_uris(&no_db);
        let with_db_templates = template_uris(&with_db);
        let events_template = "wa://events/{limit}";

        prop_assert!(!no_db_tools.contains("wa.search"));
        prop_assert!(!no_db_tools.contains("wa.events"));
        prop_assert!(!no_db_resources.contains("wa://events"));
        prop_assert!(!no_db_templates.contains(events_template));

        prop_assert!(with_db_tools.len() > no_db_tools.len());
        prop_assert!(with_db_resources.len() > no_db_resources.len());
        prop_assert!(with_db_templates.len() > no_db_templates.len());
    }
}
