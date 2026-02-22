# Plan to Deeply Integrate fastapi_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.25.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_fastapi_rust.md (ft-2vuw7.25.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Web dashboard HTTP server** -- Use fastapi_rust's `App`/`TcpServer` as the HTTP backbone for FrankenTerm's web dashboard (`ft web`), replacing the hand-wired route registration in the existing `web.rs` with fastapi's middleware stack, typed extractors, and builder pattern. The existing nine endpoints (/health, /panes, /events, /search, /bookmarks, /ruleset-profile, /saved-searches, /stream/events, /stream/deltas) migrate to fastapi route handlers with `FromRequest` extractors.

2. **WebSocket terminal streaming** -- Leverage fastapi_rust's RFC 6455 WebSocket implementation (in `fastapi-http::websocket`) to provide real-time bidirectional terminal access. This enables browser-based terminal views where a client connects via WebSocket, receives live scrollback deltas, and can send keystrokes back to panes. The current SSE-only streaming endpoints become a secondary transport alongside full-duplex WebSocket channels.

3. **Typed agent HTTP API** -- Expose FrankenTerm's swarm coordination primitives (pane reservation, workflow triggers, pattern subscriptions, agent registration) as a structured REST API with fastapi extractors (`Json<T>`, `Path<T>`, `Query<T>`, `Valid<T>`) and auto-generated OpenAPI 3.1 documentation. This gives coding agents a discoverable HTTP interface to FrankenTerm beyond the MCP transport.

4. **Shared asupersync runtime** -- Both FrankenTerm and fastapi_rust depend on asupersync 0.2.0 (same Git rev `c7c15f6`). The integration runs the HTTP server inside FrankenTerm's existing asupersync runtime, sharing the `Cx` capability context, structured concurrency regions, and cooperative cancellation across the entire process. No runtime nesting, no `block_on` bridges.

5. **Health and readiness probes** -- Integrate fastapi_rust's `HealthCheckRegistry` to expose structured liveness/readiness endpoints that report on FrankenTerm subsystems: storage connectivity, WezTerm multiplexer status, event bus health, FD budget headroom, and (if enabled) MCP server availability. These probes support fleet orchestration and monitoring dashboards.

### Constraints

- **Runtime version lock** -- Both projects must stay on the same asupersync Git rev. The workspace `Cargo.toml` already patches asupersync globally via `[patch.crates-io]` and `[patch."https://github.com/Dicklesworthstone/fastapi_rust"]`. Any upstream asupersync bump must be coordinated across both projects simultaneously.

- **Feature-gated** -- All fastapi_rust integration code lives behind the existing `web` feature flag (already defined: `web = ["dep:fastapi", "dep:asupersync"]`). Default builds (`cargo check`) compile nothing from this integration. The `streaming` feature implies `web`.

- **`#![forbid(unsafe_code)]`** -- frankenterm-core forbids unsafe code. fastapi-core also uses `#![forbid(unsafe_code)]`. The HTTP parser (`fastapi-http`) uses `#![deny(unsafe_code)]` (warns but allows). Our integration modules in frankenterm-core touch only fastapi-core and fastapi-http types through safe public APIs -- no unsafe required.

- **macOS route auto-discovery unavailable** -- fastapi-router's ELF linker-section route auto-discovery returns an empty slice on macOS/Darwin. All FrankenTerm route registration must use explicit `App::builder().route()` calls, not the `#[get]`/`#[post]` attribute macros with auto-discovery. This is already the pattern in the existing `web.rs`.

- **Binary size budget** -- fastapi_rust adds approximately 111K LOC across 8 crates. The `web` feature gate ensures this only affects builds that opt in. The existing workspace `[profile.release]` settings (LTO, `opt-level = "z"`, strip) mitigate the binary size impact.

- **HTTP/1.1 only** -- fastapi-http supports HTTP/1.1 and cleartext h2c. No TLS termination is provided; FrankenTerm's web server is intended for local/LAN use behind a reverse proxy for production deployments. The `distributed` feature (which adds `rustls`) could be composed with `web` in the future for direct TLS.

- **No proc-macro dependency in frankenterm-core** -- We do not add `fastapi-macros` as a dependency. The derive macros (`#[derive(Validate)]`, `#[derive(JsonSchema)]`) would add `syn`/`proc-macro2`/`quote` to the critical path. Instead, we implement `Validate` and `JsonSchema` manually for the small number of API types.

### Non-Goals

- **Replacing the MCP server** -- FrankenTerm's MCP integration (`mcp.rs`, behind the `mcp-server` feature) uses fastmcp_rust over stdio transport. The HTTP API is a complementary interface, not a replacement. MCP remains the primary agent-to-FrankenTerm protocol.

- **Public internet deployment** -- The fastapi_rust integration serves local dashboards and LAN-accessible APIs. We do not implement rate limiting, OAuth2 flows, or TLS certificate management. The existing `allow_public_bind` guard remains the safety net.

