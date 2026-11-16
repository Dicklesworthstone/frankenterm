# Comprehensive Analysis of fastmcp_ruse (fastmcp_rust)

> Analysis document for FrankenTerm bead `ft-2vuw7.26.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: Research pass ft-2vuw7.26.1

---

## R1: Repository Topology and Crate/Module Boundary Inventory

### Workspace Overview

The repository at `/dp/fastmcp_rust` is organized as a Cargo workspace with **9 member crates**, version-locked at `0.2.0`, using Rust **2024 edition** (nightly toolchain) with `rust-version = "1.85"`.

Total source: **~105,000 lines** of Rust across all crates (including tests).

### Crate Inventory

#### 1. `fastmcp-core` (foundation) -- 5,936 LOC

**Purpose**: Core types, error model, structured-concurrency integration, and cancel-correct primitives. Foundation layer shared by every other crate.

**Key modules**:
- `context.rs` -- `McpContext` wrapping `asupersync::Cx`, `NotificationSender`, `SamplingSender`, `ElicitationSender` traits, `ProgressReporter`, `ResourceReader`, `ToolCaller`
- `error.rs` -- `McpError`, `McpErrorCode` (JSON-RPC codes), `McpOutcome<T>` (4-valued: Ok/Err/Cancelled/Panicked), `OutcomeExt`, `ResultExt`
- `auth.rs` -- `AccessToken`, `AuthContext` (subject, scopes, claims)
- `state.rs` -- `SessionState` (thread-safe `Arc<Mutex<HashMap<String, Value>>>`) with dynamic component enable/disable
- `runtime.rs` -- `block_on()` using lazily initialized asupersync `Runtime`
- `combinator.rs` -- Parallel combinators: `join_all`, `race`, `race_timeout`, `quorum`, `quorum_timeout`, `first_ok`
- `duration.rs` -- Human-readable duration parsing (`30s`, `5m`, `1h30m`)
- `logging.rs` -- Facade logging macros

**Critical trait**: `#![forbid(unsafe_code)]`

#### 2. `fastmcp-protocol` (wire format) -- 6,721 LOC

**Purpose**: MCP protocol types and JSON-RPC 2.0 implementation. Shared vocabulary between server and client.

**Key modules**:
- `jsonrpc.rs` -- `JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcError`, `RequestId` (Number or String), zero-copy `Cow<'static, str>` version field
- `messages.rs` -- All MCP request/response params: `InitializeParams`/`Result`, `ListToolsParams`/`Result`, `CallToolParams`/`Result`, `ListResourcesParams`/`Result`, `ReadResourceParams`/`Result`, `ListPromptsParams`/`Result`, `GetPromptParams`/`Result`, `ProgressParams`, `ProgressMarker`, task-related params (`SubmitTaskParams`, `GetTaskParams`, `CancelTaskParams`, `ListTasksParams`)
- `types.rs` -- `ServerCapabilities`, `ClientCapabilities`, `Tool`, `Resource`, `ResourceTemplate`, `Prompt`, `PromptMessage`, `Content`, `ServerInfo`, `ClientInfo`, `Root`, `ElicitationCapability`, `SamplingCapability`, `TasksCapability`, tag filtering (`includeTags`/`excludeTags`)
- `schema.rs` -- JSON Schema validator covering type checking, required fields, enum, pattern, min/max, array items, additionalProperties (not full JSON Schema, subset for MCP)

**Protocol version**: `2024-11-05`

**Critical trait**: `#![forbid(unsafe_code)]`

#### 3. `fastmcp-transport` (I/O boundary) -- 9,632 LOC

**Purpose**: Transport implementations for stdio, SSE, and WebSocket. Protocol-agnostic message passing.

**Key modules**:
- `stdio.rs` -- `StdioTransport<R, W>` (generic for testing), `AsyncStdioTransport` (production). NDJSON framing.
- `sse.rs` -- `SseWriter`, `SseReader`, `SseEvent`, `SseServerTransport`. Event types: `endpoint`, `message`.
- `websocket.rs` -- `WsTransport<R, W>`, `WsFrame`, full WebSocket frame parsing with masking (using `getrandom`). Does NOT include HTTP upgrade.
- `memory.rs` -- `MemoryTransport` with channel-based pair via `create_memory_transport_pair()`. Essential for testing.
- `event_store.rs` -- `EventStore` for SSE resumability. TTL-based retention, per-stream limits.
- `codec.rs` -- `Codec` for JSON encoding/decoding of `JsonRpcMessage`.
- `async_io.rs` -- `AsyncLineReader`, `AsyncStdin`, `AsyncStdout` wrappers.
- `http.rs` -- HTTP request/response parsing for SSE transport.

**Core trait**:
```rust
pub trait Transport {
    fn send(&mut self, cx: &Cx, msg: &JsonRpcMessage) -> Result<(), TransportError>;
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError>;
}
```

Also defines `TwoPhaseTransport` (reserve/commit pattern) and `SendPermit` for cancel-safe sends.

**Critical trait**: `#![forbid(unsafe_code)]`

#### 4. `fastmcp-server` (execution engine) -- 41,238 LOC

**Purpose**: Server implementation -- request routing, handler registration, session management, middleware, auth, caching, rate limiting, OAuth/OIDC, background tasks, proxy/composition.

