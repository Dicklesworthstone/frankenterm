# Plan to Deeply Integrate fastmcp_ruse into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.26.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_fastmcp_ruse.md (ft-2vuw7.26.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Replace hand-rolled MCP server with fastmcp framework**: Migrate the existing 7,035-line `mcp.rs` (which manually implements `ToolHandler` and `ResourceHandler` for ~20 tools and ~12 resources) to use fastmcp's `ServerBuilder`, middleware pipeline, and request routing. This eliminates the custom `FormatAwareToolHandler<T>`, `AuditedToolHandler<T>` wrapper pattern and leverages fastmcp's built-in middleware system for format negotiation and audit logging.

2. **Add MCP client capability for external tool orchestration**: FrankenTerm currently acts only as an MCP server. Adding `fastmcp-client` enables FrankenTerm to connect to and orchestrate external MCP servers (filesystem, git, coding assistants) as sub-processes, using `Client::stdio()` to spawn and manage them. This is critical for swarm-native agent coordination where panes may host MCP-capable tools.

3. **Adopt proc macros for tool definition**: Replace the ~4,800 lines of boilerplate `ToolHandler` and `ResourceHandler` implementations with `#[tool]`, `#[resource]`, and `#[prompt]` proc macro attributes from `fastmcp-derive`. Each tool definition collapses from ~80-150 lines (struct + impl + schema + validation) to ~20-40 lines (annotated function + typed parameters). JSON Schema generation, input validation, and async/sync dispatch are handled automatically.

4. **Enable proxy/composition for multi-server MCP topologies**: Use `fastmcp-server::proxy` to mount external MCP servers into FrankenTerm's tool namespace. A FrankenTerm instance can proxy tools from CASS (coding agent session search), storage-ballast, or other MCP-enabled services through a single unified MCP surface, using `ProxyCatalog` and `ProxyClient`.

5. **Adopt cancel-correct request handling via McpContext**: Replace the current fire-and-forget tool execution model with fastmcp's structured concurrency pattern where every tool receives an `McpContext` wrapping an `asupersync::Cx`. This enables cooperative cancellation (via `ctx.checkpoint()?`), per-tool timeouts (via `Budget`), progress reporting, and the 4-valued `McpOutcome<T>` (Ok/Err/Cancelled/Panicked) error model.

### Constraints

- **Shared `asupersync 0.2` runtime**: Both FrankenTerm and fastmcp_rust depend on `asupersync 0.2`. Versions must remain aligned. FrankenTerm currently pins asupersync via `[patch]` sections to rev `c7c15f6` -- fastmcp_rust must use the same revision to avoid duplicate crate instances. This is already handled in the workspace `Cargo.toml` via `[patch."https://github.com/Dicklesworthstone/fastmcp_rust"]`.

- **Nightly Rust toolchain**: fastmcp_rust requires nightly (`rust-toolchain.toml` specifies nightly, MSRV 1.85). FrankenTerm already uses edition 2024 with `rust-version = "1.85"`, so this is compatible. However, the nightly requirement must be documented in FrankenTerm's build prerequisites and CI configuration.

- **Feature-gated behind `mcp-server`**: All fastmcp integration lives behind the existing `mcp-server` feature flag (defined in `crates/frankenterm-core/Cargo.toml` as `mcp-server = ["dep:fastmcp", "dep:toon_rust"]`). The default build compiles without MCP support. A new `mcp-client` feature will be added for the client-side capability.

- **`#![forbid(unsafe_code)]` in both projects**: Both FrankenTerm's core library and all fastmcp crates enforce `#![forbid(unsafe_code)]`. Integration code must maintain this invariant. No `unsafe` blocks are permitted.

- **Existing `mcp.rs` must coexist during migration**: The 7,035-line `mcp.rs` contains production-deployed tool implementations with specific error code contracts (`FT-MCP-0001` through `FT-MCP-0014`), the `McpEnvelope` response wrapper, TOON format negotiation, and storage/policy/workflow integration. This cannot be replaced atomically -- the migration must preserve API compatibility at every step.

- **Console dependency weight**: `fastmcp-server` depends on `fastmcp-console`, which pulls in `rich_rust`, `tracing-subscriber`, `strip-ansi-escapes`, and `time`. FrankenTerm already has its own tracing infrastructure. Upstream tweak R8.4 (decouple console from server) would reduce this burden, but until then, the extra dependencies are acceptable given they are already transitively present.

### Non-Goals

- **Full OAuth/OIDC deployment**: FrankenTerm's MCP server runs locally over stdio or domain sockets. OAuth 2.1 and OpenID Connect capabilities in fastmcp-server are not needed and will not be configured.

- **Redis-backed task queue**: The `Docket` distributed task system in `fastmcp-server` is designed for cloud deployments. FrankenTerm uses its own workflow engine (`WorkflowEngine` + `WorkflowRunner`) and will not adopt Docket.

- **Replacing FrankenTerm's workflow/policy engines**: fastmcp provides middleware and auth primitives, but FrankenTerm's `PolicyEngine`, `ApprovalStore`, `WorkflowEngine`, and `PatternEngine` are purpose-built for terminal automation safety. These remain as-is and are exposed through MCP tools.

- **Migrating to fastmcp-console for output**: FrankenTerm has its own TUI (`ftui`), logging, and output infrastructure. The `fastmcp-console` crate (startup banners, traffic visualization) will not be surfaced to users -- it is only pulled in as a transitive dependency.

- **WebSocket/SSE transport in initial phases**: While fastmcp provides SSE and WebSocket transport, FrankenTerm's initial integration focuses on stdio transport (for Claude Desktop, Cursor, Cline integration). Network transports are deferred to Phase 4.

---

## P2: Evaluate Integration Patterns

### Option A: Library Import (fastmcp-server + fastmcp-core + fastmcp-client)

Import `fastmcp-server`, `fastmcp-core`, `fastmcp-protocol`, `fastmcp-client`, and `fastmcp-derive` as workspace dependencies. Use the `Server` builder to construct the MCP server, register tools via proc macros and handler traits, and use `Client` to connect to external MCP servers.

