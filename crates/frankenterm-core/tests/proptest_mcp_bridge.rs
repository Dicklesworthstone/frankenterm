//! Property-based tests for the public `mcp_bridge` server wiring surface.

#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::{build_server, build_server_with_db};
use frankenterm_core::VERSION;
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn as_btree_set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

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

    #[test]
    fn build_server_with_db_is_strict_superset_of_base_surface(suffix in "[a-z0-9_-]{1,24}") {
        let no_db = build_server(&Config::default()).expect("build MCP server without DB");
        let with_db = build_server_with_db(
            &Config::default(),
            Some(PathBuf::from(format!("/tmp/ft-cod1-mcp-superset-{suffix}.sqlite3"))),
        )
        .expect("build MCP server with DB");

        let no_db_tools = tool_names(&no_db);
        let with_db_tools = tool_names(&with_db);
        let no_db_resources = resource_uris(&no_db);
        let with_db_resources = resource_uris(&with_db);
        let no_db_templates = template_uris(&no_db);
        let with_db_templates = template_uris(&with_db);

        let db_tools = as_btree_set(&[
            "wa.search",
            "wa.events",
            "wa.events_annotate",
            "wa.events_triage",
            "wa.events_label",
            "wa.reservations",
            "wa.reserve",
            "wa.release",
            "wa.send",
            "wa.workflow_run",
            "wa.accounts",
            "wa.accounts_refresh",
        ]);
        let db_resources = as_btree_set(&[
            "wa://events",
            "wa://accounts",
            "wa://reservations",
        ]);
        let db_templates = as_btree_set(&[
            "wa://events/{limit}",
            "wa://events/unhandled/{limit}",
            "wa://accounts/{service}",
            "wa://reservations/{pane_id}",
        ]);

        prop_assert!(no_db_tools.is_subset(&with_db_tools));
        prop_assert!(no_db_resources.is_subset(&with_db_resources));
        prop_assert!(no_db_templates.is_subset(&with_db_templates));

        prop_assert!(db_tools.is_subset(&with_db_tools));
        prop_assert!(db_resources.is_subset(&with_db_resources));
        prop_assert!(db_templates.is_subset(&with_db_templates));

        prop_assert!(db_tools.is_disjoint(&no_db_tools));
        prop_assert!(db_resources.is_disjoint(&no_db_resources));
        prop_assert!(db_templates.is_disjoint(&no_db_templates));
    }

    #[test]
    fn build_server_tool_names_follow_wa_namespace(suffix in "[a-z0-9_-]{1,24}") {
        let server = build_server_with_db(
            &Config::default(),
            Some(PathBuf::from(format!("/tmp/ft-cod1-mcp-names-{suffix}.sqlite3"))),
        )
        .expect("build MCP server with DB");

        for name in tool_names(&server) {
            prop_assert!(
                name.starts_with("wa."),
                "tool name must live under wa.* namespace: {name}"
            );
            prop_assert!(
                name.len() > 3,
                "tool name after 'wa.' prefix must be non-empty: {name}"
            );
            let tail = &name[3..];
            prop_assert!(
                tail.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "tool name tail must be snake_case ascii: {name}"
            );
        }
    }

    #[test]
    fn build_server_resource_uris_use_wa_scheme(suffix in "[a-z0-9_-]{1,24}") {
        let server = build_server_with_db(
            &Config::default(),
            Some(PathBuf::from(format!("/tmp/ft-cod1-mcp-uri-{suffix}.sqlite3"))),
        )
        .expect("build MCP server with DB");

        for uri in resource_uris(&server) {
            prop_assert!(
                uri.starts_with("wa://"),
                "resource URI must use wa:// scheme: {uri}"
            );
            prop_assert!(
                !uri.contains('{'),
                "concrete resource URI must not contain template placeholders: {uri}"
            );
        }
        for uri in template_uris(&server) {
            prop_assert!(
                uri.starts_with("wa://"),
                "resource template URI must use wa:// scheme: {uri}"
            );
            prop_assert!(
                uri.contains('{') && uri.contains('}'),
                "resource template must contain RFC 6570 placeholders: {uri}"
            );
        }
    }

    #[test]
    fn build_server_resources_and_templates_are_disjoint(suffix in "[a-z0-9_-]{1,24}") {
        let server = build_server_with_db(
            &Config::default(),
            Some(PathBuf::from(format!("/tmp/ft-cod1-mcp-disjoint-{suffix}.sqlite3"))),
        )
        .expect("build MCP server with DB");

        let concrete = resource_uris(&server);
        let templates = template_uris(&server);

        // Concrete URIs and template URIs must not collide literally.
        let overlap: BTreeSet<String> = concrete.intersection(&templates).cloned().collect();
        prop_assert!(
            overlap.is_empty(),
            "resource URIs must not overlap with template URIs: {:?}",
            overlap
        );
    }

    #[test]
    fn build_server_is_idempotent(suffix in "[a-z0-9_-]{1,24}") {
        let db_path = PathBuf::from(format!("/tmp/ft-cod1-mcp-idem-{suffix}.sqlite3"));
        let a = build_server_with_db(&Config::default(), Some(db_path.clone()))
            .expect("build MCP server (a)");
        let b = build_server_with_db(&Config::default(), Some(db_path.clone()))
            .expect("build MCP server (b)");
        let c = build_server_with_db(&Config::default(), Some(db_path))
            .expect("build MCP server (c)");

        prop_assert_eq!(tool_names(&a), tool_names(&b));
        prop_assert_eq!(tool_names(&b), tool_names(&c));
        prop_assert_eq!(resource_uris(&a), resource_uris(&b));
        prop_assert_eq!(resource_uris(&b), resource_uris(&c));
        prop_assert_eq!(template_uris(&a), template_uris(&b));
        prop_assert_eq!(template_uris(&b), template_uris(&c));
    }

    #[test]
    fn build_server_tools_have_non_empty_descriptions(suffix in "[a-z0-9_-]{1,24}") {
        let server = build_server_with_db(
            &Config::default(),
            Some(PathBuf::from(format!("/tmp/ft-cod1-mcp-desc-{suffix}.sqlite3"))),
        )
        .expect("build MCP server with DB");

        for tool in server.tools() {
            let description = tool.description.as_deref().unwrap_or("");
            prop_assert!(
                !description.trim().is_empty(),
                "tool `{}` must have a non-empty description",
                tool.name,
            );
        }
    }
}
