# Product Journey Contract

- Catalog contract ID: `ft.product_journey_catalog.v1`
- Human companion revision: 2
- Machine companion: `docs/design/product-journey-catalog.v1.json`
- Schema: `docs/json-schema/ft-product-journey-catalog.json`
- Owning Bead: `ft-interactive-swarm-product-convergence-7xqz4.1.1`
- Source baseline at initial draft:
  `df4414f5587cccface7edebdee6028ae758f82f8`
- Initial review date: 2026-07-27

## Purpose

This document defines the human-readable semantics of the versioned product
journey catalog. It turns the product promise into explicit actor, topology,
fleet, lifecycle, accessibility, privacy, and evidence contracts so that a
fast subsystem benchmark cannot stand in for a useful end-to-end product.

The catalog covers four product personas, two mux topologies, and four exact
fleet qualification points. The resulting 32 cells are materialized even when
the current answer is conditional or a producer is missing. An absent cell is
never interpreted as implicitly supported.

## Authority and Conflict Resolution

The following authority boundaries are deliberate:

1. `docs/design/product-journey-catalog.v1.json` is authoritative for stable
   identifiers, the 32-cell inventory, current status fields, target posture,
   cross-references, and machine validation.
2. This document is authoritative for the meaning of those fields, the
   promotion rules, non-claims, and how humans should interpret the catalog.
3. Beads are authoritative for work ownership and dependency state. An open or
   closed Bead is not, by itself, a product-support verdict.
4. Retained artifacts and their verifiers are authoritative for run and
   evidence claims. Prose, source presence, a checked-in fixture, or a passing
   reduced harness cannot mint a target-qualified result.
5. `README.md`, the GUI guide, playbooks, demos, and release notes are derived
   claim surfaces. They must not be more optimistic than the catalog and
   retained evidence.

If two sources disagree, consumers must use the less favorable interpretation,
record the contradiction, and leave promotion blocked until the authoritative
sources converge. No natural-language claim overrides a fail-closed machine
state.

## Explicit Non-Claims

This contract does **not**:

- prove that any journey passes;
- declare any of the 32 cells supported today;
- turn a direct producer Bead into direct execution evidence;
- infer a smaller-fleet result from a larger-fleet result, or vice versa;
- infer a local result from a remote result;
- infer Robot/MCP behavior from native GUI input behavior;
- infer an M5 result from an M4 result, or an AMD result from an Apple result;
- treat a synthetic, mock, headless, reduced, or fixture-only run as a native
  presented journey;
- treat exact 200-pane qualification as an open-ended `200+` claim;
- promise lossless capture when the runtime has recorded an explicit gap;
- make a human-review decision for visual quality, VoiceOver usability, or
  privacy acceptability; or
- replace the final exact-candidate release verdict owned by
  `ft-interactive-swarm-product-convergence-7xqz4.12.9`.

Journeys are contracts. Evidence and release authority remain separate.

Every document valid under the v1 schema has
`catalog_claim_state = contract_only`. That value is permanent for v1: this
catalog inventories contracts, ownership, qualifications, contradictions, and
receipt references, but neither its prose nor its JSON Schema can authenticate
a support-promotion decision. Evidence-schema work in `.1.4`, the unified
verdict in `.12.1`, and exact-candidate authority in `.12.9` must define,
produce, sign, and verify the promotion receipt. A future authority-bearing
contract requires a new schema version rather than changing this v1 field.

The Rust decoder deliberately retains an `EvidenceBound` enum variant so
negative tests and untrusted input can be decoded into a precise semantic
error. V1 semantic validation always rejects that variant, while the JSON
Schema is type-level closed to `contract_only`. Decoder reachability is not
claim authority.

## Terminology and Namespaces

### Product persona

A **product persona** is a user or controlling actor interacting with
FrankenTerm. It is represented by the `ProductPersona` enum and serialized in
the `persona` fields of persona definitions and coverage keys.

This term is intentionally distinct from the agent-profile **Persona** described
in `README.md` under “Agent Profiles, Personas, and Fleet Templates.” That
existing Persona is a set of behavioral defaults for a spawned coding agent.
It is not a product user role. Workload fixtures that use `persona` for
`idle_agents`, `noisy_agents`, or similar mixes are also workload classes, not
product personas.

New prose and test names must use the qualified terms `product_persona`,
`agent_profile_persona`, and `workload_class` rather than an ambiguous bare
`persona`; the versioned wire contract retains its deliberately shorter
`persona` field name.

### Qualification point

A **fleet qualification point** is an exact pane count exercised by a journey.
It is not a capacity range or a tuning profile.

### Topology and transport

A **topology** states where the authoritative mux/application work occurs. A
**transport** identifies the actual route used by that topology. A topology
result is not portable to another transport without an explicit qualifying
run.

### Journey, scenario, and proof

- A **journey** is an end-to-end user contract with setup, work, failure or
  pressure, recovery, outcome, and evidence requirements.
- A **scenario** is a bounded actor or fixture used by one or more journeys.
- A **run** is one execution of a journey against an exact candidate and target.
- **Evidence** is the retained, verifiable output from a run.
- A **support verdict** is a product decision made only after the required run,
  evidence, review, and freshness gates pass.

For readability, tables may abbreviate
`ft-interactive-swarm-product-convergence-7xqz4` as `7xqz4` and
`ft-interactive-systems-performance-4tenz` as `4tenz`. The machine catalog must
always retain complete Bead IDs.

## Independent State and Coverage Fields

