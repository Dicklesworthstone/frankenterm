//! Property-based tests for the public `mcp` protocol surface.

#![cfg(feature = "mcp")]

use frankenterm_core::VERSION;
use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkTestClient, framework_create_memory_transport_pair,
};
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

struct ServerSnapshot {
    tool_names: BTreeSet<String>,
    resource_uris: BTreeSet<String>,
    template_uris: BTreeSet<String>,
}

fn spawn_client(db_path: Option<PathBuf>) -> (FrameworkTestClient, ServerSnapshot) {
    let server = build_server_with_db(&Config::default(), db_path).expect("build MCP server");
    let snapshot = ServerSnapshot {
        tool_names: tool_names(server.tools()),
        resource_uris: resource_uris(server.resources()),
        template_uris: template_uris(server.resource_templates()),
    };
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    std::thread::spawn(move || {
        let _ = server.run_transport(server_transport);
    });
    (FrameworkTestClient::new(client_transport), snapshot)
}

fn tool_names(
    tools: impl IntoIterator<Item = frankenterm_core::mcp_framework::FrameworkTool>,
) -> BTreeSet<String> {
    tools.into_iter().map(|tool| tool.name).collect()
}

fn resource_uris(
    resources: impl IntoIterator<Item = frankenterm_core::mcp_framework::FrameworkResource>,
) -> BTreeSet<String> {
    resources.into_iter().map(|resource| resource.uri).collect()
}

fn template_uris(
    templates: impl IntoIterator<Item = frankenterm_core::mcp_framework::FrameworkResourceTemplate>,
) -> BTreeSet<String> {
    templates
        .into_iter()
        .map(|template| template.uri_template)
        .collect()
}

#[test]
fn initialize_reports_expected_server_identity_and_instructions() {
    let (mut client, _snapshot) = spawn_client(None);
    let init = client
        .initialize()
        .expect("initialize in-memory MCP client");

    assert_eq!(init.server_info.name, "wezterm-automata");
    assert_eq!(init.server_info.version, VERSION);
    assert_eq!(
        init.instructions.as_deref(),
        Some("ft MCP server (robot parity). See docs/mcp-api-spec.md.")
    );
    assert_eq!(
        client.server_info().map(|info| info.name.as_str()),
        Some("wezterm-automata")
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn in_memory_client_lists_same_tools_as_server(
        use_db in any::<bool>(),
        suffix in "[a-z0-9_-]{1,24}",
    ) {
        let db_path = use_db.then(|| PathBuf::from(format!("/tmp/ft-cod1-mcp-proto-{suffix}.sqlite3")));
        let (mut client, snapshot) = spawn_client(db_path);
        client.initialize().expect("initialize in-memory MCP client");

        let listed = tool_names(client.list_tools().expect("list tools"));

        prop_assert_eq!(listed, snapshot.tool_names);
    }

    #[test]
    fn in_memory_client_lists_same_resources_and_templates_as_server(
        use_db in any::<bool>(),
        suffix in "[a-z0-9_-]{1,24}",
    ) {
        let db_path = use_db.then(|| PathBuf::from(format!("/tmp/ft-cod1-mcp-proto-res-{suffix}.sqlite3")));
        let (mut client, snapshot) = spawn_client(db_path);
        client.initialize().expect("initialize in-memory MCP client");

        let listed_resources = resource_uris(client.list_resources().expect("list resources"));
        let listed_templates = template_uris(
            client
                .list_resource_templates()
                .expect("list resource templates"),
        );

        prop_assert_eq!(listed_resources, snapshot.resource_uris);
        prop_assert_eq!(listed_templates, snapshot.template_uris);
    }
}
