# Recorder Event Schema v1

**Bead:** `ft-oegrb.2.1`  
**Status:** Draft contract for implementation work

## Purpose

This defines the canonical event envelope for mux ingress/egress capture and
operational swarm evidence so producer and consumer implementations can be
built independently while preserving replay ordering, filterability, and
causal traceability.

Primary artifact:
- `docs/flight-recorder/ft-recorder-event-v1.json`

Related policy contracts:
- `docs/flight-recorder/capture-redaction-policy.md` (capture/projection/response redaction boundaries)

## Event Families

The v1 contract supports exactly four event families:

1. `ingress_text` - text or action injected into mux
2. `egress_output` - mux output content segments (including explicit gaps)
3. `control_marker` - non-text control markers (resize, approval checkpoints, etc.)
4. `lifecycle_marker` - capture lifecycle boundaries (start/stop/open/close/replay)

## Required Metadata

All events carry the same canonical metadata envelope:

- `schema_version`
- `event_id`
- `pane_id`
- `session_id`
- `workflow_id`
- `correlation_id`
- `source`
- `occurred_at_ms`
- `recorded_at_ms`
- `sequence`
- `causality` (`parent_event_id`, `trigger_event_id`, `root_event_id`)

This ensures all captured artifacts can be filtered and causally reconstructed even when
variant payloads differ.

## Operational Causal Envelope

Mux events use `ft.recorder.event.v1`. Cross-system incident reconstruction
uses the companion `ft.swarm.causal_event.v1` contract in
`frankenterm_core_replay_types::swarm_causal_event`. That envelope covers
pane, robot, MCP, workflow, policy, Beads, RCH, Agent Mail, git, operator,
runtime, and unavailable-source events. Every causal event carries:

- stable event id and schema version
- source kind and causal class
- occurred and ingested timestamps
- monotonic ingest sequence
- parent / caused-by / root event links
- workspace, pane, session, workflow, bead, thread, RCH build/worker, git,
  and command correlation keys
- redaction status, payload sensitivity, retention class, payload byte count,
  and payload hash
- bounded structured payload plus artifact references

Validation fails closed for oversized payloads, self-causality, missing
source-required correlation keys, empty artifact URIs, unavailable-source
events without reasons, and secret-bearing payloads that have not been
redacted, hash-only recorded, or marked unavailable.

## Evolution Rules

### Additive-compatible (within v1)

- New optional fields may be added.
- New optional nested keys may be added under `details`.
- New `source` enum values may be added for operational producers when the
  event family and ordering semantics remain unchanged.
- Existing required fields and semantics remain stable.

### Breaking (requires new schema version)

- Removing or renaming required fields.
- Changing the meaning or type of existing fields.
- Adding a new `event_type` variant.
- Changing causal/ordering semantics (`sequence`, `causality` model).

### Reader policy

- Readers **must reject** unknown `schema_version` values by default.
- Readers may provide compatibility shims only when explicitly configured.
