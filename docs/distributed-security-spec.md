# Distributed Security Guide (wa distributed mode)

## Summary
This guide covers operator setup and operations for secure distributed mode
(agent <-> aggregator): safe defaults, TLS/token/mTLS configuration, rotation,
doctor verification, and troubleshooting.

## Scope and Feature Gate
Distributed mode is feature-gated at compile time.

```bash
# Build from source with distributed mode enabled
cargo build -p wa --release --features distributed
```

If `distributed` is not compiled in, distributed runtime behavior is unavailable.

## Security Model
Distributed mode is designed to defend against:
- accidental exposure (`0.0.0.0` without TLS)
- unauthorized clients
- replay/injection attempts
- plaintext sniffing on LAN/WAN
- secret leakage in logs/artifacts

Security invariants:
- default bind is loopback (`127.0.0.1:4141`)
- TLS is required for non-loopback unless you explicitly set `allow_insecure = true`
- token checks are constant-time
- replay detection rejects non-monotonic sequence numbers per session
- logs/artifacts must not contain tokens, private keys, or raw secret payloads

## Config Defaults and Meaning
`[distributed]` defaults are conservative:

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Distributed mode is opt-in |
| `bind_addr` | `127.0.0.1:4141` | Loopback-only by default |
| `allow_insecure` | `false` | Plaintext override is off |
| `require_tls_for_non_loopback` | `true` | Remote bind requires TLS unless dangerous override |
| `auth_mode` | `token` | Shared token auth mode |
| `token` / `token_env` / `token_path` | unset | Exactly one source required when token auth is enabled |
| `allow_agent_ids` | empty | Optional identity allowlist |
| `tls.enabled` | `false` | TLS opt-in (required for non-loopback unless insecure override) |
| `tls.min_tls_version` | `1.2` | Minimum protocol version |

## Minimal Remote-Safe Setup (token + TLS)
1. Create a token file with strict permissions:
```bash
umask 077
openssl rand -hex 32 > ~/.config/wa/distributed.token
```
2. Provision server cert and key (from your CA or dev self-signed certs).
3. Configure `wa.toml`:
```toml
[distributed]
enabled = true
bind_addr = "0.0.0.0:4141"
allow_insecure = false
require_tls_for_non_loopback = true
auth_mode = "token"
token_path = "/home/you/.config/wa/distributed.token"
allow_agent_ids = []

[distributed.tls]
enabled = true
cert_path = "/etc/wa/tls/server.crt"
key_path = "/etc/wa/tls/server.key"
min_tls_version = "1.2"
```
4. Start wa with your normal runtime entrypoint.
5. Verify effective security posture:
```bash
wa doctor
wa doctor --json
```

## mTLS and Mixed Mode (`token+mtls`)
For stronger identity guarantees, enable mTLS:

```toml
[distributed]
auth_mode = "token+mtls"
token_path = "/home/you/.config/wa/distributed.token"
allow_agent_ids = ["agent-a", "agent-b"]

[distributed.tls]
enabled = true
cert_path = "/etc/wa/tls/server.crt"
key_path = "/etc/wa/tls/server.key"
client_ca_path = "/etc/wa/tls/clients-ca.pem"
min_tls_version = "1.2"
```

Validation rules enforced by config checks:
- mTLS modes require `distributed.tls.enabled = true`
- mTLS modes require `distributed.tls.client_ca_path`
- token auth requires exactly one token source (`token`, `token_env`, or `token_path`)

## Rotation Runbook
Token rotation (recommended via `token_path`):
1. Generate new token.
2. Write to a temp file with strict permissions.
3. Atomically replace the old token file (`mv` temp file into place).
4. Restart wa distributed services if your deployment caches credentials.
5. Run `wa doctor` and a client smoke test.

Certificate rotation:
1. Stage new cert/key files.
2. Update `distributed.tls.cert_path`/`key_path` (or rotate symlink targets).
3. Restart services to pick up new TLS material.
4. Re-run `wa doctor` and connection checks.

## Troubleshooting
Start with:
```bash
wa doctor
wa doctor --json
```

Runtime distributed security codes currently emitted:

| Code | Meaning | Typical Action |
|---|---|---|
| `dist.auth_failed` | Missing/wrong token or identity mismatch | Verify token source and presented credentials |
| `dist.replay_detected` | Duplicate or non-monotonic sequence | Check client session/sequence logic |
| `dist.session_limit` | Too many tracked sessions | Increase limits or reduce stale sessions |
| `dist.connection_limit` | Too many active connections | Increase connection budget or reduce fanout |
| `dist.message_too_large` | Message exceeds limit | Lower payload size/chunk data |
| `dist.rate_limited` | Sender exceeded rate policy | Backoff and retry with pacing |
| `dist.handshake_timeout` | Handshake did not complete in time | Check latency/network/TLS mismatch |
| `dist.message_timeout` | Message read timed out | Check sender behavior and timeouts |

Common config errors (from validation/doctor detail):
- `distributed.tls.enabled must be true for non-loopback binds ...`
- `distributed token source is ambiguous: set exactly one of ...`
- `distributed.tls.cert_path must be set when TLS is enabled`
- `distributed.tls.key_path must be set when TLS is enabled`
- `distributed.tls.client_ca_path must be set when auth_mode includes mtls`

## Logs and Artifacts
- Keep `general.log_file` configured for persistent operational logs.
- For E2E debugging, keep test artifacts (`./e2e-artifacts/<timestamp>/...`) and
  include manifest + structured logs per `docs/test-logging-contract.md`.
- Do not store raw secrets in logs/artifacts.