- **Full Python FastAPI compatibility** -- We use fastapi_rust as a Rust HTTP framework. We do not aim for request/response parity with Python FastAPI, Starlette, or Pydantic. The OpenAPI generation is a bonus, not a compatibility target.

- **Replacing tokio in the main runtime** -- FrankenTerm's core still uses tokio for the majority of async work (storage, ingest, pattern engine). The asupersync integration via `web` is additive. A full tokio-to-asupersync migration is tracked separately and is outside this bead's scope.

- **fastapi-output adoption** -- The agent detection and rich terminal output in `fastapi-output` (14K LOC) overlaps with FrankenTerm's own `environment.rs` and TUI modules. We do not adopt it; FrankenTerm already has `franken-agent-detection` and `ftui` for these purposes.

---

## P2: Evaluate Integration Patterns

### Option A: In-Process Library Dependency (Recommended)

Add fastapi_rust as an optional workspace dependency (already present in `Cargo.toml` at Git rev `9fe499e`). FrankenTerm's `web.rs` module imports `fastapi::prelude::*` and constructs an `App` with routes, middleware, and shared state. The HTTP server runs inside FrankenTerm's process, sharing memory and the asupersync runtime.

**Pros:**
- Zero-copy access to FrankenTerm internals (StorageHandle, EventBus, PaneMap) -- no serialization overhead for internal data
- Shared asupersync `Cx` context enables cooperative cancellation, budget enforcement, and structured concurrency across HTTP handlers and core logic
- fastapi's `TestClient` enables in-process HTTP testing without network I/O -- faster, more deterministic tests
- Single binary deployment -- no sidecar process to manage
- Already partially implemented: `web.rs` already imports `fastapi::prelude::*` and uses `App::builder().route()` for 9 endpoints
- WebSocket upgrade flows naturally from the TCP connection handler

**Cons:**
- Tight version coupling on asupersync (mitigated: already locked to same Git rev)
- 111K LOC added to dependency graph when `web` feature is active (mitigated: feature-gated, LTO strips unused code)
- HTTP parser bugs in fastapi-http affect FrankenTerm (mitigated: fastapi-http has extensive tests and zero-copy parser is well-fuzzed)

### Option B: Subprocess with IPC

Run fastapi_rust as a separate process. FrankenTerm spawns `ft-web-server` and communicates via Unix domain socket or TCP.

**Pros:**
- Process isolation -- HTTP server crashes don't take down FrankenTerm
- Independent deployment and restart
- Can be written in any language

**Cons:**
- Serialization overhead for every data access (panes, events, segments)
- No shared `Cx` context -- loses cooperative cancellation and budget enforcement
- Two binaries to build, distribute, and version
- SSE/WebSocket streams require proxying through IPC, adding latency and complexity
- Contradicts the "single-binary swarm node" design philosophy

### Option C: Hybrid -- Library with Optional Process Isolation

Use fastapi_rust in-process by default, but support a `--web-subprocess` flag that forks the HTTP server into a child process with a shared-memory segment for hot data.

**Pros:**
- Best of both worlds in theory
- Process isolation available for high-availability deployments

**Cons:**
- Shared memory requires unsafe code (violates `#![forbid(unsafe_code)]`)
- Massive implementation complexity for marginal benefit
- Two code paths to test and maintain
- FrankenTerm's local-first design doesn't need HA-level isolation

### Decision: Option A -- In-Process Library Dependency

**Rationale:** FrankenTerm already imports fastapi_rust as a workspace dependency and has a working `web.rs` module with 9 endpoints using the fastapi `App` builder. The shared asupersync runtime is the decisive factor: it enables the HTTP server to participate in FrankenTerm's structured concurrency, making WebSocket terminal streaming and SSE event streams first-class citizens of the same cooperative scheduling system. Option B's IPC overhead and Option C's complexity are unjustified for a local-first tool.

---

## P3: Target Placement Within FrankenTerm Subsystems

### Module Layout