**Key modules**:
- `lib.rs` (2,253 LOC) -- `Server` struct, `LifespanHooks`, `LoggingConfig`, `DuplicateBehavior` enum, main `run_stdio()`/`run_sse()`/`run_with_transport()` methods
- `builder.rs` (1,837 LOC) -- `ServerBuilder` with fluent API for configuration
- `router.rs` (4,800 LOC) -- `Router` for dispatching to `BoxedToolHandler`/`BoxedResourceHandler`/`BoxedPromptHandler`. URI template matching, pagination with base64-encoded cursors, tag filtering, `MountResult`
- `handler.rs` (1,845 LOC) -- `ToolHandler`, `ResourceHandler`, `PromptHandler` traits with sync/async dual dispatch, `BidirectionalSenders`, `ProgressNotificationSender`
- `middleware.rs` (336 LOC) -- `Middleware` trait with `on_request`/`on_response`/`on_error` hooks, `MiddlewareDecision` (Continue/Respond)
- `session.rs` (854 LOC) -- `Session` tracking initialization state, client info, capabilities, resource subscriptions, log level
- `auth.rs` (1,352 LOC) -- `AuthProvider` trait, `TokenVerifier` trait, `StaticTokenVerifier`, `AllowAllAuthProvider`, `TokenAuthProvider`, `JwtTokenVerifier` (behind `jwt` feature)
- `proxy.rs` (2,314 LOC) -- `ProxyBackend` trait, `ProxyCatalog`, `ProxyClient` for server composition (forwarding to upstream MCP servers)
- `caching.rs` (1,612 LOC) -- `ResponseCachingMiddleware` with configurable TTLs (list: 5min, call: 1hr)
- `rate_limiting.rs` (1,115 LOC) -- `RateLimitingMiddleware` (token bucket), `SlidingWindowRateLimitingMiddleware`
- `oauth.rs` (3,795 LOC) -- Full OAuth 2.0/2.1 authorization server: PKCE, token issuance/refresh/revocation, client registration, scope validation
- `oidc.rs` (2,414 LOC) -- OpenID Connect provider: ID token issuance, UserInfo endpoint, discovery document, standard claims
- `docket.rs` (3,405 LOC) -- Distributed task queue with Memory and Redis backends, worker pool, task lifecycle
- `tasks.rs` (2,221 LOC) -- `TaskManager` for background tasks (SEP-1686), task state tracking, cancellation
- `bidirectional.rs` (1,365 LOC) -- Server-to-client request infrastructure: `PendingRequests`, `RequestSender`, `TransportSamplingSender`, `TransportElicitationSender`, `TransportRootsProvider`
- `transform.rs` (1,241 LOC) -- `TransformedTool` for dynamic schema modification: rename args, defaults, hide args
- `providers/filesystem.rs` -- `FilesystemProvider` for file-as-resource serving with path traversal protection
- `tests.rs` (6,528 LOC) -- Comprehensive unit tests

**Critical trait**: `#![forbid(unsafe_code)]`

#### 5. `fastmcp-client` (companion client) -- 3,495 LOC

**Purpose**: Client-side MCP implementation for connecting to servers as subprocesses.

**Key modules**:
- `lib.rs` -- `Client` struct wrapping `Child` process + `StdioTransport`. Methods: `stdio()`, `list_tools()`, `call_tool()`, `list_resources()`, `read_resource()`, `list_prompts()`, `get_prompt()`, `ping()`, `set_log_level()`, task methods
- `builder.rs` -- `ClientBuilder` with configurable client info, capabilities, timeout, auto-init
- `session.rs` -- `ClientSession` tracking server info/capabilities after initialization
- `mcp_config.rs` -- MCP config file parsing (`mcpServers` JSON format), `ConfigLoader` with platform-specific defaults (Claude Desktop, Cursor, Cline)

**Critical trait**: `#![forbid(unsafe_code)]`

#### 6. `fastmcp-derive` (proc macros) -- 2,453 LOC

**Purpose**: Attribute macros `#[tool]`, `#[resource]`, `#[prompt]`, `#[derive(JsonSchema)]` for ergonomic handler definition.

- Expands handler functions into `ToolHandler`/`ResourceHandler`/`PromptHandler` implementations
- Generates JSON Schema from function parameters (doc comments become descriptions)
- Supports: `timeout`, `name`, `description` attributes; `Option<T>` for optional params; default values
- Compile-time validation via `trybuild` tests

**Critical trait**: `#![forbid(unsafe_code)]`, `proc-macro = true`

#### 7. `fastmcp-console` (rich output) -- 11,030 LOC

**Purpose**: Rich console output for MCP servers -- startup banners, traffic logging, error formatting, stats rendering.

**Key modules**:
- `banner.rs` -- `StartupBanner` for server startup display
- `client.rs` / `client/traffic.rs` -- `RequestResponseRenderer` for traffic visualization
- `config.rs` -- `ConsoleConfig` with `BannerStyle`, `TrafficVerbosity`
- `detection.rs` -- Agent context detection (`is_agent_context()`, `should_enable_rich()`)
- `diagnostics.rs` -- Error formatting with context
- `error/boundary.rs` -- `ErrorBoundary` wrapper
- `handlers.rs` -- `HandlerRegistryRenderer` for listing tools/resources/prompts
- `logging/` -- `RichLogFormatter`, `RichLogger`, `RichTracingSubscriber`
- `stats/` -- `ServerStats`, `StatsSnapshot`, `StatsRenderer`
- `tables.rs` -- Table rendering
- `testing/` -- Test console utilities
- `theme.rs` -- Theming system

