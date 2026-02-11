# Zellij Session Management (wa-2apg5)

Upstream: `https://github.com/zellij-org/zellij`  
Local clone: `legacy_zellij/` (gitignored; used for this analysis)  
Commit: `97744ad01270a5e0cd198d65e46fe90bcce304e7`  
Workspace version: `0.44.0` (`legacy_zellij/Cargo.toml`)

## Executive summary

Zellij’s “session persistence” has two layers:

1. **Detach / reattach to a still-running server** (tmux-like): the server stays alive; clients come and go.
2. **Resurrection after server exit/crash**: Zellij periodically snapshots a *layout + per-pane metadata* to disk. If the server later exits, the session becomes “dead but resurrectable”; attaching to it spawns a new server that loads the last snapshot.

The important design pattern is a **split between “live session metadata” vs “resurrection snapshot artifacts”** stored under a per-session folder in a cache directory.

## Session lifecycle (create, list, attach, detach, kill, delete)

### Identity + discovery

- A local session is identified by **a Unix-domain socket filename** under `ZELLIJ_SOCK_DIR`:
  - `legacy_zellij/zellij-utils/src/sessions.rs`: `get_sessions()`, `assert_socket()`
  - `legacy_zellij/zellij-utils/src/consts.rs`: `ZELLIJ_SOCK_DIR`
- Listing sessions:
  - `get_sessions()` reads `ZELLIJ_SOCK_DIR`, filters sockets, and probes them via IPC.
  - If a socket connect returns `ConnectionRefused`, it deletes the socket file (stale session cleanup).

### Attach / detach

- Attaching to an existing live session is just “connect to the socket and register as a client”.
- Detach is effectively the client disconnecting; the server continues to run PTYs and manage state.
- Multi-client is first-class; more below.

### Kill vs delete

- **Kill**: send `ClientToServerMsg::KillSession` over the socket.
  - `legacy_zellij/zellij-utils/src/sessions.rs`: `kill_session()`
- **Delete**: remove the on-disk resurrection artifacts (and optionally force-kill if still running).
  - `legacy_zellij/zellij-utils/src/sessions.rs`: `delete_session(name, force)`
  - The deletion target is the per-session cache folder from `session_info_folder_for_session(name)`.

## What state is persisted, where it lives, and how it’s updated

### On-disk layout + session-info folders

Zellij stores session artifacts in:

- `ZELLIJ_SESSION_INFO_CACHE_DIR = ZELLIJ_CACHE_DIR/contract_version_{N}/session_info/`
  - `legacy_zellij/zellij-utils/src/consts.rs`
- Each session gets a folder: `ZELLIJ_SESSION_INFO_CACHE_DIR/<session_name>/`
  - `session_info_cache_file_name(session) -> session-metadata.kdl`
  - `session_layout_cache_file_name(session) -> session-layout.kdl`

### “Live session metadata” (`session-metadata.kdl`)

- Written continuously (roughly every second) by the background-jobs thread:
  - `legacy_zellij/zellij-server/src/background_jobs.rs`: `ReadAllSessionInfosOnMachine` loop
  - `write_session_state_to_disk(...)` writes:
    - `session-metadata.kdl` from `SessionInfo`
    - and (if present) the current `session-layout.kdl` plus optional sidecar files
- On clean exit, Zellij removes the `session-metadata.kdl` file:
  - `legacy_zellij/zellij-server/src/background_jobs.rs`: `BackgroundJob::Exit`

This “metadata file present” acts like a **live marker** for a running session (and feeds the session-manager UI).

### “Resurrection snapshot” (`session-layout.kdl` + optional sidecars)

- Periodically written based on `serialization_interval` (default 60s):
  - `legacy_zellij/zellij-server/src/background_jobs.rs`: triggers `ScreenInstruction::SerializeLayoutForResurrection`
  - `legacy_zellij/zellij-server/src/screen.rs`: `SerializeLayoutForResurrection` → `dump_layout_to_hd()` (gated by `session_serialization`)
- The KDL snapshot is produced via a manifest + serializer:
  - `legacy_zellij/zellij-server/src/session_layout_metadata.rs`: `SessionLayoutMetadata` captures tabs/panes/geometry + focused clients + (optionally) serialized viewport text
  - `legacy_zellij/zellij-server/src/pty.rs`: `populate_session_layout_metadata()` fills per-pane **cwd** and **command line** by inspecting OS processes (`get_all_cmds_by_ppid`, `get_cwds`)
  - `legacy_zellij/zellij-utils/src/session_serialization.rs`: `serialize_session_layout(GlobalLayoutManifest) -> (kdl_string, BTreeMap<file_name, contents>)`

#### Optional viewport + scrollback serialization

- If `serialize_pane_viewport` is enabled, Zellij serializes pane contents:
  - `legacy_zellij/zellij-server/src/screen.rs`: `get_layout_metadata()` calls `Pane::serialize(scrollback_lines_to_serialize)`
  - `legacy_zellij/zellij-server/src/panes/grid.rs`: `Grid::serialize(...)`
- Those contents get written as **sidecar files** alongside `session-layout.kdl`:
  - `legacy_zellij/zellij-server/src/background_jobs.rs`: `write_session_state_to_disk()` writes the `BTreeMap<filename, contents>` into the session folder

