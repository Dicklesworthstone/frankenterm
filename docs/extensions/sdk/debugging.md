# Debugging Extensions

> **Experimental runtime guidance (ft-rxk40).** The logs, audit records, and
> extension states below belong to the scripting library's intended lifecycle.
> Production `ft ext list/info` reports pattern packs, not WASM/Lua runtime
> state; there is no `ft ext enable` or `.ftx` install path. A standalone
> Wasmtime run proves only that isolated module's behavior.

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
| `read access denied: /etc/foo` | Path not in `filesystem` | Add `read:` path prefix to manifest |
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
# List installed pattern packs (not scripting runtime state)
ft ext list

# Show details for one extension
ft ext info my-ext
```

Proposed scripting states, not output of the current pattern-pack commands:

- **Installed**: On disk, waiting to be loaded
- **Loaded**: Active and responding to events
- **Disabled**: Disabled in the experimental lifecycle model
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

Product integration testing is blocked on ft-rxk40: the terminal has no
loader for these packages. A future test must drive a real native event,
observe the WASM/Lua effect, and verify denied permissions and traps preserve
terminal operation. Engine-only tests cannot substitute for that path.

### Wasmtime standalone

Test that your WASM module loads correctly:

```bash
wasmtime run --invoke on_reload main.wasm
```

This command can fail because the custom host imports are absent. Preserve
the actual error; a trap or failed link is not a passing integration test.

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
