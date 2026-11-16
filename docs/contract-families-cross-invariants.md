# Contract Families Cross-Invariants

Status: v1 cross-family conformance contract for `ft-tf6g3.46`.

The context-horizon, capture-fairness, herd-wave, blocker-radar, and
resource-pressure-cockpit families are consumed together by the same operator
surfaces. A family can satisfy its own schema while still contradicting another
family. This document declares the cross-family predicates the integration
matrix must enforce.

The predicates use a small TLA+-style vocabulary:

- `[]P` means predicate `P` must hold for every matrix tuple.
- `P => Q` means `Q` is required whenever `P` is true.
- `<>P` means the matrix row must expose eventual operator evidence for `P`.
- Set membership is written as `x \in {a,b}`.

## Canonical Evidence States

The matrix axes use the shared six-state vocabulary:

`measured`, `inferred`, `simulated`, `stale`, `unavailable`, `mixed`.

Family-specific schemas may expose richer state vocabularies, but the matrix
must preserve the canonical state that drove each synthesized family snapshot.

## Invariant Table

| ID | Predicate | Families | Rationale | Beads |
| --- | --- | --- | --- | --- |
| CF-001 | `[](resource.pressure_tier \in {"red","black"} => capture.starvation_risk = TRUE)` | resource-cockpit,capture-fairness | Resource pressure and capture starvation are the same operator hazard viewed from different surfaces; red or black pressure cannot be reported as starvation-safe. | ft-rz0eb,ft-n447z |
| CF-002 | `[]((resource.pressure_tier \in {"red","black"} /\ capture.starvation_risk = TRUE) => herd.admission_action \in {"defer","degrade","shed","unavailable"})` | resource-cockpit,capture-fairness,herd-wave | A fleet under resource pressure plus capture starvation must not schedule an unrestricted burst. | ft-rz0eb,ft-n447z,ft-5bwjf |
| CF-003 | `[](context.risk_tier = "black" => herd.recommended_stagger_ms >= 1000 /\ herd.admission_action # "admit")` | context-horizon,herd-wave | A pane at black context risk is unsafe for new fanout and must force conservative dry-run planning. | ft-r920m,ft-5bwjf |
| CF-004 | `[](blocker.overall_state # "actionable" => herd.admission_action # "admit" /\ <>herd.next_action \in {"observe","pause_assignment"})` | blocker-radar,herd-wave | A blocked lane must suppress burst admission and surface read-only operator moves rather than mutating queues. | ft-9ntud,ft-5bwjf |
| CF-005 | `[](\E f \in Families: f.evidence_state \in {"stale","unavailable"} => f.unavailable_or_stale_reason_codes # {})` | all | Missing or stale evidence must fail closed with explicit reason codes instead of disappearing from a family row. | ft-r920m,ft-n447z,ft-5bwjf,ft-9ntud,ft-rz0eb |
| CF-006 | `[](\A f \in Families: f.raw_content_stored = FALSE)` | all | Cross-family rendering must preserve the per-family privacy contract: no raw pane, prompt, or transcript content. | ft-r920m,ft-n447z,ft-5bwjf,ft-9ntud,ft-rz0eb |
| CF-007 | `[](\A f \in Families: Shape(JSON(f)) = Shape(TOON(f)))` | all | AI-to-AI TOON output must not drop or retype operator-visible fields relative to JSON. | ft-r920m,ft-n447z,ft-5bwjf,ft-9ntud,ft-rz0eb |

## Matrix Scope

The integration matrix covers all `6^5 = 7776` combinations of canonical
evidence states across the five families. It keeps tuple generation
deterministic by deriving family DTOs from the tuple index and state names.

For each tuple the matrix must:

1. synthesize one read-only DTO per family,
2. validate every DTO against the family JSON schema under `docs/json-schema/`,
3. enforce every invariant above,
4. encode each DTO as TOON and compare the decoded shape with the original JSON
   shape, and
5. report per-invariant pass counts into the release-bundle attestation slot.

## Degradation Rules

If a family synthesizer is unavailable, affected rows must report
`family-unavailable` or an equivalent explicit unavailable source state. An
invariant that references the unavailable family may be skipped only when the
skip list names the family, invariant id, and tuple id. A complete release gate
requires at least 90 percent exercised tuples; the current target is 100
percent.

If the matrix runtime budget is exceeded, the retained artifact must report
`matrix-incomplete`, the completed tuple count, and the skipped tuple list. A
silent partial pass is a contract violation.

If an invariant is systematically violated, the closeout must classify whether
the code is wrong, the invariant is stale, or the family schemas need a
reconciliation bead. The matrix must not mask the violation by downgrading it to
an informational warning.
