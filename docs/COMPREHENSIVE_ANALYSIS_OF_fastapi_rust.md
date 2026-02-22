# Comprehensive Analysis of fastapi_rust

> Analysis document for FrankenTerm bead `ft-2vuw7.25.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**fastapi_rust** is a 111K LOC, 8-crate Rust web framework (edition 2024, MSRV 1.85) with zero-copy HTTP/1.1 parsing, compile-time route validation via proc macros, 25+ extractors, middleware system, dependency injection, and OpenAPI 3.1 generation. Built on **asupersync** (not tokio) for structured concurrency with cancel-correct semantics.

**Integration Value**: High — shares asupersync runtime with FrankenTerm; provides the HTTP server layer for FrankenTerm's web interfaces and MCP endpoints.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~111,000 |
| **Crate Count** | 8 |
| **Rust Edition** | 2024 |
| **MSRV** | 1.85 |
| **License** | MIT + OpenAI/Anthropic Rider |
| **Unsafe Code** | `#![forbid(unsafe_code)]` (core), `#![deny(unsafe_code)]` (http) |

### Workspace Structure

```
fastapi_rust/
├── crates/
│   ├── fastapi/           # Facade (re-exports)
│   ├── fastapi-core/      # Framework core (50K+ LOC)
│   ├── fastapi-http/      # Zero-copy HTTP parser & server
│   ├── fastapi-router/    # Radix trie router
│   ├── fastapi-macros/    # Proc macros (#[get], #[post], etc.)
│   ├── fastapi-openapi/   # OpenAPI 3.1 generation
│   ├── fastapi-types/     # Shared types (zero deps)
│   └── fastapi-output/    # Terminal output (agent-aware)
```

---

## Core Architecture

### Request Pipeline

```
TCP Connection
  → [fastapi-http] Parse HTTP/1.1 (zero-copy)
  → [fastapi-router] Match path (radix trie, O(k))
  → [fastapi-core] Create RequestContext (wraps asupersync::Cx)
  → [Middleware.before] Pre-processing (registration order)
  → [Extractors] Json<T>, Path<T>, Query<T>, Header<T>, Auth, etc.
  → [DI] Resolve Depends<T> (circular detection, request-scoped)
  → [Handler] async fn with Cx reference
  → [Middleware.after] Post-processing (reverse order)
  → [IntoResponse] Convert → HTTP response
  → [BackgroundTasks] Execute after response sent
```

### Key Features

- **25+ extractors**: Json, Path, Query, Header, State, BearerToken, BasicAuth, Cookie, Pagination
- **Middleware**: CORS, RequestId, Logger, SecurityHeaders, RateLimiter
- **Dependency injection**: Type-based Depends<T>, circular detection, cleanup stack
- **OpenAPI 3.1**: Automatic spec generation from route definitions
- **Validation**: `#[derive(Validate)]` with email, length, range, regex rules
- **Testing**: In-process TestClient with assertion macros

### Shared Runtime (asupersync)

Both FrankenTerm and fastapi_rust use asupersync 0.2:
- `Cx` capability token for structured concurrency
- `Budget` with deadline + quota enforcement
- Region-scoped cleanup (no orphaned tasks)
- `checkpoint()` for cooperative cancellation

---

## FrankenTerm Integration Assessment

### Compatible Interfaces

| Aspect | FrankenTerm | fastapi_rust | Compatibility |
|--------|------------|-------------|---------------|
| Async runtime | asupersync 0.2 | asupersync 0.2 | Shared (no nesting) |
| Context | `Cx` | `RequestContext` wraps `Cx` | Direct access |
| Cancellation | Budget, checkpoint() | Same | Identical |
| Errors | Error::Runtime(String) | HttpError | Manual mapping |
| Config | TOML/YAML | Fluent App builder | Both support env vars |

### Integration Pattern

FrankenTerm can use fastapi_rust as its HTTP server layer:
1. Add as workspace dependency (via git path)
2. Implement `FromRequest` for FrankenTerm domain types (PaneInfo, ProcessStatus)
3. Compose middleware (RequestId → FrankenTerm logging → security)
4. Use shared asupersync runtime (no async runtime nesting)
5. Leverage TestClient for integration tests

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| asupersync version drift | Medium | High | Pin workspace dependency version |
| Large dependency (111K LOC) | Low | Medium | Feature-gate server components |
| HTTP/2 not supported | Low | Low | HTTP/1.1 sufficient for local use |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | 8-crate web framework with zero-copy HTTP |
| **Key Innovation** | Compile-time routes + asupersync structured concurrency |
| **FrankenTerm Status** | No direct integration yet |
| **Integration Priority** | High — natural HTTP layer for web interfaces |
| **New Modules Needed** | 1-2 (extractors + middleware bridge) |
| **Shared Foundation** | asupersync runtime (identical version) |

---

*Analysis complete.*