Support, exact producer ownership, and target-pair qualification answer
different questions. Availability, evidence state, run outcome, and freshness
are recorded per target pair rather than as aggregate variant fields. These
axes must never be collapsed into one green/red field.

### Support axis

| Code | Catalog value | Meaning |
|---|---|---|
| `S` | `supported` | The exact cell references an externally issued and verified promotion receipt for the declared candidate, topology, transport, pane count, actor path, and required targets. |
| `C` | `conditional` | Useful surfaces or planned producers exist, but the exact cell has not satisfied every promotion gate. Conditions and missing evidence must be named. |
| `U` | `unavailable` | The journey variant is intentionally unsupported or cannot be offered on the declared product surface. A reason and safe alternative are required. |

No cell in the initial 32-cell catalog is `supported`. All are conditional.
None of the two requested mux topologies is inherently unavailable. The
reserved supported wire shape requires both `promotion_receipt_ref` and the
lowercase 64-hex `promotion_receipt_sha256`, but v1's root contract rejects
supported variants because it is permanently contract-only. The shape is
documented so later authority-bearing work cannot regress to unauthenticated
booleans. JSON Schema still cannot verify a receipt's signature, authority,
candidate binding, or contents and therefore cannot mint support.

### Target-pair qualification axes

Each cell contains exactly three `target_qualifications`, one for each Apple
controller class in the target posture. A local qualification pairs the Apple
controller with the same Apple session-host class. A Mac-to-LAN qualification
pairs that Apple controller with `trj_5995wx` as the session host. Each record
has its own availability, evidence, run verdict, freshness, route identity,
candidate identity, evidence references, and blocker references.

There is deliberately no aggregate `evidence_state`, `run_verdict`, or
`evidence_refs` on a journey variant. Combining three target results into one
field would hide unavailable targets, stale evidence, and mixed verdicts.

The target-specific evidence values mean:

| Catalog value | Meaning |
|---|---|
| `proven` | Retained evidence directly satisfies the declared evidence contract for the exact variant and target. |
| `proxy_only` | Evidence exercises a proxy rather than the complete native journey boundary. |
| `fixture_only` | Evidence establishes deterministic fixture or contract behavior only. |
| `skipped_not_proven` | A required predicate was deliberately absent; no support claim follows. |
| `blocked` | The evidence lane could not reach an admissible verdict. |
| `missing` | Required evidence has not been retained or linked. |

Evidence state is independent of support and run outcome. For example, one
target qualification may return `pass` for a reduced fixture while remaining
`fixture_only`; the variant remains conditional.

The target-specific run values mean:

| Catalog value | Meaning |
|---|---|
| `pass` | The exact declared run completed and every assertion in that run passed. Promotion may still require stronger evidence, review, freshness, and additional targets. |
| `fail` | The source or product behavior violated a required assertion. |
| `degraded` | The run completed with an admitted degraded state; the receipt must identify what remained unproved. |
| `not_run` | No qualifying run has been retained for the exact declared cell and target. |
| `target_unavailable` | The requested target could not be exercised. This says nothing by itself about product support. |

The target-specific freshness values are:

| Catalog value | Meaning |
|---|---|
| `current` | Candidate, route, configuration, renderer, hardware, and other declared qualification identities still match the receipt's freshness policy. |
| `stale` | A relevant identity or expiry boundary changed after the retained run. |
| `unknown` | The catalog has no admissible freshness receipt for this target pair. |

A target becoming unavailable changes only that target-pair qualification, not
the journey's support declaration or another pair's state.

### Producer-coverage field

| Code | Catalog value | Meaning |
|---|---|---|
| `D` | `direct` | One or more `exact_producer_bindings` name the producer Bead and this exact coverage key, actor mode, transport, controller classes, session-host classes, and source contract. This is ownership, not execution or proof. |
| `P` | `partial` | Relevant implementation, fixture, or journey work exists, but it is partial, proxy-only, or not parameterized for the exact cell. |
| `G` | `gap` | No exact field producer currently owns this cell. The gap must remain visible rather than being inferred from an adjacent cell. |

The matrix notation combines support and producer coverage. For example,
`C-D` means “conditional with an exact producer binding,” not “conditionally
proved.” A `D` cell may legitimately have no retained run, missing evidence,
and unknown freshness. Execution is represented only by its target
qualifications.

The producer arrays are structurally allow-empty so gaps can be serialized
without fabricated ownership. Semantic validation requires `D` to have at
least one matching exact binding, `P` to name at least one partial producer and
no exact binding, and `G` to have neither. Every exact binding must repeat the
variant's coverage, actor mode, and transport rather than relying on ambient
context.

### Release-requirement field

`release_requirement` is also independent:

- `required`: the declared release cannot promote while this gate or journey is
  unsatisfied;
- `optional`: reserved decoder vocabulary for a future contract that defines
  how useful non-gating coverage is authorized; and
- `excluded`: reserved decoder vocabulary for a future contract that defines
  how an omission and its reason are authorized.

Schema v1 is deliberately all-required: every gate, journey, and materialized
variant must serialize `required`. The Rust enum retains `optional` and
`excluded` only so negative tests and untrusted inputs can fail with precise
semantic errors. Neither value is valid v1 data. A future contract that admits
either value requires a new schema version and an explicit release-scope
authority rule. Marking an entry `required` still does not make it supported.

## Product Personas

The catalog maps each persona to an explicit actor mode:

| Product persona | Primary actor mode |
|---|---|
| `interactive_human` | `human_interactive` |
| `meta_agent_operator` | `meta_agent_supervised` |
| `automation_agent` | `automation_unattended` |
| `incident_responder` | `incident_response` |

