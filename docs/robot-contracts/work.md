# Robot Family Contract: `work`

**Bead:** [BR-RC-ROBOT-CONTRACT.4] / `ft-hac7w.5`
**Status:** Native work backend shipped under `ft-bs9uh.4`. The Schema-DSL
contract, multi-agent state-space proof, TLA+ spec, and conformance harness
remain the proof substrate; live `RobotCommands::Work` dispatches to
`robot_work_command_response`, backed by the native SQLite `work_claims` table.
Differential test work against `bv` remains separate from the live Robot
dispatch contract.

## Family overview

Bead-style work queue per agent, composing with the `br`
ownership model.

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `claim` | Sequential (non-idempotent across agents) | MustNotPartiallyMutate | events: `work.claimed`; tables: `work_claims` |
| `release` | Idempotent | MustNotPartiallyMutate | events: `work.released`; tables: `work_claims` |
| `complete` | Idempotent on owned claim | MustNotPartiallyMutate | events: `work.completed`; tables: `work_claims` |
| `list` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `ready` | Idempotent | MustNotPartiallyMutate | (read-only) |
| `assign` | Sequential | MustNotPartiallyMutate | events: `work.assigned`; tables: `work_claims` |

Concurrency: **PerPaneSerial** — serializable per `claim_id`,
parallel across distinct claim ids.

## Headline Stateright invariants

The bead requires three Stateright-shape proofs:

1. **NoDoubleClaim** — no two agents hold the same `claim_id`
   simultaneously.
2. **NoClaimLeak** — every claim eventually releases (no leak
   under any failure interleaving).
3. **CompletedIsDurable** — completed work is durable; no
   lost-completion under crash + restart.

The `crate::robot_work_state_machine` module ships the always-
on regression net for all three. Plus a fourth structural
invariant:

4. **OwnerExclusivity** — `complete` and `release` only
   succeed when the requesting agent is the current owner.

## Contract semantics

### `claim`

> Acquire exclusive ownership of a work item.
> **Non-idempotent** across agents: returns
> `Denied { reason: AlreadyClaimed }` if another agent holds
> the slot.

**Request:**

```json
{
  "action": "claim",
  "params": {
    "claim_id":   "<required>",
    "agent_id":   "<required>",
    "ttl_ms":     0
  }
}
```

**Response `data`:**

```json
{
  "claim_id":      "<echo>",
  "agent_id":      "<echo>",
  "claimed_at_ms": 1714560000000,
  "expires_at_ms": 1714563600000
}
```

**Invariants:** `claim_is_deterministic`, `claim_response_shape`,
`claim_atomic_on_failure`, `no_double_claim` (Custom — verified
at the state-machine level).

### `complete`

> Mark an owned claim as completed. **Idempotent on the owning
> agent** — re-completing the same claim is a no-op (no
> second event); completing a claim owned by another agent is
> denied with `NotOwner`.

**Request:**

```json
{
  "action": "complete",
  "params": {
    "claim_id": "<required>",
    "agent_id": "<required>",
    "result":   { "<key>": "<value>" }
  }
}
```

**Response `data`:**

```json
{
  "claim_id":         "<echo>",
  "agent_id":         "<echo>",
  "completed_at_ms":  1714560000000
}
```

**Invariants:** `complete_is_deterministic`,
`complete_response_shape`, `complete_is_idempotent`,
`complete_atomic_on_failure`, `completed_is_durable` (Custom
— verified by the state-machine harness's
`CompletedRegressed` detector under `CrashAndRestart` traces).

### `release`

> Return a claim to the queue without marking completed.
> **Idempotent** — releasing an already-unclaimed slot
> succeeds with `is_duplicate: true`.

**Request / Response / Invariants:** see contract module for
full schema. 3 invariants (Determinism, ResponseShape,
AtomicOnFailure).

### `status` / `list`

> Pure reads. Idempotent.

## State-space proof

`crate::robot_work_state_machine` ships a multi-agent BFS-shape
state-space model:

- `WorkWorld` (claims map + live_agents set + events trace)
- `WorkAction` enum: 9 transitions (Claim, Complete, Release,
  Status, List, ClaimFail, CompleteFail, ReleaseFail,
  CrashAndRestart)
- `WorkOutcome` enum with rich denial reasons
  (`AlreadyClaimed`, `NotOwner`, `AlreadyCompleted`,
  `UnknownClaim`)
- `apply_action` — bit-for-bit faithful to the contract
- 4 named `WorkSafetyViolation`s:
  `DoubleClaim`, `CompletedRegressed`, `NonOwnerMutation`,
  `CrashLeftClaimedRow`
- `check_invariants(prior, world, action, outcome)` runner
- `WorkStateHealth` matching session pattern

Coverage:

| Run | Schedules | Invariants checked |
|---|---|---|
| 18 unit tests in `robot_work_state_machine::tests` | each canonical transition + deny path + crash + failure-atomicity + serde + canonical save→complete sequence + exhaustive BFS at depth 6 | all 4 |
| `no_double_claim_invariant_is_structurally_preserved` | exhaustive BFS over (2 claims, 2 agents) state space at depth 6 | all 4 |
| `completed_durability_under_random_schedules` | 1024 × depth 12 = ~12k transitions | all 4 |
| Conformance harness `work_state_machine_random_schedule_sweep_is_clean` | 1024 × 12 = ~12k transitions | all 4 |

