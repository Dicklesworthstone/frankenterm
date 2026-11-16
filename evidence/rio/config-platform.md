# Rio Configuration + Platform-Layer Analysis

Bead: `ft-34sko.5`  
Scope: `legacy_rio/rio`

## 1. Config Schema, Defaults, and Hierarchy

Rio centralizes runtime options in `Config` (window, renderer, navigation, keyboard, shell, env, themes, hints, bell, fonts, etc.) (`legacy_rio/rio/rio-backend/src/config/mod.rs:84`).  
Defaults are materialized in `impl Default for Config` (`legacy_rio/rio/rio-backend/src/config/mod.rs:593`).

### Config location resolution

- `RIO_CONFIG_HOME` has highest precedence for config root on all platforms (`legacy_rio/rio/rio-backend/src/config/mod.rs:170`, `legacy_rio/rio/rio-backend/src/config/mod.rs:178`, `legacy_rio/rio/rio-backend/src/config/mod.rs:192`).
- Linux additionally consults `XDG_CONFIG_HOME` before falling back to `~/.config/rio` (`legacy_rio/rio/rio-backend/src/config/mod.rs:196`).
- Final config file path is `<config_dir>/config.toml` (`legacy_rio/rio/rio-backend/src/config/mod.rs:204`).

### Load/fallback behavior

- `Config::try_load` returns typed errors: `ErrLoadingConfig`, `ErrLoadingTheme`, `PathNotFound` (`legacy_rio/rio/rio-backend/src/config/mod.rs:36`, `legacy_rio/rio/rio-backend/src/config/mod.rs:454`).
- `rioterm` startup falls back to `Config::default()` on load error, preserving the error for later reporting (`legacy_rio/rio/frontends/rioterm/src/main.rs:155`).

### Effective precedence order at runtime

Observed from startup flow in `rioterm`:

1. Default struct values (`legacy_rio/rio/rio-backend/src/config/mod.rs:593`)
2. File values from `Config::try_load` (`legacy_rio/rio/frontends/rioterm/src/main.rs:155`)
3. OS-specific platform overlay via `overwrite_based_on_platform` (`legacy_rio/rio/frontends/rioterm/src/main.rs:171`)
4. CLI overrides (shell command, working directory, title placeholder) (`legacy_rio/rio/frontends/rioterm/src/main.rs:182`, `legacy_rio/rio/frontends/rioterm/src/main.rs:187`, `legacy_rio/rio/frontends/rioterm/src/main.rs:220`)
5. Env override of log level via `RIO_LOG_LEVEL` (`legacy_rio/rio/frontends/rioterm/src/main.rs:41`, `legacy_rio/rio/frontends/rioterm/src/main.rs:93`)
6. Environment export from resolved `config.env_vars` (`legacy_rio/rio/frontends/rioterm/src/main.rs:78`, `legacy_rio/rio/frontends/rioterm/src/main.rs:232`)

## 2. Platform Override Semantics

Rio models platform blocks as:

- `Platform { linux?, windows?, macos? }` (`legacy_rio/rio/rio-backend/src/config/platform.rs:8`)
- `PlatformConfig` with optional fields (`shell`, `navigation`, `window`, `renderer`, `env_vars`, `theme`) (`legacy_rio/rio/rio-backend/src/config/platform.rs:20`)

Merge rules in `overwrite_with_platform_config`:

- Apply only current OS branch (`legacy_rio/rio/rio-backend/src/config/mod.rs:458`)
- Field-level merge for nested structs (`window`, `navigation`, `renderer`) (`legacy_rio/rio/rio-backend/src/config/mod.rs:475`)
- Replace semantics for `shell` and `theme` (`legacy_rio/rio/rio-backend/src/config/mod.rs:478`, `legacy_rio/rio/rio-backend/src/config/mod.rs:588`)
- Append semantics for `env_vars` (`legacy_rio/rio/rio-backend/src/config/mod.rs:583`)

