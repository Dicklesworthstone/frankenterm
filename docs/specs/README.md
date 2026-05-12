# Formal Spec Conventions

`docs/specs` contains TLA+ specifications that back formal-method proof lanes.
Every spec in this directory must be directly runnable by TLC and traceable back
to the Rust model or production code it abstracts.

## File Naming

- Use one kebab-case file per subsystem: `subsystem-contract.tla`.
- The TLA+ module name must be PascalCase and must match the file topic.
- Keep the sibling TLC configuration at `docs/specs/<spec>.cfg`.
- Keep the Rust mapping document at `docs/specs/<spec>-mapping.md`.

## Required TLA+ Sections

Every `.tla` file must contain these sections or definitions:

- State variables: a `VARIABLES` block and a `vars == <<...>>` tuple.
- Initial state: `Init ==`.
- Next-state relation: `Next ==`.
- Full behavior: `Spec == Init /\ [][Next]_vars`.
- Safety invariants: named invariants plus a `SafetyInvariants ==` block.
- Liveness/progress block: temporal properties, fairness notes, convergence, or
  an explicit reason the spec is safety-only.
- TLC run note: a `Run with TLC` comment that points operators at the wrapper.

## Mapping Documents

Each `docs/specs/<spec>-mapping.md` must include these headings:

- `## Rust Correspondence`
- `## Action Mapping`
- `## Invariant Mapping`
- `## TLC Configuration`

The mapping must cite concrete Rust paths with line numbers. Line numbers are a
review aid rather than a semantic dependency; update them when the cited model
or production file moves enough to make the reference misleading.

## TLC Configurations

Each `.cfg` file must:

- Use `SPECIFICATION Spec`.
- Set deterministic constants in a `CONSTANTS` block.
- Check `INVARIANT SafetyInvariants` at minimum.
- Avoid placeholders such as `TODO`, `FIXME`, or `<...>`.

Keep constants intentionally small. The default config is for repeatable smoke
and coverage accounting; larger state-space runs should use a separate artifact
path and record their constants in the bead evidence.

## Scripts

- `scripts/check-spec-conventions.sh` validates this directory.
- `scripts/run-tlc.sh docs/specs/<spec>.tla` runs TLC with the sibling `.cfg`
  and writes a normalized JSON summary.

`scripts/run-tlc.sh` emits the G35 substrate fields:

```json
{
  "state-count": 0,
  "distinct-state-count": 0,
  "time-budget": {"seconds": 300, "enforced": true, "timed-out": false},
  "invariant-results": []
}
```