```
frankenterm-core/
  src/
    web.rs                          # EXISTING (2,701 LOC) -- refactor into module directory
    web/
      mod.rs                        # NEW: Re-exports, WebServerConfig, start_web_server
      app_builder.rs                # NEW: build_app() with APIRouter-based modular routes
      extractors.rs                 # NEW: FromRequest impls for FrankenTerm domain types
      handlers/
        mod.rs                      # NEW: Handler module re-exports
        health.rs                   # NEW: /health + /readiness with subsystem checks
        panes.rs                    # NEW: /api/v1/panes CRUD + per-pane detail
        events.rs                   # NEW: /api/v1/events query + triage
        search.rs                   # NEW: /api/v1/search full-text + semantic
        bookmarks.rs                # NEW: /api/v1/bookmarks
        rulesets.rs                 # NEW: /api/v1/ruleset-profile
        saved_searches.rs           # NEW: /api/v1/saved-searches
        agents.rs                   # NEW: /api/v1/agents -- agent registration, status
        workflows.rs                # NEW: /api/v1/workflows -- trigger, status, cancel
        reservations.rs             # NEW: /api/v1/reservations -- pane reservation API
      streaming/
        mod.rs                      # NEW: SSE + WebSocket streaming module
        sse.rs                      # NEW: /stream/events, /stream/deltas (migrated from web.rs)
        websocket.rs                # NEW: /ws/terminal/{pane_id} bidirectional terminal
      middleware.rs                 # NEW: FrankenTerm-specific middleware stack
      openapi.rs                    # NEW: OpenAPI document generation
      error.rs                      # NEW: HTTP error mapping from frankenterm_core::Error
      auth.rs                       # NEW: Bearer token + API key authentication middleware
      types.rs                      # NEW: Shared API request/response types
```

### Module Responsibilities

#### `web/mod.rs`
Top-level module. Re-exports `WebServerConfig`, `WebServerHandle`, `start_web_server()`. Replaces the current monolithic `web.rs`. Contains the `AppState` struct and server lifecycle management.

#### `web/app_builder.rs`
Constructs the fastapi `App` using `APIRouter` for modular route grouping. Registers middleware in order: BodySizeGuard, CORS, RequestSpanLogger, AuthMiddleware, StateInjector. Mounts sub-routers:
- `/health` + `/readiness` (health router)
- `/api/v1/panes` (panes router)
- `/api/v1/events` (events router)
- `/api/v1/search` (search router)
- `/api/v1/agents` (agents router)
- `/api/v1/workflows` (workflows router)
- `/stream/*` (SSE streaming router)
- `/ws/*` (WebSocket router)

#### `web/extractors.rs`
Implements `FromRequest` for FrankenTerm domain types:
- `StorageExtractor` -- extracts `StorageHandle` from request extensions
- `EventBusExtractor` -- extracts `Arc<EventBus>` from request extensions
- `RedactorExtractor` -- extracts `Arc<Redactor>` for secret filtering
- `PaneIdPath` -- extracts and validates pane ID from path parameters
- `PaginationQuery` -- extracts limit/offset with bounds enforcement
- `TimeRangeQuery` -- extracts since/until timestamp range
- `StreamConfigQuery` -- extracts max_hz, channel, pane_filter for SSE

#### `web/streaming/websocket.rs`
WebSocket terminal streaming handler. On upgrade:
1. Validates pane ID exists and is accessible
2. Subscribes to the event bus for delta events on that pane
3. Forwards scrollback deltas to the client as JSON frames
4. Receives client keystrokes and injects them via `WeztermHandle::send_text()`
5. Respects the policy engine (PolicyGatedInjector) for injection authorization
6. Handles backpressure via fastapi's cancel-aware stream primitives

#### `web/middleware.rs`
FrankenTerm-specific middleware:
- `RequestSpanLogger` -- structured tracing with method, path, status, duration (migrated from web.rs)
- `StateInjector` -- injects `AppState` into request extensions (migrated from web.rs)
- `BodySizeGuard` -- rejects oversized request bodies (migrated from web.rs)
- `CorsMiddleware` -- wraps fastapi's `Cors` with FrankenTerm defaults (localhost origins)
- `AuthMiddleware` -- optional bearer token validation for non-localhost binds

#### `web/openapi.rs`
Generates an OpenAPI 3.1 document describing all API endpoints. Uses fastapi's `OpenApiBuilder` with manually-specified schemas (no proc-macro dependency). Served at `/openapi.json` and `/docs` (JSON spec + HTML viewer).

#### `web/error.rs`
Maps `frankenterm_core::Error` variants to HTTP responses:
- `Error::Runtime(_)` -> 500 Internal Server Error
- `Error::Storage(_)` -> 503 Service Unavailable
- `Error::WeztermError(_)` -> 502 Bad Gateway
- `Error::Config(_)` -> 500 Internal Server Error
- Validation errors -> 422 Unprocessable Entity

#### `web/auth.rs`
Optional authentication for the HTTP API:
- Bearer token validation (configured via `WebServerConfig::with_auth_token()`)
- API key extraction from `X-API-Key` header or `api_key` query parameter
- Skipped for `/health` endpoint (always public)
- Skipped entirely when binding to localhost (default)

#### `web/types.rs`
Shared API envelope and view types:
- `ApiResponse<T>` -- `{ ok, data, error, error_code, version }` (migrated from web.rs)
- `PaneView`, `EventView`, `SearchResultView`, `BookmarkView` -- response DTOs
- `AgentView`, `WorkflowStatusView`, `ReservationView` -- new agent API DTOs

---

