# Config Profiles — Format + Storage Layout

**Bead:** bd-c5xl
**Author:** MagentaWaterfall
**Date:** 2026-02-01
**Status:** Draft

## Overview

This document defines how configuration profiles are stored, discovered, and
referenced. The intent is to support fast listing without scanning the entire
config directory while keeping profiles forward-compatible and safe to update.

## Design Principles

1. **Default remains implicit**: the base `wa.toml` config is always the default profile.
2. **Profiles are overlays**: a profile contains only overrides to the base config.
3. **Discoverable without full scan**: a single manifest indexes profiles.
4. **Forward-compatible metadata**: unknown fields are ignored.
5. **Safe writes**: profile updates are atomic and preserve permissions.

## Directory Layout

```
~/.config/wa/
├── wa.toml                 # Base config (implicit "default" profile)
└── profiles/
    ├── manifest.json       # Profile index + metadata (single-file listing)
    ├── local-dev.toml      # Profile override (same schema as wa.toml)
    ├── incident.toml
    └── ...
```

Notes:
- `profiles/manifest.json` is the primary discovery mechanism.
- If `manifest.json` is missing or corrupt, callers MAY scan `profiles/*.toml`
  as a fallback and rebuild the manifest.

## Profile File Format

Each profile file is a partial `wa.toml` that overrides the base configuration.
The schema is identical to `wa.toml`, with all fields optional.

Example `profiles/incident.toml`:

```toml
[general]
log_level = "debug"

[notifications]
enabled = true
min_severity = "warning"
```

## Manifest Format

`profiles/manifest.json` contains a list of profiles and their metadata.
The manifest is the only required file for discovery; profile files must exist
at the paths declared in the manifest.

```json
{
  "version": 1,
  "profiles": [
    {
      "name": "incident",
      "path": "incident.toml",
      "description": "High-signal incident response",
      "created_at": 1700000000000,
      "updated_at": 1700001000000,
      "last_applied_at": 1700002000000
    }
  ]
}
```

### Field Definitions

| Field | Type | Notes |
| --- | --- | --- |
| `version` | integer | Manifest version for migrations (start at 1) |
| `profiles` | array | List of profile metadata entries |
| `name` | string | Profile name (unique, case-insensitive) |
| `path` | string | Relative path within `profiles/` dir |
| `description` | string | Optional description for UI/CLI listing |
| `created_at` | integer | Epoch ms |
| `updated_at` | integer | Epoch ms |
| `last_applied_at` | integer | Epoch ms; optional |

## Naming Rules

- `name` must match: `[a-z0-9_-]{1,32}`
- `default` is reserved and **not** stored in the manifest.
- Names are case-insensitive; store canonical lowercase in the manifest.

## Discovery Rules

1. Read `profiles/manifest.json`.
2. If valid, list profiles from the manifest.
3. If missing/invalid, scan `profiles/*.toml` and rebuild the manifest.
4. Always include the implicit `default` profile in listing output.

## Safety + Atomic Updates

- Writes must be atomic: write to `manifest.json.tmp`, fsync, then rename.
- Profile files follow the same atomic write strategy.
- Preserve directory permissions consistent with existing config directory rules.

## Compatibility Notes

- Profile TOML files may include unknown fields; these must be ignored.
- Manifest entries may include extra fields in the future; ignore unknown keys.
- Tooling should treat missing `description`/`last_applied_at` as empty.

## Success Criteria Mapping

- **Discovery without scanning config dir**: manifest provides O(1) listing.
- **Forward-compatible format**: versioned manifest + ignored unknowns.
- **Implicit default**: no file required for the default profile.
