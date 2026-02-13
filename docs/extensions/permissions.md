# Extension Permissions

FrankenTerm extensions run in a sandboxed environment. Each extension
declares the permissions it needs in `extension.toml`, and the runtime
enforces those boundaries at every host function call.

## Permission model

Permissions follow a **deny-by-default** policy. An extension with no
`[permissions]` section has no access to the filesystem, network,
environment variables, or pane content.

## Declaring permissions

In `extension.toml`:

```toml
[permissions]
network = false
pane_access = false
filesystem_read = ["~/.config/frankenterm/"]
filesystem_write = ["~/.local/share/my-ext/"]
environment = ["TERM", "COLORTERM", "HOME", "XDG_*"]
```

## Permission types

### filesystem_read

List of path prefixes the extension may read. The sandbox checks that
the requested path starts with one of the declared prefixes.

```toml
filesystem_read = [
    "~/.config/frankenterm/",
    "/etc/frankenterm/",
]
```

### filesystem_write

List of path prefixes the extension may write to.

```toml
filesystem_write = [
    "~/.local/share/my-ext/",
]
```

### environment

List of environment variable names or patterns. A trailing `*` matches
any suffix.

```toml
environment = [
    "TERM",           # exact match
    "COLORTERM",      # exact match
    "XDG_*",          # matches XDG_CONFIG_HOME, XDG_DATA_HOME, etc.
]
```

### network

Boolean. If `true`, the extension may make outbound network connections.
Default: `false`.

### pane_access

Boolean. If `true`, the extension may read pane content (terminal output).
Default: `false`. Enable this only for extensions that need to inspect
terminal output (e.g., pattern matchers, loggers).

## Resource limits

WASM extensions also have resource limits enforced by the Wasmtime runtime:

| Limit | Default | Override |
|-------|---------|---------|
| Linear memory | 64 MiB | `[limits] max_memory_bytes` |
| Fuel per call | 1,000,000,000 | `[limits] fuel_per_call` |
| Wall time per call | 10s | `[limits] max_wall_time_secs` |

```toml
[limits]
max_memory_bytes = 33554432    # 32 MiB
fuel_per_call = 500000000      # 500M instructions
max_wall_time_secs = 5
```

## Audit trail

Every host function call is recorded in an in-memory audit trail with:

- Timestamp (relative to extension load)
- Function name
- Argument summary (truncated to 256 bytes)
- Outcome: `Ok`, `Denied(reason)`, or `Error(message)`

Access the audit trail programmatically for debugging:

```rust
let trail = enforcer.audit_trail();
for entry in trail.recent(10) {
    println!("{}: {} -> {:?}", entry.function, entry.args_summary, entry.outcome);
}
```

## Security recommendations

1. **Request minimal permissions.** Only ask for what you need.
2. **Use specific paths.** `filesystem_read = ["/"]` will raise red flags.
3. **Avoid `pane_access`** unless your extension specifically needs
   terminal output (it can contain sensitive data).
4. **Avoid `network = true`** unless your extension needs outbound
   connectivity (e.g., API calls, telemetry).
5. **Declare specific env vars.** Avoid `["*"]` which exposes all
   environment variables including secrets.
