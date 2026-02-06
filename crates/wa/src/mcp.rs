//! MCP server wiring for wa (feature-gated).

use std::path::Path;

use anyhow::{Context, bail};
use fastmcp::StdioTransport;

use wa_core::config::Config;

use super::McpCommands;

pub fn run_mcp(command: McpCommands, config: &Config, workspace_root: &Path) -> anyhow::Result<()> {
    match command {
        McpCommands::Serve { transport } => serve_mcp(&transport, config, workspace_root),
    }
}

fn serve_mcp(transport: &str, config: &Config, workspace_root: &Path) -> anyhow::Result<()> {
    if transport != "stdio" {
        bail!("Unsupported transport: {transport}");
    }

    let layout = config
        .workspace_layout(Some(workspace_root))
        .context("Failed to resolve workspace layout for MCP server")?;
    let server = wa_core::mcp::build_server_with_db(config, Some(layout.db_path))?;
    let transport = StdioTransport::stdio();
    server.run_transport(transport);
}