**Pros**:
- Cancel-correct structured concurrency via `McpContext` + `asupersync::Cx`
- Proc macro tool definitions eliminate ~4,800 lines of handler boilerplate
- Built-in middleware pipeline replaces custom `FormatAwareToolHandler` and `AuditedToolHandler` wrappers
- JSON Schema validation, pagination, and error masking come for free
- `MemoryTransport` enables in-process MCP testing without spawning subprocesses
- Server composition via `ProxyCatalog` for multi-server topologies
- MCP client capability for external tool orchestration
- Shared `asupersync` runtime means zero overhead for async boundary crossings
- `#![forbid(unsafe_code)]` alignment

**Cons**:
- Large transitive dependency surface (~105K LOC across all fastmcp crates)
- `fastmcp-server` hard-depends on `fastmcp-console` (brings `rich_rust`, `tracing-subscriber`)
- Nightly Rust requirement propagated to FrankenTerm's build chain
- `asupersync` version coupling requires coordinated patch sections
- Migration effort: existing 20+ tools and 12+ resources must be ported
- Console/logging initialization may conflict with FrankenTerm's existing `tracing` setup

### Option B: Subprocess CLI

Run `fastmcp-cli` as a separate binary process. FrankenTerm spawns it as a sidecar and communicates over stdio or SSE.

**Pros**:
- Process isolation -- fastmcp bugs cannot crash FrankenTerm
- Independent versioning and deployment
- Simple integration: just spawn a child process

**Cons**:
- IPC latency for every tool call (JSON serialization + process boundary)
- Loses direct access to FrankenTerm internals -- tools cannot call `PolicyEngine`, `StorageHandle`, etc.
- Double serialization: FrankenTerm state -> JSON -> fastmcp -> JSON -> response
- Cannot use proc macros for FrankenTerm-specific tools
- Deployment complexity: two binaries instead of one
- Cannot share `asupersync` runtime or structured concurrency

**Rejected**: FrankenTerm's tools fundamentally need direct access to internal state (pane maps, storage, policy engine, workflow runner). Process isolation defeats the purpose.

### Option C: Shared Protocol Types Only (fastmcp-protocol + fastmcp-core)

Import only `fastmcp-protocol` (wire format types) and `fastmcp-core` (error model, context) without the full server framework. Continue using FrankenTerm's custom server loop but with standard MCP types.

**Pros**:
- Minimal dependency surface (~12K LOC for core + protocol)
- Standard MCP types without the server framework weight
- Full control over request routing and handler dispatch
- No `fastmcp-console` dependency

**Cons**:
- No proc macros (still need manual `ToolHandler` implementations)
- No built-in middleware pipeline (keep custom wrappers)
- No server builder pattern
- No MCP client capability
- No proxy/composition support
- Must maintain custom JSON-RPC routing and session management
- Forgoes cancel-correct handler execution

**Rejected**: This provides types but not the framework value. FrankenTerm already uses the full `fastmcp` facade -- stepping down to protocol-only would be a regression.

### Decision: Option A -- Library Import

**Rationale**: FrankenTerm already depends on the full `fastmcp-rust` facade crate. The integration path is to deepen this dependency -- using the server builder, proc macros, middleware, and client properly -- rather than fighting against it with manual implementations. The shared `asupersync` runtime eliminates the most common integration friction point (async runtime mismatch). The ~60% boilerplate reduction from proc macros alone justifies the migration effort.

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Overview

```
frankenterm-core/
|-- src/
|   |-- mcp.rs                    # EXISTING: Current 7,035-line MCP implementation
|   |                             #   Retained during migration, gradually hollowed out
|   |
|   |-- mcp_bridge.rs             # NEW: fastmcp Server builder + tool registration
|   |                             #   Replaces build_server() / build_server_with_db()
|   |
|   |-- mcp_tools.rs              # NEW: Proc-macro-based tool handlers
|   |                             #   Migrated from mcp.rs manual ToolHandler impls
|   |
|   |-- mcp_resources.rs          # NEW: Proc-macro-based resource handlers
|   |                             #   Migrated from mcp.rs manual ResourceHandler impls
|   |
|   |-- mcp_middleware.rs          # NEW: Middleware implementations
|   |                             #   FormatNegotiation + AuditTrail as Middleware trait
|   |
|   |-- mcp_client.rs             # NEW: MCP client for external tool orchestration
|   |                             #   Wraps fastmcp-client::Client with FrankenTerm config
|   |
|   |-- mcp_proxy.rs              # NEW: Proxy/composition for external MCP servers
|   |                             #   CASS, storage-ballast, custom tool servers
|   |
|   |-- mcp_error.rs              # NEW: Error mapping layer
|   |                             #   McpError <-> frankenterm_core::Error bidirectional
|   |
|   |-- lib.rs                    # MODIFIED: Add new module declarations
|-- Cargo.toml                    # MODIFIED: Add fastmcp-client, fastmcp-derive deps
```

### How Existing `mcp.rs` Gets Migrated

The migration is incremental. The existing `mcp.rs` defines:
- `build_server()` / `build_server_with_db()` -- server construction
- `run_stdio_server()` -- transport binding
- 20 `ToolHandler` implementations (WaStateTool, WaGetTextTool, WaSendTool, etc.)
- 12 `ResourceHandler` implementations (WaPanesResource, WaEventsResource, etc.)
- `FormatAwareToolHandler<T>` -- format negotiation wrapper
- `AuditedToolHandler<T>` -- audit logging wrapper
- `McpEnvelope` -- standardized response wrapper
- `McpToolError` -- error mapping helpers
- 30+ test functions

**Phase 1**: `mcp_bridge.rs` wraps the existing `build_server_with_db()` function, adding no new behavior. Tests verify equivalence.

**Phase 2**: `mcp_middleware.rs` extracts `FormatAwareToolHandler` and `AuditedToolHandler` logic into proper `Middleware` trait implementations. The wrappers in `mcp.rs` are replaced with middleware registration on the server builder.

