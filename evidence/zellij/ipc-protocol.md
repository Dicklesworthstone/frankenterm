# Zellij IPC Protocol (wa-vcjbi)

Upstream: `https://github.com/zellij-org/zellij`  
Local clone: `legacy_zellij/` (gitignored; used for this analysis)  
Commit: `97744ad01270a5e0cd198d65e46fe90bcce304e7`  
Workspace version: `0.44.0` (`legacy_zellij/Cargo.toml`)

## Executive summary

Zellij’s local client↔server protocol is intentionally simple and “high level”:

- **Transport:** Unix-domain stream sockets (`interprocess::local_socket::LocalSocketStream`), one connection per client.
- **Framing:** 4-byte **little-endian** length prefix, followed by a single protobuf message.
- **Serialization:** `prost` protobuf messages generated from `.proto` schemas.
- **Payload model:** clients send *actions / key events / resize / attach*; server sends *full-frame renders* (`RenderMsg { content: string }`) plus control/log messages.
- **Versioning:** sockets live under a `contract_version_{N}` directory keyed by `CLIENT_SERVER_CONTRACT_VERSION`, which prevents mismatched binaries from talking.

This is closer to a **remote UI framebuffer** (render strings) than to FrankenTerm’s mux protocol (granular PDUs for state/query).

## Transport + endpoint discovery

### Local sockets (primary path)

The socket directory is versioned:

- `legacy_zellij/zellij-utils/src/consts.rs`:
  - `CLIENT_SERVER_CONTRACT_VERSION`
  - `CLIENT_SERVER_CONTRACT_DIR = contract_version_{N}`
  - `ZELLIJ_SOCK_DIR = <runtime_dir_or_tmp>/contract_version_{N}`

Session name is the socket filename under `ZELLIJ_SOCK_DIR`:

- `legacy_zellij/zellij-utils/src/sessions.rs`: `get_sessions()`, `assert_socket()`

The client creates the directory and sets restrictive permissions:

- `legacy_zellij/zellij-client/src/lib.rs`: `create_ipc_pipe()` uses `set_permissions(..., 0o700)`

### Remote attach (separate protocol)

Zellij also supports “remote attach” via a local web server capability, using WebSockets for terminal I/O and JSON control messages.
This is distinct from the local protobuf IPC and isn’t the main focus of this bead.

## Wire format (framing + protobuf)

### Framing

`legacy_zellij/zellij-utils/src/ipc.rs` implements the wire format:

- Read 4 bytes → `u32::from_le_bytes()` → payload length
- Read exactly `len` bytes
- `prost::Message::decode(&buf)`

This keeps message boundaries explicit over a byte-stream socket.

### Protobuf schema + generated code

Schemas live in:

- `legacy_zellij/zellij-utils/src/client_server_contract/*.proto`
  - `client_to_server.proto`
  - `server_to_client.proto`
  - `common_types.proto`

Generated Rust types are included from a checked-in asset:

- `legacy_zellij/zellij-utils/src/client_server_contract/mod.rs` includes `assets/prost_ipc/generated_client_server_api.rs`

Rust-facing message enums (`ClientToServerMsg`, `ServerToClientMsg`) are converted to/from protobuf types via:

- `legacy_zellij/zellij-utils/src/ipc/protobuf_conversion.rs`

## Protocol evolution / versioning strategy

Zellij’s main “versioning mechanism” is **directory separation**:

- Sockets are stored under `.../contract_version_{N}/`
- Session resurrection cache is also under a contract-version directory (`ZELLIJ_SESSION_INFO_CACHE_DIR`)

Result: a client binary will only discover and attach to sessions created under the same contract version directory, strongly reducing accidental cross-version protocol mismatch.

## Message types (what operations exist)

### Client → Server (`ClientToServerMsg`)

Defined (protobuf) in `legacy_zellij/zellij-utils/src/client_server_contract/client_to_server.proto`:

- Session control: `DetachSession`, `KillSession`, `ConnStatus`
- Terminal properties: `TerminalResize`, `TerminalPixelDimensions`, foreground/background colors, color registers
- Attach/boot: `FirstClientConnected { cli_assets, is_web_client }`, `AttachClient { ... }`
- Input/events: `Action { action, terminal_id?, client_id?, is_cli_client }`, `Key { key, raw_bytes, is_kitty_keyboard_protocol }`
- Watch mode: `AttachWatcherClient { terminal_size, is_web_client }`
- Web-server capability signaling: `WebServerStarted`, `FailedToStartWebServer`

### Server → Client (`ServerToClientMsg`)

Defined in `legacy_zellij/zellij-utils/src/client_server_contract/server_to_client.proto`:

- Rendering: `Render { content: string }`
- Flow control: `UnblockInputThread`, `UnblockCliPipeInput`
- Lifecycle: `Connected`, `Exit { exit_reason, payload? }`
- Diagnostics: `Log { lines[] }`, `LogError { lines[] }`
- Session UX: `SwitchSession { connect_to_session }`, `RenamedSession { name }`
- Misc: `CliPipeOutput { pipe_name, output }`, `QueryTerminalSize`, `StartWebServer`, `ConfigFileUpdated`

## Multiplexing + streaming model

Zellij does *not* multiplex “per-pane output streams” at the IPC layer.
Instead:

- The server owns the canonical screen model per client.
- It sends each client a **rendered frame string** (`RenderMsg`) that the client writes to its terminal.

This is very different from FrankenTerm’s architecture, where we want:

- per-pane content deltas / structured events,
- robot-mode queries (`get-text`, `state`) that target panes independently,
- and (eventually) high-throughput multiplexing across many panes.

## Backpressure + error handling

### Encoding/decoding failures

- Conversion failures from protobuf → Rust are logged and the message is dropped (`warn!(...)`).
- Any read error (eg. EOF) results in `None` from `recv_*`, which higher layers treat as a disconnect.

### “Client too slow” safety disconnect

The protocol includes an explicit disconnect reason surfaced to users:

- `legacy_zellij/zellij-utils/src/ipc.rs`: `ExitReason::Disconnect`
  - Message explains the client may have failed to process server messages quickly enough
  - Suggests reattaching with `zellij attach <session>`

This is an explicit UX for backpressure failure: **drop the client, keep the session**.

## Concrete FrankenTerm recommendations

1. **Adopt a protocol-versioned socket namespace**
   - Zellij’s `contract_version_{N}` directory is a low-effort way to prevent cross-version attach hazards.
   - For FrankenTerm, version the vendored mux socket path (or a shim proxy) by protocol version and/or schema hash.

2. **Make the robot/native protocol schema-first**
   - Zellij’s `oneof` protobuf message model (and checked-in generated code) makes the contract explicit and testable.
   - FrankenTerm could use a schema-first contract (protobuf/flatbuffers/capnp) for `ft robot` / native events to keep semantics stable across releases.

3. **Define explicit backpressure and disconnect semantics**
   - Zellij’s “Disconnect but session persists” is a sane default under overload.
   - FrankenTerm should explicitly specify:
     - what happens when clients fall behind (drop, coalesce, or apply flow control),
     - which messages are lossy vs reliable,
     - and how reconnection resumes state (snapshot replay, last-seen cursor, etc.).

4. **Decide whether the IPC layer should be “render frames” vs “structured deltas”**
   - Zellij chose render frames; it’s simple but can be heavy.
   - FrankenTerm likely wants structured, pane-addressed deltas; keep that as an explicit architectural choice (and test for throughput).

