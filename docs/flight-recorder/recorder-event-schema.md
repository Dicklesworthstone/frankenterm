# Recorder Event Schema v1

**Bead:** `ft-oegrb.2.1`  
**Status:** Draft contract for implementation work

## Purpose

This defines the canonical event envelope for mux ingress/egress capture so producer and
consumer implementations can be built independently while preserving replay ordering,
filterability, and causal traceability.

Primary artifact:
- `docs/flight-recorder/ft-recorder-event-v1.json`

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

## Evolution Rules

### Additive-compatible (within v1)

- New optional fields may be added.
- New optional nested keys may be added under `details`.
- Existing required fields and semantics remain stable.

### Breaking (requires new schema version)

- Removing or renaming required fields.
- Changing the meaning or type of existing fields.
- Adding a new `event_type` variant.
- Changing causal/ordering semantics (`sequence`, `causality` model).

### Reader policy

- Readers **must reject** unknown `schema_version` values by default.
- Readers may provide compatibility shims only when explicitly configured.