**Phase 3**: Tools are migrated one-by-one from `mcp.rs` into `mcp_tools.rs` using `#[tool]` macros. Each migration is atomic (one PR per tool) and validated by comparing MCP protocol output before/after.

**Phase 4**: Resources are migrated into `mcp_resources.rs`. After all handlers are migrated, the old struct-based implementations in `mcp.rs` are removed.

**Phase 5**: `mcp_client.rs` and `mcp_proxy.rs` add new capabilities that did not exist before.

### Module Responsibilities

#### `mcp_bridge.rs` -- Server Construction and Lifecycle

Owns the `FtMcpServer` struct that wraps `fastmcp::Server`. Responsible for:
- Building the server via `ServerBuilder` with all tools, resources, middleware
- Injecting `FtMcpState` (Config, StorageHandle, PolicyEngine, etc.) into `SessionState`
- Exposing `run_stdio()`, `run_transport()` entry points
- Feature-gated behind `#[cfg(feature = "mcp-server")]`

#### `mcp_tools.rs` -- Tool Handler Definitions

Contains all `#[tool]`-annotated functions, organized by domain:
- Pane management: `ft_state`, `ft_get_text`, `ft_send`, `ft_wait_for`
- Search: `ft_search`
- Events: `ft_events`, `ft_events_annotate`, `ft_events_triage`, `ft_events_label`
- Workflows: `ft_workflow_run`
- Rules: `ft_rules_list`, `ft_rules_test`
- CASS: `ft_cass_search`, `ft_cass_view`, `ft_cass_status`
- Reservations: `ft_reservations`, `ft_reserve`, `ft_release`
- Accounts: `ft_accounts`, `ft_accounts_refresh`

#### `mcp_resources.rs` -- Resource Handler Definitions

Contains all `#[resource]`-annotated functions:
- Static: `ft://panes`, `ft://events`, `ft://workflows`, `ft://rules`, `ft://accounts`, `ft://reservations`
- Templated: `ft://events/{pane_id}`, `ft://events/unhandled/{pane_id}`, `ft://accounts/{service}`, `ft://rules/{agent}`, `ft://reservations/{pane_id}`

#### `mcp_middleware.rs` -- Middleware Pipeline

Two middleware implementations:
- `FormatNegotiationMiddleware` -- extracts `format` param, transcodes response to TOON
- `AuditMiddleware` -- logs tool invocations to storage audit trail

#### `mcp_client.rs` -- Outbound MCP Client

Wraps `fastmcp_client::Client` for connecting to external MCP servers:
- `FtMcpClient::connect_stdio(command, args)` -- spawn subprocess
- Tool discovery and invocation
- Config-driven server list from `mcp_config::ConfigLoader`

#### `mcp_proxy.rs` -- Server Composition

Uses `ProxyCatalog` to mount external server tools:
- CASS server proxy
- Storage-ballast server proxy
- Custom user-configured MCP servers

#### `mcp_error.rs` -- Error Mapping

Bidirectional conversion between `frankenterm_core::Error` and `McpError`:
- Maps FrankenTerm error variants to `McpErrorCode` values
- Preserves error codes `FT-MCP-0001` through `FT-MCP-0014`
- Implements `From<Error> for McpError` and `From<McpError> for Error`

---

## P4: API Contracts

### Server Construction (mcp_bridge.rs)

```rust
#[cfg(feature = "mcp-server")]
pub mod mcp_bridge {
    use std::path::PathBuf;
    use std::sync::Arc;

    use fastmcp::Server;

    use crate::config::Config;
    use crate::Result;

    /// Shared state injected into all MCP tool handlers via SessionState.
    #[derive(Clone)]
    pub struct FtMcpState {
        pub config: Arc<Config>,
        pub db_path: Option<Arc<PathBuf>>,
    }

    /// Build the FrankenTerm MCP server with all tools, resources, and middleware.
    ///
    /// Feature: `mcp-server`
    pub fn build_server(config: &Config) -> Result<Server> {
        build_server_with_db(config, None)
    }

    /// Build the MCP server with explicit storage path.
    ///
    /// Feature: `mcp-server`
    pub fn build_server_with_db(
        config: &Config,
        db_path: Option<PathBuf>,
    ) -> Result<Server> {
        let state = FtMcpState {
            config: Arc::new(config.clone()),
            db_path: db_path.map(Arc::new),
        };

        let mut builder = Server::new("frankenterm", crate::VERSION)
            .instructions(
                "FrankenTerm MCP server: swarm-native terminal platform for AI agent fleets."
            )
            .request_timeout(30)
            .middleware(FormatNegotiationMiddleware::new())
            .middleware(AuditMiddleware::new(state.db_path.clone()))
            .on_startup(|| -> std::result::Result<(), std::io::Error> {
                tracing::info!("FrankenTerm MCP server starting");
                Ok(())
            })
            .on_shutdown(|| {
                tracing::info!("FrankenTerm MCP server shutting down");
            });

        // Register all tools
        builder = crate::mcp_tools::register_tools(builder, &state);

        // Register all resources
        builder = crate::mcp_resources::register_resources(builder, &state);

        Ok(builder.build())
    }

    /// Run the MCP server over stdio transport.
    ///
    /// Feature: `mcp-server`
    pub fn run_stdio_server(config: &Config, db_path: Option<PathBuf>) -> Result<()> {
        let server = build_server_with_db(config, db_path)?;
        let transport = fastmcp::StdioTransport::stdio();
        server.run_transport(transport);
        Ok(())
    }
}
```

### Tool Registration (mcp_tools.rs)

