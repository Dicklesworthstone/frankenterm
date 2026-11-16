# Zellij Architectural Inventory (wa-okyhm)

Upstream: `https://github.com/zellij-org/zellij`  
Local clone: `legacy_zellij/` (gitignored)  
Commit: `97744ad01270a5e0cd198d65e46fe90bcce304e7`  
Workspace version: `0.44.0` (`legacy_zellij/Cargo.toml`)

## Goal

Extract **actionable** architecture patterns from Zellij (Rust terminal multiplexer) that are directly relevant to FrankenTerm’s mux server layer: session management, pane/tab topology, IPC and multi-client attach, plugin extensibility, and safe orchestration.

## Repo / crate map (what is where)

- `legacy_zellij/src/main.rs`: main `zellij` CLI binary; parses args, then mostly forwards to session commands or starts client/server.
- `legacy_zellij/src/commands.rs`: CLI actions; session lifecycle (`list`, `kill`, `delete`, resurrect), start server/client; bridges into the client/server crates.
- `legacy_zellij/zellij-server/`: the mux server implementation.
- `legacy_zellij/zellij-client/`: client-side terminal input parsing, remote attach, etc.
- `legacy_zellij/zellij-utils/`: shared *contracts* and utilities:
  - IPC message types (`ipc`)
  - layout/config parsing (`input`)
  - session discovery + resurrection helpers (`sessions.rs`)
  - session layout serialization (`session_serialization.rs`)
- `legacy_zellij/zellij-tile/` + `legacy_zellij/zellij-tile-utils/`: plugin API + helpers (WASM plugins).
- `legacy_zellij/default-plugins/*`: bundled WASM plugins.

Zellij’s **key organizational decision**: push shared message schemas and layout/session modeling into a dedicated `zellij-utils` crate, so both client and server speak the same “language”.

## Server architecture (zellij-server): thread-per-subsystem + message bus

High-level: the server is a set of long-running threads (PTY, screen/state, plugins, routing, etc.) that communicate exclusively via typed instructions.

### Typed instructions

Each major subsystem has an instruction enum:

- `legacy_zellij/zellij-server/src/lib.rs`: `ServerInstruction`
- `legacy_zellij/zellij-server/src/screen.rs`: `ScreenInstruction`
- `legacy_zellij/zellij-server/src/pty.rs`: `PtyInstruction`
- `legacy_zellij/zellij-server/src/plugins/mod.rs`: `PluginInstruction`
- `legacy_zellij/zellij-server/src/background_jobs.rs`: `BackgroundJob`
- `legacy_zellij/zellij-server/src/pty_writer.rs`: `PtyWriteInstruction`

### Message bus: `Bus<T>` + `ThreadSenders`

`legacy_zellij/zellij-server/src/thread_bus.rs` defines:

- `ThreadSenders`: optional senders to each subsystem.
- `Bus<T>`: receivers + `ThreadSenders` + an optional OS API handle.

Notable pattern: send operations use a `SenderWithContext<T>` wrapper (`zellij_utils::channels`) so errors can be reported with an attached “cause-chain” context. This helps make cross-thread failures debuggable without global state.

### Major server threads and responsibilities (as used by zellij-server)

From the wiring in `legacy_zellij/zellij-server/src/lib.rs` and the instruction enums:

- **Route thread** (`legacy_zellij/zellij-server/src/route.rs`):
  - Converts high-level `Action` into subsystem instructions.
  - Orchestrates multi-step operations that span threads.
  - Implements the “logical completion” pattern (below) to avoid races.
- **Screen thread** (`legacy_zellij/zellij-server/src/screen.rs`):
  - Owns the canonical model: tabs, panes, layout, focus, render pipeline.
  - Exposes a large instruction surface (`ScreenInstruction`) for routing.
- **PTY thread** (`legacy_zellij/zellij-server/src/pty.rs`):
  - Spawns commands and reads PTY output.
  - Sends bytes to screen/state.
- **PTY writer thread** (`legacy_zellij/zellij-server/src/pty_writer.rs`):
  - Centralized serialized writes to PTYs.
- **Plugin thread** (`legacy_zellij/zellij-server/src/plugins/mod.rs`):
  - Hosts WASM plugins, permissions/capabilities, event fanout.
  - Can dump layouts / metadata via instructions.
- **Background jobs thread** (`legacy_zellij/zellij-server/src/background_jobs.rs`):
  - Offloads slower operations that shouldn’t block the hot path.

## “Logical completion” pattern (important steal)

Zellij implements a **cross-thread action completion protocol** in `legacy_zellij/zellij-server/src/route.rs`:

- When routing an action, the route thread creates a `tokio::sync::oneshot` channel.
- It wraps the sender in a RAII struct `NotificationEnd`.
- That struct is carried through instructions to other threads.
- When the “logical end” of the action is reached, the `NotificationEnd` is dropped and its `Drop` impl sends the completion payload.
- The route thread blocks waiting for completion, with a timeout.

Why it matters: it prevents concurrent “action overlap” that creates subtle races (eg. create pane → focus pane → write bytes), while still allowing actions to be processed across threads.

## Stable IDs vs positions (important steal)

`legacy_zellij/zellij-server/src/screen.rs` has explicit documentation for **stable tab IDs** vs **display positions**:

- Stable identifiers never change; used for internal tracking and maps.
- Positions change as tabs move/close; used for user-facing operations.

This explicit distinction is a good prophylactic against restore/topology bugs.

## Session discovery, death, and resurrection (zellij-utils)

Zellij session lifecycle is handled in `legacy_zellij/zellij-utils/src/sessions.rs`:

- Active sessions are Unix sockets under `ZELLIJ_SOCK_DIR`.
- “Dead but resurrectable” sessions are folders under `ZELLIJ_SESSION_INFO_CACHE_DIR` containing a cached layout file.
- `resurrection_layout(session_name)` returns a parsed layout if present.

### Layout serialization format: KDL + pane-contents sidecar map

`legacy_zellij/zellij-utils/src/session_serialization.rs` builds a `GlobalLayoutManifest`:

- global cwd
- default shell
- tabs, each with tiled panes + floating panes
- per-pane metadata: geometry, cwd, focus, borderless, title, and (optionally) initial contents

It serializes to:

- a single KDL layout document string, and
- a `BTreeMap<String, String>` containing file names → pane contents

There are snapshot tests under `legacy_zellij/zellij-utils/src/snapshots/` ensuring serialization stability.

## Plugin system (WASM-first) + capability/permission surface

`legacy_zellij/zellij-server/src/plugins/mod.rs` defines an expansive `PluginInstruction` API, plus:

- event subscription and cached-event delivery
- permission requests (`PermissionType`, `PermissionStatus`, `PluginCapabilities`)
- filesystem watch + plugin reload
- “dump layout” support (server can export session layout/metadata)

This is relevant to FrankenTerm’s policy/workflow engine: Zellij’s plugin host treats plugins like *untrusted code with declared capabilities*.

## Concrete recommendations for FrankenTerm (mux-server focused)

1. **Adopt “logical completion tokens” for multi-step robot actions**
   - Model after `NotificationEnd` in `legacy_zellij/zellij-server/src/route.rs`.
   - Use it where FrankenTerm actions span multiple subsystems (eg. mux queries + storage writes + workflow triggers).

2. **Carry cause-chain context through internal eventing**
   - Zellij’s `SenderWithContext<T>` pattern (in `legacy_zellij/zellij-server/src/thread_bus.rs`) is a strong template.
   - Map idea to FrankenTerm’s event bus + workflow engine so logs and audit entries can show a full causal chain.

3. **Make “stable ID vs position” explicit in topology + restore**
   - Zellij’s tab id vs position documentation is a concrete, low-cost design improvement.
   - Apply to FrankenTerm’s session topology (`crates/frankenterm-core/src/session_topology.rs`) + restore (`crates/frankenterm-core/src/restore_layout.rs`) to reduce ambiguity.

4. **Snapshot-test serialization for session persistence**
   - Zellij uses snapshot tests for KDL output.
   - FrankenTerm’s SQLite-backed checkpoints should similarly test stable roundtrip + compatibility across versions (topology + pane state).

5. **Manifest + payload split for checkpoints**
   - Zellij’s KDL + pane-contents sidecar map suggests a general pattern:
     - store a manifest that’s small and stable, plus separate (optional) large blobs.
   - This aligns with FrankenTerm’s needs (topology + per-pane state + optional scrollback) and helps retention/GC logic.

6. **Capability surface for “extensibility code”**
   - Zellij’s plugin permission model maps naturally to FrankenTerm’s policy engine.
   - Recommendation: require declared capabilities for workflow actions that can mutate panes / run commands, and log the capability decision along with the triggering event.

## Quick pointers (for deeper dives)

- Server bus + senders: `legacy_zellij/zellij-server/src/thread_bus.rs`
- Route orchestration + completion: `legacy_zellij/zellij-server/src/route.rs`
- Screen/tab/pane state machine: `legacy_zellij/zellij-server/src/screen.rs`, `legacy_zellij/zellij-server/src/tab/mod.rs`, `legacy_zellij/zellij-server/src/panes/*`
- Session resurrection: `legacy_zellij/zellij-utils/src/sessions.rs`
- Session layout serialization: `legacy_zellij/zellij-utils/src/session_serialization.rs`
- Plugin host surface: `legacy_zellij/zellij-server/src/plugins/mod.rs`

