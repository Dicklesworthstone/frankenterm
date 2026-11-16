# Target-Class Gate Flip-Readiness Checklist (W9.4a / ft-7h5da.10.4.1)

**Purpose.** Enumerate every site that changes the moment the W9.4 target-class
run signs a **non-skipped** `resource-cockpit-target-class.json`. With this
checklist the flip is a mechanical, auditable diff — not an investigation —
so the rented-hardware day (W9.4 / `ft-7h5da.10.4`) ends in a clean, complete
landing rather than a scavenger hunt.

**Trigger.** `scripts/run-target-class-cockpit.sh` is run on a host that
satisfies the **64 logical CPU / 256 GiB** predicate for SKU
`linux-x86_64-high-core` (the dev host has 14 CPU / 64 GiB and the RCH pool tops
at 10 CPU, so today the harness retains `skipped_not_proven`). When the predicate
is met the harness sets `PROOF_STATUS=proven_predicate_met` and the summary's
`ready_to_sign` becomes true (`scripts/run-target-class-cockpit.sh`, symbols
`PROOF_STATUS` / `proven_predicate_met` / `ready_to_sign` — line numbers omitted
because that script is under active W9.4b edits).

> Scope note: this doc only inventories **what flips**. It does not perform the
> flip and needs no `cargo`/RCH — every item is verifiable with `rg` + `jq`.

---

## A. The trigger artifact — `docs/attestations/proofs/resource-cockpit-target-class.json`

| Field | Current | After signing |
|-------|---------|---------------|
| `.status` | `skipped_not_proven` | proven (per the signed summary) |
| `.current_artifact.status` | `skipped_not_proven` | proven |
| `.current_artifact.path` | `tests/e2e/artifacts/target-class/linux-x86_64-high-core/20260512T150000Z/summary.json` | the real run's `…/<run_id>/summary.json` |
| `.current_artifact.reason` | "…local host and RCH worker pool do not satisfy the 64 logical CPU / 256 GiB high-scale predicate." | run evidence (host facts + conformance result) |

Produced by bead `ft-tf6g3.14`; major SKUs `macos-apple-silicon-dev`,
`linux-x86_64-high-core`. **Only `linux-x86_64-high-core` can unlock 64/256
claims** — `macos-apple-silicon-dev` is operator-workstation smoke only
(14 CPU / 64 GiB) and per `docs/perf/target-class-hardware.md:39` "does not
unlock 64 CPU / 256 GiB claims". A/B/C/D below all key off A.

## B. The live planner verdict (this is the real capability flip)

### B1. Envelope artifact — `docs/attestations/proofs/operating-envelope.json`
Claim `operating_envelope.target_class_capacity` (`claim_id` at line 117):

| Field | Current | After signing |
|-------|---------|---------------|
| `.readiness_state` | `skipped` | `measured` |
| `.target_class_proof.state` | `skipped_not_proven` | `measured` |
| `.target_class_proof.reason` | "…this operating-envelope slot must not graduate 64 CPU / 256 GiB or 200+ pane production-capacity wording." | retired / replaced with the proof pointer |

Also: the reason-vocabulary entry `"target-class-skipped"`
(`operating-envelope.json:41`) stops being emitted once measured.

### B2. Verdict-emitting code (DO NOT hand-edit state — it derives from A)
- `crates/frankenterm-core/src/operating_envelope.rs:1378-1398` — when
  `target_class_proof_state ∈ {SkippedNotProven, Unavailable}` it pushes
  reason code **`capacity.target_class_unproven`** + `TargetHardware:
  Unavailable` evidence; when `∈ {Measured, NotRequired}` it emits
  `target_hardware.proven_or_not_required` **Pass**. The proven variant is
  `OperatingEnvelopeProofState::Measured` (`operating_envelope.rs:108-115`).
  Signing A transitions the input state `SkippedNotProven → Measured`, which
  flips the emitted verdict from blocked/Defer to Pass.
- `crates/frankenterm-core/src/attention_router.rs:1607-1650` — classifies
  `capacity.target_class_unproven` / `target_class_unproven` /
  `target_class_skipped` as `BlockedInfra` → `Blocker` with safe-action
  `FailClosedRequestTargetedHandoffOrPickDisjointWork`. Once the reason code is
  gone, high-scale capacity facts stop classifying as blocked.