### `interactive_human`

The person typing, pasting, selecting, scrolling, resizing, zooming, moving the
window between displays, reviewing output, and recovering the session. This
persona requires the native input-to-present path, coherent intermediate
frames, readable degradation, and low intervention cost.

GUI availability alone does not qualify this persona. Native AppKit input,
mux/PTY/application response, render invalidation, and actual presentation must
be correlated.

### `meta_agent_operator`

An AI or program supervising other agents through Robot Mode, MCP, mission,
transaction, policy, approval, search, and fleet-state surfaces. It requires
stable machine envelopes, bounded calls, exact pane and mux identity,
idempotency, policy receipts, and explanations suitable for another agent.

A native GUI keypress result does not qualify Robot or MCP delivery.

### `automation_agent`

A non-interactive controller executing an approved workflow, waiting on typed
events, sending bounded actions, responding to rate limits or approvals, and
recovering after partial failure. It requires deterministic behavior,
cancel-correct waits, bounded fan-out, durable receipts, replay-safe mutation,
and explicit refusal when authority or capability is absent.

### `incident_responder`

A human or automated responder diagnosing lag, failure, data gaps, resource
pressure, or recovery after the fact. It requires bounded privacy-safe
collection, exact candidate and topology identity, uncertainty labels, causal
candidates and falsifiers, portable replay, and a safe next action.

Incident collection existing in source does not by itself qualify field
diagnosis, cross-machine replay, or support handoff.

## Exact Fleet Qualification Points

| ID | Exact panes | Product purpose |
|---|---:|---|
| `q002` | 2 | Smallest useful multi-agent product fleet and regression floor. |
| `q020` | 20 | Normal daily project with approvals, search, capture, and routine pane churn. |
| `q050` | 50 | Loaded fleet with burst output, maintenance, policy activity, and protected interaction. |
| `q200` | 200 | Target mission requiring explicit resource, fairness, recovery, and long-session qualification. |

These points are not ranges. Passing `q200` cannot qualify `q050`, `q020`, or
`q002`; performance regressions at a smaller point remain release failures.

One pane remains useful as a diagnostic or subsystem baseline, but it is not the
smallest fleet in this product contract. Existing 10-pane demos and
`fleet_10`/`fleet_50`/`fleet_200_plus` tuning profiles remain named scenarios or
configuration starting points. They do not replace these exact qualification
points.

Capacity-planning bands must be non-overlapping and separately named. The
current README `51–200` and `200+` bands overlap at exactly 200 and therefore
cannot define the catalog.

## Topology and Transport Contract

### Topology `local_only`, transport `local_mux`

The GUI, mux authority, panes, and primary applications execute on the local
Mac. Loopback IPC is permitted; a remote build worker or unrelated service does
not turn the mux topology into a remote topology.

The run must record the GUI, mux server, CLI, protocol, source/build, config,
renderer, display, and application identities actually used.

Every `local_only` variant therefore serializes `transport = local_mux`.
Transport is part of the exact producer binding and every target
qualification; it is never inferred solely from the topology label.

### Topology `mac_lan_remote`, transport `remote_mux`

The local Mac owns the interactive GUI/client while the authoritative remote
mux/application path crosses the LAN to the named remote host. The expected
field target is the `trj` route where a journey names it.

Every run must record:

- the local Mac and remote-host identities;
- route and transport kind;
- interface class and whether traffic was direct LAN, Wi-Fi, or tailnet;
- mux domain and generation;
- endpoint identity;
- RTT, jitter, loss orientation, disconnects, and reconnects; and
- whether the path was a remote mux domain, an SSH domain, or another explicit
  supported route.

This topology must not be conflated with FrankenTerm’s distributed observation
feature. Distributed remote panes currently have an intentionally unavailable
live `get-text` path; that limitation neither proves nor disproves a remote mux
journey.

Every `mac_lan_remote` variant serializes `transport = remote_mux`. A route
identity receipt is required before a target qualification can be current; a
null `route_identity_ref` records that the route is unbound.

## Initial Target Posture

Target-class IDs are availability-neutral stable data identifiers.
Availability, evidence, run verdict, and freshness belong to each
`target_qualification`; words such as `available`, `unavailable`, or `unknown`
must never be encoded into an ID. The human phrase “available-unqualified”
means `availability = available` without `evidence_state = proven`; it is not
an additional machine enum.

`TargetClassDefinition` is identity and capability inventory only: ID, title,
mode, platform, hardware identity requirements, and source provenance. It
contains no availability, evidence, verdict, or freshness state. Consumers
must not infer any cell result from target inventory metadata, hardware
presence, or another qualification that happens to name the same target.

| Target-class ID | Target mode | Initial posture |
|---|---|---|
| `mac16_11_m4_pro` | `m4_pro_native` | The local Apple M4 Pro target is available. Evidence is `missing` at every exact fleet point, including q200; no qualifying run is retained and freshness is unknown. The result applies only to the recorded SKU and configuration. |
| `m5_native` | `m5_native` | Availability is `unknown`: no retained inventory receipt proves that this target is available or unavailable. Evidence is `missing`, no qualifying run is retained, freshness is unknown, and no result is inferred from M4. |
| `m5_pro_max_native` | `m5_pro_max_native` | Transitional planning identifier only. Availability is `unknown`: no retained inventory receipt proves that either M5 Pro or M5 Max is available or unavailable. Evidence is `missing`, no qualifying run is retained, and freshness is unknown. This combined ID is never support authority: `7xqz4.12.3.1` must replace it with separate M5 Pro and M5 Max classes and qualification lanes before promotion. |
| `trj_5995wx` | `threadripper_pro_5995wx_native` | The named high-core AMD session host is available for planned Mac-to-LAN runs, but availability alone cannot promote a cell. |