## P4: API Contracts

### Core Server Types

```rust
/// Feature gate: `#[cfg(feature = "web")]`

/// Configuration for the FrankenTerm web server.
#[derive(Clone, Debug)]
pub struct WebServerConfig {
    host: String,
    port: u16,
    storage: Option<StorageHandle>,
    event_bus: Option<Arc<EventBus>>,
    allow_public_bind: bool,
    auth_token: Option<String>,
    cors_origins: Vec<String>,
    enable_openapi: bool,
    enable_websocket: bool,
    max_ws_connections: usize,
}

impl WebServerConfig {
    pub fn new(port: u16) -> Self;
    pub fn with_port(self, port: u16) -> Self;
    pub fn with_host(self, host: impl Into<String>) -> Self;
    pub fn with_storage(self, storage: StorageHandle) -> Self;
    pub fn with_event_bus(self, event_bus: Arc<EventBus>) -> Self;
    pub fn with_dangerous_public_bind(self) -> Self;
    pub fn with_auth_token(self, token: impl Into<String>) -> Self;
    pub fn with_cors_origins(self, origins: Vec<String>) -> Self;
    pub fn with_openapi(self, enable: bool) -> Self;
    pub fn with_websocket(self, enable: bool) -> Self;
    pub fn with_max_ws_connections(self, max: usize) -> Self;
}

/// Handle to a running web server.
pub struct WebServerHandle {
    bound_addr: SocketAddr,
    server: Arc<TcpServer>,
    app: Arc<App>,
    join: task::JoinHandle<std::result::Result<(), ServerError>>,
}

impl WebServerHandle {
    pub fn bound_addr(&self) -> SocketAddr;
    pub async fn shutdown(self) -> Result<()>;
}

/// Start the web server and return a handle for shutdown.
pub async fn start_web_server(config: WebServerConfig) -> Result<WebServerHandle>;
```

### Extractors

```rust
/// Feature gate: `#[cfg(feature = "web")]`

use fastapi::core::extract::FromRequest;

/// Extracts StorageHandle from request extensions.
/// Returns 503 if storage is not configured.
pub struct StorageExtractor(pub StorageHandle);

impl FromRequest for StorageExtractor {
    type Error = Response;
    fn from_request(ctx: &RequestContext, req: &Request) -> Result<Self, Self::Error>;
}

/// Extracts event bus from request extensions.
pub struct EventBusExtractor(pub Arc<EventBus>);

impl FromRequest for EventBusExtractor {
    type Error = Response;
    fn from_request(ctx: &RequestContext, req: &Request) -> Result<Self, Self::Error>;
}

/// Extracts and validates a pane ID from path parameters.
pub struct PaneIdPath(pub u64);

impl FromRequest for PaneIdPath {
    type Error = Response;
    fn from_request(ctx: &RequestContext, req: &Request) -> Result<Self, Self::Error>;
}

/// Pagination parameters with bounds enforcement.
#[derive(Debug, Clone)]
pub struct PaginationQuery {
    pub limit: usize,   // clamped to 1..=500, default 50
    pub offset: usize,  // default 0
}

impl FromRequest for PaginationQuery {
    type Error = Response;
    fn from_request(ctx: &RequestContext, req: &Request) -> Result<Self, Self::Error>;
}

/// Time range filter for event/segment queries.
#[derive(Debug, Clone)]
pub struct TimeRangeQuery {
    pub since: Option<i64>,  // epoch_ms
    pub until: Option<i64>,  // epoch_ms
}

impl FromRequest for TimeRangeQuery {
    type Error = Response;
    fn from_request(ctx: &RequestContext, req: &Request) -> Result<Self, Self::Error>;
}
```

### Handler Traits and Route Registration

```rust
/// Feature gate: `#[cfg(feature = "web")]`

