# Example: Keybinding Helper (WASM/Rust)

A WASM extension that registers custom keybindings for common actions
like toggling the tab bar or opening a quick command palette.

## File structure

```
keybinding-helper/
  extension.toml
  src/lib.rs
  Cargo.toml
```

## extension.toml

```toml
[extension]
name = "keybinding-helper"
version = "0.1.0"
description = "Custom keybinding actions"
authors = ["Author"]
license = "MIT"

[engine]
type = "wasm"
entry = "main.wasm"

[permissions]
network = false
pane_access = false

[[hooks]]
event = "key.pressed"
handler = "on_key"
```

## Cargo.toml

```toml
[package]
name = "keybinding-helper"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]
```

## src/lib.rs

```rust
extern "C" {
    fn ft_log(level: i32, msg_ptr: *const u8, msg_len: u32);
}

fn log_info(msg: &str) {
    unsafe { ft_log(2, msg.as_ptr(), msg.len() as u32) }
}

/// Called when key.pressed event fires.
/// In a full implementation, the payload would contain key + modifiers
/// and this handler would check if it matches a registered combo.
#[no_mangle]
pub extern "C" fn on_key() {
    log_info("keybinding-helper: key event received");
}
```

## Using keybindings from Rust (host-side)

Extensions that need to register keybindings do so through the
`KeybindingRegistry` at load time:

```rust
use frankenterm_scripting::{KeyCombo, KeybindingRegistry, Action, Value};

let registry = KeybindingRegistry::new();

// Register ctrl+shift+p for command palette
let combo = KeyCombo::parse("ctrl+shift+p").unwrap();
let id = registry.register(combo, "keybinding-helper", |_combo, _payload| {
    Ok(vec![Action::Custom {
        name: "keybinding-helper:palette".to_string(),
        payload: Value::Null,
    }])
});

// Register ctrl+shift+b to toggle tab bar
let combo2 = KeyCombo::parse("ctrl+shift+b").unwrap();
let id2 = registry.register(combo2, "keybinding-helper", |_combo, _payload| {
    Ok(vec![Action::SetConfig {
        key: "enable_tab_bar".to_string(),
        value: Value::Bool(false), // toggle logic would go here
    }])
});

// List all bound combos
for combo_str in registry.bound_combos() {
    println!("Bound: {combo_str}");
}
```

## Modifier aliases

The key combo parser understands these aliases:

| Input | Resolves to |
|-------|-------------|
| `ctrl` | Control |
| `shift` | Shift |
| `alt`, `opt`, `option` | Alt/Option |
| `super`, `cmd`, `command`, `meta` | Super/Command |

Examples: `"ctrl+t"`, `"cmd+shift+p"`, `"alt+enter"`