No architecture-specific default or support claim is inherited between these
targets. Thermal, power, display, memory, CPU topology, SMT, affinity, and
transport identity remain part of the evidence.

M5 Pro and M5 Max are materially distinct target classes. The combined
`m5_pro_max_native` v1 planning row must not let evidence from one SKU qualify
the other. Final 32-cell producer closure and exact-candidate promotion are
dependency-blocked on `7xqz4.12.3.1`, whose default resolution is separate
neutral IDs, hardware fingerprints, target pairs, evidence, verdicts, and
freshness. A later family-level alternative is admissible only under a new
signed rule backed by physical evidence from both SKUs and a conservative
intersection; v1 contains no such rule.

Every cell initially has three target-pair records:

| Topology | Qualification pairs | Initial target-specific state |
|---|---|---|
| `local_only` | `mac16_11_m4_pro`→itself; `m5_native`→itself; `m5_pro_max_native`→itself | Every M4 Pro point: `available` / `missing` / `not_run` / `unknown`. Each M5 class: `unknown` / `missing` / `not_run` / `unknown`. |
| `mac_lan_remote` | each of the three Apple controller IDs → `trj_5995wx` | Every M4 Pro+`trj` point: `available` / `missing` / `not_run` / `unknown`. Each M5+`trj` pair: `unknown` / `missing` / `not_run` / `unknown`. |

The four slash-separated states are availability, evidence state, run verdict,
and freshness respectively. Initial route and candidate identity references
are null; retained evidence-reference and blocker-reference arrays are empty.
No M5 inventory receipt or exact q200 journey receipt is present. Absences are
data, not implied inheritance.

`docs/attestations/proofs/resource-cockpit-target-class.json` is a narrower
Linux high-core resource-cockpit result. It is not scoped to any exact
persona/topology/fleet-point/controller/session-host/candidate/route cell in
this catalog. It therefore cannot change an M4 q200 qualification from
`missing`, establish M5 availability, populate an exact cell's evidence or
blocker references, or be inferred across cells.

## Initial 32-Cell Coverage Matrix

All cells are conditional. Their three target-pair qualifications carry the
target-specific initial states shown above; there is no aggregate cell run
verdict.

| Product persona | `local_only` q002 | `local_only` q020 | `local_only` q050 | `local_only` q200 | `mac_lan_remote` q002 | `mac_lan_remote` q020 | `mac_lan_remote` q050 | `mac_lan_remote` q200 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `interactive_human` | C-D | C-D | C-G | C-G | C-D | C-D | C-D | C-D |
| `meta_agent_operator` | C-G | C-P | C-P | C-G | C-G | C-P | C-P | C-G |
| `automation_agent` | C-G | C-P | C-P | C-G | C-G | C-P | C-P | C-G |
| `incident_responder` | C-P | C-P | C-P | C-P | C-P | C-P | C-P | C-P |

Direct producer coverage comes from:

- `.11.2`, which names two-agent local and Mac-to-`trj` work;
- `.11.3`, which names twenty-pane local or low-latency remote work;
- `.11.4`, which names the fifty-pane Mac-to-`trj` loaded fleet; and
- `.11.5`, which names the two-hundred-pane target-class mission.

The matrix intentionally exposes these producer gaps:

- no exact local-only q050 interactive field journey;
- no exact local-only q200 interactive field journey;
- no meta-agent/operator journey parameterized over all eight topology/count
  cells;
- no automation journey parameterized over all eight cells; and
- no incident journey binding all four counts and both topologies to exact
  privacy-safe diagnosis and recovery assertions.

`7xqz4.11.15` owns closure of these gaps. It must bind every one of the 32 exact
cells to a parameterized producer; separately, every required target-pair
qualification must retain its own run before final support promotion. The 14
field journeys below remain its prerequisite producer set.

The catalog may add producers without changing a cell’s support state. Support
changes only through the promotion procedure below.

## Field-Journey Crosswalk

The 14 field journeys are the required end-to-end producer set. Their Bead IDs
are stable dependency references; they are not pass receipts.

| Bead | Journey contract | Primary catalog coverage |
|---|---|---|
| `7xqz4.11.1` | Clean-Mac first hour from packaged install through launch, setup, useful work, relaunch, and safe machine control. | Installation prerequisite for all personas; primarily interactive q002 local. |
| `7xqz4.11.2` | Two-agent everyday project for four hours with impeccable interaction. | Interactive q002 local and Mac→LAN. |
| `7xqz4.11.3` | Twenty-pane local or low-latency swarm with normal coding and approvals. | Interactive q020 local and Mac→LAN; partial meta-agent and automation coverage. |
| `7xqz4.11.4` | Fifty-pane Mac-to-`trj` loaded fleet with a protected interactive pane. | Interactive Mac→LAN q050; partial meta-agent, automation, and incident coverage. |
| `7xqz4.11.5` | Two-hundred-pane target-class mission with 4h, 24h, and selected 72h proof. | Interactive Mac→LAN q200 plus long-session/resource qualification. |
| `7xqz4.11.6` | Rate-limit, compaction, approval, Verified Submit, deduplication, and attention day. | Meta-agent/operator and automation behavior, principally q020/q050. |
| `7xqz4.11.7` | Concurrent search, index, backup, GC, WAL, rules, and incident maintenance day. | Background fairness and incident response, principally q020/q050. |
| `7xqz4.11.8` | Remote host unavailable at launch and later returns. | Mac→LAN failure/recovery variants across personas. |
| `7xqz4.11.9` | Laptop LAN/Wi-Fi/tailnet roam plus sleep and wake. | Mac→LAN transport changes, reconnect, resync, and exact final intent. |
| `7xqz4.11.10` | Live component update, watcher handoff, candidate failure, and rollback. | Candidate identity and continuity across all personas. |
| `7xqz4.11.11` | Crash and recover GUI, watcher, mux, renderer, storage, and connections. | Incident response and continuity across both topologies. |
| `7xqz4.11.12` | Keyboard-only, VoiceOver, reduced-motion, and low-vision operator day. | Interactive accessibility requirements across supported GUI cells. |
| `7xqz4.11.13` | Field lag report through privacy-safe remote diagnosis and replay. | Incident responder cells, diagnostic evidence, and support handoff. |
| `7xqz4.11.14` | Version-pinned Codex, Claude, Gemini, and supported-agent compatibility dogfood. | Real-agent compatibility overlay for interactive, meta-agent, and automation cells. |

