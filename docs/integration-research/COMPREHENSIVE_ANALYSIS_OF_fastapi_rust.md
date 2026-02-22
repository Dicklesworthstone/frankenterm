# Comprehensive Analysis of fastapi_rust

> Analysis document for FrankenTerm bead `ft-2vuw7.25.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: Research pass ft-2vuw7.25.1

---

## R1: Repository Topology and Crate/Module Boundary Inventory

### Workspace Overview

The repository at `/dp/fastapi_rust` is a Rust 2024-edition workspace containing **8 crates** organized under `crates/`. The workspace version is `0.2.0`, published under MIT OR Apache-2.0 dual license. Total codebase size: **~111,700 lines of Rust**.

### Crate Inventory

| Crate | Package Name | LOC | Purpose |
|-------|-------------|-----|---------|
| `crates/fastapi-types` | `fastapi-types` | 68 | Shared zero-dependency type definitions (HTTP `Method` enum) |
| `crates/fastapi-router` | `fastapi-router` | 4,247 | Trie-based radix router with path parameters and conflict detection |
| `crates/fastapi-openapi` | `fastapi-openapi` | 2,166 | OpenAPI 3.1 schema types and document builder |
| `crates/fastapi-macros` | `fastapi-macros` | 2,937 | Procedural macros: `#[get]`, `#[derive(Validate)]`, `#[derive(JsonSchema)]` |
| `crates/fastapi-core` | `fastapi-core` | 64,941 | Core framework: Request/Response, extractors, middleware, DI, testing |
| `crates/fastapi-http` | `fastapi-http` | 20,887 | Zero-copy HTTP/1.1 parser, HTTP/2 framing, TCP server, WebSocket |
| `crates/fastapi-output` | `fastapi-output` | 14,370 | Agent-aware rich console output with environment detection |
| `crates/fastapi` | `fastapi-rust` | 2,051 | User-facing facade crate re-exporting all sub-crates |

### Dependency Graph Between Crates

```
fastapi-types (leaf, zero deps)
    ^
    |
fastapi-router (depends on: fastapi-types, serde_json)
    ^
    |
fastapi-openapi (depends on: fastapi-router, fastapi-types, serde, serde_json)
    ^
    |
fastapi-core (depends on: fastapi-types, fastapi-router, fastapi-openapi, asupersync, serde, serde_json, parking_lot)
    ^
    |
fastapi-http (depends on: fastapi-core, asupersync)
    ^
    |
fastapi-macros (standalone proc-macro: proc-macro2, quote, syn, regex, serde_json)
    ^
    |
fastapi-rust [facade] (depends on: fastapi-core, fastapi-http, fastapi-router, fastapi-macros, fastapi-openapi, fastapi-output?)
```

`fastapi-output` is optional (`dep:fastapi-output`) and independently depends on `crossterm`, `parking_lot`, `regex`, `unicode-width`, and optionally `rich_rust`.

### Key Modules per Crate

**fastapi-core** (the largest crate, 64,941 LOC):
- `middleware.rs` (13,206 LOC) -- Middleware trait, `ControlFlow`, `MiddlewareStack`, built-in middleware: CORS (`Cors`/`CorsConfig`), `SecurityHeaders`, `RequestId`, `RequestResponseLogger`, `RequireHeader`, `AddResponseHeader`, `PathPrefixFilter`, `ReferrerPolicy`
- `extract.rs` (8,445 LOC) -- `FromRequest` trait, extractors: `Json<T>`, `Path<T>`, `Query<T>`, `Header`, `BearerToken`, `BasicAuth`, `OAuth2PasswordBearer`, `Cookie`, `Pagination`, `Form`, `ApiKey`, `SessionId`, `CsrfToken`, `AppState`, `State<T>`, `Valid<T>`, `ContentType`, `Accept`, `UserAgent`, `Host`, `XRequestId`
- `testing.rs` (7,757 LOC) -- `TestClient`, `TestResponse`, `RequestBuilder`, `CookieJar`, assertion macros (`assert_status!`, `assert_header!`, `assert_body_contains!`, `assert_json!`, `assert_body_matches!`), `IntegrationTest`, `TestFixture`
- `response.rs` (3,492 LOC) -- `Response`, `StatusCode`, `ResponseBody`, `IntoResponse` trait, `Redirect`, `FileResponse`, `Html`, `Text`, `Binary`, `NoContent`, `SetCookie`, `LinkHeader`, conditional response helpers, field include/exclude, MIME detection
- `error.rs` (3,074 LOC) -- `HttpError`, `ValidationError`, `ValidationErrors`, `DebugConfig`, `DebugInfo` with source location, debug mode toggle
- `app.rs` (2,848 LOC) -- `App`, `AppBuilder`, `AppConfig`, `RouteEntry`, `StateContainer`, `ExceptionHandlers`, `StartupHook`, lifecycle management
- `dependency.rs` (2,827 LOC) -- `Depends<T>`, `FromDependency`, `FromDependencyWithCleanup`, `DependencyCache`, `DependencyOverrides`, `DependencyScope`, `CleanupStack`, circular dependency detection
- `multipart.rs` (1,846 LOC) -- `MultipartParser`, `MultipartForm`, `Part`, `UploadFile`, streaming multipart support
- `routing.rs` (1,670 LOC) -- `TrailingSlashMode`, path converters, UUID validation, path traversal prevention
- `logging.rs` (1,172 LOC) -- `LogConfig`, `LogEntry`, `LogLevel`, `Span`, `AutoSpan`, structured logging
- `api_router.rs` (1,152 LOC) -- `APIRouter` for modular route grouping with shared prefixes, tags, and dependencies
- `fixtures.rs` (1,182 LOC) -- Test fixtures and fixture management
- `bench.rs` (1,539 LOC) -- Built-in benchmarking utilities
- `context.rs` -- `RequestContext` wrapping asupersync `Cx`, body limit config, dependency cache per request
- `shutdown.rs` -- `ShutdownController`, `GracefulShutdown`, 6-phase shutdown state machine
- `websocket.rs` -- RFC 6455 WebSocket frame codec, SHA-1/base64 handshake (no external deps)
- `password.rs` -- PBKDF2-SHA256 password hashing, constant-time comparison
- `digest.rs` -- HTTP Digest authentication (RFC 7616)
- `sse.rs` -- Server-Sent Events streaming
- `ndjson.rs` -- Newline-Delimited JSON streaming
- `static_files.rs` -- Static file serving with ETag, Last-Modified, path traversal prevention
- `versioning.rs` -- API version negotiation (URL prefix, header, accept header)
- `health.rs` -- Health check registry with liveness/readiness probes
- `coverage.rs` -- Per-endpoint coverage tracking
- `fault.rs` -- Fault injection for resilience testing (delay, error, timeout, corrupt)
- `loadtest.rs` -- Load testing utilities
- `proptest.rs` -- Property-based testing strategies for HTTP requests
- `validation.rs` -- `Validate` trait, email/URL/phone/regex validation