**Depends on**: `rich_rust` (workspace dep), `tracing`, `tracing-subscriber`

#### 8. `fastmcp-cli` (operator tooling) -- 3,491 LOC

**Purpose**: CLI binary (`fastmcp`) for running, inspecting, installing, and testing MCP servers.

**CLI commands** (via `clap`):
- `run` -- Run an MCP server binary with stdio transport
- `inspect` -- Connect to server and display capabilities (text/json/mcp output)
- `install` -- Generate config snippets for Claude Desktop, Cursor, Cline
- `list` -- Scan and list configured MCP servers across clients
- `test` -- Test server connectivity (init, capability listing, ping)
- `dev` -- Development mode with file watching and auto-reload
- `tasks` -- Manage background tasks: list, submit, get, cancel

**Update checking**: Queries crates.io for latest version of `fastmcp-cli`.

#### 9. `fastmcp` (facade) -- 7,259 LOC

**Purpose**: Public API facade re-exporting types from all sub-crates. Published as `fastmcp-rust` on crates.io.

- `prelude` module for `use fastmcp_rust::prelude::*;`
- `testing` module with test utilities: assertions, test client, test server, fixtures, timing, tracing
- Examples: `calculator_server`, `echo_server`, `notes_server`, `weather_server`, `bench`
- Integration tests: `e2e_middleware`, `e2e_protocol`, `e2e_stress`, `e2e_workflow`
- `trybuild` tests for macro compile errors

### Dependency Graph Between Crates

```
fastmcp-core (foundation)
  ^
  |
  +-- fastmcp-protocol (adds: base64, regex)
  |     ^
  |     |
  |     +-- fastmcp-transport (adds: getrandom)
  |     |     ^
  |     |     |
  |     +-----+-- fastmcp-client (adds: toml, dirs)
  |     |     |
  |     +-----+-- fastmcp-server (adds: chrono, sha2, hmac, redis?, jsonwebtoken?)
  |     |           ^    |
  |     |           |    +-- depends on fastmcp-client (for proxy)
  |     |           |    +-- depends on fastmcp-console
  |     |           |
  |     |           +-- fastmcp (facade, depends on all above + fastmcp-derive)
  |     |                 ^
  |     |                 |
  |     +-----------------+-- fastmcp-cli (binary, depends on client/core/protocol/console/transport)
  |
  +-- fastmcp-console (adds: rich_rust, tracing, strip-ansi-escapes, time, regex)

fastmcp-derive (standalone: proc-macro2, quote, syn -- no internal deps)
```

---

## R2: Build/Runtime/Dependency Map and Feature-Flag Matrix

### Build Requirements

| Property | Value |
|----------|-------|
| Rust edition | 2024 |
| MSRV | 1.85 |
| Toolchain | nightly (required by `rust-toolchain.toml`) |
| Resolver | 2 |
| Lint policy | `unsafe_code = "warn"` workspace-wide; individual crates use `#![forbid(unsafe_code)]` |
| Clippy | `all` + `pedantic` (warn), with ~30+ pedantic lints allowed |

### Feature Flags

| Crate | Feature | What It Enables |
|-------|---------|----------------|
| `fastmcp-server` | `jwt` | JWT token verification via `jsonwebtoken` crate (`JwtTokenVerifier`) |
| `fastmcp-server` | `redis` | Redis backend for Docket distributed task queue |
| `fastmcp-rust` (facade) | `jwt` | Forwards to `fastmcp-server/jwt` |
| `fastmcp-console` | `full` | Enables `rich_rust/full` (all rendering features) |
| `fastmcp-console` | `syntax` | Enables `rich_rust/syntax` (syntax highlighting) |
| `fastmcp-console` | `markdown` | Enables `rich_rust/markdown` (markdown rendering) |
| `fastmcp-console` | `json` | Enables `rich_rust/json` (JSON pretty-printing) |

### External Dependencies (with versions)

**Runtime (proprietary ecosystem)**:
- `asupersync = "0.2"` -- Structured concurrency runtime (cancel-correct async, `Cx`, `Budget`, `Outcome`, `Scope`, `RegionId`). Published by same author.
- `rich_rust = "0.2"` -- Rich terminal output library. Published by same author.

**Serialization**:
- `serde = "1"` (with `derive`)
- `serde_json = "1"`
- `serde_yaml = "0.9"`

**Logging**:
- `log = "0.4"` (facade)
- `tracing = "0.1"` (console crate only)
- `tracing-subscriber = "0.3"` (console crate only)

**Networking/IO**:
- `ureq = "3"` (CLI update checking)
- `redis = "1"` (optional, Docket backend)

**CLI**:
- `clap = "4"` (derive, env, wrap_help)
- `console = "0.16"` (terminal formatting)

**Crypto**:
- `sha2 = "0.10"` (OAuth PKCE)
- `hmac = "0.12"` (OIDC signing)
- `getrandom = "0.4"` (WebSocket masking, token generation)
- `jsonwebtoken = "10"` (optional, JWT verification)

