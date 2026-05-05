# Robot Family Contract Meta-Schema

**Bead:** [BR-RC-ROBOT-CONTRACT.0] / `ft-hac7w.1`
**Audience:** Anyone closing one of the schema-driven robot families.
The original contract set covered `profile`, `checkpoint`, `context`,
`work`, and `fleet`. `profile` now has native read paths and dry-run
apply; the current NTM-gap implementation set is `checkpoint`,
`context`, `work`, and `fleet`.

This document is the **single source of truth** for what a "complete"
robot-family contract looks like. Each family is described by a
`FamilyContract` value (Rust, in `crates/frankenterm-core/src/robot_family_contract.rs`)
that emits four things from one declaration:

1. **Proptest input strategies** — drive the fuzz corpus and
   property-based regressions.
2. **JSON Schema** — validates the request envelope at the IPC boundary
   and feeds downstream client codegen (TypeScript, Python).
3. **MCP tool registration metadata** — name, description, input schema,
   idempotency hint — wired into the `mcp_framework.rs` seam so the
   `fastmcp` server can register the action with one descriptor object.
4. **Conformance-harness invariants** — named, machine-runnable predicates
   that the harness in `tests/robot_family_conformance/` enumerates and
   asserts against a real handler.

The live CLI dispatch status for the remaining NTM-gap families is tracked
in `docs/robot-contracts/current-ntm-gap-dispatch.md` and guarded by
`crates/frankenterm/tests/robot_ntm_gap_contract_tests.rs`. That harness is
separate from the schema/state-machine conformance suite: it verifies that
actions documented as fallback still return the structured
`robot.not_implemented` envelope, and gives implementation beads a single
manifest entry to flip when an action becomes native.

Everything below describes the *shape* of that declaration. New families
SHOULD NOT add ad-hoc proptest strategies, hand-rolled JSON schemas, or
free-form invariant prose. If a family genuinely needs something the
meta-schema can't express, extend the meta-schema (and revisit the other
4 families to keep them consistent) rather than working around it.

## 1. Identity

A `FamilyContract` declares:

| Field          | Required | Notes                                                          |
| -------------- | -------- | -------------------------------------------------------------- |
| `family_name`  | yes      | Lower-snake, matches `ft robot <family>` (e.g. `"profile"`).   |
| `description`  | yes      | One sentence; lands in MCP tool description and generated docs. |
| `concurrency`  | yes      | See §5 below.                                                  |
| `actions`      | yes      | One `ActionContract` per `(family, action)` pair.              |

## 2. Action contract

For each action (`profile show`, `profile list`, …):

| Field                | Notes                                                          |
| -------------------- | -------------------------------------------------------------- |
| `action`             | Lower-snake, matches `ft robot <family> <action>`.              |
| `robot_command`      | Full CLI form (`"robot profile show"`).                         |
| `mcp_tool_name`      | Dotted form (`"ft.profile.show"`); MUST be unique per process. |
| `description`        | One sentence; user-visible.                                    |
| `idempotency`        | See §3.                                                         |
| `failure_semantics`  | See §4.                                                         |
| `side_effects`       | See §6.                                                         |
| `request_schema`     | `SchemaShape` describing the request body (see §7).            |
| `response_schema`    | `SchemaShape` describing the `data` payload of `RobotResponse`. |
| `request_proptest`   | One `ProptestSeed` per request field (see §8).                 |
| `invariants`         | Named `ContractInvariant`s the harness enumerates (see §9).    |

## 3. Idempotency class

Pick exactly one — this is consumed by both the conformance harness
(decides which test to run) and the MCP descriptor (annotates the tool
with `idempotent: bool`).

| Class           | Definition                                                          | Examples                                  |
| --------------- | ------------------------------------------------------------------- | ----------------------------------------- |
| `Idempotent`    | Repeating the request twice in a row produces no extra side effects. | `profile show`, `profile list`, `checkpoint list`. |
| `Commutative`   | Two distinct requests can be reordered without observable change.   | `work claim` (when no resource contention). |
| `Sequential`    | Requests must be serialized; reordering changes the result.         | `checkpoint rollback`, `context rotate`.   |

`Idempotent` ⊂ `Commutative` ⊂ `Sequential` is **not** a subset
relationship — pick the *most specific* class that holds.

## 4. Failure semantics

How partial failure manifests to a caller. The conformance harness uses
this to pick which atomicity check applies.

| Class                              | Definition                                                                                |
| ---------------------------------- | ----------------------------------------------------------------------------------------- |
| `MustNotPartiallyMutate`           | A failed request leaves storage / IPC / event state untouched.                            |
| `CanPartiallyMutateWithReceipt`    | Partial mutation is allowed *iff* the failure response carries a typed receipt naming what landed. |
| `FireAndForget`                    | No durable effect — failure is observable only via the immediate response.                |

The default for read-only actions is `MustNotPartiallyMutate` (vacuously
true). For mutating actions you must consciously pick.

## 5. Concurrency model

Drives the locking the conformance harness sets up around the action.