**fastapi-http** (20,887 LOC):
- `server.rs` (5,947 LOC) -- `Server`, `TcpServer`, `ServerConfig`, `AppServeExt`, connection lifecycle, region hierarchy
- `parser.rs` (2,373 LOC) -- Zero-copy HTTP/1.1 parser, `RequestLine`, `HeadersIter`, `StatefulParser`
- `body.rs` (2,001 LOC) -- `BodyConfig`, `ContentLengthReader`, `ChunkedReader`, streaming body
- `websocket.rs` (1,418 LOC) -- Server-side WebSocket upgrade, frame reading/writing
- `http2.rs` (976 LOC) -- HTTP/2 framing + HPACK decoder (cleartext h2c prior-knowledge)
- `streaming.rs` (984 LOC) -- `FileStream`, `CancelAwareStream`, chunked encoding
- `connection.rs` -- HTTP Connection header parsing, hop-by-hop header stripping
- `query.rs` -- Query string parsing with percent-decoding
- `range.rs` -- HTTP Range header parsing (byte ranges)
- `expect.rs` -- `Expect: 100-continue` handling, pre-body validation hooks
- `multipart.rs` -- Server-side multipart form parsing
- `response.rs` -- HTTP response serialization, chunked encoder, trailers

**fastapi-router** (4,247 LOC):
- `trie.rs` (3,930 LOC) -- Radix trie implementation, `Router`, `Route`, path parameter extraction, converters (`Str`/`Int`/`Float`/`Uuid`/`Path`), wildcard catch-all, conflict detection
- `match.rs` -- `RouteMatch`, `RouteLookup` (Match/MethodNotAllowed/NotFound), `AllowedMethods`
- `registry.rs` -- Link-section based route auto-discovery (ELF platforms), `RouteRegistration`

**fastapi-macros** (2,937 LOC):
- `route.rs` (1,036 LOC) -- `#[get]`/`#[post]`/etc. attribute macro implementation, route attribute parsing
- `openapi.rs` -- `#[derive(JsonSchema)]` for OpenAPI 3.1 schema generation
- `validate.rs` -- `#[derive(Validate)]` with field-level constraint attributes
- `param.rs` -- Parameter metadata extraction
- `response_model.rs` -- Response model code generation

**fastapi-output** (14,370 LOC):
- `components/` -- Banner, RouteDisplay, ErrorFormatter, RequestLogger, MiddlewareStackDisplay, DependencyTreeDisplay, ShutdownProgressDisplay, TestReportDisplay, OpenApiDisplay, HelpDisplay, HttpInspector, RoutingDebug
- `facade.rs` -- `RichOutput` with global singleton, dual-mode rendering
- `detection.rs` -- Agent environment detection (Claude Code, Codex, Cursor, Aider, Copilot, etc.)
- `themes.rs` (1,613 LOC) -- FastAPI-themed color palette, `ThemePreset`
- `mode.rs` -- `OutputMode` (Rich/Plain/Minimal), auto-detection
- `testing.rs` -- `TestOutput`, capture utilities, ANSI code stripping

---

## R2: Build/Runtime/Dependency Map and Feature-Flag Matrix

### Build Requirements

| Requirement | Value |
|------------|-------|
| Rust edition | 2024 |
| MSRV (`rust-version`) | 1.85 |
| Toolchain | `nightly` (per `rust-toolchain.toml`) |
| Resolver | 2 |
| Workspace lints | `unsafe_code = "warn"`, clippy pedantic |

The nightly toolchain is required due to Rust 2024 edition features.

