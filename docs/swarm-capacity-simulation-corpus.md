# Swarm Capacity Simulation Corpus

Status: `ft-b94bx.4` deterministic high-scale simulation corpus for swarm
capacity planning.

Fixture:
`crates/frankenterm-core/tests/fixtures/swarm_capacity_simulation_corpus/high_scale.v1.jsonl`

Rust proof:
`crates/frankenterm-core/tests/swarm_capacity_simulation_corpus.rs`

E2E smoke:
`tests/e2e/test_swarm_capacity_simulation_corpus.sh`

## Contract

The root contract id is `ft.swarm_capacity_simulation_corpus.v1`.

Each JSONL row is one replayable scenario. Rows are ordered by fleet scale:
50, 100, 200, and 500 panes. The scenario body contains only bounded labels,
counts, evidence states, capacity-unit totals, and stable reason codes. It does
not store raw pane text, prompts, command bodies, environment values, or secrets.

| Field | Meaning |
| --- | --- |
| `schema_version` | Versioned row schema, currently `1`. |
| `contract_id` | Stable contract id. |
| `scenario_id` | Opaque scenario id; it is not derived from hostnames or pane content. |
| `stable_seed` | Fixed deterministic seed for replay or synthetic trace expansion. |
| `pane_count` | Fleet scale represented by this row. |
| `content_hash` | SHA-256 hash of the deterministic scenario material. |
| `workload_mix` | Per-class pane counts and requested units per pane. |
| `features` | Corpus feature flags covered by the scenario. |
| `expected_bottleneck` | Expected dominant modeled pressure point. |
| `evidence_state_assumptions` | Context, blocker, herd-wave, and resource evidence assumptions. |
| `expected_summary` | Deterministic pane and unit totals by dry-run action. |
| `decision_trace` | Per-step capacity and admission decisions emitted by the E2E harness. |
| `raw_pane_content_stored` | Always `false`. |
| `side_effects_executed` | Always `false`. |

## Scenario Set

| Panes | Scenario | Seed | Hash | Expected bottleneck |
| ---: | --- | ---: | --- | --- |
| 50 | `ft-b94bx.4.scale_50.idle_tail_build_burst` | 944204050 | `sha256:bb063f9b66ccc0c96c7bfb6aa778c554319ccc3bc87d2e7ec812324c47822775` | `none_green` |
| 100 | `ft-b94bx.4.scale_100.blocker_rate_context_mix` | 944204100 | `sha256:49e2194f1e761ebcde447b4aa40b863f8f41694c99748d5890b3a48a612a2d54` | `build_slots_yellow` |
| 200 | `ft-b94bx.4.scale_200.build_context_render_pressure` | 944204200 | `sha256:afd281c541b52e2f2251768d04affbbeeaea274cdf5b7ff5bd9cff19a3a9fa1a` | `build_and_context_red` |
| 500 | `ft-b94bx.4.scale_500.full_swarm_pressure_cascade` | 944204500 | `sha256:0f3162305de7b0fa20ef88f2db3d274f80a3be1e5f7b62e356c47ac341f52a9c` | `multi_subsystem_black` |

## Required Features

The four rows collectively cover:

- `idle_tails`
- `build_bursts`
- `rate_limits`
- `blocker_cascades`
- `context_rotations`
- `render_resize_storms`

The 50-pane row is intentionally green and small enough to catch accidental
over-throttling. The 100-pane row mixes blockers and rate limits with an
admitted stagger. The 200-pane row makes build, context, and render pressure
visible without requiring target-class hardware. The 500-pane row is a
fail-closed pressure cascade that sheds idle tails and defers non-idle pressure.

## JSONL Logging

The E2E harness writes
`tests/e2e/artifacts/goal-line/ft-b94bx.4/swarm_capacity_simulation_corpus/<run>/events.jsonl`.
Every emitted row carries the common fields used by the capacity goal-line
harnesses: timestamp, bead id, run id, scenario id, domain, step, outcome,
evidence state, reason code, artifact path, and RCH reachability booleans.

For each scenario, the harness emits:

- one scenario row with the scale and expected bottleneck
- one event per `decision_trace` entry carrying `capacity_units` and
  `admission_action`
- suite preflight, fixture, feature coverage, summary consistency, privacy, and
  optional RCH proof rows

The static mode validates the fixture and emits JSONL without compiling. The
`--run-rust-proof` mode runs only through `rch` and targets the Rust parser test:

```bash
bash tests/e2e/test_swarm_capacity_simulation_corpus.sh --run-rust-proof
```

## Proof Boundaries

This corpus is synthetic by design. It is suitable for deterministic parser,
summary, admission-trace, and harness-shape proof. It does not by itself prove
the product can run 500 live panes on target-class hardware. That claim remains
blocked on a retained RCH/live target-class capacity gauntlet artifact.