```rust
#[cfg(feature = "mcp-server")]
pub mod mcp_tools {
    use fastmcp::prelude::*;
    use fastmcp_derive::tool;
    use serde::{Deserialize, Serialize};

    use crate::mcp_bridge::FtMcpState;
    use crate::mcp_error::to_mcp_error;

    /// Register all FrankenTerm tools on the server builder.
    pub fn register_tools(
        mut builder: ServerBuilder,
        state: &FtMcpState,
    ) -> ServerBuilder {
        builder = builder
            .tool(FtStateTool::new(state.clone()))
            .tool(FtGetTextTool::new(state.clone()))
            .tool(FtSendTool::new(state.clone()))
            .tool(FtWaitForTool::new(state.clone()))
            .tool(FtSearchTool::new(state.clone()))
            .tool(FtEventsTool::new(state.clone()))
            .tool(FtWorkflowRunTool::new(state.clone()))
            .tool(FtRulesListTool::new(state.clone()))
            .tool(FtCassSearchTool::new(state.clone()));
        // ... remaining tools
        builder
    }

    // Example: ft.state tool using proc macro
    //
    // The #[tool] macro generates ToolHandler impl, JSON Schema,
    // and input validation automatically.

    #[derive(Debug, Deserialize)]
    pub struct StateParams {
        /// Filter by WezTerm domain name.
        pub domain: Option<String>,
        /// Filter by agent name/type.
        pub agent: Option<String>,
        /// Filter by specific pane ID.
        pub pane_id: Option<u64>,
    }

    #[derive(Debug, Serialize)]
    pub struct FtStateResult {
        pub version: String,
        pub panes: Vec<PaneState>,
        pub total_panes: usize,
        pub elapsed_ms: u64,
    }

    #[derive(Debug, Serialize)]
    pub struct PaneState {
        pub pane_id: u64,
        pub tab_id: u64,
        pub window_id: u64,
        pub domain: String,
        pub title: Option<String>,
        pub cwd: Option<String>,
        pub observed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub ignore_reason: Option<String>,
    }

    // For tools that need complex state access, we use struct-based handlers
    // that implement ToolHandler directly, with #[tool] for simpler tools.

    pub struct FtStateTool {
        state: FtMcpState,
    }

    impl FtStateTool {
        pub fn new(state: FtMcpState) -> Self {
            Self { state }
        }
    }

    impl ToolHandler for FtStateTool {
        fn definition(&self) -> Tool {
            Tool {
                name: "ft.state".to_string(),
                description: Some(
                    "List observed panes and their current state. \
                     Optionally filter by domain, agent, or pane_id."
                        .to_string(),
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "domain": {
                            "type": "string",
                            "description": "Filter by WezTerm domain"
                        },
                        "agent": {
                            "type": "string",
                            "description": "Filter by agent name"
                        },
                        "pane_id": {
                            "type": "integer",
                            "description": "Filter by pane ID"
                        }
                    }
                }),
                ..Default::default()
            }
        }

        fn call(
            &self,
            ctx: &McpContext,
            arguments: serde_json::Value,
        ) -> McpResult<Vec<Content>> {
            let start = std::time::Instant::now();
            let params: StateParams = serde_json::from_value(arguments)
                .map_err(|e| McpError::invalid_params(e.to_string()))?;

            // ... implementation using self.state.config, wezterm handle, etc.

            todo!("Implementation migrated from mcp.rs WaStateTool")
        }
    }
}
```

### Middleware (mcp_middleware.rs)

```rust
#[cfg(feature = "mcp-server")]
pub mod mcp_middleware {
    use fastmcp::prelude::*;

    /// Middleware that intercepts the `format` parameter from tool arguments
    /// and transcodes the response to TOON format if requested.
    ///
    /// Feature: `mcp-server`
    pub struct FormatNegotiationMiddleware;

    impl FormatNegotiationMiddleware {
        pub fn new() -> Self {
            Self
        }
    }

    impl Middleware for FormatNegotiationMiddleware {
        fn on_request(
            &self,
            _ctx: &McpContext,
            request: &JsonRpcRequest,
        ) -> McpResult<MiddlewareDecision> {
            // Extract and stash format preference in session state
            if request.method == "tools/call" {
                if let Some(args) = request.params.get("arguments") {
                    if let Some(format) = args.get("format").and_then(|v| v.as_str()) {
                        // Store format preference for on_response
                        // (Implementation detail: use request-scoped state)
                        let _ = format;
                    }
                }
            }
            Ok(MiddlewareDecision::Continue)
        }

        fn on_response(
            &self,
            _ctx: &McpContext,
            request: &JsonRpcRequest,
            response: serde_json::Value,
        ) -> McpResult<serde_json::Value> {
            // If format=toon was requested, transcode text content to TOON
            // (Implementation migrated from FormatAwareToolHandler)
            Ok(response)
        }

        fn on_error(
            &self,
            _ctx: &McpContext,
            _request: &JsonRpcRequest,
            error: McpError,
        ) -> McpError {
            error
        }
    }

    /// Middleware that logs all tool invocations to the storage audit trail.
    ///
    /// Feature: `mcp-server`
    pub struct AuditMiddleware {
        db_path: Option<std::sync::Arc<std::path::PathBuf>>,
    }

    impl AuditMiddleware {
        pub fn new(db_path: Option<std::sync::Arc<std::path::PathBuf>>) -> Self {
            Self { db_path }
        }
    }

    impl Middleware for AuditMiddleware {
        fn on_request(
            &self,
            _ctx: &McpContext,
            request: &JsonRpcRequest,
        ) -> McpResult<MiddlewareDecision> {
            if request.method == "tools/call" {
                if let Some(ref _db_path) = self.db_path {
                    // Record tool invocation in audit trail
                    // (Implementation migrated from AuditedToolHandler)
                }
            }
            Ok(MiddlewareDecision::Continue)
        }

        fn on_response(
            &self,
            _ctx: &McpContext,
            _request: &JsonRpcRequest,
            response: serde_json::Value,
        ) -> McpResult<serde_json::Value> {
            Ok(response)
        }

        fn on_error(
            &self,
            _ctx: &McpContext,
            _request: &JsonRpcRequest,
            error: McpError,
        ) -> McpError {
            error
        }
    }
}
```

### Error Mapping (mcp_error.rs)