### Feature-Flag Matrix

**Top-level `fastapi-rust` crate:**

| Feature | Default | Description |
|---------|---------|-------------|
| `output` | **yes** | Rich console output with ANSI, agent detection (enables `fastapi-output/rich`) |
| `output-plain` | no | Plain-text-only output (no ANSI, smaller binary) |
| `full` | no | All output features including all themes/components |

**`fastapi-core`:**

| Feature | Default | Description |
|---------|---------|-------------|
| `regex` | no | Regex support in testing assertions (`dep:regex`) |
| `compression` | no | Reserved for response compression middleware (gzip via flate2) |

**`fastapi-output`:**

| Feature | Default | Description |
|---------|---------|-------------|
| `rich` | **yes** | Rich terminal output via `rich_rust` |
| `full` | no | All `rich_rust` features (every theme/component) |

### External Dependencies (with versions)

**Runtime dependencies:**

| Dependency | Version | Used by | Purpose |
|-----------|---------|---------|---------|
| `asupersync` | 0.2.0 (git) | core, http | Async runtime: structured concurrency, cancel-correctness, I/O |
| `serde` | 1 (derive) | core, openapi, facade | Serialization framework |
| `serde_json` | 1 | core, router, openapi, macros | JSON serialization |
| `parking_lot` | 0.12 | core, output | Fast mutex/rwlock |
| `crossterm` | 0.28 | output | Terminal detection (TTY check) |
| `unicode-width` | 0.2 | output | Unicode character width calculation |
| `regex` | 1.12 | output, macros, core (optional) | Pattern matching |
| `rich_rust` | 0.2.0 (optional) | output | Rich terminal formatting |
| `proc-macro2` | 1 | macros | Proc-macro token streams |
| `quote` | 1 | macros | Proc-macro code generation |
| `syn` | 2 (full, parsing) | macros | Rust syntax parsing |
| `futures-executor` | 0.3 | core | `block_on` for sync test execution |

**Dev dependencies:**

| Dependency | Version | Used by |
|-----------|---------|---------|
| `serial_test` | 3.3.1 / 3.2 | core, output |
| `criterion` | 0.5 | http (benchmarks) |
| `proptest` | 1 | output |
| `insta` | 1.34 | output (snapshot tests) |

**Key external dependency: `asupersync`**

The `asupersync` crate is the async runtime foundation. It is sourced from a Git repository (`https://github.com/Dicklesworthstone/asupersync.git`, rev `c7c15f6...`) and locally patched via `.cargo/config.toml` to `../asupersync`. This is the same runtime used by FrankenTerm (noted in MEMORY.md). It provides:
- `Cx` capability context with cancellation and budgets
- `Scope`/`Region` for structured concurrency
- `TcpListener`/`TcpStream` async I/O
- `Stream` trait for async iteration
- `Budget`/`Time` for deadline enforcement
- Signal handling (`ShutdownController`/`ShutdownReceiver`)

### Runtime Requirements

- TCP socket binding (for HTTP server)
- No Tokio, Hyper, or Tower dependencies -- pure asupersync
- ELF linker section support for route auto-discovery (Linux/Android/FreeBSD); macOS falls back to empty registry (manual route registration)
- Optional: TTY for rich output detection

---

## R3: Public Surface Inventory (APIs/CLI/MCP/Config/Events)

### Core Traits

| Trait | Module | Purpose |
|-------|--------|---------|
| `FromRequest` | `fastapi_core::extract` | Extract typed data from HTTP requests |
| `IntoResponse` | `fastapi_core::response` | Convert types into HTTP responses |
| `Middleware` | `fastapi_core::middleware` | Before/after hooks with `ControlFlow` |
| `Handler` | `fastapi_core::middleware` | Async request handler abstraction |
| `Validate` | `fastapi_core::validation` | Field-level constraint validation |
| `JsonSchema` | `fastapi_openapi::schema` | OpenAPI 3.1 schema generation |
| `FromDependency` | `fastapi_core::dependency` | Dependency injection resolution |
| `FromDependencyWithCleanup` | `fastapi_core::dependency` | DI with teardown (yield-style) |
| `ShutdownAware` | `fastapi_core::shutdown` | Graceful shutdown participation |
| `IntoOutcome` | `fastapi_core::context` | Bridge HTTP errors to asupersync `Outcome` |

### Core Types

**Request/Response:**
- `Request` -- HTTP request with method, path, headers, body, query string, extensions
- `Response` -- HTTP response with status, headers, body; builder pattern (`Response::ok()`, `Response::with_status()`)
- `StatusCode` -- Newtype around `u16` with named constants
- `ResponseBody` -- `Empty`, `Bytes(Vec<u8>)`, `Stream(BodyStream)`
- `Body` -- Request body: `Empty`, `Bytes`, `Stream { stream, content_length }`
- `Headers` -- Case-insensitive header map
- `HttpVersion` -- `Http10`, `Http11`, `Http2`

