# Robot Family Contract: semantic pane API (`ft robot dom`)

> Honest name: **semantic pane API**. The CLI verb is `ft robot dom` for
> brevity, but the surface returns a **flat list of OSC 133 semantic zones** —
> it is **NOT** a DOM tree, and no field promises a tree, parent/child links, or
> nesting. This doc is the canonical contract for the `dom` family
> (ft-7h5da.2.1 / .2.6), following the methodology of
> [`profile.md`](profile.md).

## Family overview

| Verb | CLI | Query kind | Returns |
|---|---|---|---|
| zones | `ft robot dom zones <pane>` | `zones` | the flat `zones[]` list |
| last-command | `ft robot dom last-command <pane>` | `last_command` | the most recent command-input zone (`command`) |
| output-of | `ft robot dom output-of <pane> <index>` | `output_of` | the output zone for command `index` (`output`) |
| exit-code | `ft robot dom exit-code <pane> [index]` | `exit_code` | the exit status for a command (`exit_code`) |

All four verbs are **read-only** observations of the live mux's OSC 133
semantic-prompt zones (fetched via `MuxInterface::get_semantic_zones`, codec
v47). They send no input, mutate no pane, and write no storage.

## Envelope

Every verb returns a `RobotResponse` whose `data` is a single `DomData`
([`crates/frankenterm-core/src/robot_types.rs`](../../crates/frankenterm-core/src/robot_types.rs),
schema [`docs/json-schema/wa-robot-dom.json`](../json-schema/wa-robot-dom.json)):

| Field | Type | Meaning |
|---|---|---|
| `pane_id` | u64 | target pane |
| `query` | enum `zones` \| `last_command` \| `output_of` \| `exit_code` | which verb produced this |
| `source` | string | `osc133` when zones are real; `unavailable` when the pane has no OSC 133 data |
| `confidence` | f64 | 0.0–1.0; how much to trust the classification |
| `semantic_data_unavailable` | bool | **honest degradation flag** — `true` when no zones could be read |
| `unavailable_reason` | string? | populated when `semantic_data_unavailable` |
| `requested_command_index` | i64? | echoed for `output_of` / `exit_code` |
| `zones` | `DomSemanticZone[]` | **flat** list (zones verb); empty for the others |
| `command` | `DomCommandData?` | populated by `last_command` |
| `output` | `DomCommandOutputData?` | populated by `output_of` |
| `exit_code` | `DomExitCodeData?` | populated by `exit_code` |

`DomSemanticZone` = `{ start_y, start_x, end_y, end_x, semantic_type
(prompt|input|output), text }`. Zones carry terminal coordinates and text —
**not** containment relationships. There is no `children`, no `parent`, no
`root`: a consumer that wants structure must derive it from the `(start_y,
end_y)` ranges itself.

## Contract semantics

| Action | Idempotency | Failure semantics | Side effects |
|---|---|---|---|
| `zones` | Idempotent (pure read) | MustNotPartiallyMutate | read-only |
| `last-command` | Idempotent | MustNotPartiallyMutate | read-only |
| `output-of` | Idempotent on `(pane, index)` | MustNotPartiallyMutate | read-only |
| `exit-code` | Idempotent on `(pane, index)` | MustNotPartiallyMutate | read-only |

**Concurrency:** all verbs are pure reads of the live mux; concurrent calls are
independent and observationally equivalent for the same mux state.

### Degradation (the honest core)

When a pane has no OSC 133 prompt marks (most non-shell panes, or shells without
prompt integration), the surface does **not** invent structure. It returns
`semantic_data_unavailable = true`, `source = "unavailable"`, an
`unavailable_reason`, empty `zones`, and `ok = true` (a successful observation
of "no semantic data"), **never** a fabricated tree. This is the property the
"semantic pane API, not DOM" naming protects: the surface promises only what
OSC 133 actually delivers — a flat, possibly-empty set of zones.

### Error envelopes

Typed `robot.*` errors apply: `robot.pane_not_found` (no such pane),
`robot.wezterm_not_running` (mux unreachable). A pane that exists but has no
semantic data is **not** an error — it is `ok:true` with
`semantic_data_unavailable:true`.

## MCP parity status

`ApiSurface::Dom` is registered in the schema/endpoint registry
(`robot_api_contracts.rs`, `api_schema.rs`, `wa-robot-dom.json`), the CLI family
is complete, and the **`wa.dom` MCP tool mirrors all four verbs** (`WaDomTool` in
`crates/frankenterm-core/src/mcp_tools.rs`, registered as a db-gated/audited tool
in `mcp_bridge.rs` `DB_GATED_AUDITED_TOOL_NAMES`). It is a policy-gated read
(deny/approval/audit path, like `wa.get_text`) that fetches live OSC 133 zones and
builds its envelope through the **same
`frankenterm_core::robot_dom::build_dom_data` the CLI uses — so the MCP and robot
`DomData` envelopes are byte-equal by construction**. Params:
`{ pane_id, query (zones|last_command|output_of|exit_code), command_index? }`
(`query` deserializes directly to `DomQueryKind`). The tool definition is pinned
by the `mcp_manifest` golden and asserted by
`dom_tool_definition_matches_semantic_pane_contract` in `mcp_tools.rs`.
Source-landed; RCH Cargo proof deferred behind the rch remote-topology-preflight
infra block.

## Golden matrix status

The control-plane golden matrix
(`crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json`,
consumed by `control_plane_golden_matrix.rs`) is a **generated** artifact
(`generated_by`); dom-verb coverage is added by regenerating it once the binary
emits dom envelopes under the matrix harness, then pinning the four verbs'
`DomData` shapes (including the `semantic_data_unavailable` degradation case).
Regeneration requires a build, so it rides the same RCH proof lane as the MCP
mirror. The envelope contract above (field-by-field) is the authoritative shape
the matrix must freeze.

## Cross-references

- CLI: `ft robot dom {zones,last-command,output-of,exit-code}` —
  [`crates/frankenterm/src/main.rs`](../../crates/frankenterm/src/main.rs)
  (`RobotDomCommands`).
- Types: `DomData` and friends —
  [`crates/frankenterm-core/src/robot_types.rs`](../../crates/frankenterm-core/src/robot_types.rs).
- Schema: [`docs/json-schema/wa-robot-dom.json`](../json-schema/wa-robot-dom.json).
- Surface registry: `ApiSurface::Dom` in
  [`robot_api_contracts.rs`](../../crates/frankenterm-core/src/robot_api_contracts.rs);
  coverage row in [`api-surface-coverage.md`](api-surface-coverage.md).
- Substrate: codec v47 `GetSemanticZones` PDUs +
  `MuxInterface::get_semantic_zones` (ft-7h5da.2.1).
