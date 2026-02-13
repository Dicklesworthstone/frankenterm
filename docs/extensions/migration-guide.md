# Migration Guide: wezterm.lua to frankenterm.toml

FrankenTerm supports multiple config formats. This guide explains how to
migrate from a WezTerm Lua configuration to FrankenTerm's native TOML
format.

## Config format detection

FrankenTerm probes for config files in this priority order:

1. `$FRANKENTERM_CONFIG_FILE` environment variable (any format, detected by extension)
2. `frankenterm.toml` in XDG config dirs and `~/.frankenterm.toml`
3. `frankenterm.wasm` in XDG config dirs (if WASM support is compiled in)
4. Legacy `wezterm.lua` / `.wezterm.lua` paths
5. Built-in defaults

If you have both `frankenterm.toml` and `wezterm.lua`, the TOML file wins.

## Automatic migration

FrankenTerm includes a migration tool that converts Lua configs to TOML:

```bash
frankenterm migrate ~/.wezterm.lua -o ~/.config/frankenterm/frankenterm.toml
```

The migrator performs best-effort conversion:

- **Static assignments** are converted directly:
  ```lua
  config.font_size = 14.0
  config.color_scheme = "Catppuccin Mocha"
  config.scrollback_lines = 10000
  ```
  becomes:
  ```toml
  font_size = 14
  color_scheme = "Catppuccin Mocha"
  scrollback_lines = 10000
  ```

- **Nested tables** become TOML sections:
  ```lua
  config.ssh_domains = {
    { name = "work", remote_address = "work.example.com" },
  }
  ```
  becomes:
  ```toml
  [[ssh_domains]]
  name = "work"
  remote_address = "work.example.com"
  ```

- **Dynamic constructs** get `[MANUAL]` comments:
  ```lua
  -- Conditional logic, callbacks, runtime computations
  if wezterm.target_triple == "x86_64-pc-windows-msvc" then
    config.default_prog = { "pwsh.exe" }
  end
  ```
  becomes:
  ```toml
  # [MANUAL] default_prog: conditional logic (review manually)
  ```

## Manual migration reference

### Simple values

| Lua | TOML |
|-----|------|
| `config.font_size = 14.0` | `font_size = 14.0` |
| `config.color_scheme = "Dracula"` | `color_scheme = "Dracula"` |
| `config.enable_tab_bar = false` | `enable_tab_bar = false` |
| `config.scrollback_lines = 10000` | `scrollback_lines = 10000` |

### Tables

```lua
config.font = wezterm.font("JetBrains Mono")
```

becomes:

```toml
[font]
family = "JetBrains Mono"
```

### Arrays of tables

```lua
config.ssh_domains = {
  { name = "work", remote_address = "host1" },
  { name = "home", remote_address = "host2" },
}
```

becomes:

```toml
[[ssh_domains]]
name = "work"
remote_address = "host1"

[[ssh_domains]]
name = "home"
remote_address = "host2"
```

### Key bindings

```lua
config.keys = {
  { key = "t", mods = "CTRL|SHIFT", action = wezterm.action.SpawnTab("CurrentPaneDomain") },
}
```

becomes:

```toml
[[keys]]
key = "t"
mods = "CTRL|SHIFT"
action = { SpawnTab = "CurrentPaneDomain" }
```

## What can't be migrated

These Lua patterns have no TOML equivalent and must be handled manually:

1. **Conditional logic** (`if/else` based on OS, hostname, etc.)
   - Use FrankenTerm config profiles or separate config files per environment

2. **Runtime callbacks** (`wezterm.on("event", function)`)
   - Convert to a FrankenTerm extension (see [getting-started.md](getting-started.md))

3. **Dynamic font resolution** (`wezterm.font_with_fallback`)
   - Use the TOML font fallback syntax

4. **Computed values** (string concatenation, math, etc.)
   - Hard-code the result or use a WASM config evaluator

## Keeping your Lua config

You don't have to migrate. FrankenTerm loads `wezterm.lua` configs
natively through the Lua engine. The only reason to migrate is if you
want to:

- Drop the Lua dependency (build with `--no-default-features --features no-lua`)
- Use TOML-native features (config profiles, schema validation)
- Simplify your config for version control