**Misc**:
- `base64 = "0.22"` (pagination cursors, encoding)
- `semver = "1"` (version comparison)
- `chrono = "0.4"` (date/time)
- `notify = "8"` (file watching for `dev` command)
- `glob = "0.3"` (file patterns)
- `regex = "1"` (JSON Schema `pattern` validation)
- `toml = "1.0"` (config parsing)
- `dirs = "6"` (platform directories)
- `strip-ansi-escapes = "0.2"` (console output cleaning)
- `time = "0.3"` (console timestamp formatting)

**Proc macro**:
- `proc-macro2 = "1"`, `quote = "1"`, `syn = "2"` (full, parsing, extra-traits)

**Dev dependencies**:
- `trybuild = "1"` (compile-fail tests)
- `tempfile = "3"` (console tests)

### Runtime Requirements

- **No tokio dependency**: Uses `asupersync` as the async runtime. This is significant for FrankenTerm integration since FrankenTerm does not use tokio either.
- **No network server built in**: The transport layer provides SSE and WebSocket *message handling*, but does NOT include an HTTP server. HTTP upgrade / binding must be done externally.
- **Stdio is the primary transport**: Production deployment is subprocess-based (stdin/stdout).

---

## R3: Public Surface Inventory (APIs/CLI/MCP/Config/Events)

### Core Public Types and Traits

**Error model**:
- `McpError` { code: `McpErrorCode`, message: `String`, data: `Option<Value>` }
- `McpErrorCode` -- ParseError, InvalidRequest, MethodNotFound, InvalidParams, InternalError, ToolExecutionError, ResourceNotFound, ResourceForbidden, PromptNotFound, RequestCancelled, Custom(i32)
- `McpResult<T>` = `Result<T, McpError>`
- `McpOutcome<T>` = `Outcome<T, McpError>` (4-valued: Ok/Err/Cancelled/Panicked)

**Context**:
- `McpContext` -- wraps `Cx` with request_id, session state, progress reporter, sampling/elicitation/tool-call/resource-read senders
- `SessionState` -- `Arc<Mutex<HashMap<String, Value>>>` with typed get/set, dynamic component enable/disable

**Handler traits** (in `fastmcp-server`):
```rust
pub trait ToolHandler: Send + Sync {
    fn definition(&self) -> Tool;
    fn call(&self, ctx: &McpContext, arguments: Value) -> McpResult<Vec<Content>>;
    fn call_async<'a>(&'a self, ctx: &'a McpContext, arguments: Value) -> BoxFuture<'a, McpOutcome<Vec<Content>>>;
    fn timeout(&self) -> Option<Duration>;
    fn tags(&self) -> &[String];
    fn annotations(&self) -> Option<&ToolAnnotations>;
    fn output_schema(&self) -> Option<Value>;
}

pub trait ResourceHandler: Send + Sync {
    fn definition(&self) -> Resource;
    fn template(&self) -> Option<ResourceTemplate>;
    fn read(&self, ctx: &McpContext, uri: &str, params: &UriParams) -> McpResult<Vec<ResourceContent>>;
    fn read_async<'a>(...) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>>;
    fn timeout(&self) -> Option<Duration>;
}

pub trait PromptHandler: Send + Sync {
    fn definition(&self) -> Prompt;
    fn get(&self, ctx: &McpContext, arguments: HashMap<String, String>) -> McpResult<Vec<PromptMessage>>;
    fn get_async<'a>(...) -> BoxFuture<'a, McpOutcome<Vec<PromptMessage>>>;
    fn timeout(&self) -> Option<Duration>;
}
```

**Middleware**:
```rust
pub trait Middleware: Send + Sync {
    fn on_request(&self, ctx: &McpContext, request: &JsonRpcRequest) -> McpResult<MiddlewareDecision>;
    fn on_response(&self, ctx: &McpContext, request: &JsonRpcRequest, response: Value) -> McpResult<Value>;
    fn on_error(&self, ctx: &McpContext, request: &JsonRpcRequest, error: McpError) -> McpError;
}
```

**Auth**:
```rust
pub trait AuthProvider: Send + Sync {
    fn authenticate(&self, request: &AuthRequest) -> McpResult<Option<AuthContext>>;
}
pub trait TokenVerifier: Send + Sync {
    fn verify(&self, token: &AccessToken) -> McpResult<AuthContext>;
}
```

**Transport**:
```rust
pub trait Transport {
    fn send(&mut self, cx: &Cx, msg: &JsonRpcMessage) -> Result<(), TransportError>;
    fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError>;
    fn send_request(&mut self, cx: &Cx, req: &JsonRpcRequest) -> Result<(), TransportError>;
    fn send_response(&mut self, cx: &Cx, resp: &JsonRpcResponse) -> Result<(), TransportError>;
}
```

### Server Builder API

```rust
Server::new("name", "version")
    .tool(handler)                    // Register tool handler
    .resource(handler)                // Register resource handler
    .resource_template(handler, tmpl) // Register resource template
    .prompt(handler)                  // Register prompt handler
    .instructions("...")              // Set server instructions
    .request_timeout(30)              // Timeout in seconds
    .mask_error_details(true)         // Hide internal error details
    .auto_mask_errors()               // Environment-based masking
    .auth_provider(provider)          // Set auth provider
    .middleware(mw)                   // Add middleware
    .list_page_size(100)              // Pagination
    .on_duplicate(behavior)           // Error/Warn/Replace/Ignore
    .with_console_config(config)      // Console output configuration
    .without_stats()                  // Disable statistics
    .on_startup(|| Ok(()))            // Startup hook
    .on_shutdown(|| {})               // Shutdown hook
    .with_task_manager(tm)            // Background task support
    .strict_input_validation(true)    // Reject extra properties
    .build()                          // Build Server
    .run_stdio()                      // Run with stdio transport
```

