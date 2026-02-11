# Zellij WASM Plugin System (wa-1pygr)

Upstream: `https://github.com/zellij-org/zellij`  
Local clone: `legacy_zellij/` (gitignored; used for this analysis)  
Commit: `97744ad01270a5e0cd198d65e46fe90bcce304e7`  
Workspace version: `0.44.0` (`legacy_zellij/Cargo.toml`)

## Executive summary

Zellij’s plugin system is a WASI-first, sandboxed extension model:

- Plugins compile to `wasm32-wasip1` and run inside the server.
- Runtime: **wasmi** + **wasmi_wasi** (interpreter + WASI host).
  - Notably, Zellij migrated between runtimes (wasmer → wasmtime → wasmi) over time; current code uses `wasmi_wasi`.
- The host/guest ABI is deliberately narrow: one host export (`host_run_plugin_command`) plus a protobuf command/event protocol over WASI pipes.
- Capabilities are enforced via an explicit **permission model** (`PermissionType`) stored in a cache file and checked per command.

This is a strong reference design for FrankenTerm if we want “safe, language-agnostic, hot-loadable” extensions without giving plugins raw OS access.

## Plugin author API surface

The plugin author experience lives in `zellij-tile`:

- `legacy_zellij/zellij-tile/src/lib.rs`:
  - `ZellijPlugin` trait: `load`, `update(Event) -> bool`, `render(rows, cols)`
  - `register_plugin!` macro: generates the required exported functions and handles protobuf decode of config + events.
  - `ZellijWorker` + `register_worker!` for background tasks.
- `legacy_zellij/zellij-tile/src/shim.rs`:
  - “Commands” API: functions like `open_file`, `open_terminal`, `request_permission`, etc.
  - Implementation pattern: serialize `PluginCommand` → `ProtobufPluginCommand` → bytes, write to stdout pipe, call host, then read protobuf response from stdin pipe.

Conceptually:

1. Plugins **subscribe** to `EventType`s.
2. Host delivers an `Event` (protobuf) and calls the plugin’s exported `update()`.
3. Plugin may issue commands back to the host through the shim (`host_run_plugin_command()`).
4. If `update()` returns `true`, host calls `render()` and the plugin prints UI content.

## Host ↔ guest boundary (ABI + data flow)

### Single host export: `host_run_plugin_command`

On the server side, Zellij exposes exactly one host function:

- `legacy_zellij/zellij-server/src/plugins/zellij_exports.rs`:
  - `zellij_exports(linker)` registers `("zellij", "host_run_plugin_command")`
  - `host_run_plugin_command()`:
    1. reads bytes from the plugin’s stdout WASI pipe
    2. decodes a `ProtobufPluginCommand` → `PluginCommand`
    3. checks permissions (`check_command_permission`)
    4. executes the command by routing into the server (`route_action`, `ScreenInstruction`, `PtyInstruction`, etc.)
    5. writes a protobuf response back to the plugin via stdin WASI pipe

This “single dispatcher export” is a good design: the ABI stays stable even as the command surface evolves.

### Command protocol: `PluginCommand`

The full command surface is expressed as a Rust enum:

- `legacy_zellij/zellij-utils/src/data.rs`: `pub enum PluginCommand { ... }`

It includes:

- UI/session control (tabs, panes, focus, layouts)
- opening terminals / command panes / files
- sending input (write bytes/chars to panes)
- reading data (pane scrollback, focused pane info, layout dumps)
- running host commands + web requests (mediated by permissions)
- plugin orchestration (start/reload/load new plugin, message other plugins)

### Event protocol + subscriptions

Plugins subscribe to events (eg. mode changes, pane updates) via:

- `PluginCommand::Subscribe(HashSet<EventType>)`
- `PluginCommand::Unsubscribe(...)`

Events are delivered as protobuf and decoded by `register_plugin!` macro generated glue:

- `legacy_zellij/zellij-tile/src/lib.rs`: exported `update()` reads a `ProtobufEvent` from stdin and converts to `Event`.

## WASM runtime integration (wasmi + WASI)

Loading and instantiating happens via `PluginLoader`:

- `legacy_zellij/zellij-server/src/plugins/plugin_loader.rs`
  - Uses `wasmi::{Engine, Module, Store, Instance, Linker}`
  - Adds WASI via `wasmi_wasi::add_to_linker(...)`
  - Adds Zellij host export(s) via `zellij_exports(&mut linker)`

