# Herd-Wave Contract

Status: v1 planning contract. Native robot, doctor, MCP, deterministic fixture,
and RCH proof surfaces are tracked by the `ft-5bwjf` children.

This document defines the first operator-facing contract for a herd wave: a
privacy-bounded explanation of synchronized fleet bursts such as compaction
waves, retry storms, rate-limit recovery, search/index bursts, workflow fanout,
and wake-up cohorts.

The JSON schema sketch lives at
`docs/json-schema/ft-herd-wave.json`.

## Existing Anchors

The contract is grounded in code that already exists instead of inventing a
parallel vocabulary.

| Surface | Existing field or type | Contract use |
| --- | --- | --- |
| `HerdWaveEventKind` | `compaction`, `retry`, `rate_limit_recovery`, `search_burst`, `workflow_fanout`, `wake`, `other` | Stable dominant event kind and per-action kind vocabulary. |
| `HerdWaveSignal` | `pane_id`, `kind`, `timestamp_ms` | Input signal rows for synchronized burst detection. |
| `HerdWaveDetectionConfig` | detection window, pane thresholds, base/max stagger | Source of window and recommended stagger budget. |
| `HerdWavePressureSummary` | pressure tier, detected flag, event and pane counts, dominant kind, stagger timing | Required fleet-level wave summary fields. |
| `HerdWaveStaggerPlan` | summary plus staggered actions | Non-mutating dry-run planner shape. |
| `ResourceAdmissionDecisionSummary` | admission action, reason codes, raw/effective severity, priority protection | Admission and priority-protection rows. |
| `SwarmCapacityAdmissionControllerState` | admission stage, last action, last pressure action | Cooldown/circuit-breaker and admission-controller context. |
| Resource pressure cockpit | evidence-state and target-hardware proof language | Shared truth vocabulary for measured, simulated, stale, unavailable, and target-class proof status. |

This contract does not replace context horizon, blocker radar, proof-doctor
handoffs, terminal conformance, resource pressure cockpit, or fleet mutation.
It is a read-only explanation and dry-run planning surface for synchronized
burst pressure.

## Output Surfaces

Required implementation surfaces for later children:

| Command or API | Required posture |
| --- | --- |
| `ft robot herd-wave` or the nearest existing capacity namespace | Emits the full v1 JSON envelope. |
| `ft robot --format toon ...` | Preserves all canonical field names, reason codes, unavailable sources, and action ids. |
| `ft doctor --json` | May embed a herd-wave summary when telemetry is available. |
| Doctor/plain output | Shows compact operator rows without implying that any stagger or admission action executed. |
| MCP resource | Optional read-only resource. If omitted, the implementation bead must explain why and file a scoped follow-up. |

## Versioned Envelope

The contract id is `ft.herd_wave.v1`. The root object must carry:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version. Version 1 is this contract. |
| `contract_id` | Stable string, currently `ft.herd_wave.v1`. |
| `generated_at_ms` | Unix epoch milliseconds for this snapshot. |
| `source` | Producer path, for example `robot.herd_wave`. |
| `source_freshness` | Root freshness budget and source availability row. |
| `evidence_state` | Root synthesis: `measured`, `inferred`, `simulated`, `stale`, `unavailable`, or `mixed`. |
| `overall_state` | Operator-facing state: `normal`, `elevated`, `critical`, `emergency`, `missing_telemetry`, `stale_evidence`, `priority_protected`, `operator_override`, `cooldown_active`, `circuit_breaker_active`, or `unknown`. |
| `dominant_kind` | Dominant synchronized event family. |
| `event_count` | Signals considered inside the active window. |
| `distinct_panes` | Distinct panes inside the active window. |
| `window_ms` | Detection window used for the summary. |
| `pressure_tier` | Fleet pressure tier from the wave summary. |
| `admission_action` | Dry-run admission verdict: `admit`, `defer`, `degrade`, `shed`, `none`, or `unavailable`. |
| `reason_codes` | Stable reasons explaining the root state. |
| `recommended_stagger_ms` | Delay between adjacent cohort actions. |
| `cohort_max_stagger_ms` | Maximum delay assigned to the final action in the cohort. |
| `wave_summary` | Detailed burst count, timing, and dominant-kind row. |
| `priority_protection` | Priority protection and mission-critical reduction details. |
| `operator_override` | Override posture without secrets or raw operator notes. |
| `stagger_plan` | Dry-run action rows. Mutations must be impossible in v1. |
| `citations` | Redacted evidence references only; never raw pane text. |
| `next_actions` | Safe operator actions, all dry-run or manual-review oriented. |
| `forbidden_actions` | Explicitly forbidden actions such as Agent Mail repair/restart, live RCH restart/drain/update without approval, pane mutation, destructive git/filesystem operations, raw pane content emission, and target-class claims without artifacts. |
| `unavailable_sources` | Missing or stale evidence rows that affected the result. |
| `redaction_policy` | Privacy posture and raw-content prohibition. |
| `raw_pane_content_stored` | Must be `false`. |
| `target_class_hardware_proof` | Target-class proof predicate. Synthetic proof is not equivalent to 64+ CPU / 256+ GiB proof. |
| `artifact_paths` | Retained artifacts needed to audit fixtures or proof runs. |