**Extractors (all implement `FromRequest`):**
- `Json<T>` -- JSON body extraction with configurable size limits
- `Path<T>` / `PathParams` -- Path parameter extraction
- `Query<T>` / `QueryParams` -- Query string extraction
- `Header<T>` / `NamedHeader` -- HTTP header extraction
- `Cookie` / `CookieName` -- Cookie extraction
- `Form<T>` -- URL-encoded form data
- `BearerToken` -- Bearer authentication
- `BasicAuth` -- HTTP Basic authentication
- `OAuth2PasswordBearer` -- OAuth2 password flow
- `ApiKey` -- API key extraction (header, query, or cookie)
- `Pagination` / `Page` -- Pagination with `page` and `per_page`
- `State<T>` / `AppState` -- Typed application state
- `Valid<T>` -- Validated extraction (auto-validates via `Validate` trait)
- `Depends<T>` -- Dependency injection
- `BackgroundTasks` -- Post-response task queue
- `Accept`, `ContentType`, `Host`, `UserAgent`, `XRequestId`, `SessionId`, `CsrfToken`, `Authorization`

**Application:**
- `App` / `AppBuilder` -- Application container with fluent builder
- `AppConfig` -- Application configuration (name, version, debug mode, root_path, trailing slash mode, max body size, request timeout)
- `RouteEntry` -- Registered route with method, path, handler, OpenAPI metadata
- `APIRouter` -- Modular route grouping with shared prefix/tags/dependencies
- `StateContainer` -- Type-safe state storage (`TypeId` -> `Arc<dyn Any>`)
- `ExceptionHandlers` -- Type-dispatched error handler registry

**Router:**
- `Router` -- Radix trie router
- `Route` -- Route definition with method, path, path params, converters, OpenAPI metadata
- `RouteMatch` -- Matched route with extracted parameters
- `RouteLookup` -- `Match` / `MethodNotAllowed` / `NotFound`
- `Converter` -- `Str`, `Int`, `Float`, `Uuid`, `Path` (catch-all)
- `ParamValue` -- Type-converted parameter value

**Middleware (built-in):**
- `Cors` / `CorsConfig` -- Cross-Origin Resource Sharing
- `SecurityHeaders` / `SecurityHeadersConfig` -- Security response headers (CSP, HSTS, etc.)
- `RequestId` / `RequestIdConfig` / `RequestIdMiddleware` -- Request ID generation/propagation
- `RequestResponseLogger` -- Structured request/response logging
- `RequireHeader` -- Header requirement enforcement
- `AddResponseHeader` -- Static response header injection
- `PathPrefixFilter` -- Path-based middleware filtering
- `ReferrerPolicy` -- Referrer-Policy header
- `NoopMiddleware` -- No-op placeholder

**Server:**
- `Server` / `TcpServer` -- TCP server with asupersync integration
- `ServerConfig` -- Bind address, timeouts, connection limits, parse limits, keep-alive, drain timeout, host validation, pre-body validators
- `serve()` / `serve_with_config()` -- Top-level server launch functions

**Shutdown:**
- `ShutdownController` -- Shutdown signal management
- `GracefulShutdown` / `GracefulConfig` -- Grace period configuration
- `ShutdownPhase` -- 7-phase state machine: Running -> StopAccepting -> ShutdownFlagged -> GracePeriod -> Cancelling -> RunningHooks -> Stopped
- `InFlightGuard` -- RAII guard for tracking in-flight requests
- `ShutdownHook` -- Cleanup callbacks during shutdown

**OpenAPI:**
- `OpenApi` / `OpenApiBuilder` -- OpenAPI 3.1 document building
- `Schema` -- JSON Schema types: `Boolean`, `Ref`, `Object`, `Array`, `Primitive`, `Enum`, `OneOf`
- `Operation`, `PathItem`, `Parameter`, `RequestBody`, `Response` (OpenAPI spec types)
- `SchemaRegistry` -- Component schema registry

### Procedural Macros

| Macro | Type | Purpose |
|-------|------|---------|
| `#[get("/path")]` | attribute | Register GET route handler |
| `#[post("/path")]` | attribute | Register POST route handler |
| `#[put("/path")]` | attribute | Register PUT route handler |
| `#[delete("/path")]` | attribute | Register DELETE route handler |
| `#[patch("/path")]` | attribute | Register PATCH route handler |
| `#[head("/path")]` | attribute | Register HEAD route handler |
| `#[options("/path")]` | attribute | Register OPTIONS route handler |
| `#[derive(Validate)]` | derive | Generate field-level validation |
| `#[derive(JsonSchema)]` | derive | Generate OpenAPI JSON Schema |

Route macros support additional attributes: `summary`, `description`, `operation_id`, `tags`, `deprecated`, `responses`.

### Configuration Options

**`AppConfig`:**
- `name: String` -- Application name (used in logging/OpenAPI)
- `version: String` -- Application version
- `debug: bool` -- Enable debug mode
- `root_path: String` -- Root path for proxied deployments
- `root_path_in_servers: bool` -- Include root_path in OpenAPI servers
- `trailing_slash_mode: TrailingSlashMode` -- `Strict`/`Redirect`/`RedirectWithSlash`/`MatchBoth`
- `debug_config: DebugConfig` -- Debug token authentication
- `max_body_size: usize` -- Max request body size (default 1MB)
- `request_timeout_ms: u64` -- Default request timeout (default 30,000ms)