### Config knobs

The behavior is controlled by config/options:

- `session_serialization` (default true): gate resurrection snapshots.
- `serialization_interval`: seconds between snapshots (default 60s).
- `serialize_pane_viewport` (default false) + `scrollback_lines_to_serialize`: include pane viewport and optional scrollback lines.
- `disable_session_metadata`: disable writing `session-metadata.kdl`.
- `post_command_discovery_hook`: post-process discovered resurrect commands.
  - `legacy_zellij/zellij-utils/src/input/options.rs`
  - `legacy_zellij/example/default.kdl` (documented)

## Resurrection (what survives; how it works)

### “Dead but resurrectable” detection

A session is considered resurrectable when:

- It **does not have a live socket** in `ZELLIJ_SOCK_DIR`, but
- It **does have a `session-layout.kdl`** in `ZELLIJ_SESSION_INFO_CACHE_DIR/<name>/`.

Code paths:

- Listing dead sessions:
  - `legacy_zellij/zellij-utils/src/sessions.rs`: `get_resurrectable_sessions()`
  - `legacy_zellij/zellij-server/src/background_jobs.rs`: `find_resurrectable_sessions()`
- Loading the resurrection layout:
  - `legacy_zellij/zellij-utils/src/sessions.rs`: `resurrection_layout(session_name)`

On a clean shutdown, metadata is removed but layout remains, so the session becomes resurrectable.
On an unclean crash, stale sockets are removed on next listing/probe, and the last snapshot remains.

### Resurrect-by-attach workflow

When the user runs `zellij attach <name>`:

- If the session is dead and has a resurrection layout, the client constructs `ClientInfo::Resurrect`.
  - `legacy_zellij/src/commands.rs`: attach logic
  - `legacy_zellij/zellij-client/src/lib.rs`: `ClientInfo::Resurrect(name, path_to_layout, force_run_commands, cwd)`
- The client **spawns a new server process** and sends `FirstClientConnected` with a `CliAssets` payload that points at the layout file:
  - `legacy_zellij/zellij-client/src/lib.rs`: resurrection path spawns server
  - `legacy_zellij/zellij-utils/src/input/cli_assets.rs`: `force_run_layout_commands` can flip `start_suspended` → false

### What state survives resurrection?

- **Topology**: tabs, tiled splits, floating panes, geometry, focus markers.
- **Pane metadata**: per-pane cwd; and best-effort “what command was running here”.
- **Optional**: viewport + scrollback text (if enabled).

### What does *not* survive?

- **Running processes** (their memory/state) are not checkpointed; resurrection can only re-run commands.
- **In-application state** (vim buffers, REPL history, etc.) is lost unless the application itself persists it.
- Without `serialize_pane_viewport`, the resurrected session has no prior output context.

## Multi-client attachment semantics

Zellij is built for multiple simultaneously attached clients:

- Per-client active tab tracking: `active_tab_ids: BTreeMap<ClientId, usize>`
  - `legacy_zellij/zellij-server/src/screen.rs`
- The server can optionally mirror the session so clients share the same view:
  - `mirror_session` option (see `session_is_mirrored` in `Screen`)
- “Watcher clients” exist (read-only mode):
  - Client message: `AttachWatcherClient`
  - `legacy_zellij/zellij-client/src/lib.rs` (`ClientInfo::Watch`)
  - `legacy_zellij/zellij-server/src/lib.rs` / `legacy_zellij/zellij-server/src/route.rs`

Conflict model is pragmatic: if multiple clients send input to the same focused pane, input interleaves; focus/view differences are tracked per client unless mirroring is enabled.

## FrankenTerm implications (concrete “steal this”)

1. **Split “live metadata” vs “resurrection snapshot”**
   - Zellij’s `session-metadata.kdl` (live marker) vs `session-layout.kdl` (resurrection artifact) is a clean separation.
   - For FrankenTerm (wa-rsaf), model this as:
     - a fast-updating “session index” row set (for UX + discovery), and
     - a slower, durable “checkpoint” snapshot (for resurrection).

2. **Make “dead but resurrectable” an explicit state**
   - Zellij treats “no socket, but snapshot exists” as a first-class state (listed as `EXITED - attach to resurrect`).
   - FrankenTerm should surface this in `ft robot state` / UI to enable deterministic recovery flows.

3. **Snapshot as manifest + optional blob sidecars**
   - Zellij’s layout KDL plus sidecar pane-content files is a good pattern for mixing stable metadata with optional large payloads.
   - FrankenTerm can do the same in SQLite: store a compact manifest row + optional compressed scrollback blobs, to keep hot paths lean.

4. **Best-effort “resurrect command discovery” + post-processing hook**
   - Zellij inspects OS process trees to infer the current command per PTY (with a hook to sanitize wrappers).
   - FrankenTerm could optionally record:
     - `argv0/args`, cwd, env hints for each pane,
     - a “run suspended” default, and a “force run commands” mode for automation workflows.

5. **Multi-client model: per-client view state + watcher clients**
   - Zellij tracks per-client active tab/pane, and supports watcher clients for read-only observation.
   - This maps directly to FrankenTerm’s “agent swarm” scenario: real clients vs observer/robot clients, with explicit read-only semantics.

