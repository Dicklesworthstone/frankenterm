# Ruleset Profiles — Model + Config Schema

**Bead:** bd-2n9q
**Author:** MagentaWaterfall
**Date:** 2026-02-01
**Status:** Draft

## Overview

Ruleset profiles define **named configurations for pattern packs** and per-pack
rule overrides. The goal is deterministic load behavior across environments
(dev, CI, incident) while reusing the existing `PatternsConfig` schema.

## Design Principles

1. **Deterministic**: profile selection yields a single, ordered pack list.
2. **Composable**: profiles can inherit from the base config or another profile.
3. **Minimal**: profile files only describe pattern configuration.
4. **Forward-compatible**: unknown fields ignored.
5. **Default remains implicit**: base `wa.toml` patterns config is the default.

## Storage Layout

```
~/.config/wa/
├── wa.toml                  # Base config (implicit default ruleset)
└── rulesets/
    ├── manifest.json        # Ruleset index + metadata
    ├── incident.toml        # Ruleset profile (pattern overrides)
    └── ...
```

## Profile File Format

Ruleset profiles are **partial** `PatternsConfig` documents with optional
metadata. Only pattern-related fields are allowed.

Example `rulesets/incident.toml`:

```toml
name = "incident"
description = "High-signal incident response"

# Optional: inherit from base (default) or another profile
inherits = "default"

[patterns]
packs = ["builtin:core", "builtin:codex"]
quick_reject_enabled = true

[patterns.pack_overrides."builtin:core"]
disabled_rules = ["core.codex:debug"]

[patterns.pack_overrides."builtin:codex".severity_overrides]
"core.codex:usage_reached" = "critical"
```

### Allowed Fields

Top-level metadata:
- `name` (required, unique, lowercase)
- `description` (optional)
- `inherits` (optional; default `default`)

Patterns section (same schema as `PatternsConfig`):
- `patterns.packs`
- `patterns.pack_overrides`
- `patterns.quick_reject_enabled`

## Manifest Format

`rulesets/manifest.json` provides discovery without scanning the directory.

```json
{
  "version": 1,
  "rulesets": [
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

## Naming Rules

- `name` must match: `[a-z0-9_-]{1,32}`
- `default` is reserved and **not** stored in the manifest.
- Store canonical lowercase names in the manifest.

## Resolution Algorithm

1. Load base `PatternsConfig` from `wa.toml`.
2. If profile name is `default` or missing, use base config only.
3. If profile selected:
   - Load profile file.
   - If `inherits` is set and not `default`, load parent profile first.
4. Apply profile values:
   - If `patterns.packs` is set, **replace** the pack list.
   - Merge `pack_overrides` with profile overrides taking precedence.
   - If `quick_reject_enabled` is set, override base value.
5. Final pack order is the resolved `packs` list.

## Determinism Guarantees

- Pack list order is preserved exactly as defined.
- Overrides are merged by key with stable field ordering.
- Missing fields never introduce implicit defaults beyond base config.

## Validation Rules

- All pack ids must be valid `builtin:<name>` or `file:<path>` strings.
- `inherits` must exist in manifest (unless `default`).
- `pack_overrides` keys must match a pack in the resolved `packs` list.
- `severity_overrides` values must be valid severity strings.

## Success Criteria Mapping

- **Profiles load deterministically**: explicit resolution algorithm.
- **Schema defined**: profile format aligns with existing `PatternsConfig`.
- **Forward compatible**: ignored unknown fields + versioned manifest.
