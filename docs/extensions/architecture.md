# Extension Architecture

## Overview

FrankenTerm runs a dual-engine scripting runtime: Lua 5.4 for backward
compatibility with WezTerm configs, and WASM (via Wasmtime) for new
extensions. Both engines share a common event bus, keybinding registry,
and per-extension storage system.

## Component diagram

```
                    +-----------------------+
                    |  ScriptingDispatcher  |
                    |  (coordinates both    |
                    |   engines)            |
                    +-----------+-----------+
                         |           |
              +----------+           +----------+
              |                                 |
     +--------v--------+            +-----------v---------+
     |   LuaEngine     |            |    WasmEngine       |
     |   (mlua/Lua5.4) |            |    (wasmtime)       |
     +--------+--------+            +-----------+---------+
              |                                 |
              +----------+           +----------+
                         |           |
                    +----v-----------v----+
                    |     Shared Layer     |
                    |  - EventBus         |
                    |  - KeybindingRegistry|
                    |  - ExtensionStorage  |
                    |  - AuditTrail       |
                    +---------------------+
```

## Dispatch tiers

When an event fires, handlers execute in tier order:

| Tier | Engine | Overhead | Use case |
|------|--------|----------|----------|
| Native | Rust (compiled in) | ~0 | Core behavior |
| Wasm | Wasmtime | ~100us | Extensions needing speed |
| Lua | mlua | ~1ms | Config callbacks, scripting |

Within each tier, handlers run in priority order (lower number = higher
priority). Default priority is 0.

## Event bus

The `EventBus` is the primary coordination mechanism. Extensions register
hooks for event patterns, and the bus dispatches events to all matching
hooks.

### Event patterns

- Exact match: `"config.reload"` matches only that event
- Prefix wildcard: `"pane.*"` matches `pane.focus`, `pane.created`, etc.
- Global wildcard: `"*"` matches all events

### Well-known events

| Event | Payload | Fired when |
|-------|---------|------------|
| `config.reload` | Config diff | Config file reloaded |
| `pane.focus` | `{ pane_id }` | Pane gains focus |
| `pane.created` | `{ pane_id }` | New pane created |
| `pane.closed` | `{ pane_id }` | Pane closed |
| `pane.output` | `{ pane_id, text }` | Data written to pane |
| `pane.title_changed` | `{ pane_id, title }` | Pane title updated |
| `tab.created` | `{ tab_id }` | New tab created |
| `tab.closed` | `{ tab_id }` | Tab closed |
| `window.focus` | `{ window_id }` | Window gains focus |
| `key.pressed` | `{ key, modifiers }` | Key press event |
| `session.save` | `{ path }` | Session being saved |
| `session.restore` | `{ path }` | Session being restored |

### Actions

Hook handlers return zero or more `Action` values:

```rust
enum Action {
    SetConfig { key: String, value: Value },
    SendInput { pane_id: Option<u64>, text: String },
    Log { level: LogLevel, message: String },
    Custom { name: String, payload: Value },
}
```

## Keybinding registry

Extensions can register custom key bindings. When a key combo is pressed,
FrankenTerm dispatches it through the registry and collects actions.

Key combos are parsed from strings like `"ctrl+shift+t"`. Supported
modifier aliases:

| Alias | Resolves to |
|-------|-------------|
| `cmd`, `command`, `meta` | `super` |
| `opt`, `option` | `alt` |

## Extension storage

Each extension gets isolated key-value storage on disk. Keys and values
are arbitrary bytes. Storage is:

- **Isolated**: Extension A cannot read extension B's data
- **Persistent**: Survives restarts and upgrades
- **Cached**: Hot keys served from memory, cold keys read from disk
- **Cleared on remove**: `remove` deletes all stored data

Storage location: `$DATA_DIR/frankenterm/extension-storage/<extension-id>/`

## Sandbox

WASM extensions run inside a Wasmtime sandbox with configurable limits:

| Resource | Default | Configurable |
|----------|---------|-------------|
| Linear memory | 64 MiB | Yes |
| Fuel per call | 1 billion | Yes |
| Wall time per call | 10 seconds | Yes |
| Filesystem read | None | Via permissions |
| Filesystem write | None | Via permissions |
| Network access | Denied | Via permissions |
| Env var access | None | Via permissions |
| Pane content access | Denied | Via permissions |

All host function calls are recorded in the `AuditTrail` for debugging
and security review.

## Module cache

Compiled WASM modules are cached in two layers:

1. **Memory** (LRU): Instant access for hot modules
2. **Disk** (SHA-256 keyed): Survives process restarts

Cache location: `$CACHE_DIR/frankenterm/wasm-cache/`

## Package format (.ftx)

A `.ftx` file is a ZIP archive containing:

- `extension.toml` (required) -- manifest
- Entry point file (`.wasm` or `.lua`)
- Optional asset files (themes, configs)

The package is validated on install:
- Manifest must declare an entry file that exists in the package
- No directory traversal paths (`..`)
- WASM engine requires `.wasm` file, Lua requires `.lua`
- Content hash (SHA-256) computed for integrity
