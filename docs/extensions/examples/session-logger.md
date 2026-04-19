# Example: Session Logger (Lua)

A Lua extension that logs session events (pane create/close, tab
create/close) to a file for audit or debugging.

## File structure

```
session-logger/
  extension.toml
  main.lua
```

## extension.toml

```toml
[extension]
name = "session-logger"
version = "0.1.0"
description = "Log session lifecycle events to a file"
authors = ["Author"]
license = "MIT"

[engine]
type = "lua"
entry = "main.lua"

[permissions]
filesystem = ["write:~/.local/share/frankenterm/logs/"]
pane_access = false
network = false
environment = ["HOME"]

[hooks]
"pane.created" = "on_pane_event"
"pane.closed" = "on_pane_event"
"tab.created" = "on_tab_event"
"tab.closed" = "on_tab_event"
```

## main.lua

```lua
local log_path = os.getenv("HOME") .. "/.local/share/frankenterm/logs/session.log"

local function append_log(line)
    local f = io.open(log_path, "a")
    if f then
        f:write(os.date("%Y-%m-%dT%H:%M:%S") .. " " .. line .. "\n")
        f:close()
    end
end

function on_pane_event(event, payload)
    append_log(event .. " pane_id=" .. tostring(payload.pane_id or "?"))
    return nil
end

function on_tab_event(event, payload)
    append_log(event .. " tab_id=" .. tostring(payload.tab_id or "?"))
    return nil
end
```

## Package and install

```bash
cd session-logger
zip -r ../session-logger.ftx extension.toml main.lua
ft ext install ../session-logger.ftx
```

Log output appears at `~/.local/share/frankenterm/logs/session.log`:

```
2026-02-13T14:30:01 pane.created pane_id=1
2026-02-13T14:30:02 tab.created tab_id=1
2026-02-13T14:35:10 pane.closed pane_id=1
```
