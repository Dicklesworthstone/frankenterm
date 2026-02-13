# Writing WASM Extensions

WASM extensions compile to `wasm32-wasip1` and run inside a Wasmtime
sandbox. They offer strong isolation, predictable resource usage, and
support for any language that compiles to WASM.

## When to use WASM

- New extensions (recommended default)
- When sandbox isolation is required
- Performance-sensitive hooks (lower per-call overhead than Lua)
- Extensions distributed to untrusted users
- Writing extensions in Rust, Go, C, or other WASM-targeting languages

## Capabilities

| Feature | Available |
|---------|-----------|
| Async execution | No (synchronous calls) |
| Filesystem access | Via permissions |
| Network access | Via permissions |
| Sandboxed | Yes |
| Memory limit | 64 MiB default |
| Execution timeout | 10s default |

## Rust setup

### Cargo.toml

```toml
[package]
name = "my-extension"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

# Keep the binary small
[profile.release]
opt-level = "s"
lto = true
strip = true
```

### Build

```bash
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release
```

The output is at `target/wasm32-wasip1/release/my_extension.wasm`.

## Host function ABI

WASM extensions communicate with FrankenTerm through imported host
functions. Declare them as `extern "C"`:

```rust
extern "C" {
    /// Emit a log message.
    /// level: 0=Trace, 1=Debug, 2=Info, 3=Warn, 4=Error
    fn ft_log(level: i32, msg_ptr: *const u8, msg_len: u32);

    /// Read an environment variable.
    /// Returns value length, or -1 if not set / denied.
    /// Value is written to the return buffer.
    fn ft_get_env(key_ptr: *const u8, key_len: u32) -> i32;

    /// Copy from host return buffer into WASM memory.
    /// Returns bytes actually copied.
    fn ft_return_buffer_read(out_ptr: *mut u8, out_len: u32) -> i32;
}
```

## Exported entry points

Each hook declared in the manifest maps to an exported function:

```toml
[[hooks]]
event = "config.reload"
handler = "on_reload"
```

```rust
#[no_mangle]
pub extern "C" fn on_reload() {
    // Handle config reload
}
```

The function must be `extern "C"`, `#[no_mangle]`, and take no arguments.
Event payload is passed through host function calls (ABI to be stabilized).

## Helper patterns

### Logging wrapper

```rust
fn log(level: i32, msg: &str) {
    unsafe { ft_log(level, msg.as_ptr(), msg.len() as u32) }
}

fn log_info(msg: &str) { log(2, msg) }
fn log_warn(msg: &str) { log(3, msg) }
fn log_error(msg: &str) { log(4, msg) }
```

### Environment variable reader

```rust
fn get_env(key: &str) -> Option<String> {
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
```

## Module caching

Compiled WASM modules are cached in two layers:

1. **Memory**: LRU cache for instant access during the same process
2. **Disk**: SHA-256-keyed `.cwasm` files that survive restarts

First load compiles from `.wasm` source (~100ms for small modules).
Subsequent loads from cache are near-instant.

## Binary size tips

- Use `opt-level = "s"` and `lto = true` in release profile
- Avoid large dependencies (serde adds ~200KB to WASM)
- Use `strip = true` to remove debug symbols
- Consider `wasm-opt` for further size reduction

Typical extension binary: 50-200KB after optimization.

## Debugging

Enable debug logging to see host function calls:

```bash
RUST_LOG=frankenterm_scripting=debug frankenterm
```

The audit trail records all host function calls with timestamps and
outcomes. Access it through the debug console or programmatically.

## Go extensions

Go can target WASM via TinyGo:

```go
package main

//go:wasmimport env ft_log
func ftLog(level int32, msgPtr *byte, msgLen uint32)

func logInfo(msg string) {
    if len(msg) > 0 {
        ftLog(2, &[]byte(msg)[0], uint32(len(msg)))
    }
}

//export on_reload
func onReload() {
    logInfo("Hello from Go!")
}

func main() {}
```

Build with:

```bash
tinygo build -o main.wasm -target=wasip1 .
```