A journey covering several facets does not automatically upgrade every related
cell to `D`. Direct coverage requires an `exact_producer_binding` whose Bead
contract names the exact persona, topology, transport, qualification point,
actor mode, and controller/session-host target sets. No retained execution is
required to record that ownership. Runs, evidence, and freshness remain
separate target-qualification facts.

## Required Journey Lifecycle

Every journey instance must define and retain the following phases.
To preserve the v1 wire shape, the machine catalog maps conceptual phases 1
and 2 into one ordered `setup` array: `setup[0]` is always the pre-mutation
identity/preflight boundary, and `setup[1..]` contains clean setup. Semantic
validation requires both portions; consumers must not treat the whole array as
undifferentiated setup.

### 1. Identity and preflight

- exact packaged candidate and source/build identity;
- GUI, CLI, mux-server, protocol, config, schema, and renderer compatibility;
- persona, topology, transport, q-point, target, display, power, and network
  state;
- feature and policy prerequisites;
- proof capability and instrumentation-overhead state; and
- typed `conditional`, `unavailable`, or `target_unavailable` exits before
  mutation when a prerequisite is absent.

### 2. Clean setup

- installation or launch state;
- configuration provenance;
- workspace/project preparation;
- pane and actor creation;
- deterministic seeds and pinned real-agent versions where applicable;
- accessibility settings; and
- a bounded intervention count.

“Clean workspace bootstrap” is not synonymous with “clean Mac installation.”
The journey must state which boundary it exercises.

### 3. Steady work

- representative typing, paste, mouse, IME, Robot, MCP, workflow, search,
  capture, storage, and approval activity for the persona;
- realistic foreground and background output;
- exact correctness and SLO assertions;
- progress/fairness for the focused pane and fleet; and
- bounded metrics that do not capture pane contents by default.

### 4. Overload or failure

At least one declared pressure or failure transition must be exercised where
the journey requires it: output burst, queue saturation, disconnect, remote
unavailability, component crash, storage contention, update failure, resource
pressure, sleep/wake, display change, or policy/approval blockage.

The expected degraded state, permitted deferral order, reason code, and
forbidden behavior must be declared before the run.

### 5. Recovery and convergence

- no ambiguous key or action replay;
- no duplicate, reordered, or silently discarded final intent;
- generation-safe resync;
- visible progress and bounded retry/backoff;
- authoritative terminal, storage, workflow, and geometry convergence;
- retained explicit gaps where continuity was not guaranteed; and
- a safe next action when full recovery is impossible.

### 6. Teardown and outcome

- cancel-correct shutdown;
- no stranded process, lock, subscription, queue, or temporary profile;
- retained artifacts and hashes;
- resource and latency slope verdicts where applicable;
- intervention count and elapsed time;
- user-visible outcome; and
- an exact run verdict without promotion by implication.

## Appearance and Text Contract

Interactive journeys involving a GUI must exercise both intermediate and final
presentation for:

- continuous resize and resize storms;
- zoom and font-scale changes;
- DPI and display migration;
- 60 Hz and 120 Hz presentation where the target supports them;
- cursor, selection, hyperlinks, underlines, tabs, splits, and scroll position;
- Unicode, emoji, combining marks, wide glyphs, bidi/RTL, and IME composition;
- ligatures and fallback fonts;
- images and other non-text cells where supported;
- glyph-cache warm and cold behavior; and
- final authoritative geometry and text reflow.

Passing a headless grid calculation or dirty-row benchmark is not equivalent to
a correctly presented native frame. Critical intermediate-frame defects block
the journey even if the final frame eventually converges.

Human visual review may supplement deterministic comparison, but it must use a
declared corpus and retained review record. Machine replay cannot fabricate a
human-locked visual or accessibility judgment.

## Accessibility Contract

Accessibility is an orthogonal requirement, not a fifth persona. Applicable
journeys must cover:

- keyboard-only navigation and operation;
- visible, stable focus;
- VoiceOver names, roles, values, ordering, announcements, and geometry;
- reduced-motion behavior;
- low-vision zoom, contrast, cursor, selection, and focus visibility;
- IME and international text input;
- dialog, command-palette, tab, split, and pane traversal; and
- equivalence of recoverability and policy feedback without relying solely on
  color, animation, or pointer input.

The existing `docs/a11y/scenario-corpus.md` is a proof substrate, not an
end-to-end support verdict. `.11.12` owns the field-level accessibility day.

## Privacy and Evidence Contract

