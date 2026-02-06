# Distributed Security Spec (wa distributed mode)

## Summary
This document defines the security model for distributed mode (agent ↔ aggregator).
The goal is **secure‑by‑default** behavior with explicit configuration, deterministic
tests, and stable error codes.

## Threat Model (explicit)
At minimum, defend against:
- **Accidental exposure**: binding to `0.0.0.0` without realizing it.
- **Unauthorized clients**: unknown agents attempting to connect.
- **Replay/injection**: forged or replayed deltas/events.
- **Traffic sniffing**: plaintext on LAN/WAN.
- **Credential leakage**: secrets in logs/artifacts.

## Security Goals
- No plaintext remote connections by default.
- No silent downgrade from TLS to plaintext.
- Strong identity where enabled (mTLS).
- Deterministic failure modes with stable error codes.
- Logs never include secrets, keys, or tokens.

## Transport Security (TLS / mTLS)
Baseline rules:
- **Loopback only** by default.
- If binding to non‑loopback, **TLS is required** unless an explicit dangerous
  override is set.
- Optional **mTLS**: verify client cert + optional allowlist of identities.

Certificate handling:
- Support operator‑provided cert/key paths.
- Support self‑signed dev certs (explicit, non‑default).
- Never generate or write keys without explicit user intent.

## Authentication / Identity
Supported modes (configurable):
- **Shared token** (baseline).
- **mTLS client identity** (recommended for serious deployments).

Rules:
- Constant‑time comparisons for tokens.
- Log only safe metadata (peer addr, session id, TLS version, client CN/SAN if
  allowlisted).
- No secrets in logs or artifacts.

## Rotation Workflow (Operator)
Recommended rotation steps (safe-by-default):
1. Generate new token / certs offline.
2. Prefer file-based tokens: write the token to `distributed.token_path` and rotate by
   updating the file contents (atomic replace) without embedding secrets in `wa.toml`.
3. Update `distributed.tls.*` paths in `wa.toml` for TLS cert rotation (server restart may be required).
4. Run `wa doctor` to confirm the effective distributed security status.

Notes:
- Do not log or paste secret material into logs or diagnostics.
- Prefer path-based certs/keys; avoid inline PEM in config.

## Replay Protection & Session Semantics
Define a session per connection:
- **session_id** per connection
- **monotonic seq** per session
- Optional session expiration/rotation (time‑based only if deterministic tests
  can control time)

Reject:
- Non‑monotonic seq values
- Duplicate message IDs within a session
- Messages missing required auth context

## Safe Defaults
Defaults must be conservative:
- Bind to `127.0.0.1` unless explicitly configured otherwise.
- TLS required for non‑loopback.
- `allow_insecure = false` unless explicitly set.
- `distributed.enabled = false` unless explicitly set.

If `allow_insecure = true`, emit:
- Prominent warnings in CLI output
- `wa doctor` warning entry
- Structured audit note

## Configuration (proposed schema)
```toml
[distributed]
enabled = false
bind_addr = "127.0.0.1:4141"
allow_insecure = false
require_tls_for_non_loopback = true
auth_mode = "token"          # token | mtls | token+mtls
# Token source (set exactly one when auth_mode includes token):
token_path = "/path/to/token.txt"   # recommended (supports rotation via file update)
# token_env = "WA_DISTRIBUTED_TOKEN" # alternative
# token = "..."                     # inline (discouraged)
allow_agent_ids = ["agent-a", "agent-b"]  # optional allowlist

[distributed.tls]
enabled = true
cert_path = "/path/to/server.crt"
key_path = "/path/to/server.key"
client_ca_path = "/path/to/clients.pem"   # for mTLS
min_tls_version = "1.2"
```

## Error Codes (stable)
Proposed error codes (example names):
- `dist.tls_required` — TLS required for non‑loopback
- `dist.tls_handshake_failed` — TLS handshake failure
- `dist.auth_failed` — token/mTLS auth failed
- `dist.replay_detected` — non‑monotonic seq or duplicate msg
- `dist.insecure_disabled` — rejected due to allow_insecure=false

## Logging & Redaction Rules
- Never log tokens, private keys, or raw cert PEM contents.
- Redact any auth headers or bearer tokens.
- Safe metadata only:
  - peer addr
  - session id (opaque)
  - TLS version/cipher
  - client cert CN/SAN (if allowlisted)

## Deterministic Tests (required)
Testing must be deterministic and offline:
- Valid TLS handshake succeeds (test certs).
- Invalid/expired certs fail.
- Plaintext connection rejected when TLS required.
- Token auth fails with wrong token.
- Replay rejection for duplicate seq.
- No secrets in logs/artifacts.

## Invariants (non‑negotiable)
- No plaintext remote connections by default.
- No silent downgrade from TLS to plaintext.
- Replay protection enforced per session.
- Secrets never logged or exported.

## Implementation Checklist
- Config parsing + validation
- TLS server/client wiring
- Auth/mTLS verification + allowlist
- Replay guard (monotonic seq)
- Structured error codes
- `wa doctor` diagnostics
- Deterministic integration tests
