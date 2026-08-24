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
cargo build --profile release-interactive -p frankenterm-gui
```

`release-interactive` is required for shipped CLI, GUI, and mux-server builds;
the ordinary `release` profile also unwinds as a fail-safe default. Only the
explicit `release-abort-probe` negative-control profile disables recovery.

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
./target/release-interactive/frankenterm-gui
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
| `font_dirs` | `["fonts"]` | bundled app fonts directory |
| `[[font.font]].family` | `"Pragmasevka Nerd Font"` | add additional fallback `[[font.font]]` entries |
| `[[font.font]].harfbuzz_features` | `["calt=0","clig=0","liga=0"]` | tune ligatures per preference |
| `window_background_opacity` | `1.0` | `0.95` for transparency |
| `text_background_opacity` | `1.0` | usually keep aligned with window opacity |
| `scrollback_lines` | `100000` | lower for memory-constrained hosts |
| `enable_scroll_bar` | `true` | `false` for minimal UI |
| `initial_rows` | `40` | window startup rows |
| `initial_cols` | `120` | window startup columns |
| `window_decorations` | macOS: `"INTEGRATED_BUTTONS | RESIZE"`; other platforms: `"TITLE | RESIZE"` | OS-dependent behavior |
| `window_close_confirmation` | `"AlwaysPrompt"` | controls tab/window/quit confirmation prompts |
| `skip_close_confirmation_for_processes_named` | `[]` | keep empty to prompt for ordinary shell-backed tabs |
| `click_interval_ms` | `500` | accessibility: raise to `1000`-`2000` for a slower double-click cadence |
| `[window_padding].left/right/top/bottom` | `4` | pixel padding around terminal viewport |
| `enable_tab_bar` | `true` | show/hide tab bar |
| `hide_tab_bar_if_only_one_tab` | `false` | keeps the macOS integrated titlebar surface visible |
| `tab_bar_at_bottom` | `false` | set `true` for bottom tab bar |
| `[leader]` (optional) | unset | tmux-style leader key chord |
| `unix_domains` | implicit `"unix"` domain | add custom domains only for non-default mux sockets |
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

Swap layouts and floating panes are currently keybinding-driven features, not
TOML-gated features. The default sample config does not list `swap_layout_*` or
`floating_pane_*` keys because those fields are not parsed as active GUI config
fields yet.

SSH domain fields (optional per entry):

- `name`
- `remote_address`
- `username`
- `multiplexing` (`"WezTerm"` or `"None"`)
- `ssh_backend` (`"LibSsh"` or `"Ssh2"`)
- `connect_automatically`
- `default_prog`

### Remembered domain attachments

FrankenTerm remembers an explicit remote-domain attach or detach across GUI
restarts. This is a desired-attachment preference, not a serialized network
connection or a promise that an incompatible or unavailable mux can be reached.
The built-in reconnect supervisor applies three states:

- no remembered record: follow the domain's `connect_automatically` setting;
- remembered attached: keep retrying the configured domain even when
  `connect_automatically` is false;
- remembered detached: do not auto-connect that domain, even when its
  `connect_automatically` setting is true.

An explicit later attach or detach replaces the remembered choice before the
mux mutation begins, so an unreachable domain can still retain the operator's
desired state. A Lua script that later calls `domain:attach()` is itself an
explicit attach and changes a remembered detached state back to attached; a
periodic user-authored Lua watchdog therefore remains authoritative over its
own actions. If a command-palette or Lua attach cannot reach the mux, the failed
attempt releases its single-flight transport claim before FrankenTerm rebuilds
the supervisor with that exact remembered domain in its retry frontier. A Lua
detach fences the old supervisor generation immediately after the detached
intent is durable, so a stale retry cannot undo the operator's choice. All
attach, detach, startup, retry, and configuration-reload actions for the same
domain alias are ticket-ordered through persistence, mux mutation, and retry
handoff; cancelling a queued action cannot let its successor overtake the
currently active action. Different domains remain independent, with separate
health discovery and backoff, so an unavailable domain cannot starve a newly
detached peer. A successful explicit attach refreshes the remembered supervisor
plan as well as the live connection, preserving automatic recovery if the
connection's internal retry budget is later exhausted.

If an intent write reports a late failure, FrankenTerm reloads the replicated
authority before deciding what happened. A two-replica commit is repaired and
accepted only when the recovered quorum contains the exact requested intent; a
pre-commit failure republishes the proven older quorum and leaves the requested
live mutation unperformed. If the reload itself cannot establish a quorum, the
in-memory snapshot is cleared and the reconnect supervisor is cancelled. An
explicit attach may still continue as a direct operator action, but it is not
advertised as remembered and cannot inherit stale automatic-reconnect state.

Hot configuration reload retires an old exact client-domain generation before
publishing its replacement, but continues adding unrelated domains and a safe
default while that guard drains. It retries the newest configuration until the
same-name fence clears, including a client-to-raw domain transition; a stale
reload generation cannot overwrite a newer one. Reload notification first
publishes a fail-closed validation gate and revokes both the old scheduler
request and supervisor epochs. Automatic connection cannot read or dial from
the new domain configuration until that exact generation passes aggregate
reconciliation. An invalid or duplicate configuration therefore leaves the
last live registry intact and automatic reconnect visibly paused; fixing and
successfully reloading the configuration resumes it. Temporary main-thread
scheduler saturation is retained by one serialized retry coordinator, and a
thread that failed to start is never reported as a pending reconnect.

The preference is stored under FrankenTerm's mode-0700 private data directory
in three replicated, mode-0600 checksummed files. Schema v2 requires two exact
replicas of the same complete generation before that state is authoritative. A
higher-generation singleton is an interrupted publication and cannot displace
the older two-replica quorum. A preference update reports success only after
all three replicas have been written, synchronized, read back, and verified.
On load, FrankenTerm repairs a missing, damaged, or stale replica only from an
already authoritative quorum. If no quorum remains, reconnect is paused and
the fault is reported; the loader does not select an older generation or fall
back to `connect_automatically`.

Existing schema-v1 two-slot state is migrated in place while holding the same
exclusive authority lock. Migration accepts either two valid legacy slots (the
newer generation wins, while divergent equal generations fail closed) or the
canonical first publication in slot 0 at generation 1 with slot 1 absent. A
single valid legacy slot paired with a damaged or empty slot is ambiguous and
is never used as authority. Migration publishes schema v2 to the third slot
first, then the stale legacy slot, and finally the active legacy slot. After a
crash, it resumes only from an exact cross-schema content quorum or a normal
schema-v2 quorum; once schema v2 is present, an unrelated older schema-v1 state
is never a fallback.

The files store only domain-separated SHA-256 fingerprints of domain aliases,
never raw aliases, addresses, usernames, socket paths, or credentials. The
directory is pinned as a capability before the lock or any slot is opened;
every leaf is opened relative to that descriptor without following symlinks,
constrained to one private regular-file link owned by the same account, and
revalidated against its name after I/O. Replacing the directory, lock, or slot
therefore fails the operation instead of splitting lock authority or falsely
reporting a durable preference. An explicit operator-requested attach still
proceeds when this optional preference cannot be written, with a persistent
warning that restart recovery was not remembered. An explicit detach remains
fail-closed until its durable negative intent commits, preventing an older
`Attached` record from reconnecting behind the operator's request.

### Accessibility Timing

`click_interval_ms` controls how much time FrankenTerm allows between successive clicks when deciding whether a gesture counts as a double-click or triple-click selection. The default is `500`, which matches common desktop defaults, but operators who need a slower cadence can raise it to `1000`-`2000` in `frankenterm.toml`.

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
2. `ft watch` can consume best-effort GUI state/lifecycle hints when the
   authenticated native event socket is explicitly enabled. Raw pane-output
   bytes still use the polling capture path.
3. Existing workspace state (`ft.toml`, `ft.db`) remains usable.
4. `FrankenTerm.app` can replace prior wrapper app bundles.
5. WezTerm can remain installed side-by-side during migration.

### D. Features changed or intentionally different

- FrankenTerm TOML is the primary GUI config path.
- Lua callbacks/conditional logic from `wezterm.lua` are not 1:1 TOML mappings.
- Use extension and workflow surfaces for advanced automation patterns.

### E. Current mux and SSH behavior contracts

- `LocalDomain`, `RemoteSshDomain`, and `TermWizTerminalDomain` are intentionally non-detachable. FrankenTerm reports them as not detachable and returns explicit errors instead of pretending those panes can survive a detach operation.
- `TmuxDomain` detach is supported only for a live launcher pane. FrankenTerm detaches by sending the control-mode detach key to that pane and then reports the domain as detached after tmux exits. Direct `spawn` and `spawn_pane` calls on `TmuxDomain` remain intentionally unsupported because tmux windows and panes materialize asynchronously from control-mode events.
- SSH domain discovery still auto-loads `~/.ssh/config`, including `Match exec` criteria. FrankenTerm evaluates those commands locally by default during config resolution. Callers that need a no-spawn posture must deny `Match exec` evaluation explicitly and should consume match diagnostics rather than assuming silent fallback.
- For libssh-backed SFTP files, metadata mutation on an open file is path-based rather than handle-based. Permission changes and modified-time changes are supported; access-time mutation is rejected explicitly so operators do not get a fake success path.

## Agent Fleet Guide (200+ panes)

### Recommended baseline

GUI-side (`frankenterm.toml`):

```toml
scrollback_lines = 100000
resize_wrap_scorecard_enabled = true
resize_wrap_readability_gate_enabled = true
max_fps = 60
```

ft runtime-side (`~/.config/ft/ft.toml`):

```toml
[native]
enabled = true
# `socket_path` is optional. When omitted, FrankenTerm resolves a private
# per-user runtime path under `$TMPDIR` (or `/tmp`).
# The authenticated bridge is currently available on Apple, Linux, Android,
# FreeBSD, and DragonFly targets. Other targets fail closed.

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
- Native state/lifecycle hints use bounded buffering and can be dropped under
  pressure or an indeterminate socket write. Polling remains the authoritative
  pane-text path; keep `ft watch` running so the hint queue drains continuously.
- For release posture, hardware-tier defaults, and fallback expectations, use `docs/resize-user-facing-release-tuning-guidance-wa-1u90p.8.5.md`.
- For exact runtime knob ranges, use `docs/tuning-reference.md`.

### Distributed mode setup (feature-gated)

Distributed mode is optional and off by default.

```bash
cargo build -p frankenterm --profile release-interactive --features distributed
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
2. Ensure `[native].enabled = true`; merely creating the default socket or
   setting `WEZTERM_FT_SOCKET` does not bypass explicit disablement.
3. Ensure the GUI and watcher resolve the same socket path and that its parent
   directory is owned by the effective user with mode `0700`.
4. Check logs for authentication, known queue-drop, indeterminate-write, or
   reconnect warnings.
