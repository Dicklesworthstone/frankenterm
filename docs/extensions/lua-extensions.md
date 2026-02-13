# Writing Lua Extensions

Lua extensions run in the existing WezTerm-compatible Lua 5.4 runtime.
They have full access to the `wezterm` module and can use all existing
WezTerm callbacks.

## When to use Lua

- Porting existing WezTerm plugins
- Quick prototyping
- Extensions that rely on WezTerm's Lua API surface
- When you don't need sandbox isolation

## Capabilities

| Feature | Available |
|---------|-----------|
| Async execution | Yes |
| Filesystem access | Yes (unrestricted) |
| Network access | Yes (unrestricted) |
| Sandboxed | No |
| Memory limit | None |
| Execution timeout | None |

Lua extensions are **not sandboxed**. They run with the same privileges
as the FrankenTerm process. Use WASM extensions for untrusted code.

## Entry point

The `engine.entry` field in `extension.toml` points to a `.lua` file.
FrankenTerm loads and executes this file in the shared Lua context.

```toml
[engine]
type = "lua"
entry = "main.lua"
```

## Registering event handlers

Use `wezterm.on()` to register callbacks:

```lua
wezterm.on("config.reload", function(window, pane)
    wezterm.log_info("Config reloaded!")
end)

wezterm.on("pane.created", function(window, pane)
    wezterm.log_info("New pane: " .. tostring(pane:pane_id()))
end)
```

Or declare handlers in the manifest and define named functions:

```toml
[[hooks]]
event = "config.reload"
handler = "on_reload"
```

```lua
function on_reload(event, payload)
    wezterm.log_info("Config reloaded via manifest hook")
    return nil
end
```

## Returning actions

Hook handlers can return `nil` (no action) or a table of actions:

```lua
function on_reload(event, payload)
    return {
        { SetConfig = { key = "font_size", value = 14.0 } },
        { Log = { level = "info", message = "applied font size" } },
    }
end
```

## Dispatch tier

Lua handlers execute in the `Lua` tier (priority 2), which runs after
`Native` (0) and `Wasm` (1) handlers. This means Lua hooks see events
after WASM hooks have had a chance to handle them.

## Accessing the WezTerm API

The full WezTerm Lua API is available:

```lua
local wezterm = require("wezterm")

-- Font configuration
local config = wezterm.config_builder()
config.font = wezterm.font("JetBrains Mono")
config.font_size = 14.0

-- Action helpers
local act = wezterm.action

config.keys = {
    { key = "t", mods = "CTRL|SHIFT", action = act.SpawnTab("CurrentPaneDomain") },
}

return config
```

## Limitations

- No sandbox: Lua has full process access
- Global state: all Lua extensions share one Lua context
- No hot reload: changing a Lua extension requires config reload
- Thread safety: Lua state is `Send` but not `Sync`; the runtime
  manages this via the LuaPipe mechanism
