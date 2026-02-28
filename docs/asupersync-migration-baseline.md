# asupersync Migration Baseline (ft-e34d9.10.1)

Status: in progress  
Last updated: 2026-02-28  
Owners: SageHawk (current), asupersync migration swarm

This document is the canonical baseline for the asupersync migration program. It defines:

1. inventory truth (what runtime surfaces exist today),
2. doctrine (what "correct" migration behavior means), and
3. risk and sequencing controls (how downstream beads should execute safely).

## Artifact Set

- Machine-readable inventory: `docs/asupersync-runtime-inventory.json`
- Doctrine ADR (normative): `docs/adr/0012-asupersync-runtime-doctrine.md`
- Machine-readable doctrine/invariants pack: `docs/asupersync-runtime-invariants.json`
- Versioned doctrine contract pack: `docs/asupersync-runtime-doctrine-v1.json`
- Migration playbook: `docs/asupersync-migration-playbook.md`
- Architecture anchor: `docs/architecture.md` (asupersync baseline section)
- Inventory generator: `scripts/generate_asupersync_runtime_inventory.sh`
- Inventory e2e validator: `tests/e2e/test_asupersync_runtime_inventory.sh`
- Scoreboard generator: `scripts/generate_asupersync_migration_scoreboard.sh`
- Scoreboard artifacts: `docs/asupersync-migration-scoreboard.json`, `docs/asupersync-migration-scoreboard.md`
- Scoreboard e2e validator: `tests/e2e/test_asupersync_migration_scoreboard.sh`
- rch execution policy (normative): `docs/asupersync-rch-execution-policy.md`
- rch evidence schema: `docs/asupersync-rch-evidence-schema.json`
- rch policy validator: `scripts/validate_asupersync_rch_execution_policy.sh`
- rch policy e2e validator: `tests/e2e/test_asupersync_rch_execution_policy.sh`
- Doctrine e2e validator: `tests/e2e/test_asupersync_runtime_doctrine.sh`
- Doctrine pack validator: `scripts/validate_asupersync_doctrine_pack.sh`
- Doctrine pack e2e validator: `tests/e2e/test_ft_e34d9_10_1_2_doctrine_pack.sh`
- Cutover runtime guard policy: `docs/asupersync-cutover-runtime-guardrails.json`
- Cutover runtime guard validator: `scripts/validate_asupersync_cutover_runtime_guards.sh`
- Cutover runtime guard e2e validator: `tests/e2e/test_ft_e34d9_10_8_2_cutover_runtime_guards.sh`

## Inventory Snapshot (2026-02-22)

Source: `docs/asupersync-runtime-inventory.json`.
The inventory now includes `migration_classification` entries with owner module,
criticality, migration difficulty, and recommended target primitive.

### Global reference counts

| Symbol family | Reference count | Files |
|---|---:|---:|
| `tokio::` | 1272 | 76 |
| `asupersync::` | 166 | 32 |
| `runtime_compat::` | 547 | 74 |
| `smol::` | 68 | 11 |
| `async_std::` | 0 | 0 |

### Highest concentration files

| File | Runtime refs |
|---|---:|
| `crates/frankenterm-core/src/runtime_compat.rs` | 215 |
| `crates/frankenterm/src/main.rs` | 171 |
| `crates/frankenterm-core/src/storage.rs` | 86 |
| `crates/frankenterm-core/src/workflows.rs` | 86 |
| `crates/frankenterm-core/src/pool.rs` | 65 |

### Critical observations

1. `crates/frankenterm/src/main.rs` has high asupersync density and currently fails compilation under mixed runtime assumptions.
2. `crates/frankenterm-core/src/runtime_compat.rs` is the highest-leverage normalization boundary and must remain the primary migration choke point.
3. Vendored crates retain smol-heavy surfaces (`frankenterm/ssh`, `frankenterm/codec`, `frankenterm/config`, `frankenterm/pty`) and represent late-stage migration risk.

## Runtime Doctrine (Version 1.0.0 Contract)

Normative runtime doctrine now lives in:

1. `docs/adr/0012-asupersync-runtime-doctrine.md` (decision rationale and review contract)
2. `docs/asupersync-runtime-invariants.json` (machine-readable invariants, anti-patterns, and user guarantees)

### Invariants

1. `INV-001`: Scope ownership is explicit for long-lived loops; orphan task lifetimes are rejected by default.
2. `INV-002`: Capability context (`Cx`) propagation is explicit; no ambient runtime effects.
3. `INV-003`: Outcome semantics preserve success, error, cancellation, and panic distinctions.
4. `INV-004`: Cancellation boundaries are intentional and guard irreversible side effects.
5. `INV-005`: Runtime API access is centralized through `runtime_compat` / `cx` boundaries.

### Canonical mapping (old -> target)