Rio already unit-tests these behaviors (env merge, field-level merge, shell replacement, non-interference across OS blocks) (`legacy_rio/rio/rio-backend/src/config/mod.rs:1246`, `legacy_rio/rio/rio-backend/src/config/mod.rs:1327`, `legacy_rio/rio/rio-backend/src/config/mod.rs:1500`).

## 3. Platform Abstraction Boundaries (`rio-window`)

### Boundary 1: Frontend orchestration boundary

- `rioterm` constructs `EventLoop<EventPayload>`, builds `Application`, and runs `run_app` (`legacy_rio/rio/frontends/rioterm/src/main.rs:235`, `legacy_rio/rio/frontends/rioterm/src/main.rs:239`, `legacy_rio/rio/frontends/rioterm/src/application.rs:174`).
- `Application` consumes platform events through `ApplicationHandler<EventPayload>` (`legacy_rio/rio/frontends/rioterm/src/application.rs:179`).

### Boundary 2: Backend-to-window event bridge

- `rio-backend` owns event domain types (`RioEvent`, `EventPayload`) and bridges via `EventProxy(EventLoopProxy<EventPayload>)` (`legacy_rio/rio/rio-backend/src/event/mod.rs:290`, `legacy_rio/rio/rio-backend/src/event/mod.rs:291`, `legacy_rio/rio/rio-backend/src/event/mod.rs:304`).

### Boundary 3: Compile-time platform partitioning

- `rio-window` exposes platform extension modules with cfg gates (`legacy_rio/rio/rio-window/src/platform/mod.rs:6`, `legacy_rio/rio/rio-window/src/platform/mod.rs:12`, `legacy_rio/rio/rio-window/src/platform/mod.rs:16`, `legacy_rio/rio/rio-window/src/platform/mod.rs:18`).
- `platform_impl` selects one backend module by target and hard-fails unsupported targets at compile-time (`legacy_rio/rio/rio-window/src/platform_impl/mod.rs:6`, `legacy_rio/rio/rio-window/src/platform_impl/mod.rs:66`).
- Feature documentation confirms default Unix backends (`x11`, `wayland`) (`legacy_rio/rio/rio-window/src/lib.rs:142`, `legacy_rio/rio/rio-window/src/lib.rs:143`).

Interpretation for FrankenTerm: config semantics belong in core config layer; window/event portability belongs in adapter layer; event payload translation is the seam between them.

## 4. FrankenTerm Improvements (With Migration + Test Requirements)

### Improvement 1: Deterministic effective-config resolver with provenance

Proposal:
- Add a resolver that materializes one effective config plus source metadata per field (`default | file | platform | cli | env`), mirroring Rio’s ordering while making it inspectable.

Migration notes:
- Introduce `ResolvedConfig` + `ResolvedField` metadata in `crates/frankenterm-core/src/config.rs`.
- Implement `ft config resolve --format json|toon`.
- Route existing startup paths to resolver output instead of ad-hoc precedence logic.

Required downstream tests:
- Unit: precedence ordering, source tagging, and hash stability for identical inputs.
- Unit: fallback behavior when config file missing/invalid (`PathNotFound` / parse error equivalent).
- Integration: startup resolution across fixture matrix for Linux/macOS/Windows platform blocks.
- E2E: run full matrix script (defined in Section 6) with deterministic fixtures and compare effective outputs.

### Improvement 2: Explicit env-var merge/conflict policy with validation

Proposal:
- Replace silent append/override ambiguity with explicit policy (`error`, `last-writer-wins`, or `first-writer-wins`) configured per mode.

Migration notes:
- Add parser/validator for `KEY=VALUE` format and duplicate-key detection.
- Add strict-mode behavior: invalid or duplicate sensitive keys fail startup.
- Preserve non-strict behavior via warning + deterministic resolution.