### Procedural Macro Attributes

```rust
#[tool]                      // Basic tool
#[tool(name = "custom")]     // Custom name
#[tool(timeout = "30s")]     // Per-tool timeout
#[tool(description = "...")]  // Override description

#[resource(uri = "config://app")]           // Static resource
#[resource(uri = "file://{path}")]          // Templated resource
#[resource(uri = "...", timeout = "1m")]    // With timeout
#[resource(uri = "...", mime_type = "...")]  // MIME type

#[prompt]                     // Basic prompt
#[prompt(name = "custom")]    // Custom name
#[prompt(timeout = "10s")]    // Per-prompt timeout
```

### CLI Commands and Flags

| Command | Description | Key Flags |
|---------|-------------|-----------|
| `fastmcp run <server> [args]` | Run MCP server binary | `--cwd`, `-e KEY=VAL` |
| `fastmcp inspect <server> [args]` | Display capabilities | `-f text\|json\|mcp`, `-o file` |
| `fastmcp install <name> <server> [args]` | Install config | `-t claude\|cursor\|cline`, `--dry-run` |
| `fastmcp list` | List configured servers | `-t target`, `-c config`, `-f table\|json\|yaml`, `-v` |
| `fastmcp test <server> [args]` | Test connectivity | (inherits timeout) |
| `fastmcp dev <server> [args]` | Dev mode with file watching | `--watch`, `--exclude` |
| `fastmcp tasks list <server> [args]` | List tasks | (per-server) |
| `fastmcp tasks submit <server> <name>` | Submit task | `--params` |
| `fastmcp tasks get <server> <id>` | Get task status | |
| `fastmcp tasks cancel <server> <id>` | Cancel task | `--reason` |

### Configuration Options

**Environment variables** (server-side):
- `FASTMCP_LOG` -- Log level (error/warn/info/debug/trace)
- `FASTMCP_LOG_TIMESTAMPS` -- Show timestamps (0/false to disable)
- `FASTMCP_LOG_TARGETS` -- Show module targets (0/false to disable)
- `FASTMCP_LOG_FILE_LINE` -- Show file:line (1/true to enable)
- `FASTMCP_ENV` -- Environment (production enables masking)
- `FASTMCP_MASK_ERRORS` -- Override error masking (true/false)
- `FASTMCP_CONSOLE_BANNER` -- Banner style
- `FASTMCP_CONSOLE_TRAFFIC` -- Traffic verbosity
- `FASTMCP_CHECK_FOR_UPDATES` -- Disable update checking (0/false)

**MCP config file format** (`mcpServers`):
```json
{
    "mcpServers": {
        "server-name": {
            "command": "binary",
            "args": ["--flag"],
            "env": { "KEY": "value" }
        }
    }
}
```

### Event/Callback Hooks

- `LifespanHooks`: `on_startup` (fallible), `on_shutdown`
- `NotificationSender`: `send_progress(progress, total, message)`
- `SamplingSender`: `create_message(request)` -- server-to-client LLM request
- `ElicitationSender`: `elicit(request)` -- server-to-client user input request
- `ResourceReader`: `read_resource(uri)` -- nested resource reading
- `ToolCaller`: `call_tool(name, arguments)` -- nested tool calling
- Resource subscriptions: `subscribe`/`unsubscribe` with change notifications

---

## R4: Execution-Flow Tracing Across Core Workflows

### MCP Server Lifecycle

1. **Build phase**: `ServerBuilder::new(name, version)` -> configure via fluent API -> `.build()` produces `Server`
2. **Startup**: `server.run_stdio()` (or `run_sse()`, `run_with_transport()`)
   - Initialize logging via `RichLoggerBuilder`
   - Display startup banner via `StartupBanner`
   - Execute `LifespanHooks::on_startup`
   - Create `Session` (uninitialized)
   - Enter message loop
3. **Message loop** (`fn run_with_transport()`):
   - `transport.recv(&cx)` -- blocking read
   - Route `JsonRpcMessage` through pipeline:
     - If request: `handle_request()`
     - If response: route to `PendingRequests` for bidirectional flow
   - If notification: dispatch (e.g., `cancelled`, `initialized`, `resources/subscribe`)
4. **Shutdown**: `LifespanHooks::on_shutdown`, drop resources

### Request Handling Pipeline (`Server::handle_request`)

