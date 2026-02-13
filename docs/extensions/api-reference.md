# API Reference

This document covers all host functions available to FrankenTerm extensions.

## WASM host functions

These functions are linked into the WASM runtime and callable from any
WASM extension via `extern "C"` declarations.

### ft_log

```rust
extern "C" {
    fn ft_log(level: i32, msg_ptr: *const u8, msg_len: u32);
}
```

Emit a log message. Level values:

| Level | Value |
|-------|-------|
| Trace | 0 |
| Debug | 1 |
| Info | 2 |
| Warn | 3 |
| Error | 4 |

### ft_get_env

```rust
extern "C" {
    fn ft_get_env(key_ptr: *const u8, key_len: u32) -> i32;
}
```

Read an environment variable. Returns the length of the value, or -1 if
the variable is not set or access is denied by sandbox policy. The value
is written to the return buffer; use `ft_return_buffer_read` to retrieve it.

**Requires**: `environment` permission for the variable name (or a matching
wildcard pattern).

### ft_return_buffer_read

```rust
extern "C" {
    fn ft_return_buffer_read(out_ptr: *mut u8, out_len: u32) -> i32;
}
```

Copy data from the host's return buffer into WASM linear memory. Returns
the number of bytes actually copied. Call this after `ft_get_env` or any
other function that writes to the return buffer.

## Lua API

Lua extensions use the existing WezTerm Lua API surface. Key functions:

### wezterm.log_info / wezterm.log_warn / wezterm.log_error

```lua
wezterm.log_info("message")
wezterm.log_warn("message")
wezterm.log_error("message")
```

### Event callbacks

Register a callback for a named event:

```lua
wezterm.on("config.reload", function(window, pane)
    -- handle event
    return nil  -- or return action table
end)
```

### Config access

```lua
local config = wezterm.config_builder()
config.font_size = 14.0
config.color_scheme = "Dracula"
return config
```

## Action types

Both WASM and Lua handlers can return actions:

### SetConfig

Update a configuration value at runtime.

```rust
Action::SetConfig {
    key: "color_scheme".to_string(),
    value: Value::String("Dracula".to_string()),
}
```

Lua equivalent:

```lua
return { SetConfig = { key = "color_scheme", value = "Dracula" } }
```

### SendInput

Send text input to a pane.

```rust
Action::SendInput {
    pane_id: Some(42),
    text: "ls -la\n".to_string(),
}
```

### Log

Emit a log message (same as calling `ft_log` directly).

```rust
Action::Log {
    level: LogLevel::Info,
    message: "something happened".to_string(),
}
```

### Custom

Extension-defined action for inter-extension communication.

```rust
Action::Custom {
    name: "my-extension:notify".to_string(),
    payload: Value::String("hello".to_string()),
}
```

## Extension storage (Rust)

The `ExtensionStorage` API provides persistent key-value storage. From
within the host runtime, extensions interact with storage through their
assigned `extension_id`:

```rust
// Store a value
storage.set("my-ext", "last_theme", b"dracula")?;

// Retrieve a value
let theme = storage.get("my-ext", "last_theme")?;

// List keys
let keys = storage.keys("my-ext")?;

// Delete a key
storage.delete("my-ext", "last_theme")?;

// Clear all data for extension
storage.clear_extension("my-ext")?;
```

**Constraints**:
- Keys: non-empty, max 256 bytes
- Values: arbitrary bytes (no size limit, but be reasonable)
- Isolation: each extension_id has its own namespace
- Persistence: stored on disk, cached in memory

## Keybinding registration (Rust)

```rust
use frankenterm_scripting::{KeyCombo, KeybindingRegistry, Action, Value};

let registry = KeybindingRegistry::new();
let combo = KeyCombo::parse("ctrl+shift+p")?;

let id = registry.register(combo, "my-ext", |combo, payload| {
    Ok(vec![Action::Custom {
        name: "my-ext:palette".to_string(),
        payload: Value::Null,
    }])
});

// Later: unregister
registry.unregister(id);
```

## Event bus registration (Rust)

```rust
use frankenterm_scripting::{EventBus, DispatchTier, Action, Value};

let bus = EventBus::new();

let hook_id = bus.register(
    "pane.*",              // pattern (prefix wildcard)
    DispatchTier::Wasm,    // tier
    0,                     // priority
    Some("my-ext"),        // extension_id
    |event, payload| {
        Ok(vec![Action::Log {
            level: LogLevel::Debug,
            message: format!("saw event: {event}"),
        }])
    },
);

// Fire an event
let actions = bus.fire("pane.focus", &Value::Null)?;

// Unregister
bus.unregister(hook_id);
```

## Performance budgets

Extensions should meet these targets:

| Metric | Budget |
|--------|--------|
| Hook handler execution | < 1ms per call |
| Extension cold load | < 500ms |
| Extension warm load (cached) | < 10ms |
| Memory per extension | < 64 MiB |