**`ServerConfig`:**
- `bind_addr: String` -- Bind address (default `127.0.0.1:8080`)
- `request_timeout: Time` -- Default 30s
- `max_connections: usize` -- Default 0 (unlimited)
- `read_buffer_size: usize` -- Default 8,192 bytes
- `parse_limits: ParseLimits` -- Max request line, header count/size
- `allowed_hosts: Vec<String>` -- Host header validation
- `trust_x_forwarded_host: bool` -- Proxy support
- `tcp_nodelay: bool` -- Default true
- `keep_alive_timeout: Duration` -- Default 75s
- `max_requests_per_connection: usize` -- Default 100
- `drain_timeout: Duration` -- Default 30s
- `pre_body_validators: PreBodyValidators` -- Pre-body validation hooks

**Environment Variables (output detection):**
- `FASTAPI_OUTPUT_MODE` -- Force output mode: `rich`/`plain`/`minimal`
- `FASTAPI_AGENT_MODE=1` -- Force agent detection
- `FASTAPI_HUMAN_MODE=1` -- Force human detection
- `FORCE_COLOR` -- Force rich output
- `NO_COLOR` -- Force plain output
- Agent detection: `CLAUDE_CODE`, `CODEX_CLI`, `CURSOR_SESSION`, `AIDER_SESSION`, `AGENT_MODE`, `WINDSURF_SESSION`, `CLINE_SESSION`, `COPILOT_AGENT`
- CI detection: `CI`, `GITHUB_ACTIONS`, `GITLAB_CI`, `JENKINS_URL`, `CIRCLECI`, `TRAVIS`, `BUILDKITE`

### Events/Hooks

- `StartupHook` -- Sync or async hooks run before server starts accepting connections
- `ShutdownHook` -- Cleanup callbacks during shutdown
- `CleanupStack` -- LIFO cleanup functions for dependency teardown (yield-style DI)
- `BackgroundTasks` -- Post-response task queue
- Middleware `before`/`after` hooks

### CLI

No standalone CLI binary is included in the workspace. The framework is a library; applications build their own binaries using `App::builder().build()` and `serve()`.

---

## R4: Execution-Flow Tracing Across Core Workflows

### Request Lifecycle (end-to-end)

```
1. TCP Accept (server.rs)
   └── TcpListener::accept() in server region
   └── Create Connection Region (child of Server Region)

2. Read & Parse (parser.rs)
   └── Read bytes from TcpStream into buffer
   └── Parser::parse() -> RequestLine + HeadersIter
   └── Construct Request { method, path, query, headers, body }
   └── HTTP version detection (HTTP/1.1 vs HTTP/2 via preface)

3. Pre-Body Validation (expect.rs)
   └── Evaluate PreBodyValidators (auth, content-type, content-length checks)
   └── If Expect: 100-continue -> send "100 Continue" or reject

4. Body Handling (body.rs)
   └── Content-Length: ContentLengthReader with size limit enforcement
   └── Chunked: ChunkedReader with async chunk parsing
   └── Streaming: Body::Stream wraps as async Stream

5. Route Matching (trie.rs)
   └── Router::lookup(method, path) -> RouteLookup
   └── Priority: static > named params > wildcards
   └── Extract path parameters -> RouteMatch { route, params }
   └── 404 if NotFound, 405 if MethodNotAllowed (with Allow header)

6. Request Context Creation (context.rs)
   └── RequestContext::new(Cx, request_id)
   └── Per-request: DependencyCache, DependencyOverrides, ResolutionStack, CleanupStack
   └── Body limit from AppConfig

7. Middleware Before Phase (middleware.rs)
   └── For each middleware in registration order:
       └── mw.before(ctx, req) -> ControlFlow
       └── Continue: proceed to next
       └── Break(response): skip remaining before hooks + handler

8. Handler Execution (app.rs)
   └── RouteEntry.call(ctx, req) -> Response
   └── Extractors resolve: FromRequest::from_request(ctx, req)
   └── Dependency injection: Depends<T> resolves via FromDependency
       └── Circular dependency detection via ResolutionStack
       └── Request-scoped caching via DependencyCache
   └── Handler returns Response

9. Middleware After Phase (middleware.rs)
   └── For each middleware in REVERSE order:
       └── mw.after(ctx, req, response) -> Response
       └── Cleanup middleware always runs (even on short-circuit)

10. Dependency Cleanup (dependency.rs)
    └── CleanupStack::run_cleanups() in LIFO order
    └── Yield-style dependencies get their teardown

11. Background Tasks
    └── BackgroundTasks execute after response is sent

12. Response Serialization (response.rs in http)
    └── ResponseWriter::write(status, headers, body)
    └── Content-Length or Chunked encoding
    └── Keep-alive evaluation

13. Connection Management (server.rs)
    └── Keep-alive: wait for next request (up to keep_alive_timeout)
    └── Max requests per connection check
    └── Connection close on error or limit
```

### Graceful Shutdown Flow

```
Signal (SIGTERM/SIGINT)
  -> ShutdownController::trigger()
  -> Phase: Running -> StopAccepting
     - Server stops accepting new TCP connections
  -> Phase: StopAccepting -> ShutdownFlagged
     - New requests on existing connections receive 503
  -> Phase: ShutdownFlagged -> GracePeriod
     - In-flight requests get remaining time budget
     - InFlightGuard tracks active requests
  -> Phase: GracePeriod -> Cancelling (after drain_timeout)
     - Budget exhaustion cancels remaining requests
     - Cx.checkpoint() returns CancelledError
  -> Phase: Cancelling -> RunningHooks
     - ShutdownHooks execute (cleanup, close pools, etc.)
  -> Phase: RunningHooks -> Stopped
     - Server region closes
```