| Legacy pattern | Migration target | Required behavior | User-visible behavior change |
|---|---|---|---|
| `tokio::spawn(...)` | `cx::spawn_with_cx(...)` or scope-owned spawn | no orphan tasks; explicit owner | shutdown progress and stalled-owner diagnostics become explicit instead of silent background exits |
| `tokio::select!` race patterns | asupersync race/select with explicit cancellation handling | no dropped critical messages | contention paths report deterministic cancellation reasons instead of generic timeout noise |
| `tokio::time::sleep/timeout` | `runtime_compat::sleep/timeout` then Cx-aware adapters | deterministic timeout handling and structured errors | timeout failures include stable reason codes and remediation hints |
| `tokio::sync::*` + lossy channel flows | `runtime_compat` wrappers first, then reserve/commit semantics | no cancellation-loss windows | event/command delivery surfaces must report cancellation explicitly, never as silent loss |
| ambient runtime bootstraps | `cx::CxRuntimeBuilder` / unified runtime bootstrap | single policy surface per process role | startup/shutdown status messaging is consistent across CLI/watch/robot/web roles |

### Anti-patterns (reject in review)

1. Direct `tokio::*` usage introduced in files already migrated to doctrine-compliant surfaces.
2. Mixed direct `asupersync::*` and `runtime_compat::*` calls in the same module without an explicit boundary reason.
3. `Cx::for_testing()` in production code.
4. New detached task spawns without ownership/cancellation narrative.
5. Converting cancellation/panic states into generic errors without preserving reason in logs/metrics.

### User-facing guarantees

1. `UG-001` no silent command/event loss: cancellation-sensitive paths must log explicit cancellation outcomes.
2. `UG-002` deterministic shutdown messaging: shutdown states are visible and reason-coded for operators.
3. `UG-003` actionable failures: timeout/cancellation/error outcomes include remediation hints and evidence refs.

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
2. `docs/asupersync-runtime-invariants.json` (machine-readable runtime doctrine contract).
3. Structured migration log lines with fields:
   - `event`
   - `bead_id`
   - `module`
   - `risk_id`
   - `decision`
   - `evidence_ref`
4. Scoreboard updates tied to bead IDs (`ft-e34d9.*`) and status transitions.
5. Scoreboard machine/human artifacts:
   - `docs/asupersync-migration-scoreboard.json`
   - `docs/asupersync-migration-scoreboard.md`
6. Doctrine validation artifacts from `tests/e2e/test_asupersync_runtime_doctrine.sh`.
7. Scoreboard validation artifacts from `tests/e2e/test_asupersync_migration_scoreboard.sh`.
8. rch policy validation artifacts from `tests/e2e/test_asupersync_rch_execution_policy.sh`.
9. Cutover runtime guard artifacts:
   - `docs/asupersync-cutover-runtime-guard-validation.json`
   - `tests/e2e/logs/ft_e34d9_10_8_2_cutover_runtime_guards_*.jsonl`

### Minimum validation cadence

1. Unit-level: runtime boundary wrappers (`runtime_compat`, `cx`) for parity and cancellation-sensitive behavior.
2. Integration-level: representative module migrations compile and test under active feature sets, plus scoreboard auto-update checks from bead graph snapshots.
3. E2E-level: reproducible inventory/report generation with stable JSON output suitable for diffing and recovery/failure guardrails.

### Repro commands

```bash
# Regenerate machine-readable runtime inventory
bash scripts/generate_asupersync_runtime_inventory.sh

# Validate deterministic generation + docs drift + key assertions
bash tests/e2e/test_asupersync_runtime_inventory.sh

# Generate migration scoreboard (machine + operator readable)
bash scripts/generate_asupersync_migration_scoreboard.sh

# Validate scoreboard schema, auto-update behavior, and drift guards
bash tests/e2e/test_asupersync_migration_scoreboard.sh

# Validate runtime doctrine contract + failure/recovery guardrails
bash tests/e2e/test_asupersync_runtime_doctrine.sh

# Validate rch-only heavy compute policy + evidence contract guardrails
bash scripts/validate_asupersync_rch_execution_policy.sh --self-test
bash tests/e2e/test_asupersync_rch_execution_policy.sh

# Validate cutover runtime dependency/import regression guardrails
bash scripts/validate_asupersync_cutover_runtime_guards.sh --self-test
bash tests/e2e/test_ft_e34d9_10_8_2_cutover_runtime_guards.sh

# For heavy compile/test/clippy runs, offload with rch
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
rch exec -- cargo test --workspace
```

### Current blocker note

As of 2026-02-22, validation breadth is constrained by existing compile failures in `crates/frankenterm/src/main.rs` under mixed runtime migration paths. This blocker is tracked and should be treated as a prerequisite for broad-stage validation closure.

## Downstream Bead Guidance

Every migration PR or bead completion comment should cite:

1. the doctrine invariant(s) it satisfies,
2. the risk ID(s) it reduces, and
3. the stage gate evidence it contributes.

If an implementation needs to violate doctrine temporarily, record the exception explicitly with expiration criteria.

All doctrine artifacts and e2e logs must satisfy secret-safe redaction policy and avoid embedding sensitive values.