/// Build the FrankenTerm App with all route groups.
fn build_app(config: &WebServerConfig) -> App {
    let mut builder = App::builder()
        .middleware(BodySizeGuard)
        .middleware(CorsMiddleware::new(&config.cors_origins))
        .middleware(RequestSpanLogger::default())
        .middleware(StateInjector::new(config))
        .middleware(AuthMiddleware::new(config.auth_token.clone()));

    // Health (ungrouped, no auth)
    builder = builder
        .route("/health", Method::Get, handlers::health::handle_health)
        .route("/readiness", Method::Get, handlers::health::handle_readiness);

    // API v1 routes
    builder = builder
        .route("/api/v1/panes", Method::Get, handlers::panes::list_panes)
        .route("/api/v1/panes/{pane_id:int}", Method::Get,
               handlers::panes::get_pane)
        .route("/api/v1/events", Method::Get, handlers::events::list_events)
        .route("/api/v1/search", Method::Get, handlers::search::search)
        .route("/api/v1/bookmarks", Method::Get,
               handlers::bookmarks::list_bookmarks)
        .route("/api/v1/ruleset-profile", Method::Get,
               handlers::rulesets::get_profile)
        .route("/api/v1/saved-searches", Method::Get,
               handlers::saved_searches::list)
        .route("/api/v1/agents", Method::Get, handlers::agents::list_agents)
        .route("/api/v1/agents/{agent_id}", Method::Get,
               handlers::agents::get_agent)
        .route("/api/v1/workflows", Method::Get,
               handlers::workflows::list_workflows)
        .route("/api/v1/workflows", Method::Post,
               handlers::workflows::trigger_workflow)
        .route("/api/v1/reservations", Method::Get,
               handlers::reservations::list_reservations)
        .route("/api/v1/reservations", Method::Post,
               handlers::reservations::create_reservation);

    // SSE streaming
    builder = builder
        .route("/stream/events", Method::Get, streaming::sse::stream_events)
        .route("/stream/deltas", Method::Get, streaming::sse::stream_deltas);

    // WebSocket terminal access
    if config.enable_websocket {
        builder = builder.route(
            "/ws/terminal/{pane_id:int}", Method::Get,
            streaming::websocket::terminal_ws,
        );
    }

    // OpenAPI documentation
    if config.enable_openapi {
        builder = builder
            .route("/openapi.json", Method::Get, openapi::serve_spec)
            .route("/docs", Method::Get, openapi::serve_docs);
    }

    builder.build()
}
```

### WebSocket Terminal Protocol

```rust
/// Feature gate: `#[cfg(feature = "web")]`

