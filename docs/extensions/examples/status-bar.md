# Example: Status Bar (WASM/Rust)

A WASM extension that updates the terminal status bar with system
information on pane focus events.

## File structure

```
status-bar/
  extension.toml
  src/lib.rs
  Cargo.toml
```

## extension.toml

```toml
[extension]
name = "status-bar-info"
version = "0.1.0"
description = "Show system info in the status bar on pane focus"
authors = ["Author"]
license = "MIT"

[engine]
type = "wasm"
entry = "main.wasm"

[permissions]
environment = ["USER", "HOSTNAME", "PWD"]
network = false
pane_access = false

[[hooks]]
event = "pane.focus"
handler = "on_pane_focus"

[[hooks]]
event = "window.focus"
handler = "on_window_focus"
```

## Cargo.toml

```toml
[package]
name = "status-bar-info"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[profile.release]
opt-level = "s"
lto = true
strip = true
```

## src/lib.rs

```rust
mod ft {
    extern "C" {
        pub fn ft_log(level: i32, msg_ptr: *const u8, msg_len: u32);
        pub fn ft_get_env(key_ptr: *const u8, key_len: u32) -> i32;
        pub fn ft_return_buffer_read(out_ptr: *mut u8, out_len: u32) -> i32;
    }

    pub fn log_info(msg: &str) {
        unsafe { ft_log(2, msg.as_ptr(), msg.len() as u32) }
    }

    pub fn get_env(key: &str) -> Option<String> {
        let len = unsafe { ft_get_env(key.as_ptr(), key.len() as u32) };
        if len < 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let read = unsafe {
            ft_return_buffer_read(buf.as_mut_ptr(), buf.len() as u32)
        };
        buf.truncate(read as usize);
        String::from_utf8(buf).ok()
    }
}

fn build_status_line() -> String {
    let user = ft::get_env("USER").unwrap_or_else(|| "?".into());
    let host = ft::get_env("HOSTNAME").unwrap_or_else(|| "localhost".into());
    let pwd = ft::get_env("PWD").unwrap_or_else(|| "~".into());

    // Shorten PWD if it starts with the home dir
    let display_pwd = if let Some(home) = ft::get_env("HOME") {
        if pwd.starts_with(&home) {
            format!("~{}", &pwd[home.len()..])
        } else {
            pwd
        }
    } else {
        pwd
    };

    format!("{user}@{host} | {display_pwd}")
}

#[no_mangle]
pub extern "C" fn on_pane_focus() {
    let status = build_status_line();
    ft::log_info(&format!("Status: {status}"));
    // In a full implementation, this would return a SetConfig action
    // to update the status bar text
}

#[no_mangle]
pub extern "C" fn on_window_focus() {
    let status = build_status_line();
    ft::log_info(&format!("Window focused. Status: {status}"));
}
```

## Build and package

```bash
cd status-bar
cargo build --target wasm32-wasip1 --release
cp target/wasm32-wasip1/release/status_bar_info.wasm main.wasm
zip status-bar-info.ftx extension.toml main.wasm
frankenterm extension install status-bar-info.ftx
```