Evidence must be useful for diagnosis without becoming a second terminal
transcript store.

By default, artifacts may retain:

- correlation and stable redacted journey IDs;
- durations, counts, queue ages, sizes, hashes, reason codes, and type names;
- candidate, config, protocol, renderer, display, route, and target identities;
- resource, frame, latency, network, and recovery measurements;
- content classifications and redaction receipts; and
- bounded causal candidates, confidence, and falsifiers.

Raw key contents, pasted text, pane payloads, secrets, tokens, credentials,
private file paths, and unredacted agent conversations are excluded by default.
Any content-bearing collection requires explicit scope, redaction tier,
retention, access authority, and expiry.

Reference fields have distinct authority:

- `source_refs` are nonempty references to the source contracts that define a
  catalog object. They establish provenance and intended meaning, but are never
  run evidence.
- `evidence_refs` are allow-empty references to retained run or gate receipts.
  An empty array means no evidence is linked.
- `blocker_refs` are allow-empty references to retained target-unavailability,
  preflight, or proof-blocker receipts. An empty array means no blocker receipt
  is linked; prose alone does not manufacture one.
- `resolution_refs` are allow-empty retained receipts for a structured
  contradiction. A contradiction marked `resolved` needs externally checked
  resolution authority; schema-valid paths alone do not prove resolution.
- nullable `candidate_identity_ref` and `route_identity_ref` values record
  whether the exact candidate and route were bound. Null means unbound, not
  “use the ambient checkout” or “infer the usual LAN path.”

### Digest domains and self-reference

The v1 catalog contains no full-file self-hash. Receipt digests always bind an
external immutable receipt:

- `authority_receipt_sha256` is SHA-256 over the exact raw bytes at
  `authority_receipt_ref`, with no newline, Unicode, JSON, or path
  normalization. The detached receipt must itself bind the review ID, catalog
  revision, non-null immutable reviewed commit, authority kind, disposition,
  and scope.
- `promotion_receipt_sha256` is SHA-256 over the exact raw bytes at
  `promotion_receipt_ref` under the same no-normalization rule. The detached
  receipt must itself bind the claim ID, exact candidate, coverage, transport,
  controller/session-host pairs, qualifications, gates, and authorizing
  release decision.
- `claim_sha256` is SHA-256 over the UTF-8 bytes of the decoded `claim_text`
  string exactly as serialized semantically, excluding JSON quotes and escape
  syntax and with no whitespace or newline normalization.

Thus a digest stored inside the catalog never claims to hash the catalog bytes
that contain it. The current informational review has null receipt fields and
a null reviewed commit. If a future contract needs a digest of catalog content
it must define a versioned detached canonical projection (excluding at least
`review_history` and `change_history`) in a new authority contract; v1 does not
silently invent such a projection.

Every evidence bundle must state:

- what was measured directly;
- what was a proxy;
- what was unavailable or skipped;
- instrumentation and sampling overhead;
- data-loss or explicit-gap state;
- target and candidate freshness;
- artifact hashes and replay command; and
- whether human review remains required.

An incident bundle being collectable does not imply that the canonical
privacy-safe schema, diagnosis, or cross-machine replay has qualified.

## Dependency and Evidence Map

| Contract facet | Required owners or evidence |
|---|---|
| Catalog and status authority | `7xqz4.1.1`; SLO catalog `.1.2`; degradation taxonomy `.1.3`; evidence schema `.1.4`; README mapping `.1.5`; privacy `.1.6`; maturity ladder `.1.7`. |
| Exact installed identity | `7xqz4.2.1`–`.2.3`, `.2.9`; `4tenz.9.1`; existing mismatch-resolution work such as `ft-1itzl`. |
| Native human interaction | `4tenz.2.1`–`.2.8`; product interaction `.4.2`–`.4.6`; product quality `.9.1`–`.9.9`. |
| Resize, zoom, and appearance | `4tenz.3.1`–`.3.8`, `.7`, `.8`; product `.9.1`–`.9.9`; `docs/resize-baseline-scenarios.md` only as a deterministic substrate. |
| q050/q200 fairness | `4tenz.5`, `4tenz.6`; product workload lab `7xqz4.3`; journeys `.11.4` and `.11.5`. |
| Four-/24-/72-hour stability | `4tenz.4.1`–`.4.9`; product resources `.10`; journeys `.11.2` and `.11.5`. |
| Meta-agent and automation | Existing Robot golden work `ft-0elb9`; product `.4.7`; journeys `.11.6` and `.11.14`; an explicit all-cell producer remains missing. |
| Incident response | Privacy `.1.6`; diagnostics `.7.1`–`.7.8`; continuity `.8`; journeys `.11.11` and `.11.13`; canonical incident-schema adoption work such as `ft-x8e67`. |
| Target-class qualification | `4tenz.9.3`–`.9.7`; product `.10.6` and `.10.9`; M5 Pro/Max authority split `.12.3.1`; existing target gate `ft-tf6g3.14`. |
| Exact 32-cell producer closure | `7xqz4.11.15`, blocked on `.11.1`–`.11.14`; every cell must retain its own parameterized run and may not inherit an adjacent result. |
| Final promotion | Exact field journey; 32-cell closure `.11.15`; signed promotion-receipt contract and evidence work `.1.4`; unified verdict `.12.1`; target matrix `.12.3`; derived README claims `.12.7`; final exact-candidate authority `.12.9`. |

Existing artifacts retain their narrower boundaries:

- `docs/demo-scenarios.md` is onboarding and deterministic regression coverage,
  not target-capacity proof.