| Model            | Definition                                                                                   |
| ---------------- | -------------------------------------------------------------------------------------------- |
| `Serializable`   | Action is serialized at the family level — at most one concurrent invocation, period.        |
| `PerPaneSerial`  | At most one per pane (default for pane-bound actions like `send`).                            |
| `Parallel`       | Multiple invocations may run concurrently with no interlock.                                  |

## 6. Observable side-effect surface

Every action declares an explicit side-effect surface. This is the
falsification anchor for the conformance harness — if a real handler
mutates a table or emits an event the contract didn't declare, the
harness fails.

| Field                     | Notes                                                                       |
| ------------------------- | --------------------------------------------------------------------------- |
| `events_emitted`          | Event-bus event types the action MAY emit (e.g. `"profile.applied"`).      |
| `storage_tables_mutated`  | Storage table names the action MAY mutate (e.g. `"profiles"`).              |
| `ipc_targets`             | IPC destinations (e.g. `"mux"`, `"watcher"`).                              |

Read-only actions declare empty vectors for all three.

## 7. Schema shape

A `SchemaShape` is a stripped-down subset of JSON Schema sufficient for
request envelopes:

```rust
SchemaShape {
    kind: SchemaKind::Object,
    fields: vec![
        SchemaField { name: "name", kind: SchemaKind::String, required: true,  description: Some("Profile name") },
        SchemaField { name: "tags", kind: SchemaKind::Array,  required: false, description: None },
    ],
}
```

`FamilyContract::json_schema()` walks this and emits a Draft 2020-12
JSON Schema document validatable by the existing `jsonschema` runtime
validator (the same one used by `tests/conformance_robot_envelope_schema.rs`).

The shape deliberately does NOT cover every JSON-Schema feature
(no `oneOf`, `not`, `anyOf`, regex patterns). If a family needs more
expressive validation, that family's request type should be split into
multiple actions, not the schema shape extended.

## 8. Proptest seed

Each request field carries a `ProptestStrategyHint` describing what
inputs the harness should generate:

| Hint                            | Generates                                              |
| ------------------------------- | ------------------------------------------------------ |
| `AsciiString { max_len }`       | Printable ASCII strings up to `max_len` chars.          |
| `U32Range { min, max }`         | Integers in `[min, max]`.                                |
| `Bool`                          | `true` / `false`.                                        |
| `OptionString { max_len }`      | `Some(ascii)` / `None`.                                  |
| `StringMap { max_entries }`     | `HashMap<String, String>` with up to `max_entries` keys. |

The harness in `tests/robot_family_conformance/` walks the seeds and
constructs a `BoxedStrategy<serde_json::Value>` that produces request
JSON. New hints are added to the enum in lockstep with the harness so
families can't drift.

## 9. Conformance invariants

A `ContractInvariant` is one named, machine-runnable check. The harness
selects an implementation by `kind`:

| `InvariantKind`        | What the harness checks                                                                                                |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `Determinism`          | Same input → same output across two runs (modulo a documented volatility allowlist for timestamps / generated ids).     |
| `Idempotence`          | Repeating the request twice in a row produces the same result and no additional side effects.                          |
| `AtomicOnFailure`      | A handler that errors mid-flight leaves no observable state mutation (only valid for `MustNotPartiallyMutate`).         |
| `Commutativity`        | Two distinct successful requests produce the same final state regardless of order (only valid for `Commutative`).       |
| `ResponseShape`        | The handler's response `data` payload validates against the declared `response_schema`.                                 |
| `Custom(name)`         | Family-specific predicate; the family supplies the implementation in its conformance test file.                        |

Every action MUST declare at least `Determinism` and `ResponseShape`.
Mutating actions MUST additionally declare `AtomicOnFailure` (or, if
they declare `CanPartiallyMutateWithReceipt`, a `Custom` invariant
naming the receipt fields).

## 10. Wiring

The MCP server seam in `mcp_framework.rs` consumes
`FamilyContract::mcp_tool_descriptors()` to register all of a family's
actions in one call. The conformance harness in
`tests/robot_family_conformance/` consumes the contract directly. The
JSON Schema produced by `FamilyContract::json_schema()` is committed
under `docs/json-schema/robot-family-<family>.json` so client codegen
sees it.

## 11. Closure checklist

A family bead (`ft-hac7w.2` … `ft-hac7w.6`) is "closed" when all of:

- [ ] `FamilyContract` value is committed in `robot_family_contract.rs`.
- [ ] All declared invariants pass in `tests/robot_family_conformance/<family>.rs`.
- [ ] The 1000-request differential corpus from `ft-hac7w.1.1` runs
      against the family with zero divergence (or a documented
      classification of the divergences in
      `docs/robot-contracts/<family>-ntm-divergence.md`).
- [ ] The MCP descriptor is registered (or, for families with no MCP
      surface, that decision is recorded in this checklist).
- [ ] The JSON Schema is committed and validated by the existing
      `tests/conformance_robot_envelope_schema.rs` harness.