```
JsonRpcRequest
  |
  +-- Auth: auth_provider.authenticate(AuthRequest)
  |     -> populates AuthContext in SessionState
  |
  +-- Pre-initialized check: only allow "initialize" and "ping" before init
  |
  +-- Middleware::on_request (in registration order)
  |     -> MiddlewareDecision::Continue or Respond (short-circuit)
  |
  +-- Router dispatch by method name:
  |     "initialize"              -> session.initialize()
  |     "initialized"             -> (notification, no response)
  |     "ping"                    -> echo {}
  |     "tools/list"              -> router.list_tools()
  |     "tools/call"              -> router.call_tool()
  |     "resources/list"          -> router.list_resources()
  |     "resources/read"          -> router.read_resource()
  |     "resources/templates/list" -> router.list_resource_templates()
  |     "resources/subscribe"     -> session.subscribe_resource()
  |     "resources/unsubscribe"   -> session.unsubscribe_resource()
  |     "prompts/list"            -> router.list_prompts()
  |     "prompts/get"             -> router.get_prompt()
  |     "logging/setLevel"        -> session.set_log_level()
  |     "notifications/cancelled" -> cancel active request
  |     "tasks/submit"            -> task_manager.submit()
  |     "tasks/get"               -> task_manager.get()
  |     "tasks/cancel"            -> task_manager.cancel()
  |     "tasks/list"              -> task_manager.list()
  |     _                         -> MethodNotFound error
  |
  +-- Middleware::on_response (in reverse order)
  |     or Middleware::on_error (in reverse order, on failure)
  |
  +-- Error masking: McpError::masked(mask_error_details)
  |
  +-- Stats recording: ServerStats.record_request()
  |
  +-- transport.send_response()
```

### Tool Call Flow (deep dive)

```
Router::call_tool(name, params, cx, state, notification_sender, senders)
  |
  +-- Lookup handler in tools HashMap
  +-- Extract arguments from params (default to {})
  +-- Validate against JSON Schema (schema::validate or validate_strict)
  +-- Check session-level disabled tools
  +-- Create McpContext with progress reporter + bidirectional senders
  +-- Apply per-handler timeout (if set) via child budget
  +-- handler.call_async(ctx, arguments).await
  |     -> McpOutcome<Vec<Content>>
  +-- Convert outcome to CallToolResult { content, is_error }
```

### Client Connection Flow

```
Client::stdio(command, args)
  |
  +-- Spawn subprocess with piped stdin/stdout
  +-- Create StdioTransport<ChildStdout, ChildStdin>
  +-- Send InitializeParams { protocolVersion, capabilities, clientInfo }
  +-- Receive InitializeResult { protocolVersion, capabilities, serverInfo }
  +-- Send "notifications/initialized" notification
  +-- Store session state (ClientSession)
```

### Proxy/Composition Flow

```
Server.mount(proxy_client)
  |
  +-- proxy_client.list_tools() -> create ProxyToolHandler for each
  +-- proxy_client.list_resources() -> create ProxyResourceHandler for each
  +-- proxy_client.list_prompts() -> create ProxyPromptHandler for each
  +-- Register all handlers in Router
  |
  // At call time:
  ProxyToolHandler.call(ctx, args)
    -> proxy_backend.call_tool(name, args)
    -> forwarded to upstream MCP server
```

---

## R5: Persistence, State, and Data Contracts

### State Management

**No persistent storage**: FastMCP does not use any database or on-disk storage for its own state. All state is in-memory and per-process.

**Session state** (`SessionState`):
- `Arc<Mutex<HashMap<String, serde_json::Value>>>`
- Per-session (one per client connection)
- Thread-safe, supports typed get/set
- Special keys: `fastmcp.auth` (AuthContext), `fastmcp.disabled_tools`, `fastmcp.disabled_resources`, `fastmcp.disabled_prompts`
- Cloning shares the same underlying HashMap (Arc semantics)

**Server state**:
- `Server.active_requests: Mutex<HashMap<RequestId, ActiveRequest>>` -- tracks in-flight requests for cancellation
- `Server.pending_requests: Arc<PendingRequests>` -- bidirectional request tracking
- `Server.stats: Option<ServerStats>` -- atomic counters for request/error/duration metrics

**Docket task state** (in-memory or Redis):
- `TaskManager`: `RwLock<HashMap<TaskId, TaskState>>`
- Memory backend: `VecDeque` per queue
- Redis backend: Redis lists/hashes (optional feature)

### Serialization Formats

**Wire format**: JSON-RPC 2.0 over NDJSON (newline-delimited JSON)
```json
{"jsonrpc":"2.0","method":"tools/call","params":{"name":"greet","arguments":{"name":"World"}},"id":1}
{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"Hello, World!"}],"isError":false},"id":1}
```

**Pagination cursors**: Base64-encoded JSON `{"offset": N}`

**Content types** (`Content` enum):
- `Text { text: String }` -- plain text
- `Image { data: String, mime_type: String }` -- base64-encoded image
- `Audio { data: String, mime_type: String }` -- base64-encoded audio
- `Resource { resource: ResourceContent }` -- embedded resource

**Resource content** (`ResourceContent`):
- `uri: String`, `name: Option<String>`, `description: Option<String>`
- `text: Option<String>` or `blob: Option<String>` (base64)
- `mime_type: Option<String>`

### Data Contracts

**MCP protocol version**: `2024-11-05`

**JSON Schema validation**: Subset implementation covering type, required, enum, pattern, min/max, items, properties, additionalProperties. Not a full JSON Schema Draft 7/2020 implementation.

**OAuth tokens**: In-memory `HashMap<String, OAuthToken>`. Tokens are hex-encoded random bytes (32 bytes via `getrandom`).

---

## R6: Reliability, Performance, and Security Posture

### Error Handling Patterns