### Error Handling Flow

```
Handler error path:
  1. HttpError::not_found() / ::bad_request() / ::internal_server_error()
  2. HttpError implements IntoResponse -> JSON { detail, status_code, [debug_info] }
  3. ExceptionHandlers registry matches error type
  4. Fallback: 500 Internal Server Error

Validation error path:
  1. Valid<T> extractor calls T::validate()
  2. Failure -> ValidationErrors with field-specific messages
  3. ValidationErrors implements IntoResponse -> 422 Unprocessable Entity

Cancellation error path:
  1. ctx.checkpoint() returns CancelledError
  2. CancelledError -> 499 Client Closed Request or 504 Gateway Timeout
  3. IntoOutcome bridges to asupersync Outcome::Cancelled

Parse error path:
  1. Parser returns ParseError
  2. Server converts to 400 Bad Request response
```

---

## R5: Persistence, State, and Data Contracts

### Database/Storage

fastapi_rust has **no built-in database layer**. The framework is storage-agnostic; applications supply their own database connections via `State<T>` or `Depends<T>`. The examples use `Mutex<HashMap<u64, User>>` as in-memory storage.

### State Management

**Application State:**
- `StateContainer` -- Type-keyed map (`TypeId` -> `Arc<dyn Any + Send + Sync>`)
- Inserted at build time via `AppBuilder::state(value)`
- Extracted via `State<T>` or `AppState<T>` extractors
- Shared across all requests (read-only, behind `Arc`)

**Request-Scoped State:**
- `RequestContext` -- Per-request context with:
  - `request_id: u64` -- Unique request identifier
  - `DependencyCache` -- Request-scoped dependency cache (cached `FromDependency` results)
  - `DependencyOverrides` -- Per-request dependency overrides (primarily for testing)
  - `ResolutionStack` -- Cycle detection for dependency graphs
  - `CleanupStack` -- LIFO cleanup functions
  - `BodyLimitConfig` -- Per-request body size limit
  - `Cx` -- asupersync capability context

**Request Extensions:**
- `Request` supports typed extensions via `TypeId`-keyed `HashMap`
- `ConnectionInfo` stored as extension (TLS detection)

### Serialization Formats

| Format | Usage | Implementation |
|--------|-------|---------------|
| JSON | Request/response bodies, OpenAPI spec, error responses | `serde_json` |
| NDJSON | Streaming data export | `NdjsonResponse` (custom serialization) |
| SSE | Server-Sent Events | `SseResponse` (text/event-stream) |
| URL-encoded forms | Form data extraction | Custom parser in `extract.rs` |
| Multipart | File uploads | Custom parser in `multipart.rs` |
| HTTP/1.1 | Wire protocol | Zero-copy parser in `parser.rs` |
| HTTP/2 | Wire protocol (h2c) | Frame parser + HPACK in `http2.rs` |
| WebSocket | Bidirectional messages | Frame codec in `websocket.rs` |
| PBKDF2-SHA256 | Password hashing | Custom implementation in `password.rs` |
| MD5/SHA-256 | Digest authentication | Custom implementation in `digest.rs` |
| OpenAPI 3.1 | API documentation | `serde`-based types in `spec.rs` |

### Data Contracts

**Request -> Handler:**
- `Request { method, path, query, headers, body, extensions }`
- Typed extraction via `FromRequest` trait implementations

**Handler -> Response:**
- `Response { status, headers, body }` via `IntoResponse` trait
- `ResponseBody::Bytes(Vec<u8>)` for buffered responses
- `ResponseBody::Stream(BodyStream)` for streaming responses

**OpenAPI Contract:**
- `OpenApi` document with `Info`, `Server`, `PathItem`, `Operation`, `Parameter`, `RequestBody`, `Response`, `Schema`
- Generated from route metadata + `JsonSchema` derive

---

## R6: Reliability, Performance, and Security Posture

### Error Handling Patterns

1. **Type-safe error hierarchy:** `HttpError` with named constructors (`not_found()`, `bad_request()`, etc.) carrying status codes and detail messages
2. **Validation errors:** Field-specific errors with location paths (body/query/path), FastAPI-compatible JSON format
3. **Debug mode:** Optional `DebugInfo` with source location, handler name, route pattern -- gated behind `DebugConfig` with token authentication
4. **Exception handler registry:** Type-dispatched error handlers via `ExceptionHandlers` with downcasting
5. **Cancellation handling:** `CancelledError` from `ctx.checkpoint()` converts to 499/504
6. **Startup error classification:** `StartupHookError` with `abort` flag differentiating fatal vs non-fatal
7. **Dependency cycle detection:** Runtime detection with full cycle path in panic message

### Performance Characteristics