```rust
#[cfg(feature = "mcp-server")]
pub mod mcp_error {
    use fastmcp::prelude::*;

    use crate::error::Error;

    /// FrankenTerm MCP error codes.
    pub const FT_MCP_INVALID_ARGS: &str = "FT-MCP-0001";
    pub const FT_MCP_CONFIG: &str = "FT-MCP-0003";
    pub const FT_MCP_WEZTERM: &str = "FT-MCP-0004";
    pub const FT_MCP_STORAGE: &str = "FT-MCP-0005";
    pub const FT_MCP_POLICY: &str = "FT-MCP-0006";
    pub const FT_MCP_PANE_NOT_FOUND: &str = "FT-MCP-0007";
    pub const FT_MCP_WORKFLOW: &str = "FT-MCP-0008";
    pub const FT_MCP_TIMEOUT: &str = "FT-MCP-0009";
    pub const FT_MCP_NOT_IMPLEMENTED: &str = "FT-MCP-0010";
    pub const FT_MCP_FTS_QUERY: &str = "FT-MCP-0011";
    pub const FT_MCP_RESERVATION_CONFLICT: &str = "FT-MCP-0012";
    pub const FT_MCP_CAUT: &str = "FT-MCP-0013";
    pub const FT_MCP_CASS: &str = "FT-MCP-0014";

    /// Convert a frankenterm_core::Error to an McpError with the
    /// appropriate FT-MCP error code.
    pub fn to_mcp_error(err: Error) -> McpError {
        match &err {
            Error::Wezterm(_) => McpError::internal_error(format!(
                "[{FT_MCP_WEZTERM}] {err}"
            )),
            Error::Storage(_) => McpError::internal_error(format!(
                "[{FT_MCP_STORAGE}] {err}"
            )),
            Error::Config(_) => McpError::internal_error(format!(
                "[{FT_MCP_CONFIG}] {err}"
            )),
            Error::Runtime(msg) if msg.contains("timeout") => {
                McpError::internal_error(format!(
                    "[{FT_MCP_TIMEOUT}] {err}"
                ))
            }
            _ => McpError::internal_error(err.to_string()),
        }
    }

    /// Structured tool error with code, message, and optional hint.
    ///
    /// Preserves the FT-MCP error code contract from the existing mcp.rs.
    #[derive(Debug)]
    pub struct FtToolError {
        pub code: &'static str,
        pub message: String,
        pub hint: Option<String>,
    }

    impl From<FtToolError> for McpError {
        fn from(err: FtToolError) -> Self {
            let msg = if let Some(hint) = &err.hint {
                format!("[{}] {} (hint: {})", err.code, err.message, hint)
            } else {
                format!("[{}] {}", err.code, err.message)
            };
            McpError::internal_error(msg)
        }
    }
}
```

### MCP Client (mcp_client.rs)

```rust
#[cfg(feature = "mcp-client")]
pub mod mcp_client {
    use std::collections::HashMap;

    use fastmcp_client::{Client, ClientBuilder};
    use fastmcp_protocol::{Content, Tool};

    use crate::config::Config;
    use crate::Result;

    /// Configuration for an external MCP server connection.
    #[derive(Debug, Clone)]
    pub struct ExternalServerConfig {
        pub name: String,
        pub command: String,
        pub args: Vec<String>,
        pub env: HashMap<String, String>,
    }

    /// FrankenTerm MCP client for connecting to external MCP servers.
    ///
    /// Feature: `mcp-client`
    pub struct FtMcpClient {
        client: Client,
        server_name: String,
    }

    impl FtMcpClient {
        /// Connect to an external MCP server via stdio subprocess.
        pub fn connect_stdio(server: &ExternalServerConfig) -> Result<Self> {
            let mut client = Client::stdio(&server.command, &server.args)?;
            Ok(Self {
                client,
                server_name: server.name.clone(),
            })
        }

        /// List tools available on the connected server.
        pub fn list_tools(&mut self) -> Result<Vec<Tool>> {
            Ok(self.client.list_tools()?)
        }

        /// Call a tool on the connected server.
        pub fn call_tool(
            &mut self,
            name: &str,
            arguments: serde_json::Value,
        ) -> Result<Vec<Content>> {
            Ok(self.client.call_tool(name, arguments)?)
        }

        /// Discover MCP servers from platform config files.
        ///
        /// Reads Claude Desktop, Cursor, and Cline configuration files
        /// to discover configured MCP servers.
        pub fn discover_servers(
            _config: &Config,
        ) -> Result<Vec<ExternalServerConfig>> {
            use fastmcp_client::mcp_config::ConfigLoader;
            let loader = ConfigLoader::new();
            let configs = loader.load_all();
            let mut servers = Vec::new();
            for (name, server_config) in configs {
                servers.push(ExternalServerConfig {
                    name,
                    command: server_config.command,
                    args: server_config.args,
                    env: server_config.env,
                });
            }
            Ok(servers)
        }
    }
}
```

### Feature Gate Annotations

```toml
# In crates/frankenterm-core/Cargo.toml

[dependencies]
# MCP server (optional, behind feature)
fastmcp = { workspace = true, optional = true }
fastmcp-derive = { workspace = true, optional = true }
toon_rust = { workspace = true, optional = true }

# MCP client (optional, behind feature)
fastmcp-client = { workspace = true, optional = true }

[features]
mcp-server = ["dep:fastmcp", "dep:fastmcp-derive", "dep:toon_rust"]
mcp-client = ["dep:fastmcp-client"]
mcp = ["mcp-server"]
mcp-full = ["mcp-server", "mcp-client"]
```

---

## P5: Migration Strategy

### Migration Approach: Strangler Fig Pattern

The existing `mcp.rs` is wrapped (not replaced) by the new modules. New code delegates to old code until individual handlers are migrated. This ensures zero regression at every step.

### Step 1: Extract Error Mapping (mcp_error.rs)

Move `McpToolError`, `map_mcp_error()`, `map_caut_error()`, and all `FT-MCP-*` constants from `mcp.rs` to `mcp_error.rs`. Update imports in `mcp.rs`. This is a pure refactoring step with no behavior change.

**Verification**: All existing tests pass unchanged.

### Step 2: Extract Middleware (mcp_middleware.rs)

Implement `FormatNegotiationMiddleware` and `AuditMiddleware` as proper `fastmcp::Middleware` trait implementations. These replace `FormatAwareToolHandler<T>` and `AuditedToolHandler<T>`.