## States

Missing telemetry is not green.

| State | Meaning | Allowed operator use |
| --- | --- | --- |
| `normal` | Fresh evidence shows no synchronized wave above threshold. | Informational only. |
| `elevated` | A wave is detected but stays below critical thresholds. | May support manual staggering or assignment pacing. |
| `critical` | A wave is likely to cause throughput collapse without intervention. | May justify operator review and manual pacing. |
| `emergency` | Existing fleet pressure vocabulary reports emergency pressure. | Must stay explicit; do not collapse into `critical`. |
| `missing_telemetry` | Required source is absent or unwired. | Fail closed and list `unavailable_sources`. |
| `stale_evidence` | Evidence exceeded its freshness budget. | Cannot justify live mutation or high-scale proof. |
| `priority_protected` | Priority or mission-critical protection changed the effective admission action. | Explain the protection rather than hiding the raw pressure. |
| `operator_override` | An explicit override changed the default posture. | Must include override provenance without secrets. |
| `cooldown_active` | Admission or stagger action is held by cooldown. | Show remaining posture if known; do not execute. |
| `circuit_breaker_active` | Controller is intentionally suppressing further disruption. | Recommend inspection and proof capture, not more automation. |
| `unknown` | The implementation cannot classify the state. | Fail closed. |

`pressure_tier` follows the existing fleet vocabulary:
`normal`, `elevated`, `critical`, `emergency`, or `unknown`.

## Reason Codes

Reason codes are stable machine strings. V1 codes should use these prefixes:

| Prefix | Examples |
| --- | --- |
| `herd_wave.kind.*` | `herd_wave.kind.compaction`, `herd_wave.kind.retry`, `herd_wave.kind.rate_limit_recovery` |
| `herd_wave.threshold.*` | `herd_wave.threshold.distinct_panes`, `herd_wave.threshold.window` |
| `herd_wave.telemetry.*` | `herd_wave.telemetry.missing`, `herd_wave.telemetry.stale` |
| `herd_wave.admission.*` | `herd_wave.admission.defer`, `herd_wave.admission.shed`, `herd_wave.admission.unavailable` |
| `herd_wave.priority.*` | `herd_wave.priority.protected`, `herd_wave.priority.operator_override` |
| `herd_wave.safety.*` | `herd_wave.safety.dry_run_only`, `herd_wave.safety.no_target_class_artifact` |

Existing enum names may be transformed into these strings, but the exposed
contract must not leak Rust debug formatting as a public API.

## Dry-Run Stagger Plan

Each `stagger_plan` row describes a recommendation, not an action that has been
sent to a pane or queue:

| Field | Meaning |
| --- | --- |
| `action_id` | Stable id within this snapshot. |
| `pane_id` | Pane id when pane-scoped; nullable for fleet rows. |
| `cohort_rank` | Deterministic order within the detected cohort. |
| `event_kind` | Event kind that placed the row in the cohort. |
| `scheduled_after_ms` | Suggested delay from the dry-run plan start. |
| `admission_action` | Dry-run admission verdict for this row. |
| `mutation_allowed` | Must be `false` in v1. |
| `reason_codes` | Stable reasons. |
| `citation_ids` | References into `citations`. |

Later policy-gated mutation work must be a separate bead. This contract only
allows observation and dry-run planning.

## Privacy Invariants

The herd-wave surface must never store or emit raw private content:

- no raw pane transcript,
- no prompt body,
- no session cookies, API keys, or bearer tokens,
- no unbounded text excerpts,
- no hidden mutation through a recommended command,
- no target-class performance claim without retained proof artifacts.

The root field `raw_pane_content_stored` must be `false`. Citations may use
bounded identifiers, counters, event ids, hashes, artifact paths, and redacted
labels. If a future implementation needs content-derived evidence, it must emit
a redaction reason and bounded citation rather than the content itself.

## Golden Fixture and Conformance Policy

Every deterministic herd-wave fixture row must carry enough metadata for a
reviewer to understand what is stable, what is intentionally volatile, and what
proof claim the fixture is allowed to support.

Required golden confidence matrix fields:

| Field | Meaning |
| --- | --- |
| `scenario_id` | Stable fixture scenario id, shared by JSON, TOON, e2e, and proof logs. |
| `surface` | Contract surface under test, for example robot JSON, robot TOON, doctor JSON, doctor text, or MCP resource. |
| `determinism` | `deterministic`, `platform_dependent`, or `volatile`. |
| `comparison_strategy` | `exact`, `canonical_json`, `canonical_toon`, `contains_rows`, `schema_only`, or `privacy_negative`. |
| `canonicalizer` | Named scrub/canonicalization rule set used before comparison. |
| `contract_requirements` | Requirement ids or field names covered by this fixture row. |
| `update_command` | Exact intentional-update command or `none`. |
| `review_required` | Must be `true` for any fixture that can change committed goldens. |

Golden canonicalizers may scrub volatile transport and runtime details only:
`generated_at_ms`, `run_id`, `correlation_id`, selected worker ids, absolute
paths, artifact roots, queue timestamps, wall-clock durations, and environment
labels that vary across RCH workers. Canonicalizers must not scrub contract
truth. In particular, `reason_codes`, `admission_action`, `pressure_tier`,
`event_count`, `distinct_panes`, `unavailable_sources`, `forbidden_actions`,
`raw_pane_content_stored`, and `target_class_hardware_proof` must remain
asserted values.

JSON and TOON goldens must come from the same canonical snapshot. A JSON pass
and a TOON pass over independently generated snapshots are not parity proof.
The parity check must compare decoded canonical values after format-specific
serialization differences have been normalized.

Committed goldens are immutable by default. Intentional updates must require an
explicit environment gate, currently documented as
`UPDATE_HERD_WAVE_GOLDENS=1`, and must emit a structured update event with
`scenario_id`, `old_sha256`, `new_sha256`, `changed_fixture_count`,
`update_command`, and `review_required=true`. CI must fail when generated
goldens differ from the committed files and the update gate is absent.

Conformance accounting must be explicit. The conformance matrix should map each
contract requirement to fixture coverage with:

| Field | Meaning |
| --- | --- |
| `requirement_id` | Stable id for the contract field, state, invariant, or proof rule. |
| `level` | `MUST`, `SHOULD`, or `MAY`. |
| `fixture_ids` | Fixture rows that exercise the requirement. |
| `surfaces` | Surfaces covered by those fixtures. |
| `status` | `covered`, `partial`, `planned`, `blocked`, or `not_applicable`. |
| `divergence` | Required explanation for partial, blocked, or intentional surface differences. |

Privacy negative fixtures are required. At minimum they must prove that prompt
text, bearer-token-like strings, cookies, and raw pane excerpts cannot appear in
robot, doctor, MCP, artifact, JSONL e2e, or committed golden output. These
fixtures should fail closed as `privacy_violation` if redaction or bounded
citation behavior regresses.

Synthetic scale fixtures are not target-class hardware proof. A deterministic
200-pane synthetic replay can prove schema behavior, scheduling math, and
privacy invariants, but it must not set or imply a 64+ CPU / 256+ GiB
target-class claim. Target-class proof requires retained RCH artifacts that
record the worker predicate, exact commands, isolated target directory, and
artifact hashes.

## Failure Classification

Contract and proof artifacts should classify failures as:

| Class | Meaning |
| --- | --- |
| `source_regression` | The implementation violates the schema or expected behavior. |
| `privacy_violation` | Raw or secret-like content escaped into output or fixtures. |
| `environment_blocked` | Storage, RCH, worker, or platform dependency prevented proof. |
| `unavailable_evidence` | The surface ran but required evidence was missing. |
| `target_hardware_skipped` | High-scale 64+ CPU / 256+ GiB claims were not proven. |

## Proof Expectations

The implementation is not complete until later beads add:

- DTO conversion tests from the existing scheduler/admission telemetry types,
- deterministic fixture cases for compaction, retry, rate-limit recovery,
  search burst, mixed wave, missing telemetry, stale evidence, priority
  protection, operator override, cooldown, circuit breaker, and no-wave,
- Robot JSON and TOON parity tests,
- privacy fixtures that fail on raw prompt or secret leakage,
- structured JSONL e2e logs with `bead_id`, `scenario_id`, `surface`, `step`,
  `outcome`, `reason_code`, `error_code`, `artifact_path`, `selected_worker`,
  and whether Cargo/rustc/test execution was reached,
- RCH-backed proof artifacts with exact commands and isolated target dirs,
- a synthetic 200-pane proof and a separate target-class hardware predicate.

Until those land, this document and the schema define the v1 operator contract,
but they do not claim that the robot, doctor, MCP, or high-scale proof surfaces
are implemented.

## Static Checks For This Contract

This contract bead can be checked without heavy Cargo work:

```bash
jq empty docs/json-schema/ft-herd-wave.json
rg -n 'ft-herd-wave.json|ft.herd_wave.v1|raw_pane_content_stored' \
  docs/herd-wave-contract.md docs/json-schema/ft-herd-wave.json
git diff --check -- docs/herd-wave-contract.md docs/json-schema/ft-herd-wave.json
```

Later implementation beads should add docs-smoke and schema-golden coverage and
run those Rust lanes through RCH.