**4-valued outcome model**: All handler operations return `McpOutcome<T>` which is `Outcome<T, McpError>`:
- `Ok(value)` -- success
- `Err(McpError)` -- recoverable application error
- `Cancelled(CancelReason)` -- request was cancelled (maps to code -32004)
- `Panicked(PanicPayload)` -- handler panicked (maps to code -32603)

**Error masking**: `McpError::masked(bool)` strips internal details from `InternalError`, `ToolExecutionError`, and `Custom` codes. Client errors (ParseError, InvalidRequest, etc.) are preserved.

**Structured cancellation**: Via `asupersync::Cx::checkpoint()` at transport boundaries. Handlers can cooperatively check cancellation. Server tracks active requests and supports `notifications/cancelled`.

**Middleware error rewriting**: `Middleware::on_error()` allows middleware to transform errors before client sees them.

### Performance Characteristics

**Cancel-correct async**: Built on `asupersync` which provides:
- Structured concurrency (regions, scopes)
- Budget-aware timeouts
- Two-phase send pattern for transport safety
- No orphan tasks (all futures are properly drained)

**Zero-copy optimizations**:
- `JsonRpcRequest.jsonrpc: Cow<'static, str>` avoids allocation for `"2.0"`
- NDJSON line buffer reuse (`String::with_capacity(4096)`)
- `Codec` with pre-allocated encode buffer

**Statistics overhead**: Atomic counters (`ServerStats`) with negligible overhead. Can be disabled via `.without_stats()`.

**Pagination**: Cursor-based with configurable page size. Base64 cursors prevent offset manipulation.

**Response caching**: `ResponseCachingMiddleware` with per-method TTLs and max item size limits.

**Rate limiting**: Token bucket (burst-friendly) or sliding window (precise tracking).

### Security Posture

**Strong points**:
- `#![forbid(unsafe_code)]` in all crates
- Error masking to prevent information leakage
- Path traversal protection in `FilesystemProvider`
- OAuth 2.1 with PKCE required
- Access token values redacted in `Debug` output
- Redirect URI exact-match validation
- Single-use authorization codes
- Cryptographically random token generation

**Considerations**:
- OAuth/OIDC tokens stored in-memory only (no persistence = no persistence-based attacks, but also no crash recovery)
- No TLS built in (relies on transport layer / reverse proxy)
- No request signing
- `SessionState` uses `Mutex` (potential contention under high parallelism, but appropriate for per-session use)

---

## R7: Integration Seams and Extraction Candidates

### FrankenTerm's Current MCP Usage

FrankenTerm already depends on `fastmcp-rust` via git:
```toml
# Cargo.toml (workspace)
fastmcp = { version = "0.2.0", git = "https://github.com/Dicklesworthstone/fastmcp_rust", package = "fastmcp-rust" }
```

The integration lives in `crates/frankenterm-core/src/mcp.rs` behind the `mcp-server` feature flag (which also pulls in `toon_rust`). The MCP server exposes FrankenTerm's core capabilities (pane management, search, workflows, policy, storage) as MCP tools and resources.

### Natural Integration Points

**1. Transport Layer (`fastmcp-transport`)**

FrankenTerm could use the transport layer independently:
- `StdioTransport` -- already used for MCP server subprocess communication
- `MemoryTransport` -- valuable for in-process testing of MCP tools
- `SseServerTransport` / `WsTransport` -- could enable network-attached MCP endpoints for FrankenTerm