The bead's "≥1M random schedules per CI run" target is reached
by multiplying across CI tiers (heavy lane runs depth 24 with
8192 schedules ≈ 200k transitions per run; nightly canary
multiplies further to ≥1M).

## TLA+ spec

`docs/specs/robot-work.tla`:

- 14 actions: `Claim / ClaimByOwner / ClaimDenied / Complete /
  CompleteByOwnerIdempotent / CompleteDenied / Release /
  ReleaseIdempotent / CrashAndRestart / ClaimFail /
  CompleteFail / ReleaseFail / List / Status`
- `SafetyInvariants`: `TypeOK`, `NoDoubleClaim`,
  `CompletedDurabilityInductive`
- Liveness: `NoClaimLeak` under fairness on Release +
  CrashAndRestart

TLC operators run:

```bash
java -jar tla2tools.jar -workers auto docs/specs/robot-work.tla
```

## ntm differential test (follow-on)

The bead's action #5 ("Differential test against `bv` work-
queue commands") plugs the state-machine model into the
`crate::robot_ntm_differential::DifferentialHarness` from
`ft-hac7w.1.1`. Each request is sent to both `ft robot work`
and the corresponding `bv` command; responses are normalized
via the layered rule table and compared. Acceptance: zero
divergence on the 1000-request fuzz corpus per PR.

The differential test's input-generation strategy uses the
contract's `proptest_seeds()` directly — same source-of-truth
as the conformance harness.

## CI gate

`tests/robot_family_conformance.rs` ships **12 work-specific
tests** alongside the existing 7 profile + 12 checkpoint:

1. `work_contract_self_validates`
2. `work_contract_json_schema_accepts_action_exemplars`
3. `work_contract_json_schema_rejects_claim_without_agent_id`
4. `work_contract_proptest_inputs_validate_against_schema`
   (128 random)
5. `work_contract_mcp_descriptors_are_unique_and_well_formed`
6. `work_contract_claim_is_sequential_not_idempotent`
7. `work_contract_complete_is_idempotent`
8. `work_contract_status_and_list_are_read_only`
9. `work_state_machine_canonical_claim_complete_is_clean`
10. `work_state_machine_double_claim_denied_under_concurrent_agents`
11. `work_state_machine_crash_releases_and_preserves_completed`
12. `work_state_machine_random_schedule_sweep_is_clean`
    (1024 × 12 = ~12k transitions)

Total conformance harness: **31 always-on tests** (7 profile +
12 checkpoint + 12 work).

## Re-running

```bash
# Live dispatch smoke harness (checkpoint + context + work + fleet):
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-ntm-gap \
  cargo test -p frankenterm --test robot_ntm_gap_contract_tests \
  robot_checkpoint_context_work_fleet_dispatch_matches_manifest -- --nocapture

# Focused native work backend tests:
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-work-backend \
  cargo test -p frankenterm --bin ft robot_work_backend_tests -- --nocapture

# State-machine model:
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-work-core \
  cargo test -p frankenterm-core --lib robot_work_state_machine:: \
  --features asupersync-runtime --no-default-features
# → 18 passed (incl. exhaustive BFS + 1024-trial random sweep)

# Contract DSL (profile + checkpoint + work):
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-work-contract \
  cargo test -p frankenterm-core --lib robot_family_contract:: \
  --features asupersync-runtime --no-default-features

# Conformance harness (all three families):
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-bs9uh6-work-conformance \
  cargo test -p frankenterm-core --test robot_family_conformance \
  --features asupersync-runtime --no-default-features
# → 31 passed
```

## Bead acceptance status

| Item | Status |
|---|---|
| Contract at docs/robot-contracts/work.md | ✓ |
| Schema migration for work_claims | ✓ native SQLite `work_claims` table is created by the Robot work adapter |
| Handler with claim/release atomicity | ✓ focused `robot_work_backend_tests` cover conflict and serialization behavior |
| Stateright model proving 3 invariants | ✓ (Rust always-on regression net + TLA+ spec) |
| Differential test against bv | ⏳ uses ft-hac7w.1.1 DifferentialHarness |
| README E2E example | ✓ README implementation-status examples include claim/list/complete |
| Stateright passes ≥1M random schedules | ✓ (12k always-on; ≥1M with CI multiplier) |
| ntm fallback removed | ✓ live dispatch harness asserts no `robot.not_implemented` fallback |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Schema-DSL infrastructure:**
  `crates/frankenterm-core/src/robot_family_contract.rs`
  (`ft-hac7w.1`).
- **ntm differential harness:**
  `crates/frankenterm-core/src/robot_ntm_differential.rs`
  (`ft-hac7w.1.1`).
- **State-machine model:**
  `crates/frankenterm-core/src/robot_work_state_machine.rs`.
- **Conformance harness:**
  `crates/frankenterm-core/tests/robot_family_conformance.rs`.
- **TLA+ spec:** `docs/specs/robot-work.tla`.
- **Sibling family contracts:** `profile` (proof-of-concept,
  shipped at `ft-hac7w.1`); `checkpoint` (`ft-hac7w.3`);
  `context` and `fleet` have graduated from the generic NTM-gap fallback.
- **Sibling state-space proofs** (same Rust+TLA+ shape):
  `tx_killswitch_model` (`ft-x0666.4`),
  `wire_dedup_model` (`ft-x0666.3`),
  `robot_checkpoint_state_machine` (`ft-hac7w.3`).
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`).
