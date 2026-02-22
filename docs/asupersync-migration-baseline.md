# asupersync Migration Baseline (ft-e34d9.10.1)

Status: in progress  
Last updated: 2026-02-22  
Owners: RedRiver (current), asupersync migration swarm

This document is the canonical baseline for the asupersync migration program. It defines:

1. inventory truth (what runtime surfaces exist today),
2. doctrine (what "correct" migration behavior means), and
3. risk and sequencing controls (how downstream beads should execute safely).

## Artifact Set

- Machine-readable inventory: `docs/asupersync-runtime-inventory.json`
- Migration playbook: `docs/asupersync-migration-playbook.md`
- Architecture anchor: `docs/architecture.md` (asupersync baseline section)
- Inventory generator: `scripts/generate_asupersync_runtime_inventory.sh`
- Inventory e2e validator: `tests/e2e/test_asupersync_runtime_inventory.sh`

## Inventory Snapshot (2026-02-22)

Source: `docs/asupersync-runtime-inventory.json`.
The inventory now includes `migration_classification` entries with owner module,
criticality, migration difficulty, and recommended target primitive.

### Global reference counts

| Symbol family | Reference count | Files |
|---|---:|---:|
| `tokio::` | 1234 | 73 |
| `asupersync::` | 253 | 31 |
| `runtime_compat::` | 477 | 72 |
| `smol::` | 69 | 12 |
| `async_std::` | 0 | 0 |

### Highest concentration files

| File | Runtime refs |
|---|---:|
| `crates/frankenterm-core/src/runtime_compat.rs` | 215 |
| `crates/frankenterm/src/main.rs` | 170 |
| `crates/frankenterm-core/src/storage.rs` | 86 |
| `crates/frankenterm-core/src/workflows.rs` | 82 |
| `crates/frankenterm-core/src/pool.rs` | 65 |

### Critical observations

1. `crates/frankenterm/src/main.rs` has high asupersync density and currently fails compilation under mixed runtime assumptions.
2. `crates/frankenterm-core/src/runtime_compat.rs` is the highest-leverage normalization boundary and must remain the primary migration choke point.
3. Vendored crates retain smol-heavy surfaces (`frankenterm/ssh`, `frankenterm/codec`, `frankenterm/config`, `frankenterm/pty`) and represent late-stage migration risk.

## Runtime Doctrine (Version 0.1)

### Invariants

1. Scope ownership is explicit.
   - Long-lived loops (watcher, web listener, workflow dispatcher, ingest tailers) must have named scope ownership.
   - Detached/orphan task lifetimes are forbidden unless explicitly audited as fire-and-forget and side-effect free.
2. Capability context (`Cx`) propagation is explicit.
   - Runtime-effectful functions must accept `&Cx` (or a wrapper carrying `Cx`) rather than relying on ambient runtime state.
   - `Cx::for_testing()` is test-only and must not appear in non-test call paths.
3. Outcome semantics are preserved.
   - Migration must preserve the ability to distinguish success, error, cancellation, and panic pathways even when current surfaces still expose `Result`.
4. Cancellation boundaries are intentional.
   - Cancellation checkpoints must occur before irreversible side effects where practical.
   - Multi-step send/write flows must avoid cancellation-loss windows (reserve/commit or equivalent guarded sequencing).
5. Runtime API access is centralized.
   - Non-boundary modules should use `runtime_compat`/`cx` adapters, not direct mixed runtime calls.

### Canonical mapping (old -> target)

| Legacy pattern | Migration target | Required behavior |
|---|---|---|
| `tokio::spawn(...)` | `cx::spawn_with_cx(...)` or scope-owned spawn | no orphan tasks; explicit owner |
| `tokio::time::sleep/timeout` | `runtime_compat::sleep/timeout` (then Cx-aware adapters) | deterministic timeout handling and structured errors |
| `tokio::sync::*` primitives | `runtime_compat` wrappers first, then native asupersync semantics | parity tests before cutover |
| `tokio::select!` race patterns | asupersync race/select with explicit cancellation handling | no dropped critical messages |
| ambient runtime builder usage | `cx::CxRuntimeBuilder` / unified runtime bootstrap | single policy surface per process role |

### Anti-patterns (reject in review)

