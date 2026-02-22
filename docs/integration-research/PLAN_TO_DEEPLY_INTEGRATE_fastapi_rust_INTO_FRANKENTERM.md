# Plan to Deeply Integrate fastapi_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.25.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_fastapi_rust.md (ft-2vuw7.25.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **HTTP server layer**: Use fastapi_rust as FrankenTerm's HTTP server for web dashboards and API endpoints
2. **Shared asupersync runtime**: Leverage common structured concurrency foundation (no async runtime nesting)
3. **Domain extractors**: Implement `FromRequest` for FrankenTerm types (PaneInfo, SessionId, etc.)
4. **OpenAPI documentation**: Auto-generate API specs from FrankenTerm's HTTP endpoints

### Constraints

- **Shared asupersync 0.2**: Both projects use asupersync; versions must stay aligned
- **Feature-gated**: Behind `http-server` feature flag
- **No HTTP/2**: fastapi_rust is HTTP/1.1 only; sufficient for local/LAN use
- **Binary size**: fastapi_rust adds significant code; feature gate mitigates

### Non-Goals

- **Replacing existing web module**: FrankenTerm's web.rs may coexist or migrate gradually
- **Public-facing API server**: fastapi_rust serves local dashboards, not internet traffic
- **Full FastAPI compatibility**: Only use the Rust framework features, not Python API compatibility

---

## P2: Evaluate Integration Patterns

### Option A: Workspace Dependency (Chosen)

Add fastapi_rust as optional workspace dependency via git path.

**Pros**: Shared asupersync runtime, type-safe extractors, OpenAPI generation, zero-copy HTTP
**Cons**: Large dependency (111K LOC), asupersync version coupling
**Chosen**: Natural HTTP layer for FrankenTerm's web interfaces

### Option B: Standalone HTTP Server

Keep existing web.rs with manual HTTP handling.

**Pros**: No new dependency, full control
**Cons**: Reinventing middleware, routing, extractors
**Rejected**: fastapi_rust provides these well

### Decision: Option A — Workspace dependency

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── http_bridge.rs       # NEW: fastapi_rust integration layer
```

### Module Responsibilities

#### `http_bridge.rs`

- `FrankenTermApp` — fastapi_rust App configured with FrankenTerm routes
- `FromRequest` impls for PaneInfo, SessionId, RecorderHandle
- Middleware: RequestId, logging, auth
- Route registration for dashboard, API, MCP endpoints

---

## P4: API Contract

```rust
#[cfg(feature = "http-server")]
pub mod http_bridge {
    pub struct FrankenTermApp {
        app: fastapi::App,
    }

    impl FrankenTermApp {
        pub fn new(state: AppState) -> Self;
        pub async fn serve(self, addr: &str) -> Result<()>;
    }

    pub struct AppState {
        pub recorder: RecorderHandle,
        pub pane_map: PaneMap,
        pub config: Config,
    }
}
```

---

## P5-P8: Testing, Rollout

**No migration** — new capability alongside existing web module.

**Tests**: 30+ tests using fastapi_rust's TestClient for in-process HTTP testing.

**Rollout**: Phase 1 (http_bridge.rs with basic routes) → Phase 2 (domain extractors) → Phase 3 (OpenAPI generation) → Phase 4 (migrate existing web.rs routes).

**Rollback**: Disable `http-server` feature; existing web.rs unaffected.

### Summary

**Workspace dependency** on fastapi_rust for HTTP server layer. One new module: `http_bridge.rs`. Shared asupersync runtime. Feature-gated behind `http-server`.

---

*Plan complete.*