**Verification**: Wire both middleware implementations into the existing `build_server_with_db()`. Run the MCP protocol test suite and verify identical JSON-RPC responses for every tool call.

### Step 3: Create Bridge Module (mcp_bridge.rs)

Create `FtMcpState` and a new `build_server_v2()` function that constructs the server using middleware instead of handler wrappers. The existing `build_server_with_db()` delegates to `build_server_v2()`.

**Verification**: `run_stdio_server()` produces identical behavior with the new server builder.

### Step 4: Migrate Tools Incrementally (mcp_tools.rs)

Migrate one tool at a time from struct-based `ToolHandler` to `mcp_tools.rs`. Priority order based on complexity (simplest first):

1. `WaRulesListTool` -> `ft_rules_list` (stateless, simple)
2. `WaRulesTestTool` -> `ft_rules_test` (stateless)
3. `WaCassStatusTool` -> `ft_cass_status` (CASS client, simple)
4. `WaCassViewTool` -> `ft_cass_view` (CASS client)
5. `WaCassSearchTool` -> `ft_cass_search` (CASS client)
6. `WaStateTool` -> `ft_state` (WezTerm handle, config-aware)
7. `WaWaitForTool` -> `ft_wait_for` (async wait loop)
8. `WaEventsTool` -> `ft_events` (storage query)
9. `WaEventsAnnotateTool` -> `ft_events_annotate` (storage write)
10. `WaEventsTriageTool` -> `ft_events_triage` (storage write)
11. `WaEventsLabelTool` -> `ft_events_label` (storage write)
12. `WaGetTextTool` -> `ft_get_text` (WezTerm + truncation)
13. `WaSearchTool` -> `ft_search` (RRF fusion, complex)
14. `WaSendTool` -> `ft_send` (policy engine, most complex)
15. `WaReservationsTool` -> `ft_reservations` (storage)
16. `WaReserveTool` -> `ft_reserve` (storage + policy)
17. `WaReleaseTool` -> `ft_release` (storage + policy)
18. `WaAccountsTool` -> `ft_accounts` (storage)
19. `WaAccountsRefreshTool` -> `ft_accounts_refresh` (CAUT client)
20. `WaWorkflowRunTool` -> `ft_workflow_run` (workflow engine, complex)

Each migration is a single commit. The old handler is removed and the new one registered in the builder.

### Step 5: Migrate Resources (mcp_resources.rs)

Resources are simpler than tools. Migrate all 12 resource handlers to `mcp_resources.rs` using `#[resource]` macros where applicable, or direct `ResourceHandler` implementations for templated resources.

### Step 6: Add Client Capability (mcp_client.rs)

New feature -- does not replace anything. Add `mcp-client` feature flag, implement `FtMcpClient`, and wire into the config system for server discovery.

### Step 7: Add Proxy Support (mcp_proxy.rs)

New feature. Use `ProxyCatalog` to mount discovered external MCP servers into FrankenTerm's tool namespace.

### Step 8: Remove Legacy Code

After all handlers are migrated and tested, remove:
- `FormatAwareToolHandler<T>` (replaced by `FormatNegotiationMiddleware`)
- `AuditedToolHandler<T>` (replaced by `AuditMiddleware`)
- All `Wa*Tool` and `Wa*Resource` structs (replaced by `mcp_tools.rs` / `mcp_resources.rs`)
- Orphaned helper functions in `mcp.rs`

The remaining `mcp.rs` becomes a thin re-export shim or is fully absorbed into the new modules.

---

## P6: Testing Strategy

### Unit Tests per New Module (30+ each)

#### `mcp_error.rs` tests (target: 30+)

- `test_error_code_constants` -- verify all FT-MCP-* codes
- `test_wezterm_error_mapping` -- Error::Wezterm -> McpError
- `test_storage_error_mapping` -- Error::Storage -> McpError
- `test_config_error_mapping` -- Error::Config -> McpError
- `test_timeout_error_mapping` -- Runtime("timeout") -> FT-MCP-0009
- `test_generic_error_mapping` -- Other Error -> InternalError
- `test_ft_tool_error_to_mcp_error` -- FtToolError conversion
- `test_ft_tool_error_with_hint` -- hint included in message
- `test_ft_tool_error_without_hint` -- no hint
- `test_caut_error_mapping` -- CautError -> McpError
- `test_cass_error_mapping` -- CassError -> McpError
- `test_error_code_stability` -- codes do not change between versions
- `test_error_message_format` -- `[FT-MCP-XXXX] message` format
- `test_roundtrip_preservation` -- error info preserved through mapping
- Plus additional boundary/edge tests to reach 30+

#### `mcp_middleware.rs` tests (target: 30+)

- `test_format_negotiation_json_default` -- no format param = JSON
- `test_format_negotiation_toon_explicit` -- format=toon transcodes
- `test_format_negotiation_invalid_format` -- format=xml rejected
- `test_format_negotiation_non_tool_passthrough` -- non tools/call unaffected
- `test_format_negotiation_stripped_from_args` -- format removed before handler
- `test_audit_middleware_records_tool_call` -- audit entry created
- `test_audit_middleware_records_tool_name` -- correct tool name captured
- `test_audit_middleware_records_arguments` -- arguments captured
- `test_audit_middleware_records_timestamp` -- timestamp present
- `test_audit_middleware_no_db_noop` -- no db_path = no recording
- `test_audit_middleware_non_tool_passthrough` -- non tools/call skipped
- `test_audit_middleware_error_still_recorded` -- failed calls audited
- `test_middleware_ordering` -- format runs before audit
- `test_middleware_continue_decision` -- always returns Continue
- `test_middleware_error_passthrough` -- on_error returns original
- Plus additional tests to reach 30+

#### `mcp_bridge.rs` tests (target: 30+)

