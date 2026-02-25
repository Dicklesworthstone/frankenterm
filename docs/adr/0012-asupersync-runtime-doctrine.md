# ADR-0012: asupersync Runtime Doctrine and Invariants Contract

**Date:** 2026-02-25  
**Status:** Accepted  
**Bead:** `ft-e34d9.10.1.2`

## Context

FrankenTerm is migrating from mixed runtime behavior to an asupersync-first model.
Without a single doctrine contract, migration slices can drift in semantics:
orphan task lifetimes, cancellation-loss windows, ambiguous shutdown behavior, and
inconsistent user-visible error surfaces.

This ADR defines the authoritative runtime doctrine used by all downstream
`ft-e34d9.*` beads.

## Decision

Adopt runtime doctrine contract version `1.0.0` with machine-readable companion
file `docs/asupersync-runtime-invariants.json`.

### Invariants (normative)

1. `INV-001` Scope ownership is explicit.
   - Long-lived loops must be owned by named scopes.
   - Detached task lifetimes are forbidden unless explicitly audited.
2. `INV-002` Capability context propagation is explicit.
   - Runtime-effectful paths must propagate `Cx` (or an equivalent wrapper).
   - `Cx::for_testing()` is test-only.
3. `INV-003` Outcome semantics remain distinguishable.
   - Success, error, cancellation, and panic pathways must remain separable.
4. `INV-004` Cancellation boundaries are intentional.
   - Checkpoints should precede irreversible effects.
   - Multi-step send/write flows must avoid cancellation-loss windows.
5. `INV-005` Runtime API access is centralized.
   - Prefer `runtime_compat`/`cx` boundary surfaces over ad hoc mixed runtime calls.

### Legacy-to-target mapping and user-visible behavior

| Legacy | Target | Required behavior | User-visible implications |
|---|---|---|---|
| `tokio::spawn` | scope-owned spawn / `cx::spawn_with_cx` | no orphan tasks | shutdown traces identify owners instead of silent background exits |
| `tokio::select!` | explicit race/select with cancellation handling | no dropped critical messages | cancellation outcomes are deterministic and reason-coded |
| `tokio::time::*` | `runtime_compat::*` then Cx-aware adapters | deterministic timeout semantics | timeout failures include stable reason codes + remediation hints |
| lossy channel/send patterns | reserve/commit or equivalent guarded sequencing | no command/event silent loss | cancellation/loss is explicit in logs and operator messages |
| ambient runtime startup | unified runtime bootstrap (`CxRuntimeBuilder` policy) | single policy surface per role | startup/shutdown messaging is consistent across CLI/watch/robot/web |

### Anti-patterns (must reject)

1. New direct `tokio::*` usage in doctrine-migrated modules.
2. Mixed direct `asupersync::*` + `runtime_compat::*` without explicit boundary rationale.
3. `Cx::for_testing()` in production paths.
4. Detached spawn without ownership/cancellation narrative.
5. Mapping cancellation/panic into generic errors that erase reason visibility.

### User-facing guarantees (normative)

1. `UG-001`: No silent command/event loss.
2. `UG-002`: Deterministic shutdown messaging (reason-coded and stage-aware).
3. `UG-003`: Actionable failures (reason code + remediation hint + evidence ref).

## Validation Contract

### Unit-level contract checks

- Validate doctrine pack structure and invariant IDs from
  `docs/asupersync-runtime-invariants.json`.
- Validate mapping and guarantee coverage (`spawn`, `select`, `timeout`,
  channel delivery, shutdown).

### Integration-level contract checks

- Validate doctrine references are wired into baseline architecture docs.
- Validate inventory artifact still tracks representative runtime surfaces used
  by migration planning (`runtime_compat`, CLI main runtime, IPC boundary files).

### E2E contract checks

Run:

```bash
bash tests/e2e/test_asupersync_runtime_doctrine.sh
```

The script must include:

1. structured logs (`timestamp`, `component`, `scenario_id`, `correlation_id`,
   `decision_path`, `input_summary`, `outcome`, `reason_code`, `error_code`, `artifact_path`)
2. failure injection (invalid doctrine pack should fail validation)
3. recovery validation (canonical doctrine pack passes)

### Compute policy

Heavy compile/test/clippy/benchmark workloads for runtime migration must run via:

```bash
rch exec -- <command>
```

## Consequences

1. Migration PRs must cite doctrine IDs (`INV-*`, `UG-*`) and risk reductions.
2. Temporary doctrine violations require explicit exception notes and expiry.
3. Doctrine updates must keep both human-readable ADR and machine-readable
   invariants pack in sync.

## References

- `docs/asupersync-migration-baseline.md`
- `docs/asupersync-runtime-invariants.json`
- `docs/asupersync-runtime-inventory.json`
- `docs/asupersync-migration-playbook.md`