Seam: The `Transport` trait is clean and transport-agnostic. FrankenTerm could implement custom transports (e.g., over WezTerm's IPC) by implementing this trait.

**2. Protocol Types (`fastmcp-protocol`)**

All MCP message types are in this crate. FrankenTerm's `mcp.rs` already uses these transitively through the facade. Direct dependency would reduce compile scope:
- `Tool`, `Resource`, `Prompt` definitions
- `Content` variants for responses
- JSON Schema validation for tool inputs

**3. Server Builder Pattern (`fastmcp-server`)**

The `ServerBuilder` pattern maps well to FrankenTerm's needs:
- `.tool()` / `.resource()` / `.prompt()` registration
- `.auth_provider()` for gating access to pane operations
- `.middleware()` for rate limiting or audit logging
- `.request_timeout()` for preventing long-running operations

**4. Proc Macros (`fastmcp-derive`)**

The `#[tool]`, `#[resource]`, `#[prompt]` macros could significantly reduce boilerplate in FrankenTerm's `mcp.rs`. Currently, FrankenTerm manually implements handler traits. The macros auto-generate:
- JSON Schema from function signatures
- Input validation
- Error conversion
- Async/sync dispatch

**5. Handler Traits**

FrankenTerm could implement `ToolHandler` directly for complex tools that need custom logic beyond what macros support. The trait design (sync with async override) matches FrankenTerm's mixed sync/async architecture.

### Extractable Components

**High reuse value for FrankenTerm**:

1. **`fastmcp-core::combinator`** -- `join_all`, `race`, `quorum`, `first_ok` are generically useful for FrankenTerm's concurrent pane operations (already uses similar patterns in its own codebase)

2. **`fastmcp-server::caching`** -- `ResponseCachingMiddleware` could cache FrankenTerm MCP responses for frequently-queried pane state

3. **`fastmcp-server::rate_limiting`** -- Both token bucket and sliding window implementations are well-tested and could protect FrankenTerm's MCP surface from overload

4. **`fastmcp-server::transform`** -- `TransformedTool` for wrapping FrankenTerm tools with LLM-friendly names and defaults without changing underlying implementation

5. **`fastmcp-client::mcp_config`** -- Configuration file loading for discovering other MCP servers. FrankenTerm could use this to connect to external tools.

6. **`fastmcp-server::proxy`** -- Server composition. FrankenTerm could proxy to other MCP servers (e.g., filesystem server, git server) and expose them alongside its own tools.

### How fastmcp_rust Fits with FrankenTerm

FrankenTerm's existing `mcp.rs` (~4000+ lines) manually constructs `ToolHandler` implementations for operations like:
- Pane management (list, send_text, wait, get_text)
- Search (unified search with RRF fusion)
- Workflows (execute, list, status)
- Storage (query events)
- Policy (check permissions)
- CASS/CAUT integration
- Config management

The fastmcp_rust library provides the framework within which these handlers run. Key integration quality observations:

1. **Runtime compatibility**: Both use `asupersync` as the async runtime (no tokio). This is an excellent fit since there are no runtime conflicts.

2. **Error model alignment**: FrankenTerm uses its own `Error` type but converts to `McpError` at the boundary. The 4-valued `McpOutcome` maps well to FrankenTerm's error scenarios (cancelled pane operations, timed-out waits, etc.).

3. **Feature gating**: The MCP server is behind `mcp-server` feature flag. This is good -- it keeps the core library clean and optional.

4. **Missing abstraction**: FrankenTerm doesn't currently use `fastmcp-derive` macros, meaning substantial boilerplate for each tool. Migrating to macro-based handlers would reduce ~60% of the `mcp.rs` code.

---

## R8: Upstream Tweak Recommendations

### Priority 1: Direct Integration Improvements

**R8.1: Add `#[tool(tag = "...")]` attribute for tag-based filtering**

FrankenTerm has many tools (~30+). Tag filtering (`includeTags`/`excludeTags` in `ListToolsParams`) is already supported in the protocol layer, but the macro doesn't support setting tags declaratively. Adding `#[tool(tags = ["pane", "management"])]` would allow FrankenTerm to organize tools into categories without manual `ToolHandler` implementation.

**R8.2: Support `#[tool(annotations(...))]` for behavioral hints**

The protocol defines `ToolAnnotations` (destructive, idempotent, read_only, open_world_hint), but the macro doesn't expose them. FrankenTerm tools have clear read/write semantics that should be advertised.

**R8.3: Expose `Server::run_with_transport()` as public API**

Currently `run_stdio()` and `run_sse()` are the primary entry points, but FrankenTerm may want to run with custom transports (e.g., over WezTerm's domain socket). Making `run_with_transport()` a first-class public API would enable this.

### Priority 2: Architecture Improvements

**R8.4: Decouple `fastmcp-server` from `fastmcp-console`**

The server crate has a hard dependency on `fastmcp-console`, which pulls in `rich_rust`, `tracing`, `tracing-subscriber`, `strip-ansi-escapes`, and `time`. For embedded use (FrankenTerm already has its own logging), this adds unnecessary compile time and binary size. Making console support optional via a feature flag would improve integration ergonomics.

**R8.5: Extract `Middleware` into `fastmcp-core`**

The `Middleware` trait in `fastmcp-server` is conceptually a core abstraction. Moving it to `fastmcp-core` would allow `fastmcp-transport` and other crates to use middleware-like patterns without depending on the full server.

**R8.6: Provide `async fn` trait methods when stable**

The current `BoxFuture<'a, T>` pattern for async handler methods adds allocation overhead and complexity. When async fn in traits stabilizes (it has in nightly), updating the handler traits would improve ergonomics and performance.

### Priority 3: Feature Additions

**R8.7: Add subscription/notification forwarding in proxy**

The proxy system currently forwards tool calls, resource reads, and prompt fetches, but doesn't forward resource change notifications or tool list change notifications. FrankenTerm's pane state changes frequently, and upstream consumers need to be notified.

**R8.8: Support typed tool arguments via generic ToolHandler**

Currently, tool arguments are always `serde_json::Value` requiring manual deserialization. A `TypedToolHandler<Args: Deserialize>` trait variant would eliminate this boilerplate:
```rust
fn call(&self, ctx: &McpContext, args: MyArgs) -> McpResult<Vec<Content>>;
```
The macro already does this transformation, but a trait-level abstraction would help non-macro users.

**R8.9: Add health check / readiness probe support**

For FrankenTerm's deployment scenarios, a built-in health check mechanism (beyond `ping`) that reports tool/resource availability and handler health would be valuable.

**R8.10: First-class WebSocket transport server**

The WebSocket module provides frame-level handling but no server binding. Adding a minimal `WsServer` (even if just a listener that accepts upgraded connections) would enable FrankenTerm to expose its MCP surface over WebSocket without pulling in a full HTTP framework.

### Priority 4: Testing Improvements

**R8.11: Publish `testing` module as part of `fastmcp-rust`**

The `fastmcp::testing` module contains excellent utilities (`TestServer`, `TestClient`, assertions, fixtures) that FrankenTerm could use for its MCP integration tests. Currently these are available but not prominently documented.

**R8.12: Expand `MemoryTransport` with inspection capabilities**

Adding message recording/playback to `MemoryTransport` would enable FrankenTerm to write deterministic integration tests that verify exact MCP protocol sequences.

---

*Analysis complete.*