- `test_build_server_returns_ok` -- server construction succeeds
- `test_build_server_with_db` -- db_path propagated to state
- `test_build_server_without_db` -- None db_path accepted
- `test_server_name_and_version` -- correct server info
- `test_server_instructions_set` -- instructions string present
- `test_all_tools_registered` -- expected tool count
- `test_all_resources_registered` -- expected resource count
- `test_middleware_registered` -- middleware pipeline active
- `test_ft_mcp_state_clone` -- state cloning works
- `test_ft_mcp_state_config_access` -- config accessible from state
- `test_build_server_parity` -- v2 produces same tools as v1
- `test_tool_names_stable` -- tool names match existing API
- `test_resource_uris_stable` -- resource URIs match existing API
- `test_startup_hook_called` -- on_startup executes
- `test_shutdown_hook_called` -- on_shutdown executes
- Plus additional tests to reach 30+

#### `mcp_tools.rs` tests (target: 40+)

- Per-tool parameter deserialization tests (already exist in mcp.rs, migrated)
- Per-tool response schema validation tests
- Per-tool error path tests
- `test_state_params_minimal` -- empty params accepted
- `test_state_params_with_domain` -- domain filter
- `test_get_text_params_defaults` -- default tail=500, escapes=false
- `test_search_params_minimal` -- query-only accepted
- `test_search_params_with_filters` -- pane, since, until, mode
- `test_send_params_with_defaults` -- dry_run=false, timeout=10
- `test_wait_for_params_regex` -- regex flag
- `test_workflow_run_params` -- name + pane_id required
- All existing serialization tests from mcp.rs migrated
- Plus new tests for macro-generated schema validation

#### `mcp_resources.rs` tests (target: 30+)

- Per-resource URI matching tests
- Per-resource response content validation
- Template parameter extraction tests
- `test_panes_resource_definition` -- correct URI and name
- `test_events_resource_definition` -- correct URI and name
- `test_events_template_uri_parsing` -- {pane_id} extraction
- `test_accounts_template_uri_parsing` -- {service} extraction
- `test_rules_template_uri_parsing` -- {agent} extraction
- All existing resource tests from mcp.rs migrated

#### `mcp_client.rs` tests (target: 30+)

- `test_external_server_config_construction` -- fields set correctly
- `test_discover_servers_empty` -- no config files returns empty
- `test_discover_servers_claude_desktop` -- parses Claude config
- `test_discover_servers_cursor` -- parses Cursor config
- `test_connect_stdio_invalid_command` -- graceful failure
- `test_list_tools_empty_server` -- empty tool list
- `test_call_tool_not_found` -- unknown tool error
- Integration tests using `MemoryTransport` for in-process testing

#### `mcp_proxy.rs` tests (target: 30+)

- `test_proxy_catalog_empty` -- no backends registered
- `test_proxy_catalog_mount` -- backend mounted successfully
- `test_proxy_tool_forwarding` -- call forwarded to backend
- `test_proxy_resource_forwarding` -- read forwarded to backend
- `test_proxy_error_propagation` -- backend errors forwarded
- `test_proxy_tool_name_prefixing` -- namespace prefixing
- Integration tests with mock backends

### Property-Based Tests (proptest)

A new file `tests/proptest_mcp_bridge.rs`:

- `proptest_error_mapping_preserves_code` -- arbitrary Error variants map to valid McpError
- `proptest_format_negotiation_idempotent` -- double-format-negotiation produces same result
- `proptest_tool_schema_valid_json` -- all generated schemas are valid JSON Schema
- `proptest_tool_names_ascii` -- tool names contain only valid MCP characters
- `proptest_resource_uris_valid` -- resource URIs are valid URI syntax
- `proptest_state_params_deserialize` -- arbitrary JSON deserializes without panic
- `proptest_search_params_deserialize` -- arbitrary search params
- `proptest_send_params_deserialize` -- arbitrary send params
- `proptest_envelope_serialization_roundtrip` -- McpEnvelope -> JSON -> McpEnvelope
- `proptest_audit_entry_serialization` -- AuditEntry -> JSON -> AuditEntry
- Additional properties to reach 30+ tests

### Integration Tests

A new file `tests/mcp_integration_tests.rs`:

- Full MCP protocol roundtrip using `MemoryTransport`
- Initialize -> list_tools -> call_tool -> verify response
- Middleware pipeline end-to-end
- Error masking verification
- Cancel-correctness (cancel during tool execution)
- Parity tests: old mcp.rs vs new mcp_bridge.rs produce identical responses

---

## P7: Rollout

### Phase 1: Foundation (Week 1-2)

**Deliverables**:
1. `mcp_error.rs` extracted from `mcp.rs` with all error codes
2. `mcp_middleware.rs` with `FormatNegotiationMiddleware` and `AuditMiddleware`
3. `mcp_bridge.rs` with `FtMcpState` and `build_server_v2()`
4. Existing `build_server_with_db()` delegates to new code
5. All existing tests pass unchanged
6. 90+ new unit tests across 3 modules

**Acceptance Gate**:
- `cargo check --workspace --all-targets` passes with and without `mcp-server` feature
- `cargo test --workspace` -- zero regressions
- `cargo clippy --workspace --all-targets -- -D warnings`
- Side-by-side MCP protocol comparison: v1 and v2 produce identical responses

### Phase 2: Tool Migration (Week 3-5)

**Deliverables**:
1. `mcp_tools.rs` with all 20 tools migrated from manual `ToolHandler` impls
2. `mcp_resources.rs` with all 12 resources migrated
3. `FormatAwareToolHandler<T>` and `AuditedToolHandler<T>` removed from `mcp.rs`
4. Old `Wa*Tool` and `Wa*Resource` structs removed
5. 70+ new unit tests for tools and resources

**Acceptance Gate**:
- Protocol-level parity: every tool call returns structurally identical JSON
- Error code stability: all FT-MCP-* codes preserved
- `McpEnvelope` response format preserved
- TOON transcoding equivalence verified

### Phase 3: Client Capability (Week 6-7)

**Deliverables**:
1. `mcp-client` feature flag added to `Cargo.toml`
2. `mcp_client.rs` with `FtMcpClient`
3. Server discovery from Claude Desktop, Cursor, Cline configs
4. `mcp_proxy.rs` with `ProxyCatalog` integration
5. 60+ new unit tests

