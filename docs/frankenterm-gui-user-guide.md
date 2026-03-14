# FrankenTerm GUI User Guide and WezTerm Migration

This guide covers the current `frankenterm-gui` workflow in this repository:

- first launch and day-to-day command usage
- `frankenterm.toml` configuration reference (current default keys)
- migration guidance from WezTerm and from ft v1-style workflows
- swarm/agent operating guidance
- extension/plugin entry points

## Quick Start

### 1. Install or build

Option A (app bundle release):

1. Install `FrankenTerm.app`.
2. Move it to `/Applications`.

Option B (build from source):

```bash
cargo build --release -p frankenterm-gui
```

### 2. Create your GUI config

Start from the repo default:

```bash
mkdir -p ~/.config/frankenterm
cp crates/frankenterm-gui/frankenterm.toml ~/.config/frankenterm/frankenterm.toml
```

### 3. Launch

If installed on `PATH`:

```bash
frankenterm-gui
```

From the build tree:

```bash
./target/release/frankenterm-gui
```

### 4. Verify ft integration

Run watcher in one terminal:

```bash
ft watch --foreground
```

Launch GUI in another terminal:

```bash
frankenterm-gui
```

Then confirm events are flowing:

```bash
ft events --limit 20
```

## GUI Command Reference

`frankenterm-gui` subcommands:

- `start`: start GUI and optionally run a command in the initial tab
- `connect <domain>`: attach to a named mux domain
- `ssh [user@]host[:port]`: open remote SSH session in GUI
- `serial <port>`: open serial device session
- `ls-fonts`: inspect font resolution and font inventory
- `show-keys`: print effective key assignments

Useful examples:

```bash
# Start normally
frankenterm-gui start

# Start in a new process (don't reuse an existing GUI instance)
frankenterm-gui start --always-new-process

# Start and run a command
frankenterm-gui start -- bash -lc "htop"

# Connect to an existing domain from config
frankenterm-gui connect production

# Create/attach to a named session (workspace alias)
frankenterm-gui start --session agent-fleet
frankenterm-gui connect production --session agent-fleet

# Open direct SSH session
frankenterm-gui ssh deploy@10.0.0.5

# Inspect key assignments
frankenterm-gui show-keys
frankenterm-gui show-keys --lua
```

Default session-manager entry point:

- `Cmd+S` opens the session manager launcher (workspace/domain/session switch surface).
- Session rows include active/current marker plus window and pane counts.
- Session rows are listed before domain and command-palette rows so the overlay opens session-first.
- Domain rows surface the current mux connection state (`connected` or `detached`).

Global launch options:

- `--config-file <path>`: force a specific config file
- `--config name=value`: override individual config values for one run
- `--skip-config`: run without loading config files
- `--workspace <name>` / `--session <name>`: select or create named session namespace

## Config Reference (`frankenterm.toml`)

Primary GUI config location:

- `~/.config/frankenterm/frankenterm.toml`

Current default key set from `crates/frankenterm-gui/frankenterm.toml`:

| Key | Default | Example / Notes |
|---|---|---|
| `color_scheme` | `"Builtin Dark"` | `color_scheme = "Dracula"` |
| `font_size` | `14.0` | `font_size = 12.0` |
| `[[font.font]].family` | `"JetBrains Mono"` | add additional fallback `[[font.font]]` entries |
| `harfbuzz_features` | `["calt=0","clig=0","liga=0"]` | tune ligatures per preference |
| `window_background_opacity` | `1.0` | `0.95` for transparency |
| `text_background_opacity` | `1.0` | usually keep aligned with window opacity |
| `scrollback_lines` | `100000` | lower for memory-constrained hosts |
| `enable_scroll_bar` | `true` | `false` for minimal UI |
| `initial_rows` | `40` | window startup rows |
| `initial_cols` | `120` | window startup columns |
| `window_decorations` | `"TITLE | RESIZE"` | OS-dependent behavior |
| `window_close_confirmation` | `"NeverPrompt"` | set stricter confirmation if desired |
| `[window_padding].left/right/top/bottom` | `4` | pixel padding around terminal viewport |
| `enable_tab_bar` | `true` | show/hide tab bar |
| `hide_tab_bar_if_only_one_tab` | `true` | keeps single-tab window clean |
| `tab_bar_at_bottom` | `false` | set `true` for bottom tab bar |
| `[leader]` (optional) | unset | tmux-style leader key chord |
| `[[unix_domains]].name` | `"local"` | add named local mux domains |
| `[[unix_domains]].connect_automatically` | `false` | auto-connect on startup if `true` |
| `swap_layout_enabled` | `true` | enables layout cycling support |
| `swap_layout_cycle` (optional) | unset | example: `["grid-4","main-side","stacked"]` |
| `floating_panes_enabled` | `true` | enables floating pane toggles |
| `floating_pane_opacity` (optional) | unset | example: `0.95` |
| `resize_wrap_scorecard_enabled` | `true` | emits resize wrap quality telemetry |
| `resize_wrap_readability_gate_enabled` | `true` | fallback gate for unreadable wraps |
| `resize_wrap_readability_max_line_badness_delta` | `500` | stricter = lower |
| `resize_wrap_readability_max_total_badness_delta` | `2000` | aggregate threshold |
| `resize_wrap_readability_max_fallback_ratio_percent` | `20` | % of lines allowed to fallback |
| `resize_wrap_kp_*` (optional) | unset | advanced KP tuning knobs |
| `[[ssh_domains]]` (optional) | auto-discovered from `~/.ssh/config` | explicit named SSH targets |
| `max_fps` | `60` | lower on constrained GPUs |
| `front_end` | `"WebGpu"` | rendering backend preference |
| `check_for_updates` | `false` | disable update checks by default |
| `automatically_reload_config` | `true` | hot-reload config changes |