1. **Zero-copy HTTP parsing:** `RequestLine` and `HeadersIter` borrow from the original buffer; no allocation on the hot path
2. **Radix trie routing:** O(path_length) lookups with priority-based matching (static > param > wildcard)
3. **Lock-free read path:** `parking_lot::RwLock` for shared state; `AtomicBool`/`AtomicU64` for hot counters (server metrics)
4. **Structured concurrency:** Each request runs in its own asupersync region; connection pooling via region hierarchy
5. **Cancel-correct:** Cooperative checkpoints at middleware boundaries and body reading; budget-based timeouts
6. **Pre-allocated buffers:** `DEFAULT_READ_BUFFER_SIZE = 8192` for socket reads
7. **Keep-alive:** Connection reuse with configurable timeout (75s default) and max requests per connection (100 default)
8. **Streaming responses:** `BodyStream` and `ChunkedEncoder` avoid buffering large responses
9. **Criterion benchmarks:** HTTP parser benchmarks in `crates/fastapi-http/benches/http_parser.rs`
10. **Built-in load testing:** `LoadTest` utility for stress testing handlers

### Security Posture

**Positive:**
1. `#![forbid(unsafe_code)]` in `fastapi-core`, `fastapi-types`, `fastapi-openapi`; `#![deny(unsafe_code)]` in `fastapi-http`, `fastapi-output`
2. **CRLF injection prevention:** Header name/value validation rejects control characters
3. **Path traversal prevention:** Converter rejects `..` and `.` segments; `static_files.rs` validates resolved paths
4. **Body size limits:** Default 1MB with per-request override; enforced at parser and extractor levels
5. **Host header validation:** `ServerConfig::allowed_hosts` prevents host header attacks
6. **CORS middleware:** Configurable origin patterns, credentials, methods, headers
7. **Security headers middleware:** CSP, HSTS, X-Frame-Options, X-Content-Type-Options, Referrer-Policy, Permissions-Policy
8. **Constant-time password comparison:** `constant_time_eq()` in `password.rs`
9. **PBKDF2-SHA256 hashing:** Configurable iterations (default 600,000)
10. **Digest authentication:** RFC 7616 support with nonce verification
11. **Cookie security:** `SameSite`, `Secure`, `HttpOnly` attributes
12. **CSRF token extraction:** `CsrfToken` and `CsrfTokenCookie` extractors

**Considerations:**
1. `#![warn(unsafe_code)]` in `fastapi-router` (ELF linker section access in `registry.rs`)
2. Route auto-discovery via linker sections only works on Linux/Android/FreeBSD (macOS returns empty)
3. SHA-1 implementation for WebSocket handshake is custom (not audited crypto)
4. MD5 in Digest auth is inherently weak (supported alongside SHA-256)
5. No rate limiting middleware built-in (documented as a pattern but not implemented)

---

## R7: Integration Seams and Extraction Candidates

### Shared Runtime Foundation: asupersync

Both FrankenTerm and fastapi_rust use `asupersync` as their async runtime. This is the most significant integration seam:

- **Shared `Cx` context:** FrankenTerm's `RequestContext` wrapping `Cx` is directly compatible
- **Structured concurrency:** Both use region-based task hierarchy
- **Cancel-correctness:** Both use `checkpoint()` for cooperative cancellation
- **Budget/Time:** Both use budget-based timeouts

The `.cargo/config.toml` in both projects patches `asupersync` to a local path (`../asupersync`), confirming they share the same runtime version.

### Natural Integration Points

1. **HTTP Server for FrankenTerm's Web UI / MCP Server**
   - `fastapi-http::Server` + `fastapi-core::App` could serve FrankenTerm's web dashboard
   - `ServerConfig` provides connection management, timeouts, graceful shutdown
   - Already cancel-correct via asupersync integration
   - WebSocket support for real-time terminal streaming

2. **Middleware Pipeline for FrankenTerm's HTTP Endpoints**
   - `MiddlewareStack` with `before`/`after` hooks maps directly to FrankenTerm's middleware needs
   - Built-in: CORS, SecurityHeaders, RequestId, logging
   - FrankenTerm could add custom middleware for session management, terminal auth

3. **OpenAPI Documentation for FrankenTerm API**
   - `OpenApiBuilder` + `#[derive(JsonSchema)]` for automatic API documentation
   - FrankenTerm's MCP endpoints could be documented via OpenAPI

4. **Health Check Registry**
   - `HealthCheckRegistry` with liveness/readiness probes maps to FrankenTerm's monitoring needs
   - Check terminal multiplexer connectivity, storage health, etc.

5. **Agent Detection for Terminal Output**
   - `fastapi-output::detection` already detects Claude Code, Codex, Cursor, etc.
   - FrankenTerm could use this for output mode selection in its CLI
   - Overlaps with FrankenTerm's own agent-awareness goals

6. **TestClient for FrankenTerm's HTTP Testing**
   - `TestClient` provides in-process HTTP testing without network
   - Cookie jar, request builder, assertion macros
   - Would simplify testing FrankenTerm's HTTP endpoints

### Extraction Candidates

1. **`fastapi-router` as standalone crate** (4,247 LOC)
   - Zero dependency on asupersync or HTTP specifics
   - Only depends on `fastapi-types` (68 LOC, just `Method` enum)
   - Trie-based routing with path params, converters, wildcards, conflict detection
   - Could serve FrankenTerm's command routing or URL matching needs
   - Extraction cost: low (already clean boundary)