**Acceptance Gate**:
- Can connect to and list tools from an external MCP server
- Can proxy external tools through FrankenTerm's MCP surface
- `cargo check` passes with `mcp-client` enabled and disabled

### Phase 4: Network Transports (Week 8-9)

**Deliverables**:
1. SSE transport support for web dashboard integration
2. WebSocket transport support (if fastmcp upstream adds `WsServer`)
3. Transport selection via config
4. Integration tests for each transport

**Acceptance Gate**:
- SSE transport functional end-to-end
- No stdio transport regression
- Transport-agnostic tool behavior verified

### Phase 5: Cleanup and Optimization (Week 10)

**Deliverables**:
1. Remove remaining `mcp.rs` legacy code
2. Rename `mcp_bridge.rs` to `mcp.rs` (or consolidate modules)
3. Performance benchmarking: middleware overhead, tool dispatch latency
4. Documentation updates

**Acceptance Gate**:
- `mcp.rs` is the new fastmcp-based implementation
- No dead code warnings
- Benchmark comparison shows no regression >5%

### Rollback Strategy

Each phase can be independently rolled back:

- **Phase 1 rollback**: Remove new modules, revert `mcp.rs` delegation. Single `git revert`.
- **Phase 2 rollback**: Re-introduce old `Wa*Tool` structs, remove `mcp_tools.rs`. `mcp.rs` still has the old code until Phase 5.
- **Phase 3 rollback**: Disable `mcp-client` feature flag. No impact on server functionality.
- **Phase 4 rollback**: Remove transport configuration. Fall back to stdio-only.
- **Phase 5 rollback**: Restore old `mcp.rs` from git history.

**Critical invariant**: The `mcp-server` feature flag is the master kill switch. Disabling it removes all MCP code from the binary, which has been true since the initial integration and remains true throughout.

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Proc macro compile errors on nightly update | Medium | Medium | Pin nightly version in `rust-toolchain.toml`; `trybuild` tests catch macro regressions |
| asupersync version drift between FrankenTerm and fastmcp | Low | High | `[patch]` sections in workspace Cargo.toml pin to exact rev |
| Middleware ordering bugs (format vs audit) | Medium | Low | Explicit ordering tests; middleware integration test suite |
| Tool migration introduces response format differences | Medium | Medium | Golden-file response comparison for every migrated tool |
| Console/logging initialization conflict | Low | Medium | Disable fastmcp console output; use FrankenTerm's tracing subscriber |
| Compile time increase from fastmcp-derive proc macros | Medium | Low | Proc macros only compiled when `mcp-server` feature enabled |

---

## P8: Summary

### Architecture Decision

**Library import** (Option A) of `fastmcp-server`, `fastmcp-core`, `fastmcp-protocol`, `fastmcp-client`, and `fastmcp-derive` as workspace dependencies. The shared `asupersync 0.2` runtime eliminates async boundary friction and makes this the natural MCP framework for FrankenTerm.

### New Modules

| Module | Responsibility | LOC Estimate | Test Target |
|--------|---------------|-------------|-------------|
| `mcp_error.rs` | Error code mapping (McpError <-> Error) | ~200 | 30+ |
| `mcp_middleware.rs` | Format negotiation + audit trail middleware | ~300 | 30+ |
| `mcp_bridge.rs` | Server construction, state injection, lifecycle | ~250 | 30+ |
| `mcp_tools.rs` | All 20 tool handlers (proc macro + manual) | ~2,500 | 40+ |
| `mcp_resources.rs` | All 12 resource handlers | ~800 | 30+ |
| `mcp_client.rs` | Outbound MCP client for external servers | ~300 | 30+ |
| `mcp_proxy.rs` | Server composition via ProxyCatalog | ~250 | 30+ |
| **Total** | | **~4,600** | **220+** |

Net code change: The existing 7,035-line `mcp.rs` is replaced by ~4,600 lines across 7 focused modules -- a ~35% reduction driven primarily by proc macro adoption eliminating boilerplate.

### Upstream Tweak Proposals (for fastmcp_rust)

1. **R8.1: `#[tool(tags = [...])]` attribute** -- FrankenTerm has 20+ tools that need categorization (pane, search, events, workflow, CASS, accounts). Tag-based filtering is supported in the protocol but not in the proc macro.

2. **R8.2: `#[tool(annotations(...))]` attribute** -- FrankenTerm tools have clear read/write semantics (`ft.state` is read-only, `ft.send` is destructive). `ToolAnnotations` should be settable via macro.

3. **R8.3: Public `run_with_transport()` API** -- FrankenTerm may need custom transports (WezTerm domain socket IPC). The current `run_transport()` method should be a stable public API.

4. **R8.4: Optional `fastmcp-console` dependency** -- Feature-gate the console crate dependency in `fastmcp-server` so embedded users (like FrankenTerm) can avoid pulling in `rich_rust`, `tracing-subscriber`, `strip-ansi-escapes`, and `time`.

5. **R8.8: Typed tool handler trait** -- `TypedToolHandler<Args: Deserialize>` would eliminate `serde_json::from_value()` boilerplate in manually implemented handlers.

6. **R8.12: `MemoryTransport` inspection** -- Message recording/playback for deterministic integration tests.

### Beads References

- `ft-2vuw7.26.1` (CLOSED): Research pass -- fastmcp_rust repository exploration
- `ft-2vuw7.26.2` (CLOSED): Comprehensive analysis document
- `ft-2vuw7.26.3` (THIS DOCUMENT): Integration plan
- Next: `ft-2vuw7.26.3.*` sub-beads for implementation phases:
  - `ft-2vuw7.26.3.1`: Phase 1 -- Foundation (mcp_error, mcp_middleware, mcp_bridge)
  - `ft-2vuw7.26.3.2`: Phase 2 -- Tool migration (mcp_tools, mcp_resources)
  - `ft-2vuw7.26.3.3`: Phase 3 -- Client capability (mcp_client, mcp_proxy)
  - `ft-2vuw7.26.3.4`: Phase 4 -- Network transports
  - `ft-2vuw7.26.3.5`: Phase 5 -- Cleanup and optimization

---

*Plan complete.*