SSH domain fields (optional per entry):

- `name`
- `remote_address`
- `username`
- `multiplexing` (`"WezTerm"` or `"None"`)
- `ssh_backend` (`"LibSsh"` or `"Ssh2"`)
- `connect_automatically`
- `default_prog`

## Migration Guide: WezTerm -> FrankenTerm

### A. Config migration (Lua -> TOML)

1. Back up your current `wezterm.lua`.
2. Create `~/.config/frankenterm/frankenterm.toml`.
3. Port common keys directly:

| WezTerm Lua | FrankenTerm TOML |
|---|---|
| `config.font_size = 14.0` | `font_size = 14.0` |
| `config.color_scheme = "Dracula"` | `color_scheme = "Dracula"` |
| `config.scrollback_lines = 100000` | `scrollback_lines = 100000` |
| `config.enable_tab_bar = false` | `enable_tab_bar = false` |

4. Port SSH domains:

```toml
[[ssh_domains]]
name = "production"
remote_address = "10.0.0.5:22"
username = "deploy"
```

5. Validate runtime config:

```bash
ft config validate
```

For a deeper mapping guide, see [docs/extensions/migration-guide.md](./extensions/migration-guide.md).

### B. Keybinding migration

Check effective bindings in GUI:

```bash
frankenterm-gui show-keys
```

New GUI actions surfaced in FrankenTerm include:

- swap layout cycling
- floating pane toggle
- stack cycle controls

### C. v1 (`ft` + stock WezTerm bridge) -> v2 (`frankenterm-gui`)

Operationally important points:

1. `ft` CLI workflows remain valid.
2. `ft watch` can consume GUI native events when native event socket is available.
3. Existing workspace state (`ft.toml`, `ft.db`) remains usable.
4. `FrankenTerm.app` can replace prior wrapper app bundles.
5. WezTerm can remain installed side-by-side during migration.

### D. Features changed or intentionally different

- FrankenTerm TOML is the primary GUI config path.
- Lua callbacks/conditional logic from `wezterm.lua` are not 1:1 TOML mappings.
- Use extension and workflow surfaces for advanced automation patterns.

## Agent Fleet Guide (200+ panes)

### Recommended baseline

GUI-side (`frankenterm.toml`):

```toml
scrollback_lines = 100000
swap_layout_enabled = true
floating_panes_enabled = true
resize_wrap_scorecard_enabled = true
resize_wrap_readability_gate_enabled = true
max_fps = 60
```

ft runtime-side (`~/.config/ft/ft.toml`):

```toml
[native]
enabled = true
socket_path = "/tmp/wa/events.sock"

[ingest]
poll_interval_ms = 200
max_concurrent_captures = 10
```

### Run sequence

```bash
# Terminal 1: watcher
ft watch --foreground

# Terminal 2: GUI
frankenterm-gui

# Terminal 3: machine control plane (optional)
ft robot --format toon state
```

For MCP-based automation:

```bash
ft mcp serve
```

### Backpressure and stability tuning

- If memory pressure rises, lower `scrollback_lines`.
- If capture load is high, increase `ingest.poll_interval_ms` and/or lower `max_concurrent_captures`.
- Native event bridge uses bounded buffering; keep `ft watch` running so events drain continuously.

### Distributed mode setup (feature-gated)

Distributed mode is optional and off by default.

```bash
cargo build -p frankenterm --release --features distributed
```

Follow [docs/distributed-security-spec.md](./distributed-security-spec.md) for TLS/token/mTLS setup and `ft doctor` verification.

## Plugin / Extension Development

Current stable CLI surface for extension management:

```bash
ft ext list
ft ext validate ./my-pack.toml
ft ext install ./my-pack.toml
ft ext info my-pack
```

For WASM-oriented extension architecture and packaging details, use:

- [docs/extensions/getting-started.md](./extensions/getting-started.md)
- [docs/extensions/architecture.md](./extensions/architecture.md)
- [docs/extensions/api-reference.md](./extensions/api-reference.md)

## Troubleshooting Checklist

```bash
ft doctor
ft status --health
ft events --limit 50
frankenterm-gui show-keys
```

If GUI-native events are missing:

1. Ensure `ft watch` is running.
2. Ensure `[native].enabled = true` (or default socket exists).
3. Check logs for native bridge reconnect warnings.