### Sandboxed filesystem

The WASI context preopens a small set of directories:

- `legacy_zellij/zellij-server/src/plugins/plugin_loader.rs`: `create_wasi_ctx(...)` mounts:
  - `/host` → plugin cwd (not the full filesystem)
  - `/data` → plugin-owned data dir
  - `/cache` → plugin-owned cache dir
  - `/tmp` → shared Zellij temp dir

Plugins do not get raw network access via WASI; “web access” happens only through `PluginCommand::WebRequest(...)` and host mediation.

### Resource limits

Zellij sets store limits for memory safety:

- `create_optimized_store_limits()` caps instances/memories and sets a 16MiB-per-memory limit.

## Permission / capability model

Zellij treats third-party plugins as untrusted and gates powerful operations.

### Permission types

Permissions are enumerated as `PermissionType`:

- `legacy_zellij/zellij-utils/src/data.rs`: `PermissionType::{ OpenFiles, RunCommands, WebAccess, ... }`

### Enforcement

- `legacy_zellij/zellij-server/src/plugins/zellij_exports.rs`: `check_command_permission(env, command)`
  - Built-in plugins (`plugin.is_builtin()`) bypass all checks.
  - Third-party plugins map each `PluginCommand` to a required `PermissionType`.
  - If the permission isn’t in `env.permissions`, the command is denied and the plugin can request permission.

### Persistence

Granted permissions are cached on disk:

- `legacy_zellij/zellij-utils/src/input/permission.rs`: `PermissionCache`
- `legacy_zellij/zellij-utils/src/consts.rs`: `ZELLIJ_PLUGIN_PERMISSIONS_CACHE` path

## Plugin lifecycle + hot reload

Key lifecycle operations are modeled as commands:

- `PluginCommand::StartOrReloadPlugin(url)`
- `PluginCommand::ReloadPlugin(plugin_id)`
- `PluginCommand::LoadNewPlugin { url, config, load_in_background, skip_plugin_cache }`

On the host side, plugin loading is transactional via a shared `PluginMap` held during `PluginLoader` lifetime, reducing races when multiple clients load plugins concurrently.

Crash handling is explicit:

- `legacy_zellij/zellij-server/src/plugins/zellij_exports.rs`: `report_panic(...)` → `handle_plugin_crash(...)`

## Performance considerations

The hot path is “event → update() → (optional) render()” with occasional command calls back to host.

Notable design choices:

- Using `wasmi` avoids JIT startup/compile overhead at runtime, at the cost of slower execution for compute-heavy plugins.
- The host ABI is synchronous per command call (`host_run_plugin_command`), which simplifies correctness but can amplify latency if plugins overuse host RPCs.

For FrankenTerm, this suggests a split:

- keep “render + lightweight state reads” in the plugin hot path,
- push heavy/slow tasks into a worker model (Zellij’s `ZellijWorker`) or host-managed background jobs.

## FrankenTerm implications + go/no-go

### What we can steal directly

1. **Single host command dispatcher export**
   - Stable ABI; evolve semantics via a schema’d command enum + protobuf.

2. **Capability-mediated effects**
   - Map plugin actions onto FrankenTerm policy primitives (read-only state, workflow execution, pane mutation, filesystem, network).

3. **WASI sandbox + preopened dirs**
   - Provide per-plugin `/data` + `/cache` and keep host filesystem access behind explicit capabilities.

4. **Worker model for long-running tasks**
   - Keep UI plugins responsive while computations happen off-thread/off-VM.

### Gaps / changes needed for FrankenTerm

- **Env leakage:** Zellij’s WASI ctx inherits env (`inherit_env()`); FrankenTerm should likely default to an allowlist.
- **Robot + pane-heavy workload:** FrankenTerm’s high-throughput agent swarm scenarios need careful backpressure + budgeting; plugin RPCs must be rate-limited.

### Recommendation

**Go**, but only as a **scoped extension layer**:

- Start with read-only plugins (state inspection, UI dashboards, alerts).
- Require explicit capabilities for any mutation (send input, run commands, kill panes).
- Keep workflow execution in the host, with plugins only triggering *requests* that the policy engine evaluates and audits.