- `docs/high-scale-operator-rehearsals.md` is bounded rehearsal evidence and may
  correctly report `SKIPPED_NOT_PROVEN`.
- `docs/swarm-capacity-simulation-corpus.md` is synthetic capacity behavior.
- `docs/ft-xbnl0-5-4-operator-acceptance-scenarios.json` covers legacy operator
  guidance, not this complete product matrix.
- `docs/attestations/proofs/resource-cockpit-target-class.json` currently keeps
  a narrower Linux high-core resource-cockpit result fail-closed. It is not an
  exact-cell M4 q200 receipt, an M5 inventory receipt, or authority for any
  persona/topology/candidate/route qualification, and must not be inferred
  across those boundaries.
- `docs/perf/resize-quality-slo.json` contains useful benchmark/proxy lanes but
  does not, by itself, provide native input-to-present or presented-resize proof.

README mappings bind the exact derived `claim_text` together with a lowercase
SHA-256 digest in `claim_sha256`; a line-number reference alone is too unstable
to prove which wording was reviewed. Changing the claim text invalidates that
mapping until both text and digest are refreshed and re-reviewed.

## Known Claim and Contract Contradictions

These are required top-level `contradictions` records, not prose observations
to silently normalize away. Each begins with `status = open`, nonempty tracking
Bead and source references, an explicit `blocks_all_claims` decision,
allow-empty affected-claim IDs, and an empty `resolution_refs` array. Empty
affected-claim IDs means that exact claim bindings still need to be enumerated;
it never means “affects nothing.”

| Contradiction ID | Status | Blocks all claims? | Contradiction and current truth | Required resolution |
|---|---|---:|---|---|
| `contradiction.readme_200_plus_scope` | `open` | No | README says FrankenTerm operates fleets of `200+`, while q200 is an exact qualification point and cannot support open-ended `200+`. | Narrow derived wording to exact qualified scope or add and pass an above-200 journey before restoring `200+`. |
| `contradiction.readme_lossless_capture` | `open` | Yes | README says FrankenTerm “captures every byte ... across every pane,” but capture paths can record explicit gaps and native-event truncation/gap proof remains unfinished. | Say that deltas are captured and explicit gaps recorded until a stronger invariant is proved. |
| `contradiction.readme_capacity_overlap` | `open` | No | README bands `51–200` and `200+` overlap at exactly 200 and are not qualification points. | Define non-overlapping sizing bands separately from q002/q020/q050/q200. |
| `contradiction.clean_first_hour_gap` | `open` | No | The product contract requires an atomic clean first hour, while the GUI guide requires manual config copying plus separate watcher and GUI launch. | Keep `.11.1` conditional until the exact packaged setup and relaunch journey passes. |
| `contradiction.legacy_rch_local_fallback` | `open` | Yes | Legacy OA JSON permits local fallback when RCH cannot remain remote, while current proof policy fails closed. | Correct the legacy contract before importing its evidence; never count local Cargo output as remote proof. |
| `contradiction.performance_q001_product_q002` | `open` | No | The performance workload uses `1, 20, 50, 200`, while the product contract requires `2, 20, 50, 200`. | Keep one pane as a subsystem diagnostic and q002 as the product floor. |
| `contradiction.persona_namespace_collision` | `open` | Yes | README's agent-profile “Persona” shares a word with the four product personas even though they are different dimensions. | Use qualified names and never join the dimensions by a bare `persona` label. |
| `contradiction.remote_path_conflation` | `open` | No | Distributed-mode remote panes and Mac-to-LAN remote mux paths can both be called “remote,” despite different capabilities and input/read paths. | Bind explicit topology and transport in every producer, qualification, and artifact. |

No claim may promote while an open contradiction either names that claim in
`affected_claim_ids` or has `blocks_all_claims = true`. Changing `status` to
`resolved` requires a later contract with a content-bound resolution receipt
and the appropriate human or release authority. V1 has no such receipt field
or verifier, so both its JSON Schema and Rust semantic validator reject every
`resolved` record even when a repository path is present. In v1, all eight
canonical contradictions remain `open` and carry empty `resolution_refs`.

## Review Record

Review record ID: `ft.product-journey-review.2026-07-27.initial`

| Field | Value |
|---|---|
| `reviewed_at_utc` | The exact timestamp serialized in canonical `YYYY-MM-DDTHH:MM:SSZ` UTC form; offsets, fractional seconds, and impossible calendar dates are rejected. |
| `reviewed_catalog_revision` | The exact catalog revision reviewed, not an ambient latest revision. |
| `reviewed_commit` | `null`; the draft catalog was uncommitted, so the shared source baseline `df4414f5587cccface7edebdee6028ae758f82f8` cannot honestly identify the reviewed catalog bytes. |
| `reviewer` | Codex systems architecture review. |
| `authority_kind` | `automated_informational` |
| `disposition` | `informational` |
| `scope` | `Personas, exact fleet points, mux topologies, target posture, 32-cell coverage, 14 field journeys, lifecycle, visual, accessibility, privacy, dependencies, and known claim drift.` |
| `authority_receipt_ref` | `null` |
| `authority_receipt_sha256` | `null` |
| `notes` | `AI-authored initial informational review. Commit df4414f5587cccface7edebdee6028ae758f82f8 is only the shared source baseline and does not contain these catalog bytes. Human product-owner approval and later human-locked visual and accessibility reviews remain pending; no cell is supported.` |
| `source_refs` | `AGENTS.md`; `README.md`; `.beads/issues.jsonl`; `docs/design/product-journey-contract.md`; `docs/json-schema/ft-product-journey-catalog.json`; `docs/perf/mux-long-session-performance-campaign.md`; `docs/ft-xbnl0-5-4-operator-acceptance-scenarios.json`; `docs/demo-scenarios.md`; `docs/high-scale-operator-rehearsals.md`; `docs/a11y/scenario-corpus.md`; `docs/perf/target-class-hardware.md`; `docs/ft-xbnl0-5-5-closure-metadata.json`. |

