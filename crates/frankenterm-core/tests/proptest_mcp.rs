//! Property-based tests for the public `mcp` protocol surface.

#![cfg(feature = "mcp")]

use frankenterm_core::VERSION;
use frankenterm_core::config::Config;
use frankenterm_core::mcp::{build_server_degraded, build_server_with_db};
use frankenterm_core::mcp_framework::{
    FrameworkTestClient, framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_async::CompatRuntime;
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

struct ServerSnapshot {
    tool_names: BTreeSet<String>,
    resource_uris: BTreeSet<String>,
    template_uris: BTreeSet<String>,
}

struct ClientHarness {
    client: FrameworkTestClient,
    server_join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ClientHarness {
    fn drop(&mut self) {
        self.client.close();
        if let Some(join) = self.server_join.take() {
            let result = join.join();
            if !std::thread::panicking() {
                result.expect("MCP server thread must finish successfully");
            }
        }
    }
}

fn spawn_client(db_path: Option<PathBuf>) -> (ClientHarness, ServerSnapshot) {
    let config = Config::default();
    let (snapshot_sender, snapshot_receiver) = std::sync::mpsc::sync_channel(1);
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    let server_join = std::thread::spawn(move || {
        let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("build MCP test runtime");
        runtime.block_on(async {
            let cx = frankenterm_core::cx::Cx::current().expect("runtime-owned MCP context");
            let server = match db_path {
                Some(db_path) => build_server_with_db(&cx, &config, Some(db_path)).await,
                None => build_server_degraded(&cx, &config).await,
            }
            .expect("build MCP server");
            snapshot_sender
                .send(ServerSnapshot {
                    tool_names: tool_names(server.tools()),
                    resource_uris: resource_uris(server.resources()),
                    template_uris: template_uris(server.resource_templates()),
                })
                .expect("publish MCP metadata");
            server
                .run_transport_returning_with_cx(&cx, server_transport)
                .expect("run MCP transport");
        });
    });
    // Own cleanup before receiving metadata so startup failures also join.
    let harness = ClientHarness {
        client: FrameworkTestClient::new(client_transport),
        server_join: Some(server_join),
    };
    let snapshot = snapshot_receiver.recv().expect("receive MCP metadata");
    (harness, snapshot)
}

#[test]
fn client_harness_propagates_server_thread_failure() {
    let (client_transport, _server_transport) = framework_create_memory_transport_pair();
    let harness = ClientHarness {
        client: FrameworkTestClient::new(client_transport),
        server_join: Some(std::thread::spawn(|| panic!("server failure oracle"))),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(harness)));
    assert!(result.is_err(), "server failure must fail the owning test");
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
    let (mut harness, _snapshot) = spawn_client(None);
    let client = &mut harness.client;
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
        let (mut harness, snapshot) = spawn_client(db_path);
        let client = &mut harness.client;
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
        let (mut harness, snapshot) = spawn_client(db_path);
        let client = &mut harness.client;
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
