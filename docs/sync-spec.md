# Sync Spec (wa sync / asupersync)

## Summary
Define a safe, explicit sync contract for moving wa assets between machines.
The sync feature is plan-first, dry-run by default, and never touches the live
SQLite database. Only approved assets are allowed to move.

## Goals
- Allow operators to sync wa assets between machines safely.
- Make sync behavior explicit, deterministic, and auditable.
- Provide a simple target model for remote destinations.

## Non-goals
- Live database replication.
- Secret propagation (tokens, credentials, private keys).
- Implicit overwrites without confirmation.

## What can be synced (initial scope)
- `wa` binary (optional)
- `~/.config/wa/` (config)
- exported DB snapshots (never live DB files)

## Safety rules
- Default is dry-run with a full plan preview.
- No overwriting unless explicitly enabled and confirmed.
- Denylist always wins over allowlist.
- Never sync live DB files: `ft.db`, `ft.db-wal`, `ft.db-shm`.
- Never sync secrets; redaction must be applied before transfer.

## CLI contract (planned)

### Commands
```
wa sync targets list
wa sync push <target> [--dry-run] [--yes]
wa sync pull <target> [--dry-run] [--yes]
```

### Flags
- `--dry-run`: default; prints plan and exits.
- `--yes`: skip interactive confirmation (only with `--dry-run=false`).
- `--allow-overwrite`: allow replacing existing files.
- `--include <binary|config|snapshots>`: opt-in per payload type.
- `--exclude <glob>`: optional additional excludes.
- `--target-root <path>`: override target root path for this run.

### Output
- Human output by default.
- JSON output when `FT_OUTPUT_FORMAT=json` is set.
- Plan preview includes:
  - target name + endpoint
  - payload list with source/target paths
  - overwrite warnings
  - redaction summary

## Target model
A target is a named remote destination with transport and root path.
Initial transport is SSH (scp/rsync style); other transports can follow.

## Configuration schema

```toml
[sync]
# Feature gate
enabled = false
# Require confirmation for any write
require_confirmation = true
# Default overwrite policy
allow_overwrite = false
# Payload toggles (global defaults)
allow_binary = false
allow_config = true
allow_snapshots = true
# Optional allow/deny path globs
allow_paths = ["~/.config/wa/ft.toml", "~/.config/wa/patterns.toml"]
deny_paths = [
  "~/.local/share/wa/ft.db",
  "~/.local/share/wa/ft.db-wal",
  "~/.local/share/wa/ft.db-shm"
]

[[sync.targets]]
name = "staging"
transport = "ssh"
endpoint = "wa@staging-host"
root = "~/.local/share/wa/sync"
# push or pull
default_direction = "push"
# Optional per-target overrides
allow_binary = true
allow_config = true
allow_snapshots = true
```

## Payload layout (target root)
```
<root>/
  bin/
    wa
  config/
    ft.toml
    patterns.toml
  snapshots/
    wa-export-YYYYMMDD-HHMMSS.jsonl
```

## Snapshots
- Snapshots must be produced by `ft export ...` or backup tooling.
- Sync never reads or writes the live database file.

## Logging and audit
- Each sync run emits a plan summary (human + JSON).
- Audit logs must redact secrets and include the final payload list.

## Testing expectations
- Path allow/deny rules enforced.
- Dry-run output stable and deterministic.
- Overwrite attempts require explicit confirmation.
- Live DB paths are always denied.
