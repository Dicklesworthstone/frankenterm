//! MCP server wiring for ft (feature-gated).

use std::path::Path;

use anyhow::{Context, bail};

use frankenterm_core::config::Config;

use super::McpCommands;

pub async fn run_mcp(
    cx: &frankenterm_core::cx::Cx,
    command: McpCommands,
    config: &Config,
    workspace_root: &Path,
) -> anyhow::Result<()> {
    match command {
        McpCommands::Serve { transport } => serve_mcp(cx, &transport, config, workspace_root).await,
    }
}

async fn serve_mcp(
    cx: &frankenterm_core::cx::Cx,
    transport: &str,
    config: &Config,
    workspace_root: &Path,
) -> anyhow::Result<()> {
    if transport != "stdio" {
        bail!("Unsupported transport: {transport}");
    }

    let layout = config
        .workspace_layout(Some(workspace_root))
        .context("Failed to resolve workspace layout for MCP server")?;
    frankenterm_core::mcp::run_stdio_server(cx, config, Some(layout.db_path))
        .await
        .context("Failed to start MCP stdio transport")?;
    Ok(())
}
