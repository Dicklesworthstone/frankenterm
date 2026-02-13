# Getting Started with FrankenTerm Extensions

This guide walks you through creating, packaging, and installing your first
FrankenTerm extension.

## Prerequisites

- FrankenTerm installed and running
- Rust toolchain (for WASM extensions) or Lua 5.4 (for Lua extensions)
- `wasm32-wasip1` target: `rustup target add wasm32-wasip1`

## Extension structure

Every extension is a `.ftx` package (a ZIP file) containing at minimum:

```
my-extension/
  extension.toml    # Manifest (required)
  main.wasm         # WASM entry point (or main.lua for Lua)
```

## Step 1: Write the manifest

Create `extension.toml`:

```toml
[extension]
name = "hello-world"
version = "0.1.0"
description = "Logs a message on every config reload"
authors = ["Your Name"]
license = "MIT"

[engine]
type = "wasm"           # "wasm", "lua", or "both"
entry = "main.wasm"

[permissions]
network = false
pane_access = false
filesystem_read = []
filesystem_write = []
environment = []

[[hooks]]
event = "config.reload"
handler = "on_reload"
```

### Manifest fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Unique extension identifier (lowercase, hyphens OK) |
| `version` | no | SemVer string (default `"0.0.0"`) |
| `description` | no | Short summary |
| `authors` | no | Author list |
| `license` | no | SPDX identifier |
| `engine.type` | yes | `"wasm"`, `"lua"`, or `"both"` |
| `engine.entry` | yes | Entry file path relative to package root |
| `permissions.*` | no | Security policy (see [permissions.md](permissions.md)) |
| `hooks` | no | Event-to-handler bindings |

## Step 2: Write the extension (WASM/Rust)

Create a new Rust library:

```bash
cargo init --lib hello-world
cd hello-world
```

In `Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
# No external deps needed for basic extensions
```

In `src/lib.rs`:

```rust
// FrankenTerm host functions (provided by the runtime)
extern "C" {
    fn ft_log(level: i32, msg_ptr: *const u8, msg_len: u32);
}

fn log_info(msg: &str) {
    unsafe { ft_log(2, msg.as_ptr(), msg.len() as u32) }
}

// Called by FrankenTerm when config.reload fires
#[no_mangle]
pub extern "C" fn on_reload() {
    log_info("Hello from my extension! Config was reloaded.");
}
```

Build for WASM:

```bash
cargo build --target wasm32-wasip1 --release
cp target/wasm32-wasip1/release/hello_world.wasm main.wasm
```

## Step 2 (alternative): Write the extension (Lua)

Create `main.lua`:

```lua
-- Called by FrankenTerm when config.reload fires
function on_reload(event, payload)
    wezterm.log_info("Hello from my Lua extension! Config was reloaded.")
    return nil  -- return nil for no actions, or a table of actions
end
```

## Step 3: Package as .ftx

Create a ZIP file with the manifest and entry point:

```bash
zip hello-world.ftx extension.toml main.wasm
```

The `.ftx` format is a standard ZIP archive. The manifest must be at the
top level (not nested in a subdirectory).

## Step 4: Install

```bash
# Install from .ftx file
frankenterm extension install hello-world.ftx

# Verify installation
frankenterm extension list

# Enable (extensions are enabled on install by default)
frankenterm extension enable hello-world
```

Extensions are installed to:
- Linux: `$XDG_DATA_HOME/frankenterm/extensions/` (default `~/.local/share/frankenterm/extensions/`)
- macOS: `~/Library/Application Support/frankenterm/extensions/`

## Step 5: Test

Reload your config (or restart FrankenTerm). You should see the log message
in the debug output.

## Extension lifecycle

```
Install  -->  Installed  -->  Loaded  (running)
                  |              |
                  v              v
               Disabled      Error(msg)
                  |
                  v
               Removed
```

- **Installed**: On disk, not yet loaded
- **Loaded**: Active, hooks registered, responding to events
- **Disabled**: Explicitly disabled; hooks unregistered, state preserved
- **Error**: Failed to load; error message stored for diagnostics

State persists across restarts via `.state.toml` in the extension directory.

## What's next

- [Architecture](architecture.md) -- how the scripting engine works
- [API Reference](api-reference.md) -- complete host function reference
- [Permissions](permissions.md) -- permission model explained
- [Migration Guide](migration-guide.md) -- porting wezterm.lua configs
