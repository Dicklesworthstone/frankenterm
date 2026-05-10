# Context Horizon Contract

Status: v1 planning contract; implementation and RCH proof pending under
`ft-r920m`

This document defines the first operator-facing contract for a context horizon:
a privacy-bounded forecast of which panes are likely to hit context pressure,
rate-limit stall, or handoff risk before the swarm loses useful throughput.

The JSON schema sketch lives at
`docs/json-schema/ft-context-horizon.json`.

## Existing Anchors

The contract is grounded in surfaces that already exist:

| Surface | Existing field or type | Contract use |
| --- | --- | --- |
| `ft robot context status` | `pane_contexts`, `context_rotations`, `pressure_tier`, token counters, rotation depth | Native context registry evidence for pane pressure and compaction history. |
| `ft robot context rotate` | durable rotation receipts and idempotency keys | Candidate action shape for later policy-gated mutation; v1 horizon only recommends dry-run actions. |
| `ft robot context history` | recent rotations with token deltas | Trend and compaction ROI evidence. |
| Pattern/runtime events | session compaction and rate-limit detections | Risk hints when a pane is near context or provider pressure. |
| Handoff capsules | inspect/import/export contract | Recommended handoff path when the pane is too risky to continue locally. |
| Resource cockpit | evidence-state and artifact-path posture | Reused truth vocabulary for missing, stale, simulated, and measured evidence. |

This contract does not replace incident bundles, proof-doctor handoffs, resource
pressure cockpit output, capture fairness telemetry, or fleet mutation planning.
It sits above them as a read-only forecast and recommendation layer.

## Output Surfaces

Required implementation surfaces:

| Command or API | Required posture |
| --- | --- |
| `ft robot context horizon` | Emits the full v1 JSON envelope. |
| `ft robot --format toon context horizon` | Preserves all pane risk rows, reason codes, unavailable domains, and recommendation ids. |
| `ft doctor --json` | May embed a context-horizon summary when context telemetry is available. |
| Doctor/plain output | Shows compact operator rows without implying that dry-run recommendations have executed. |
| MCP/Robot read resource | Optional for v1; if implemented, it must return the same contract and remain read-only. |

## Versioned Envelope

The contract id is `ft.context_horizon.v1`. The root object must carry:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer schema version. Version 1 is this contract. |
| `contract_id` | Stable string, currently `ft.context_horizon.v1`. |
| `generated_at_ms` | Unix epoch milliseconds for this forecast. |
| `source` | Producer path, for example `robot.context_horizon`. |
| `evidence_state` | Root synthesis: `measured`, `inferred`, `simulated`, `stale`, `unavailable`, or `mixed`. |
| `horizon_window_ms` | Forecast window for risk decisions. |
| `fleet_summary` | Pane counts, highest risk, and top recommended operator move. |
| `pane_risks` | Per-pane context, compaction, rate-limit, and handoff risk rows. |
| `recommendations` | Dry-run advisor rows with policy posture and reason codes. |
| `citations` | Redacted evidence references only; never raw pane text. |
| `unavailable_domains` | Missing or stale evidence domains that affected the forecast. |
| `redaction_policy` | Privacy posture and raw-content prohibition. |
| `artifact_paths` | Retained artifacts needed to audit fixtures or proof runs. |

## Evidence States

Missing telemetry is not green.

| State | Meaning | Allowed operator use |
| --- | --- | --- |
| `measured` | Fresh data collected from the workspace represented by this forecast. | May support operator decisions when cited and fresh. |
| `inferred` | Derived from measured counters or event history without direct provider/token introspection. | May guide dry-run recommendations; not proof of provider state. |
| `simulated` | Fixture, replay, synthetic, or model-only evidence. | Development and planning only. |
| `stale` | Evidence exists but exceeds its freshness budget. | Must create an unavailable/stale domain and cannot justify mutation. |
| `unavailable` | Evidence could not be collected or is not wired. | Must be visible and fail closed. |
| `mixed` | The root combines domains with different states. | Root must expose every contributing domain state. |

Every pane risk, recommendation, and unavailable domain must include stable
`reason_codes`. If evidence is stale or unavailable, the corresponding row must
say so instead of disappearing.

## Pane Risk Rows

Each `pane_risks` entry describes one pane:

| Field | Meaning |
| --- | --- |
| `pane_id` | Stable pane id. |
| `risk_tier` | `green`, `yellow`, `red`, `black`, or `unknown`. |
| `evidence_state` | Evidence state for this pane row. |
| `context_utilization` | Fraction of known context budget consumed when available. |
| `tokens_consumed` | Observed or estimated tokens consumed. |
| `token_budget` | Context budget used for this row. |
| `rotation_depth` | Number of known context rotations. |
| `ms_since_last_rotation` | Freshness of the most recent context rotation. |
| `compaction_pressure` | Context compaction pressure tier. |
| `rate_limit_risk` | Provider/rate-limit risk tier. |
| `handoff_readiness` | Whether handoff should be prepared. |
| `reason_codes` | Stable reasons explaining the row. |
| `citation_ids` | References into `citations`. |

Implementations may add optional numeric trend fields later, but v1 must stay
usable when only native context registry data is present.

## Recommendations

Recommendations are dry-run advice, not executed actions.

Required fields:

| Field | Meaning |
| --- | --- |
| `recommendation_id` | Stable id within the forecast. |
| `scope` | `pane` or `fleet`. |
| `pane_id` | Pane id when scope is `pane`. |
| `action_kind` | `rotate_context`, `prepare_handoff`, `reduce_fanout`, `pause_assignment`, `inspect_prompt`, `collect_incident_bundle`, or `none`. |
| `mutation_allowed` | Must be `false` for the v1 horizon. |
| `policy_state` | `allowed_dry_run`, `requires_approval`, `blocked`, or `unavailable`. |
| `operator_summary` | Concise human summary. |
| `suggested_command` | Optional command text; must never restart/repair Agent Mail or mutate panes in v1. |
| `reason_codes` | Stable reasons. |
| `citation_ids` | Evidence references. |

The advisor must distinguish source regressions from environmental blockers and
missing evidence. A missing provider token counter is not the same as a healthy
pane.

## Privacy Invariants

The context horizon must never store or emit raw private content:

- no raw pane transcript,
- no prompt body,
- no session cookies, API keys, or bearer tokens,
- no unbounded text excerpts,
- no hidden mutation through a recommended command.

The root field `raw_context_content_stored` must be `false`. Citations may use
bounded identifiers, counters, event ids, hashes, and redacted labels. If a
future implementation needs content-derived evidence, it must emit a redaction
reason and bounded citation rather than the content itself.

## Failure Classification

Contract and proof artifacts should classify failures as:

| Class | Meaning |
| --- | --- |
| `source_regression` | The implementation violates the schema or expected behavior. |
| `privacy_violation` | Raw or secret-like content escaped into output or fixtures. |
| `environment_blocked` | Storage, RCH, worker, or platform dependency prevented proof. |
| `unavailable_evidence` | The horizon ran but required evidence was missing. |
| `target_hardware_skipped` | High-scale 64 CPU / 256 GiB claims were not proven. |

## Proof Expectations

The contract is not complete until later beads add:

- deterministic schema/golden fixtures,
- docs-smoke checks that keep docs and schema names aligned,
- Robot JSON and TOON contract tests,
- privacy fixtures that fail on raw prompt or secret leakage,
- RCH-backed proof artifacts with exact commands and isolated target dirs.

Until then, this document is a v1 contract and planning surface, not proof that
the context horizon is implemented.