Required downstream tests:
- Unit: valid/invalid `KEY=VALUE`, duplicate-key detection, deterministic policy outcomes.
- Unit: redaction tagging for sensitive keys (used by logging contract).
- Integration: env injection into runtime process context across platform overlays.
- E2E: cross-platform config permutations with global+platform+CLI/env conflicts.

### Improvement 3: Strict validation + doctor report with controlled fallback

Proposal:
- Add `--strict-config` and `ft config doctor` so fallback-to-default is opt-in, explicit, and auditable.

Migration notes:
- Keep non-strict startup compatibility for current workflows.
- Add strict mode that aborts on unknown keys, invalid enums, invalid platform-only fields, or theme resolution failures.
- Emit machine-readable diagnostics for operators and CI.

Required downstream tests:
- Unit: schema validation for unknown fields and enum/value bounds.
- Unit: strict-vs-nonstrict decision logic and fallback path selection.
- Integration: invalid fixture boot behavior (strict fails, non-strict degrades with diagnostics).
- E2E: matrix scenarios with broken theme paths, malformed TOML, unsupported platform fields.

### Improvement 4: Platform capability surface over compile-time adapters

Proposal:
- Keep compile-time adapter split (Rio model) but expose runtime `PlatformCapabilities` so higher layers avoid scattered `cfg` branching.

Migration notes:
- Add capability descriptor emitted by platform layer at startup (`supports_tabs`, `supports_wayland_fractional_scale`, `supports_transparency`, etc.).
- Gate config application against capabilities with explicit decision reasons.

Required downstream tests:
- Unit: capability descriptor completeness and stable serialization.
- Unit: mapping of config knobs to capability checks.
- Integration: backend feature-set permutations (x11/wayland/windows/macos) choose expected behavior.
- E2E: config matrix verifies unsupported options degrade predictably with structured diagnostics.

## 5. Required Logging + Redaction Contract (For Downstream Config E2E)

All downstream config e2e scripts must emit structured JSONL rows containing:

- `config_source`
- `override_path`
- `effective_value_hash`
- `decision_reason`
- `redacted_fields`

Redaction requirements:

- Never log raw values for fields classified sensitive (`*.token`, `*.password`, `*.secret`, auth headers, private key material, env keys matching `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, `*_KEY`).
- `effective_value_hash` must be deterministic per value and algorithm-versioned (for reproducible diffing).
- `redacted_fields` must list every redacted config path for that decision row.
- `decision_reason` must explicitly identify why an override won (precedence tier, strict-mode fallback, capability gate, or validation failure).

## 6. Mandatory Downstream E2E Contract

Required script and layout:

- Script: `tests/e2e/rio/test_config_platform_matrix.sh`
- Fixtures: `fixtures/rio/config_platform`
- Artifacts: `e2e-artifacts/rio/config_platform/<run_id>.jsonl`

Mandatory JSONL fields per row:

- `run_id`
- `scenario_id`
- `platform`
- `config_source`
- `override_path`
- `decision_reason`
- `redacted_fields`
- `outcome`
- `error_code`

Minimum scenario matrix in `tests/e2e/rio/test_config_platform_matrix.sh`:

1. Baseline defaults (no config file)
2. Valid config file only
3. Platform override modifies subset fields (field-level merge)
4. Shell/theme replacement behavior
5. Env-var duplicate/conflict policy
6. Strict validation failure path
7. Non-strict fallback path
8. Capability-gated unsupported option path

Example artifact row:

```json
{"run_id":"rio-config-20260218-001","scenario_id":"platform-window-merge","platform":"macos","config_source":"platform.macos.window.mode","override_path":"window.mode","decision_reason":"platform override applied after file parse; field-level merge preserved width/height","redacted_fields":[],"outcome":"ok","error_code":null}
```

## 7. Cross-References

- `ft-vv3h` (naming/config consistency)
- `ft-1u90p.7` (testing/operational readiness)
