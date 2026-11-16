# Golden Artifact Provenance

This directory holds frozen byte outputs for serde-serialized types that are
load-bearing on the wire or in persisted state. Any accidental change to a
struct's field order, rename, or type that shifts the serialized bytes must
fail the golden suite.

## Why this exists

- `wire_protocol.rs` types are the distributed-mode protocol between
  `wa-agent` and the aggregator. A schema drift that is not caught at compile
  time (e.g. adding a new field with a default, renaming with `rename =`,
  changing tag layout) silently breaks deployed agents.

- `snapshot_engine.rs` types are persisted into SQLite as JSON blobs. A rename
  or reorder breaks replay of older sessions.

- **Varbincode types (in `frankenterm/codec`) are positional.** They are
  covered by `frankenterm/codec/tests/conformance_pdu_wire.rs`; this suite
  does not duplicate that coverage. Per project memory: **never add
  `#[serde(skip_serializing_if = ...)]` to a varbincode-serialized struct.**
  Goldens here are intentionally JSON because that is the serializer used by
  the in-scope `wire_protocol.rs` and `snapshot_engine.rs`.

## Layout

```
tests/golden/
├── PROVENANCE.md                  (this file)
├── wire_protocol/
│   ├── envelope_pane_meta.json
│   ├── envelope_pane_delta.json
│   ├── envelope_gap.json
│   ├── envelope_detection.json
│   └── envelope_panes_meta.json
└── snapshot/
    └── snapshot_triggers.json
```

## Regenerating

```bash
UPDATE_GOLDENS=1 cargo test -p frankenterm-core --test golden_integration
git diff crates/frankenterm-core/tests/golden/  # REVIEW every change
git add crates/frankenterm-core/tests/golden/
git commit -m "goldens: <why the change>"
```

Never regenerate without reading the diff. A golden change without
review is a schema migration without review.

## When a golden fails

If a test fails with `GOLDEN MISMATCH`, the options are:

1. **The change is a bug** — fix the code, re-run, golden should match again.
2. **The change is intentional** — the schema IS changing. Regenerate, diff,
   commit. If this is a wire-protocol change, bump `PROTOCOL_VERSION` in
   `wire_protocol.rs`. If this is a snapshot schema change, add migration.
3. **The change is a formatting drift** (whitespace, key order from
   HashMap non-determinism) — fix the constructor to use deterministic
   inputs (BTreeMap, sorted Vec) so the golden stays stable.

`serde_json::to_vec` produces compact JSON with map keys in insertion order.
If a struct contains `HashMap` fields, replace with `BTreeMap` in tests OR
canonicalize before compare. The goldens below assume all map-shaped fields
are either absent or populated deterministically.
