# Example: Auto Dark Mode (WASM/Rust)

A WASM extension that detects the OS color scheme and switches the
terminal theme accordingly on config reload.

## File structure

```
auto-dark-mode/
  extension.toml
  src/lib.rs
  Cargo.toml
```

## extension.toml

```toml
[extension]
name = "auto-dark-mode"
version = "0.1.0"
description = "Automatically switch theme based on OS dark/light mode"
authors = ["Author"]
license = "MIT"

[engine]
type = "wasm"
entry = "main.wasm"

[permissions]
environment = ["COLORFGBG", "TERM_PROGRAM"]
network = false
pane_access = false

[[hooks]]
event = "config.reload"
handler = "check_dark_mode"
```

## Cargo.toml

```toml
[package]
name = "auto-dark-mode"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]
```

## src/lib.rs

```rust
use std::slice;

// Host function imports
extern "C" {
    fn ft_log(level: i32, msg_ptr: *const u8, msg_len: u32);
    fn ft_get_env(key_ptr: *const u8, key_len: u32) -> i32;
    fn ft_return_buffer_read(out_ptr: *mut u8, out_len: u32) -> i32;
}

fn log_info(msg: &str) {
    unsafe { ft_log(2, msg.as_ptr(), msg.len() as u32) }
}

fn get_env(key: &str) -> Option<String> {
    let len = unsafe { ft_get_env(key.as_ptr(), key.len() as u32) };
    if len < 0 {
        return None;
    }
    let len = len as usize;
    let mut buf = vec![0u8; len];
    let read = unsafe { ft_return_buffer_read(buf.as_mut_ptr(), len as u32) };
    buf.truncate(read as usize);
    String::from_utf8(buf).ok()
}

fn is_dark_mode() -> bool {
    // COLORFGBG is set by some terminals: "15;0" means light-on-dark
    if let Some(colorfgbg) = get_env("COLORFGBG") {
        if let Some(bg) = colorfgbg.split(';').last() {
            if let Ok(n) = bg.parse::<u32>() {
                return n < 8; // dark backgrounds are typically 0-7
            }
        }
    }
    true // default to dark
}

#[no_mangle]
pub extern "C" fn check_dark_mode() {
    let dark = is_dark_mode();
    let scheme = if dark { "Dracula" } else { "Solarized Light" };
    log_info(&format!("Dark mode: {dark}, using scheme: {scheme}"));

    // In a real extension, you'd return a SetConfig action here.
    // The action return mechanism depends on the host function ABI
    // which will be stabilized in a future release.
}
```

## Build and package

```bash
cd auto-dark-mode
cargo build --target wasm32-wasip1 --release
cp target/wasm32-wasip1/release/auto_dark_mode.wasm main.wasm
zip auto-dark-mode.ftx extension.toml main.wasm
frankenterm extension install auto-dark-mode.ftx
```