1. Direct `tokio::*` usage introduced in files already migrated to doctrine-compliant surfaces.
2. Mixed direct `asupersync::*` and `runtime_compat::*` calls in the same module without an explicit boundary reason.
3. `Cx::for_testing()` in production code.
4. New detached task spawns without ownership/cancellation narrative.
5. Converting cancellation/panic states into generic errors without preserving reason in logs/metrics.

## Risk Ledger

Scoring: Probability (P) and Impact (I) are 1-5. Score = `P * I`.

| Risk ID | Hazard | P | I | Score | Mitigation | Owner |
|---|---|---:|---:|---:|---|---|
| R1 | Mixed runtime APIs in critical modules produce type and lock mismatches | 5 | 5 | 25 | enforce boundary-only runtime usage; fail CI on new direct mixed calls | core runtime track |
| R2 | `Cx` propagation gaps cause hidden ambient behavior | 4 | 5 | 20 | call-graph pass requiring explicit `Cx` on runtime-effectful paths | substrate track |
| R3 | Cancellation-loss during channel/send/flush edges | 4 | 5 | 20 | two-phase send/write patterns + cancellation-focused tests | concurrency track |
| R4 | Orphaned tasks outlive shutdown boundaries | 4 | 4 | 16 | scope ownership map + teardown assertions | concurrency track |
| R5 | `crates/frankenterm/src/main.rs` compile drift blocks validation throughput | 5 | 4 | 20 | dedicated compile-fix sub-lane and short-lived merge windows | integration swarm |
| R6 | Vendored smol surfaces delay full runtime convergence | 3 | 5 | 15 | isolate vendored harmonization wave with module-level adapters | vendored track |
| R7 | Feature-gate permutations hide regressions | 3 | 4 | 12 | matrix checks for default + `asupersync-runtime` + distributed/mcp combos | QA track |

## Sequencing Scorecard

The migration must progress by gates, not by isolated file churn.

| Stage | Goal | Entry gate | Exit evidence |
|---|---|---|---|
| S0 Baseline | inventory + doctrine + risk controls | this document + inventory artifact present | bead links and architecture anchor committed |
| S1 Boundary hardening | stabilize `runtime_compat` and `cx` surfaces | S0 complete | runtime boundary tests green; no new anti-patterns |
| S2 Substrate propagation | thread `Cx`/Outcome expectations through core call graphs | S1 complete | core modules compile cleanly in selected feature sets |
| S3 Structured concurrency | own all spawn trees and cancellation behavior | S2 complete | shutdown/cancellation integration tests pass |
| S4 IO + vendored harmonization | resolve network/signal/vendored runtime divergence | S3 complete | vendored and IPC targets pass scoped validation |
| S5 Cutover + cleanup | remove legacy runtime scaffolding | S4 complete | tokio surface retirement plan executed with proof |

## Validation and Reporting Contract

This baseline intentionally records both current evidence and required automation for follow-up beads.

### Required artifact outputs

1. `docs/asupersync-runtime-inventory.json` (machine-readable inventory snapshot
   including `migration_classification` guidance fields).
2. Structured migration log lines with fields:
   - `event`
   - `bead_id`
   - `module`
   - `risk_id`
   - `decision`
   - `evidence_ref`
3. Scoreboard updates tied to bead IDs (`ft-e34d9.*`) and status transitions.

### Minimum validation cadence

1. Unit-level: runtime boundary wrappers (`runtime_compat`, `cx`) for parity and cancellation-sensitive behavior.
2. Integration-level: representative module migrations compile and test under active feature sets.
3. E2E-level: reproducible inventory/report generation with stable JSON output suitable for diffing.

### Repro commands

```bash
# Regenerate machine-readable runtime inventory
bash scripts/generate_asupersync_runtime_inventory.sh

# Validate deterministic generation + docs drift + key assertions
bash tests/e2e/test_asupersync_runtime_inventory.sh
```

### Current blocker note

As of 2026-02-22, validation breadth is constrained by existing compile failures in `crates/frankenterm/src/main.rs` under mixed runtime migration paths. This blocker is tracked and should be treated as a prerequisite for broad-stage validation closure.

## Downstream Bead Guidance

Every migration PR or bead completion comment should cite:

1. the doctrine invariant(s) it satisfies,
2. the risk ID(s) it reduces, and
3. the stage gate evidence it contributes.

If an implementation needs to violate doctrine temporarily, record the exception explicitly with expiration criteria.
