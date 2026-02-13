# Debugging Extensions

## Log output

Extensions emit logs through the `ft_log` host function (WASM) or
`wezterm.log_info/warn/error` (Lua). View logs by starting FrankenTerm
with debug logging:

```bash
RUST_LOG=frankenterm_scripting=debug frankenterm
```

Log levels:

| Level | Value | Function |
|-------|-------|----------|
| Trace | 0 | Verbose internal state |
| Debug | 1 | Development details |
| Info | 2 | Normal operation |
| Warn | 3 | Potential issues |
| Error | 4 | Failures |

## Audit trail

Every host function call made by a WASM extension is recorded in the
audit trail. Each entry contains:

- **elapsed**: Time since extension load
- **extension_id**: Which extension made the call
- **function**: Host function name (e.g., `ft_get_env`)
- **args_summary**: Argument summary (truncated to 256 bytes)
- **outcome**: `Ok`, `Denied(reason)`, or `Error(message)`

To view the audit trail programmatically:

```rust
let trail = enforcer.audit_trail();
for entry in trail.recent(20) {
    eprintln!(
        "[{:?}] {}.{}: {} -> {:?}",
        entry.elapsed,
        entry.extension_id,
        entry.function,
        entry.args_summary,
        entry.outcome,
    );
}
```

## Permission denials

When an extension tries to access a resource it doesn't have permission
for, the call fails with `Denied` and the reason is recorded in the
audit trail. Common causes:

| Error | Cause | Fix |
|-------|-------|-----|
| `read access denied: /etc/foo` | Path not in `filesystem_read` | Add path prefix to manifest |
| `env var denied: SECRET_KEY` | Var not in `environment` list | Add var name or pattern |
| `network access denied` | `network = false` | Set `network = true` |
| `pane access denied` | `pane_access = false` | Set `pane_access = true` |

## WASM traps

If a WASM extension traps (panics, runs out of fuel, exceeds memory),
the extension transitions to the `Error` state with the trap message.

Common traps:

| Trap | Cause | Fix |
|------|-------|-----|
| `out of fuel` | Exceeded fuel budget | Increase `fuel_per_call` or optimize code |
| `memory.grow failed` | Exceeded memory limit | Increase `max_memory_bytes` or reduce allocations |
| `unreachable` | Rust panic in WASM | Fix the panic in your extension code |
| `call stack exhausted` | Deep recursion | Reduce recursion depth |

## Extension state

Check extension state via the CLI:

```bash
# List all extensions with state
frankenterm extension list

# Show details for one extension
frankenterm extension show my-ext
```

States:

- **Installed**: On disk, waiting to be loaded
- **Loaded**: Active and responding to events
- **Disabled**: User disabled; re-enable with `frankenterm extension enable`
- **Error(msg)**: Failed to load; check the error message

## Testing WASM extensions

### Unit tests (native)

Test pure logic in native Rust tests. Mock host functions:

```rust
#[cfg(test)]
mod tests {
    // Test logic that doesn't call host functions
    #[test]
    fn test_parse_config() {
        let result = parse_my_config("key=value");
        assert_eq!(result, ("key", "value"));
    }
}
```

Run with `cargo test` (not `--target wasm32-wasip1`).

### Integration tests

For full integration testing, install the extension in a test
FrankenTerm instance:

```bash
frankenterm extension install my-ext.ftx
RUST_LOG=debug frankenterm
# Trigger the event your extension handles
# Check logs for expected output
```

### Wasmtime standalone

Test that your WASM module loads correctly:

```bash
wasmtime run --invoke on_reload main.wasm 2>&1 || echo "Expected: host function trap"
```

This will trap on host function calls (they don't exist outside
FrankenTerm), but confirms the module structure is valid.

## Performance profiling

Include criterion benchmarks in your extension test suite:

```toml
[dev-dependencies]
criterion = "0.5"

[[bench]]
name = "handler_bench"
harness = false
```

Target budgets:

| Metric | Budget |
|--------|--------|
| Hook handler execution | < 1ms |
| Extension cold load | < 500ms |
| Extension warm load | < 10ms |
| Memory footprint | < 64 MiB |