/// WebSocket message types for terminal streaming.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsClientMessage {
    /// Send keystrokes to a pane.
    Input { pane_id: u64, data: String },
    /// Request a full scrollback snapshot.
    Snapshot { pane_id: u64 },
    /// Subscribe to additional pane streams.
    Subscribe { pane_ids: Vec<u64> },
    /// Unsubscribe from pane streams.
    Unsubscribe { pane_ids: Vec<u64> },
    /// Client-side ping for connection liveness.
    Ping { seq: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsServerMessage {
    /// Scrollback delta for a pane.
    Delta {
        pane_id: u64,
        seq: u64,
        content: String,
        captured_at: i64,
    },
    /// Full scrollback snapshot.
    Snapshot {
        pane_id: u64,
        lines: Vec<String>,
        cursor_row: u32,
        cursor_col: u32,
    },
    /// Pattern detection event.
    Detection {
        pane_id: u64,
        rule_id: String,
        severity: String,
        matched_text: String,
    },
    /// Pane state change (created, closed, resized).
    PaneEvent {
        pane_id: u64,
        event: String,
    },
    /// Pong response to client ping.
    Pong { seq: u64 },
    /// Error response.
    Error { code: String, message: String },
}

/// Handler for WebSocket terminal connections.
///
/// Upgrades the HTTP connection to WebSocket, then:
/// 1. Authenticates the connection (if auth is configured)
/// 2. Subscribes to event bus deltas for the requested pane
/// 3. Enters a bidirectional message loop
pub async fn terminal_ws(
    ctx: &RequestContext,
    req: &mut Request,
) -> Response;
```

### Health Check Integration

```rust
/// Feature gate: `#[cfg(feature = "web")]`

/// Subsystem health status.
#[derive(Debug, Serialize)]
pub struct SubsystemHealth {
    pub name: &'static str,
    pub status: HealthStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

/// GET /readiness response.
#[derive(Debug, Serialize)]
pub struct ReadinessResponse {
    pub ready: bool,
    pub version: &'static str,
    pub subsystems: Vec<SubsystemHealth>,
}

/// Checks all registered subsystems and returns aggregate readiness.
pub async fn check_readiness(state: &AppState) -> ReadinessResponse;
```

### OpenAPI Types (Manual Implementation)

```rust
/// Feature gate: `#[cfg(feature = "web")]`

use fastapi::openapi::{OpenApi, OpenApiBuilder, Schema};

/// Build the FrankenTerm OpenAPI 3.1 specification.
pub fn build_openapi_spec() -> OpenApi {
    OpenApiBuilder::new()
        .title("FrankenTerm API")
        .version(crate::VERSION)
        .description("HTTP API for FrankenTerm swarm-native terminal platform")
        .server("/", "Local server")
        .path("/api/v1/panes", /* ... */)
        .path("/api/v1/events", /* ... */)
        .path("/api/v1/search", /* ... */)
        .path("/api/v1/agents", /* ... */)
        .path("/api/v1/workflows", /* ... */)
        .build()
}
```

---

## P5: Migration Strategy

### No Data Migration Required

This integration adds new capability alongside the existing `web.rs` module. There is no data schema change, no database migration, and no configuration file format change.

### Code Migration: Monolithic web.rs to Module Directory

The existing `web.rs` (2,701 LOC) already uses fastapi types. The migration is a structural refactor:

1. **Phase 1: Create `web/` directory** -- Move `web.rs` to `web/mod.rs` unchanged. Verify all 2,701 LOC compiles identically. This is a zero-behavior-change refactor.

2. **Phase 2: Extract handlers** -- Move each handler function (`handle_panes`, `handle_events`, `handle_search`, etc.) into its corresponding `handlers/*.rs` file. Keep the function signatures identical. Re-export from `handlers/mod.rs`.

3. **Phase 3: Extract middleware** -- Move `RequestSpanLogger`, `StateInjector`, `BodySizeGuard` into `middleware.rs`. Keep signatures identical.

4. **Phase 4: Extract types** -- Move `ApiResponse`, `PaneView`, `EventView`, `SearchResultView`, `BookmarkView`, etc. into `types.rs`.

5. **Phase 5: Extract streaming** -- Move `handle_stream_events`, `handle_stream_deltas`, `TokioSseStream`, and rate-limiting helpers into `streaming/sse.rs`.

Each phase is a standalone commit. Tests pass at every step. The final `web/mod.rs` is a thin orchestrator that calls `build_app()` and `start_web_server()`.

### Backward Compatibility

- The `web` feature flag name does not change
- The `WebServerConfig` public API is extended (new methods), not changed
- The `WebServerHandle` public API is unchanged
- All existing endpoints remain at the same paths
- New `/api/v1/*` endpoints are additive
- The `--port` and `--host` CLI flags work identically

---

## P6: Testing Strategy

### Unit Tests (30+ per module)

Each new module must have at least 30 unit tests, consistent with FrankenTerm's project-wide requirement. Test categories per module:

#### `web/extractors.rs` (target: 35 tests)
- `StorageExtractor` succeeds when storage is configured
- `StorageExtractor` returns 503 when storage is absent
- `EventBusExtractor` succeeds when event bus is configured
- `EventBusExtractor` returns 503 when event bus is absent
- `PaneIdPath` extracts valid pane IDs
- `PaneIdPath` rejects non-numeric paths (400)
- `PaneIdPath` rejects zero pane ID (400)
- `PaginationQuery` defaults to limit=50, offset=0
- `PaginationQuery` clamps limit to MAX_LIMIT (500)
- `PaginationQuery` rejects negative offset (400)
- `TimeRangeQuery` parses epoch_ms since/until
- `TimeRangeQuery` accepts empty (no time filter)
- `TimeRangeQuery` rejects since > until (400)
- Each extractor variant tested with malformed input, missing headers, etc.

#### `web/handlers/health.rs` (target: 30 tests)
- `/health` returns 200 with `{ ok: true, version }`
- `/readiness` returns 200 when all subsystems healthy
- `/readiness` returns 503 when storage unhealthy
- `/readiness` returns 503 when event bus unavailable
- `/readiness` includes per-subsystem latency
- `/readiness` degrades gracefully with partial failures
- Health check timeout behavior

#### `web/handlers/panes.rs` (target: 32 tests)
- List panes returns empty array when no panes
- List panes returns all panes with correct view fields
- List panes respects pagination limit/offset
- Get pane by ID returns 404 for unknown pane
- Get pane by ID returns correct PaneView
- Redactor applied to title and cwd fields
- All PaneView fields serialized correctly

#### `web/handlers/agents.rs` (target: 30 tests)
- List agents returns registered agents
- Get agent by ID returns 404 for unknown agent
- Agent view includes pane associations
- Agent view includes activity timestamps
- Pagination and filtering tests

#### `web/middleware.rs` (target: 30 tests)
- RequestSpanLogger records method, path, status, duration
- StateInjector injects AppState into extensions
- BodySizeGuard rejects bodies > MAX_REQUEST_BODY_BYTES
- BodySizeGuard allows bodies within limit
- BodySizeGuard allows requests without Content-Length
- CorsMiddleware sets correct headers for configured origins
- CorsMiddleware handles preflight OPTIONS requests
- AuthMiddleware passes unauthenticated requests on localhost
- AuthMiddleware rejects unauthenticated requests on non-localhost
- AuthMiddleware accepts valid bearer token
- Middleware ordering: body guard before auth before state injection

#### `web/streaming/websocket.rs` (target: 32 tests)
- WebSocket upgrade succeeds for valid pane ID
- WebSocket upgrade returns 404 for unknown pane
- Client Input message triggers send_text on pane
- Client Snapshot request returns current scrollback
- Server sends Delta messages on event bus updates
- Server sends Detection messages for pattern matches
- Server sends PaneEvent for pane lifecycle changes
- Client Ping receives Pong response
- Connection closes gracefully on client disconnect
- Backpressure: slow client receives lag notification
- Max connections limit enforced
- Policy engine gates input injection

#### `web/error.rs` (target: 30 tests)
- Each `Error` variant maps to correct HTTP status code
- Error response body matches `ApiResponse<()>` format
- Error codes are stable strings
- Redaction applied to error messages
- Debug info excluded in non-debug mode

#### `web/openapi.rs` (target: 30 tests)
- Generated spec is valid OpenAPI 3.1
- All registered paths appear in the spec
- Path parameters documented with correct types
- Response schemas match actual response types
- `/openapi.json` endpoint returns valid JSON
- `/docs` endpoint returns HTML

### Property-Based Tests

#### `tests/proptest_web_extractors.rs` (target: 20 tests)
- Arbitrary query strings never panic extractors (only produce Ok or well-formed error)
- PaginationQuery: `limit` always in `[1, MAX_LIMIT]` after extraction
- PaginationQuery: `offset` always non-negative after extraction
- TimeRangeQuery: when both `since` and `until` present, `since <= until`
- PaneIdPath: rejects all non-u64 strings, accepts all valid u64 strings

#### `tests/proptest_web_types.rs` (target: 15 tests)
- ApiResponse roundtrip: serialize then deserialize preserves all fields
- PaneView serialization: skip_serializing_if fields absent when None
- All view types: arbitrary valid instances serialize to valid JSON

### Integration Tests

#### `tests/web_integration_tests.rs` (target: 25 tests)
Using fastapi's `TestClient` for in-process HTTP testing:
- Full request lifecycle: build app, send request, verify response
- Middleware chain execution order
- SSE stream connection, receive events, disconnect
- WebSocket upgrade, send/receive messages, disconnect
- Health endpoint with mock storage (healthy/unhealthy)
- Concurrent requests don't deadlock
- Graceful shutdown drains active connections
- Large response pagination
- Error responses include correct status codes and error codes

---

## P7: Rollout Plan

### Phase 1: Structural Refactor (1 week)

**Goal:** Decompose `web.rs` into `web/` module directory without behavior change.

**Steps:**
1. Create `web/mod.rs` from existing `web.rs`
2. Extract handlers, middleware, types, and streaming into sub-modules
3. Verify all existing `#[cfg(test)]` tests pass unchanged
4. Verify `ft web` command works identically

**Rollback:** Revert to monolithic `web.rs` (single `git revert`).

**Validation:** `cargo test --features web` passes. Manual smoke test of all 9 endpoints.

### Phase 2: Typed Extractors and API Versioning (1 week)

**Goal:** Replace manual `require_storage(req)` / `require_event_bus(req)` patterns with `FromRequest` extractors. Add `/api/v1/` prefix to data endpoints.

**Steps:**
1. Implement `StorageExtractor`, `EventBusExtractor`, `PaneIdPath`, `PaginationQuery`, `TimeRangeQuery` with 30+ tests each
2. Refactor existing handlers to use extractors
3. Mount data endpoints under `/api/v1/` prefix
4. Keep original paths as redirects for backward compatibility
5. Add `web/error.rs` for centralized error mapping

**Rollback:** Revert extractor commits; original `require_*` functions remain as fallback.

**Validation:** All handler tests pass with extractors. Integration tests verify both old and new paths.

### Phase 3: WebSocket Terminal Streaming (2 weeks)

**Goal:** Add bidirectional WebSocket terminal access.

**Steps:**
1. Implement `WsClientMessage` and `WsServerMessage` types
2. Implement WebSocket upgrade handler in `streaming/websocket.rs`
3. Wire event bus subscription for delta forwarding
4. Wire `WeztermHandle::send_text()` for input injection
5. Implement connection lifecycle (auth, backpressure, graceful close)
6. Add `enable_websocket` config flag (default: false)
7. Write 32+ tests using mock WebSocket clients

**Rollback:** Set `enable_websocket: false` (default). WebSocket route not registered.

**Validation:** Manual test with `websocat` or browser. Automated tests with mock client.

### Phase 4: Agent HTTP API and OpenAPI (1 week)

**Goal:** Expose agent/workflow/reservation endpoints with OpenAPI documentation.

**Steps:**
1. Implement `handlers/agents.rs` with list/get endpoints
2. Implement `handlers/workflows.rs` with list/trigger/cancel endpoints
3. Implement `handlers/reservations.rs` with list/create endpoints
4. Build OpenAPI 3.1 spec in `openapi.rs`
5. Serve `/openapi.json` and `/docs`
6. Write 30+ tests per handler module

**Rollback:** Remove new route registrations from `build_app()`. Existing endpoints unaffected.

**Validation:** OpenAPI spec validates against OpenAPI 3.1 schema. Postman/curl testing of all new endpoints.

### Phase 5: Health Check Integration and Hardening (1 week)

**Goal:** Implement structured readiness probes and production hardening.

**Steps:**
1. Implement `check_readiness()` with subsystem health checks
2. Add CorsMiddleware with configurable origins
3. Add AuthMiddleware for non-localhost deployments
4. Performance testing: verify <1ms latency for /health under load
5. Document all endpoints in `docs/web-api.md`
6. Add `ft web --openapi` CLI flag to dump spec to stdout

**Rollback:** Readiness degrades to simple `/health` endpoint.

**Validation:** Health checks report correct status for each subsystem. Auth middleware blocks unauthorized requests.

### Global Rollback Strategy

The entire fastapi_rust integration is gated behind `#[cfg(feature = "web")]`. To roll back all phases:
1. Remove the `web` feature from any build configurations
2. The `web/` module directory compiles to nothing
3. No runtime behavior change for builds without `web`
4. The `mcp` feature (MCP server) is completely independent and unaffected

---

## P8: Summary

### New Modules

| Module | LOC (est.) | Tests (min) | Purpose |
|--------|-----------|-------------|---------|
| `web/mod.rs` | 200 | -- | Module root, re-exports, server lifecycle |
| `web/app_builder.rs` | 150 | 30 | Route registration with APIRouter |
| `web/extractors.rs` | 400 | 35 | FromRequest impls for domain types |
| `web/handlers/health.rs` | 200 | 30 | /health + /readiness probes |
| `web/handlers/panes.rs` | 300 | 32 | Pane list/detail endpoints |
| `web/handlers/events.rs` | 300 | 30 | Event query endpoints |
| `web/handlers/search.rs` | 250 | 30 | Full-text + semantic search |
| `web/handlers/agents.rs` | 300 | 30 | Agent registration/status API |
| `web/handlers/workflows.rs` | 350 | 30 | Workflow trigger/status/cancel |
| `web/handlers/reservations.rs` | 250 | 30 | Pane reservation API |
| `web/handlers/bookmarks.rs` | 150 | 30 | Bookmark list endpoint |
| `web/handlers/rulesets.rs` | 150 | 30 | Ruleset profile endpoint |
| `web/handlers/saved_searches.rs` | 150 | 30 | Saved searches endpoint |
| `web/streaming/sse.rs` | 500 | 30 | SSE event/delta streams (migrated) |
| `web/streaming/websocket.rs` | 600 | 32 | WebSocket terminal streaming |
| `web/middleware.rs` | 350 | 30 | FrankenTerm middleware stack |
| `web/openapi.rs` | 400 | 30 | OpenAPI 3.1 spec generation |
| `web/error.rs` | 200 | 30 | Error-to-HTTP mapping |
| `web/auth.rs` | 250 | 30 | Bearer token + API key auth |
| `web/types.rs` | 300 | 30 | Shared API DTOs |
| `tests/proptest_web_extractors.rs` | 300 | 20 | Property-based extractor tests |
| `tests/proptest_web_types.rs` | 200 | 15 | Property-based type tests |
| `tests/web_integration_tests.rs` | 500 | 25 | Integration tests with TestClient |

**Total estimated:** ~6,250 LOC new code, ~659 tests minimum.

### Upstream Tweak Proposals

1. **macOS route auto-discovery** (high priority) -- The `fastapi-router::registry` linker-section approach returns empty on macOS. Propose adding `__DATA` Mach-O section support or a build-script fallback. Tracked as potential upstream PR. FrankenTerm workaround: explicit `App::builder().route()` registration (already in use).

2. **Feature-gate `asupersync` in `fastapi-core`** (medium priority) -- Allow using fastapi-core types (Request, Response, extractors) without the full asupersync runtime. This would let FrankenTerm use fastapi types in test code without activating the `web` feature.

3. **Make `ServerMetrics` observable** (low priority) -- Expose `ServerMetrics` as a structured snapshot for integration with FrankenTerm's `telemetry.rs` and `storage_telemetry.rs` modules.

4. **Publish `asupersync` and `rich_rust` to crates.io** (medium priority) -- Currently sourced via Git with path patches. Publishing would simplify `Cargo.toml` and enable proper semver resolution.

### Beads References

| Bead | Description |
|------|-------------|
| `ft-2vuw7.25.1` | Research pass: identify integration candidates |
| `ft-2vuw7.25.2` | Comprehensive analysis of fastapi_rust (prerequisite) |
| `ft-2vuw7.25.3` | This integration plan |
| `ft-2vuw7.25.4` | (future) Phase 1 implementation: structural refactor |
| `ft-2vuw7.25.5` | (future) Phase 2 implementation: typed extractors |
| `ft-2vuw7.25.6` | (future) Phase 3 implementation: WebSocket streaming |
| `ft-2vuw7.25.7` | (future) Phase 4 implementation: agent API + OpenAPI |
| `ft-2vuw7.25.8` | (future) Phase 5 implementation: health checks + hardening |

---

*Plan complete.*
