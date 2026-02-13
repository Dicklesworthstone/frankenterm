# Rust SDK Quick Start

This guide covers writing FrankenTerm WASM extensions in Rust using
direct host function imports. A higher-level SDK crate
(`frankenterm-extension-sdk`) is planned for a future release.

## Minimal extension

### 1. Create the project

```bash
cargo init --lib my-ext
cd my-ext
```

### 2. Configure Cargo.toml

```toml
[package]
name = "my-ext"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[profile.release]
opt-level = "s"
lto = true
strip = true
```

### 3. Write src/lib.rs

```rust
// FrankenTerm host function imports
mod ft {
    extern "C" {
        pub fn ft_log(level: i32, msg_ptr: *const u8, msg_len: u32);
        pub fn ft_get_env(key_ptr: *const u8, key_len: u32) -> i32;
        pub fn ft_return_buffer_read(out_ptr: *mut u8, out_len: u32) -> i32;
    }

    pub fn log_info(msg: &str) {
        unsafe { ft_log(2, msg.as_ptr(), msg.len() as u32) }
    }

    pub fn log_warn(msg: &str) {
        unsafe { ft_log(3, msg.as_ptr(), msg.len() as u32) }
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

// Hook handler: called on config.reload
#[no_mangle]
pub extern "C" fn on_reload() {
    ft::log_info("Extension loaded, config reloaded");

    if let Some(term) = ft::get_env("TERM") {
        ft::log_info(&format!("TERM = {term}"));
    }
}
```

### 4. Create extension.toml

```toml
[extension]
name = "my-ext"
version = "0.1.0"
description = "My first extension"

[engine]
type = "wasm"
entry = "main.wasm"

[permissions]
environment = ["TERM"]

[[hooks]]
event = "config.reload"
handler = "on_reload"
```

### 5. Build and package

```bash
cargo build --target wasm32-wasip1 --release
cp target/wasm32-wasip1/release/my_ext.wasm main.wasm
zip my-ext.ftx extension.toml main.wasm
```

### 6. Install

```bash
frankenterm extension install my-ext.ftx
```

## Testing locally

You can test the WASM module outside FrankenTerm using `wasmtime`:

```bash
wasmtime run --invoke on_reload main.wasm
```

Note: host functions won't be available outside FrankenTerm, so calls to
`ft_log` etc. will trap. For unit testing, mock the host functions or
test the pure logic separately.

## Project template

A `cargo-generate` template is planned. For now, copy the minimal
extension above and modify it.

## Common patterns

### State between calls

WASM linear memory persists between calls within the same extension
instance. Use `static mut` or `thread_local!` for state:

```rust
static mut CALL_COUNT: u64 = 0;

#[no_mangle]
pub extern "C" fn on_event() {
    unsafe {
        CALL_COUNT += 1;
        ft::log_info(&format!("Call #{}", CALL_COUNT));
    }
}
```

### Allocator

For extensions that use `Vec`, `String`, or other heap types, you need
a global allocator. The default Rust allocator works with WASI:

```rust
// No special setup needed -- the default allocator targets WASI's
// memory.grow when compiling to wasm32-wasip1.
```

For memory-constrained extensions, consider `wee_alloc`:

```toml
[dependencies]
wee_alloc = "0.4"
```

```rust
#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;
```
