# Duel-Program Verification Conventions (W12.1)

> Bead: `ft-7h5da.13.1` (epic `ft-7h5da` — the 2026-06-06 dueling-idea-wizards
> implementation program). This is the **single standard** every per-workstream
> test bead (`W{n}.T`) and the cross-cutting harness (`W12.2`) must satisfy
> before its epic is considered done. It does **not** restate the whole testing
> system — it **binds the duel beads to the existing canonical contracts** and
> adds the program-specific obligations the duel surfaced.

## Canonical sources this addendum builds on (do not duplicate — comply)

| Concern | Canonical source |
|---|---|
| Structured test logging (JSONL rows, 7 kinds, run_id) | [`crates/ft-test-log`](../../crates/ft-test-log/src/lib.rs) — `TestLogger`; rows `{ts, area, test, kind, payload, run_id}`; kinds `assertion / stage_enter / stage_exit / measurement / error / decision / evidence_emit`; files under `target/test-logs/<area>/<test>/<run_id>.jsonl` |
| Test logging + artifact contract | [`docs/test-logging-contract.md`](../test-logging-contract.md) |
| Log row JSON schema | [`docs/testing/log-format-schema.json`](./log-format-schema.json) |
| Robot/MCP golden-envelope + contract doctrine | [`docs/robot-contracts/`](../robot-contracts/) (per-family contract md + golden matrices) |
| Proof techniques (Loom/TLA+/proptest/dylint) | [`docs/methodology/proof-techniques.md`](../methodology/proof-techniques.md) |
| Statistics (SPRT/Hoeffding/conformal) | [`docs/methodology/statistics.md`](../methodology/statistics.md) |
| Read-path redaction matrix | [`docs/security/`](../security/) (W0.1 output) |
| RCH remote-proof recipe | `AGENTS.md` → "Compiler Checks" / "RCH" |

## Obligation 1 — Unit tests (inline `#[cfg(test)]`, + proptest where shape allows)

For every duel feature:

- **Branch coverage, not line coverage.** Every typed enum state and every
  decision branch gets a test. Concretely: all five `SubmitReceipt` states
  (W2), every operating-envelope reason code touched (W4/W5/W9), every
  proof-quality class (W8), every `AttentionItem` source (W6), every steer
  refusal reason (W5), every error code (W12.3).
- **Serde round-trip proptests** for every new wire/persisted type, in **both**
  JSON and TOON, including `schema_version` back-compat (additive-field proof).
- **State machines** (verification SM, steer lifecycle, economic breaker,
  deadwire gate, cursor lifecycle) get exhaustive transition tests **including
  illegal-transition rejection**.
- **Float/NaN/Inf rejection** at every public `f64`/`f32` boundary
  (substrate-audit rule). No rubber-stamp `is_safe()` — a `true` verdict must
  require recorded evidence (assert the cold-start case returns
  `Unknown`/`false`).
- **Fail-open / fail-closed branches are both tested.** e.g. W2's
  `verification_unavailable` must be proven byte-equivalent to today's send
  behavior; W4/W8 fail-closed paths must deny on missing telemetry.

## Obligation 2 — E2E shell scripts (`tests/e2e/<feature>.sh`, no mocks where a real surface exists)

- Drive the real `ft` binary / robot CLI / a live NTM pane; **assert observable
  outcomes**, not internal state.
- **Structured logging is mandatory and detailed.** Emit `ft-test-log`-style
  rows (or the shell-side equivalent the contract defines): ISO-8601 UTC
  timestamps, explicit **PHASE markers** (`SETUP` / `ACT` / `ASSERT` /
  `TEARDOWN` → `stage_enter`/`stage_exit`), one PASS/FAIL line per assertion
  (`assertion` rows), the **exact command run**, and the **artifact path** for
  every captured output. *A human reading only the log must be able to tell
  what happened and why a failure failed.*
- **Retain artifacts** under
  `tests/e2e/artifacts/<bead-id>/<UTC-timestamp>/` with at minimum
  `commands.txt`, `env.txt`, `structured.log` (JSONL), `stdout.txt`,
  `stderr.txt`, `summary.json` — mirroring the `ft-akx00` /
  resource-cockpit-conformance lanes.
- **Deterministic.** Seed RNG; pin time via the `VirtualClock`/test harness;
  use **replay-corpus fixtures** for pane output so runs are byte-reproducible.
- **Negative / adversarial cases are first-class, not optional:** malformed
  input; **plant a unique canary secret and assert it never appears** in the
  surface under test (redaction proof); policy-denied paths; RCH-unavailable
  paths; tampered-hash / expired-receipt paths; stuck-composer paths.

## Obligation 3 — Golden matrices for machine-facing surfaces

Every new robot family / MCP tool ships a **golden envelope matrix** (JSON +
TOON canonical forms) and a `docs/robot-contracts/<family>.md` contract, per the
profile-family doctrine. CI fails on drift. Canonicalization handles the f64
overflow + key-ordering rules already used by the existing golden matrices.

## Obligation 4 — Observability is part of the deliverable (the "great detailed logging" rule, applied to **production** code)

This is not just about test logs. Every new **production** code path must:

- Emit **structured `tracing` spans** with stable field names: `pane_id`,
  `rule_id`, `receipt_id`, `decision`, `reason_code`, `elapsed_ms`,
  `agent_type`, `cursor` (as applicable).
- Increment a **named counter** for every typed decision / denial / state
  transition (so dashboards and the W2.8 drift-alert can read them).
- **Redact before emit** — spans, events, logs, and notifications all route
  through the `Redactor` before any secret-bearing field is written.
- The feature's e2e script **asserts on the emitted spans/counters**, so the
  logging itself is regression-tested (a feature that stops logging fails CI).

## Obligation 5 — Remote proof for closeout

Unit + e2e must pass under the RCH remote lane:

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
  env CARGO_TARGET_DIR=/tmp/ft-<bead>-test cargo test -p <crate> <filter> -- --nocapture
```

`[RCH] local` / `running locally` / `no admissible workers` / `worker=null` =
**blocked, not proof**. Record the exact command, worker/job id, and
`[RCH] remote` in the bead's closing comment (see W1.1's close comment for the
exemplar format). Bench-class assertions cite a **retained artifact**, never an
inline number.

## Per-bead closeout footer (paste into the bead comment on close)

```text
Scope: <what landed, files touched>
Unit: <crate/test names; states/branches covered>
E2E: tests/e2e/<feature>.sh — artifacts at tests/e2e/artifacts/<bead>/<ts>/
Golden: <matrix path> (JSON+TOON) [if a robot/MCP surface]
Observability: <spans/counters added; asserted by <test>>
Remote proof: PASS RCH ... worker <id> job <id> exit 0 [RCH] remote
```

## Definition of done for any `W{n}.T` test bead

All listed unit + e2e cases pass under the RCH remote lane; e2e emits
phase-marked structured logs with retained artifacts; every negative / canary /
policy-denied / typed-failure case produces the correct typed-or-denied result;
new spans + counters are asserted; golden matrices pass for any robot/MCP
surface touched.
