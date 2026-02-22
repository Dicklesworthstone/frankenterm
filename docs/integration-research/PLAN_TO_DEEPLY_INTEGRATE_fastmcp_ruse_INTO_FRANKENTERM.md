# Plan to Deeply Integrate fastmcp_ruse into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.26.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_fastmcp_ruse.md (ft-2vuw7.26.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **MCP server framework**: Use fastmcp_rust as FrankenTerm's MCP server implementation
2. **Tool registration via macros**: Register FrankenTerm's MCP tools using `#[tool]`, `#[resource]`, `#[prompt]` macros
3. **Cancel-correct handlers**: Leverage McpContext + Budget for cooperative cancellation in MCP tool execution
4. **Multi-transport support**: stdio for Claude Desktop, SSE/WebSocket for web dashboard

### Constraints

- **Shared asupersync 0.2**: Both projects use asupersync; versions must stay aligned
- **Nightly Rust**: fastmcp_rust requires nightly; check FrankenTerm compatibility
- **Feature-gated**: Behind `mcp-server` feature flag
- **Existing MCP code**: FrankenTerm has `mcp.rs`; may need migration or coexistence

### Non-Goals

- **Replacing all MCP logic**: Gradual migration from existing mcp.rs
- **Custom transport implementation**: Use fastmcp_rust's provided transports
- **OAuth/OIDC setup**: Not needed for local MCP server

---

## P2: Evaluate Integration Patterns

### Option A: Workspace Dependency (Chosen)

Add fastmcp_rust as optional workspace dependency via git path.

**Pros**: Cancel-correct MCP, proc macro tools, multi-transport, shared asupersync
**Cons**: Large dependency (105K LOC), nightly requirement, asupersync coupling
**Chosen**: Natural MCP framework for FrankenTerm

### Option B: Keep Existing mcp.rs

Continue with manual MCP implementation.

**Pros**: No new dependency, full control
**Cons**: Missing cancel-correctness, no proc macros, manual JSON-RPC
**Rejected**: fastmcp_rust provides these correctly

### Decision: Option A — Workspace dependency

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── mcp_bridge.rs        # NEW: fastmcp_rust integration layer
```

### Module Responsibilities

#### `mcp_bridge.rs`

- `FrankenTermMcpServer` — fastmcp Server configured with FrankenTerm tools
- Tool implementations: search, pane management, recorder query, agent coordination
- McpContext → FrankenTerm context bridge
- Error mapping: McpError ↔ frankenterm_core::Error

---

## P4: API Contract

```rust
#[cfg(feature = "mcp-server")]
pub mod mcp_bridge {
    pub struct FrankenTermMcpServer {
        server: fastmcp::Server,
    }

    impl FrankenTermMcpServer {
        pub fn new(name: &str, version: &str, state: McpState) -> Self;
        pub async fn run_stdio(self) -> !;
        pub async fn run_sse(self, addr: &str) -> Result<()>;
    }

    pub struct McpState {
        pub recorder: RecorderHandle,
        pub pane_map: PaneMap,
        pub search: Option<SearchEngine>,
    }
}
```

### Example Tool Registration

```rust
#[tool(name = "ft_search", description = "Search FrankenTerm sessions")]
async fn ft_search(ctx: &McpContext, query: String, limit: usize) -> McpResult<Value> {
    ctx.checkpoint()?;
    let results = state.search.query(&query, limit).await?;
    Ok(serde_json::to_value(results)?)
}
```

---

## P5-P8: Testing, Rollout

**Migration**: Gradual — existing mcp.rs tools move to `#[tool]` macro registration over time.

**Tests**: 30+ tests using fastmcp's memory transport for in-process MCP testing.

**Rollout**: Phase 1 (mcp_bridge.rs with core tools) → Phase 2 (migrate existing tools) → Phase 3 (SSE/WebSocket transport) → Phase 4 (deprecate old mcp.rs).

**Rollback**: Disable `mcp-server` feature; existing mcp.rs continues working.

### Summary

**Workspace dependency** on fastmcp_rust for MCP server framework. One new module: `mcp_bridge.rs`. Shared asupersync runtime. Cancel-correct tool execution. Feature-gated behind `mcp-server`.

---

*Plan complete.*