2. **`fastapi-output` as standalone crate** (14,370 LOC)
   - Independent of all other fastapi crates
   - Agent detection, rich/plain output modes, themed formatting
   - Could replace or augment FrankenTerm's CLI output layer
   - Extraction cost: low (no internal dependencies)

3. **HTTP Parser (`fastapi-http::parser`)** (~2,400 LOC)
   - Zero-copy HTTP/1.1 parser
   - Depends only on `fastapi-core::Request` type
   - Could be used for FrankenTerm's HTTP proxy or MCP HTTP transport
   - Extraction cost: medium (depends on core Request type)

4. **WebSocket Implementation** (~2,400 LOC across core + http)
   - Custom RFC 6455 implementation without external deps
   - Could serve FrankenTerm's real-time terminal streaming
   - Extraction cost: medium (depends on asupersync I/O traits)

5. **Middleware Abstractions** (conceptual extraction)
   - `Middleware` trait, `ControlFlow`, `MiddlewareStack` pattern
   - Clean onion model with short-circuit support
   - FrankenTerm could adopt the same middleware pattern for its pipeline

6. **Password/Security Utilities**
   - `PasswordHasher`, `constant_time_eq`, Digest auth
   - Self-contained, no external crypto dependencies
   - Could secure FrankenTerm's remote access features

### API Boundaries Suitable for Cross-Project Use

| Boundary | Interface | Direction |
|----------|-----------|-----------|
| HTTP Server | `App` + `serve()` | FrankenTerm embeds fastapi server |
| Router | `Router::add()` + `Router::lookup()` | FrankenTerm uses for command dispatch |
| Middleware | `Middleware` trait + `MiddlewareStack::execute()` | FrankenTerm wraps handlers |
| Extractors | `FromRequest` trait | FrankenTerm adds custom extractors |
| OpenAPI | `OpenApiBuilder::build()` -> `OpenApi` | FrankenTerm generates API docs |
| Output | `RichOutput::auto()` + component methods | FrankenTerm formats terminal output |
| Agent Detection | `detect_environment()` -> `DetectionResult` | FrankenTerm checks agent mode |
| Health | `HealthCheckRegistry::check_all()` | FrankenTerm monitors subsystems |

---

## R8: Upstream Tweak Recommendations

### High Priority

1. **Make `fastapi-types` include `StatusCode`**
   Currently `StatusCode` lives in `fastapi-core::response`, but it is fundamental enough to belong in `fastapi-types`. Moving it would allow `fastapi-router` and `fastapi-http` to reference status codes without depending on the full core crate, reducing coupling for extraction scenarios.

2. **Decouple `fastapi-router` from `fastapi-types`**
   The router only needs `Method`, which is a simple 8-variant enum. Consider making `Method` a standalone type or accepting it as a generic parameter, so the router can be used without any fastapi dependency at all.

3. **macOS Route Auto-Discovery**
   The linker-section based route registration (`registry.rs`) returns an empty slice on macOS/Darwin. Since FrankenTerm targets macOS heavily, this should be addressed -- either via `__DATA` Mach-O section support or by providing a build-script-based fallback that collects routes at compile time.

4. **Feature-gate `asupersync` dependency in `fastapi-core`**
   For FrankenTerm integration, it would be useful to use `fastapi-core` types (Request, Response, extractors) without requiring the full asupersync runtime. Consider making the `Cx`-dependent features optional behind a feature flag, with a simpler `RequestContext` that doesn't require a runtime.

### Medium Priority

5. **Extract `fastapi-output::detection` into its own micro-crate**
   The agent environment detection logic (8 agent variables, 7 CI variables, TTY check, override modes) is independently useful for any CLI tool. Extracting it to a ~200-line crate would benefit the Rust ecosystem and simplify FrankenTerm's agent detection.

6. **Add `#[cfg(feature = "proptest")]` guard to `proptest.rs`**
   The `proptest` module in `fastapi-core` is always compiled but only useful for testing. Feature-gating it would reduce compile times for production builds.

7. **Publish workspace crates to crates.io**
   Currently `asupersync` and `rich_rust` are sourced via Git/local path patches. Publishing them (even as pre-release) would simplify dependency management for downstream consumers like FrankenTerm.

8. **Add `Display` for `Response` and `Request`**
   Currently debugging requires manual field inspection. Adding `Display` implementations with summary output (method, path, status, body length) would improve developer experience.

### Low Priority

9. **Generalize `TestClient` beyond `Handler`**
   The `TestClient` currently requires a `Handler` implementation. Adding support for raw request/response testing (bypassing the middleware stack) would make it more versatile for integration testing scenarios.

10. **Document the shutdown budget subdivision**
    The `subdivide_grace_budget()` function in `shutdown.rs` is exported but its semantics around how grace period budget is split between drain timeout and hook execution are not well-documented. This matters for FrankenTerm's graceful shutdown integration.

11. **Make `ServerMetrics` observable**
    `ServerMetrics` in `server.rs` tracks connection counts and request counts via atomics. Exposing these as a structured snapshot (rather than individual atomic reads) would integrate with FrankenTerm's telemetry infrastructure.

12. **Add `#[must_use]` to `Response` builder methods**
    Several `Response` builder methods (`header()`, `body()`, `cookie()`) return `Self` but don't have `#[must_use]`. Adding this would catch accidental value drops.

---

*Analysis complete.*
