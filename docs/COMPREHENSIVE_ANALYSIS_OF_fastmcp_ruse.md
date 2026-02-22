# Comprehensive Analysis of fastmcp_ruse

> Analysis document for FrankenTerm bead `ft-2vuw7.26.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**fastmcp_rust** is a 105K LOC, 9-crate MCP (Model Context Protocol) framework built on asupersync for cancel-correct async. It provides a complete MCP server/client implementation with stdio, SSE, WebSocket, and HTTP transports, plus OAuth/OIDC authentication, task queues, and a rich console for diagnostics.

**Integration Value**: Very High — FrankenTerm's MCP server layer should be built on fastmcp_rust for protocol correctness and shared asupersync runtime.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~105,000 |
| **Crate Count** | 9 |
| **Rust Edition** | 2024 (nightly) |
| **MSRV** | 1.85 |
| **License** | MIT + OpenAI/Anthropic Rider |
| **Unsafe Code** | `#![forbid(unsafe_code)]` |

### Workspace Structure

```
fastmcp_rust/
├── crates/
│   ├── fastmcp/           # Facade (re-exports)
│   ├── fastmcp-core/      # Context, errors, outcome (3.1K LOC)
│   ├── fastmcp-protocol/  # JSON-RPC + MCP types (5.1K LOC)
│   ├── fastmcp-transport/ # stdio, SSE, WebSocket, HTTP (7.7K LOC)
│   ├── fastmcp-server/    # Server, router, handlers, auth (22K LOC)
│   ├── fastmcp-client/    # Client library (1.6K LOC)
│   ├── fastmcp-macros/    # #[tool], #[resource], #[prompt] (2.5K LOC)
│   ├── fastmcp-console/   # Rich stderr output, stats (8K LOC)
│   └── fastmcp-cli/       # run, inspect, install (3.5K LOC)
```

---

## Core Architecture

### Layered Design

```
fastmcp (facade)
  ├── fastmcp-core       (McpContext wraps Cx, errors, Outcome<T,E>)
  ├── fastmcp-protocol   (JSON-RPC 2.0, MCP domain model)
  ├── fastmcp-transport   (Transport trait, NDJSON codec)
  ├── fastmcp-server     (ServerBuilder, Router, handlers)
  ├── fastmcp-client     (Client, stdio spawning)
  ├── fastmcp-macros     (attribute macros → handler impls + JSON schema)
  └── fastmcp-console    (rich rendering, stats, diagnostics)
```

### Key Abstractions

- **McpContext**: Wraps `asupersync::Cx` with request lifetime, budget, session state
- **Outcome<T,E>**: 4-valued type (Ok/Err/Cancelled/Panicked) vs Result's 2
- **Handler traits**: ToolHandler, ResourceHandler, PromptHandler (sync + async)
- **Router**: Method dispatch table, composable via `mount()`
- **Budget**: Deadline + poll/cost quotas (superior to simple timeouts)

### Cancel-Correct Semantics

- Handlers call `ctx.checkpoint()` for cooperative cancellation
- Budget exhaustion → `McpError::request_cancelled()`
- Region-scoped cleanup ensures no orphaned tasks
- `ctx.masked(fn)` for critical sections without cancellation

### Transports

| Transport | Protocol | Use Case |
|-----------|----------|----------|
| stdio | NDJSON | Claude Desktop, CLI tools |
| SSE | Server-Sent Events | Web clients |
| WebSocket | ws:// | Bidirectional real-time |
| HTTP | REST | Stateless API calls |
| memory | In-process | Testing |

---

## FrankenTerm Integration Assessment

### Shared Foundation

| Aspect | FrankenTerm | fastmcp_rust | Bridge |
|--------|------------|-------------|--------|
| Async runtime | asupersync 0.2 | asupersync 0.2 | Shared (no nesting) |
| Context | Cx | McpContext wraps Cx | `ctx.cx()` |
| Cancellation | Budget, checkpoint() | Same | Identical |
| Errors | Error::Runtime(String) | McpError | Manual mapping |
| Outcome | 4-valued | 4-valued | Direct interop |
| Spawning | ctx.spawn() | ctx.spawn() | Same API |

### Integration Pattern

FrankenTerm should use fastmcp_rust as its MCP server framework:
1. `Server::new("frankenterm", version)` as the MCP server
2. Register FrankenTerm's MCP tools via `#[tool]` macro
3. Use `McpContext` to pass cancellation through to FrankenTerm operations
4. Use stdio transport for Claude Desktop integration
5. Use SSE/WebSocket for web dashboard

### MCP Tools Registration Example

```rust
#[tool(name = "ft_search", description = "Search FrankenTerm sessions")]
async fn search(ctx: &McpContext, query: String, limit: usize) -> McpResult<Vec<SearchResult>> {
    ctx.checkpoint()?;
    // ... FrankenTerm search logic
}
```

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| asupersync version drift | Medium | High | Pin workspace dependency |
| Nightly requirement | Medium | Medium | fastmcp requires nightly; FrankenTerm may not |
| Large dependency (105K LOC) | Low | Medium | Feature-gate transports |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | 9-crate MCP framework with cancel-correct async |
| **Key Innovation** | 4-valued Outcome type + budget-based cancellation |
| **FrankenTerm Status** | No direct integration yet |
| **Integration Priority** | Very High — natural MCP server layer |
| **New Modules Needed** | 1 (MCP tool registration bridge) |
| **Shared Foundation** | asupersync runtime (identical version) |

---

*Analysis complete.*