**Verification:** the artifact→state wiring must be exercised by regenerating
the cockpit/envelope artifacts (not by editing the JSON state literal). The
post-flip envelope must drop `capacity.target_class_unproven` from its
`reason_codes`.

## C. README high-scale wording held back (the "appears TWICE" hard hedges)

The two **hard held-back** statements that flip from hedge to claim:
- `README.md:4003` — "…the high-scale memory envelope wording for 200+ panes is
  **held back** until the target-class hardware gate signs a non-skipped
  artifact (currently `skipped_not_proven`)." → lift to the proven claim.
- `README.md:4015` — table row `| Target-class memory envelope claims | Held
  back pending non-skipped target-class artifact |` → flip to proven.

Supporting `skipped_not_proven` mentions to update for consistency:
- `README.md:490` — "…artifact remains `skipped_not_proven`."
- `README.md:2622` — "…target-class proof…is currently `skipped_not_proven`."
- `README.md:3009` — capacity table `| 200+ panes | … | target-class artifact
  required … |`; once signed, cite the proven envelope figure.
- `README.md:1614` — "Target-class artifact missing or `skipped_not_proven` →
  reason `capacity.target_class_unproven`; high-scale claims are held back."
  This documents the **gate behavior** — keep it as gate documentation, but note
  the live state now passes.

**Do NOT touch** (descriptive/architecture 200+-pane mentions, no hedge):
README.md lines 304, 329, 475, 520, 539, 1357, 1518, 2103, 2366, 2666, 3008,
3752, 3878, 4070, 4072, 4090, 4117.

## D. Per-major-SKU release-bundle rule

- `resource-cockpit-target-class.json` `.release_bundle_wiring`:
  `.required_per_major_sku` stays `true`; `.status`
  `rule_published_for_g16` → satisfied for the signed SKU; owning bead
  `ft-tf6g3.1` (G16 final release-bundle materialization).
- `docs/attestations/manifest.json:34` — slot `perf/headline-claims` →
  `docs/attestations/perf/swarm-capacity-envelope.json` (produced by
  `ft-b94bx.8`): description "Optional until the target-class gauntlet retains a
  non-skipped linux-x86_64-high-core artifact; while status is
  `blocked_target_class_not_proven` it documents why high-scale memory-envelope
  wording cannot graduate." → graduates from optional/blocked once signed.

## E. Consistency sweep — other artifacts encoding the skipped state

Flip these to stay consistent:
- `docs/attestations/perf/swarm-capacity-envelope.json:8` `.status`
  `blocked_target_class_not_proven` → the non-blocked `status_enum` value;
  `.blocked_reason` `target_class_artifact_status_not_proven` clears.
- `docs/attestations/proofs/swarm-capacity-readiness.json:566` reason
  `capacity.target_class_unproven` → drops out.

**Do NOT flip — test fixtures that pin the skipped path** (they must keep
passing; add a separate "proven" fixture instead of mutating these):
- `fixtures/operating-envelope/manifest.json:48`,
  `fixtures/operating-envelope/valid/target-hardware-skipped.json`,
  `fixtures/operating-envelope/target-class-skipped.json`
- `crates/frankenterm/tests/fixtures/golden_artifacts/swarm_capacity_operator/doctor-remediation.json:92`

## Verification (no `cargo`, no RCH)

```bash
# every JSON listed parses
jq empty docs/attestations/proofs/resource-cockpit-target-class.json \
         docs/attestations/proofs/operating-envelope.json \
         docs/attestations/manifest.json \
         docs/attestations/perf/swarm-capacity-envelope.json \
         docs/attestations/proofs/swarm-capacity-readiness.json
# no stray skipped/blocked target-class strings survive once flipped
rg -n 'skipped_not_proven|blocked_target_class_not_proven|capacity\.target_class_unproven' \
   docs/ README.md
# confirm the harness proven path exists
rg -n 'proven_predicate_met|ready_to_sign' scripts/run-target-class-cockpit.sh
```

**Order of operations when W9.4 lands:** A (sign artifact) → regenerate B1 via
the cockpit/envelope build (B2 follows automatically) → C (README) → D/E (bundle
+ consistency) → run the Verification block. Each row is one anchored edit.