This is an informational automated review because the reviewer has no delegated
`human_product_owner`, `human_visual`, `human_accessibility`, or
`human_privacy` authority and produced no authority receipt. Schema validation
must not upgrade it to `approved`.

V1 deliberately rejects every `approved` or `changes_requested` record,
including a perfectly shaped record pointing at real repository bytes. A path,
hash, reviewer string, and claimed authority kind are not a root of trust.
Later human decisions require a new schema/contract version with a trusted
signer registry and a verified detached signature binding exact catalog
revision, commit, scope, disposition, reviewer identity, and receipt bytes.
Until that verifier exists, human decisions remain external pending authority
and cannot be encoded as catalog approval.

Approval scopes are exact and non-interchangeable:
`human_product_owner` owns `catalog_contract`, `human_visual` owns
`visual_quality`, `human_accessibility` owns `accessibility`, and
`human_privacy` owns `privacy`. One authority cannot approve another scope.
Product-owner approval and human visual/accessibility/privacy judgments remain
pending.

## Change History

| Contract version | Date | Change | Authority |
|---|---|---|---|
| `ft.product_journey_catalog.v1` / companion revision 1 | 2026-07-27 | Initial four-persona, two-topology, q002/q020/q050/q200 contract; 32 conditional cells; 14-journey crosswalk; fail-closed target posture and promotion rules. | `7xqz4.1.1`; human approval pending. |
| `ft.product_journey_catalog.v1` / companion revision 2 | 2026-07-28 | Retained revision 1 unchanged; added revision-bound review/change history, fail-closed human-authority rejection, candidate/route identity requirements, a frozen transitional combined M5 Pro/Max lane, and an enforced `setup[0]` identity/preflight boundary. | Automated adversarial review only; human approval pending. |

History is append-only. Corrections add a new row and catalog revision; they do
not rewrite the fact that an earlier revision existed. Every review names a
retained change-history revision, and the current revision has its own review;
historical reviews are never rewritten to the ambient current revision.
Breaking field or semantic changes require a new schema/contract version.

## Promotion and Update Procedure

Each cell promotes independently:

1. Resolve the exact cell ID and confirm persona, topology, transport, q-point,
   actor mode, controller/session-host target pairs, display, route, feature
   set, and actor versions.
2. Confirm that `.11.15` records a matching `exact_producer_binding`. Adding
   that binding may move producer coverage from `G` or `P` to `D`, but records
   ownership only; it neither runs the journey nor changes support.
3. For each of the cell's three target pairs, bind
   `candidate_identity_ref`, `route_identity_ref`, and transport. A null
   identity leaves freshness `unknown` and cannot be inferred from ambient
   checkout or network state.
4. Run the complete lifecycle against the exact packaged candidate for each
   required target pair. Local Cargo output, a source scan, or a neighboring
   cell cannot replace the required run.
5. Retain bounded structured artifacts with hashes, privacy classification,
   sampling/overhead state, and offline replay instructions. Set each target
   qualification's evidence state independently to `proven`, `proxy_only`,
   `fixture_only`, `skipped_not_proven`, `blocked`, or `missing`.
6. Apply the frozen SLO, correctness, recovery, resource, appearance,
   accessibility, and intervention gates. Record each target-specific
   `pass`, `fail`, `degraded`, `not_run`, or `target_unavailable` verdict
   without translating it into support.
7. Verify target-specific freshness against candidate, config, protocol,
   renderer, display, hardware, route, topology, and real-agent versions.
   Record `current`, `stale`, or `unknown`; never hide mixed states in an
   aggregate variant field.
8. Obtain every required machine and human review. Visual, accessibility,
   privacy, and product-owner authority remains human where the contract
   requires it. Each review binds revision, commit, scope, authority kind, and
   authority receipt.
9. Resolve or explicitly scope every open structured contradiction and run the
   schema, completeness, unique-ID, cross-reference, target-pair,
   contradiction, digest, and unsupported-case validators.
10. Ask `.1.4` and `.12.1` to construct the signed promotion input only after
    exact producer binding and every required current, proven, passing
    target-pair qualification exist. `.12.9` remains the authority that
    verifies the exact-candidate receipt and decides whether release/maturity
    policy permits the claim.
11. If authority promotes the cell, retain the signed receipt externally and
    move the claim into a later authority-bearing schema/version. That contract
    may serialize the reserved `supported` shape only with
    `promotion_receipt_ref` and its lowercase SHA-256 digest. V1 must remain
    conditional and `catalog_claim_state = contract_only`; it is an input and
    historical contract record, not the promotion output.
12. Update derived README, GUI guide, playbook, attestation, and release-note
    wording only from the later receipt-authorized claim; append v1 review and
    change history with exact commit and artifact identities. Derived prose
    must never lead authority-bearing catalog data.

Any candidate, config, protocol, renderer, display, hardware, route, topology,
privacy policy, SLO, or supported-agent-version change that can affect a
journey changes its target qualification to `stale` until requalification.

A failed run demotes or blocks its exact claim. A missing target records
`target_unavailable`. Neither result may be hidden by a passing adjacent cell.
