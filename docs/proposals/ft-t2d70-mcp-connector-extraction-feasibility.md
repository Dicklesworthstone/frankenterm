# ft-t2d70 — `frankenterm-core-mcp` + `frankenterm-core-connectors` extraction feasibility

**Bead:** ft-t2d70 = ft-y0loj.5 (tier-2 entry)
**Status:** **PARK** until prerequisite leaf-types extractions land
**Predecessors:** ft-y0loj.1 (tantivy ✓), ft-y0loj.2 / ft-mr35k (ars ✓ — cc7 lane), ft-y0loj.3 (fleet partial ✓), ft-y0loj.3.1 / ft-usvnt (resource-types ✓), ft-y0loj.6 / ft-l3tfo (cold-build ADR ✓)
**Follow-up filed:** ft-yf2am (ft-y0loj.3.2)

---

## TL;DR

Both clusters are **cycle-blocked at the same place** as `frankenterm-core-fleet`
was in ft-y0loj.3:

- **mcp/\*** (12 files, ~21K LOC): the cluster has *zero in-core reverse-deps*
  but every file references `frankenterm_core::config::*`, `::error::*`,
  `::policy::*`, `::cass::*`, `::caut::*`, etc. — about 250 cross-cluster
  refs to 16 distinct core modules. Making the new crate a path-dep on
  `frankenterm-core` is unavoidable; making `frankenterm-core` an optional
  back-dep on the new crate (so existing call sites + external tests keep
  resolving) re-creates the cargo cycle.
- **connector_\*** (14 files, ~22K LOC): hits the *same* cycle plus four
  in-core importers (`policy.rs`, `config.rs`, `chaos_scale_harness.rs`,
  `runtime_telemetry.rs`) that consume connector types substantively. Cannot
  move without splitting or relocating those importers.

The unblocker is identical to fleet's: **extract the leaf "core types" crates
first**, then re-attempt mcp/connector. The extraction proven by ft-usvnt
(resource-types) is the template.

## Survey of the mcp/\* cluster

```
3554  mcp.rs              (#[path]-aggregator for 7 inner submods)
 258  mcp_bridge.rs       (#[path] inside mcp.rs)
1062  mcp_client.rs       (top-level, mutually-referential with framework)
 468  mcp_error.rs        (top-level)
 445  mcp_framework.rs    (top-level, mutually-referential with client)
1291  mcp_helpers.rs      (ORPHANED — not declared in lib.rs or mcp.rs)
 514  mcp_middleware.rs   (#[path] inside mcp.rs)
1706  mcp_missions.rs     (#[path] inside mcp.rs)
 914  mcp_proxy.rs        (#[path] inside mcp.rs, gated mcp-client)
 898  mcp_resources.rs    (#[path] inside mcp.rs)
8484  mcp_tools.rs        (#[path] inside mcp.rs)
2924  mcp_types.rs        (#[path] inside mcp.rs)
─────
21518 total
```

**Reverse-dep check:** zero non-mcp in-core files import `crate::mcp_*`.
External workspace: only `crates/frankenterm/src/mcp.rs` calls
`frankenterm_core::mcp::run_stdio_server`. Tests + benches in
`crates/frankenterm-core/tests/` use `frankenterm_core::mcp_framework::*`
patterns (~36 occurrences).

**Forward deps (cross-cluster):**

| Module                   | crate::* refs |
| ------------------------ | ------------- |
| `crate::plan`            | 73            |
| `crate::storage`         | 70            |
| `crate::wezterm`         | 32            |
| `crate::error`           | 20            |
| `crate::config`          | 19            |
| `crate::policy`          | 12            |
| `crate::Result`          | 6             |
| `crate::caut`            | 5             |
| `crate::cass`            | 3             |
| (plus 8 more, ≤2 each)   | ~10           |

## Survey of the connector_\* cluster

```
2017  connector_bundles.rs
2077  connector_credential_broker.rs
2209  connector_data_classification.rs
1317  connector_event_model.rs
2160  connector_governor.rs
1681  connector_host_runtime.rs
1574  connector_inbound_bridge.rs
2143  connector_lifecycle.rs
1359  connector_mesh.rs
2672  connector_outbound_bridge.rs
1836  connector_registry.rs
1349  connector_reliability.rs
2904  connector_sdk.rs
1187  connector_testbed.rs
─────
22485 total
```

**Reverse-dep check (THE BLOCKER):**

```
crates/frankenterm-core/src/policy.rs        — 8 connector_* type refs
crates/frankenterm-core/src/config.rs        — 10 connector_* type refs
crates/frankenterm-core/src/chaos_scale_harness.rs — 2 connector_* refs
crates/frankenterm-core/src/runtime_telemetry.rs   — 6 connector_* refs
```

`policy.rs` (the hottest reverse-dep) carries fields like:

```rust
pub credential_broker: crate::connector_credential_broker::CredentialBrokerTelemetrySnapshot,
pub connector_governor: crate::connector_governor::GovernorSnapshot,
pub connector_registry: crate::connector_registry::RegistryTelemetrySnapshot,
// ...etc.
```

— substantive consumption, not just a façade. Same pattern in `config.rs`.

## What was actually attempted under ft-t2d70

A foundational 3-file slice was prototyped:

1. Created `crates/frankenterm-core-mcp/` with `mcp` + `mcp-client` features
   matching the parent's feature triad.
2. `git mv`'d `mcp_error.rs` (468 LOC) + `mcp_framework.rs` (445 LOC) +
   `mcp_client.rs` (1062 LOC) — the smallest mutually-coherent subset
   (`framework` ↔ `client` are mutually-referential, and `error` is a leaf
   they both depend on).
3. Sed-rewrote 5 cross-cluster paths (`crate::cass`/`caut`/`error`/`config`/
   `policy` → `frankenterm_core::*`).
4. Wired the workspace member + made `frankenterm-core` carry an optional
   back-dep on the new crate, with `mcp = [..., "frankenterm-core-mcp/mcp"]`
   plumbing.
5. Rewrote in-core consumers (mcp.rs + 7 #[path] submodules) to call
   `frankenterm_core_mcp::*` directly instead of via re-export.

**Result: cargo refused with `cyclic package dependency`.** The new crate
needs `frankenterm-core` for `Config`/`Error`/`Policy`/`CassError`/
`CautError` (real production-code refs, not just dev-deps). The parent crate
needs the new crate to satisfy in-core mcp.rs imports of `mcp_error::*` and
`mcp_framework::*`. Cargo's cycle detector rejects the path-dep loop
regardless of `optional = true`.

The attempt was reverted — none of those changes are in the tree.

## Why the cycle is fundamental (not a wiring oversight)

The pattern is identical to the one ft-usvnt unblocked for fleet:

1. Extract a tier-2 cluster from `frankenterm-core`.
2. The cluster has heavy forward deps on core's "shared types" (Config,
   Error, Policy, etc.).
3. The cluster has back-edges into core (either via in-core consumers, or
   via re-exports needed for external test compatibility).
4. → cargo cycle.

ft-usvnt fixed this for `BackpressureTier`/`QueueDepths` by extracting them
into a leaf crate *below* core, so the cluster (and core itself) reach down
to the leaf without crossing each other. The same fix must precede mcp +
connector extraction.

## Prerequisite work (recommended sequencing)

Before re-attempting either extraction, file and ship:

1. **`frankenterm-core-error-types`** — leaf crate containing `error::Error`,
   `StorageError`, `WeztermError`, `ConfigError`, plus `crate::Result<T>`.
   Currently 117 LOC of definitions in `error.rs`; extract just the type
   definitions, leave any rendering/conversion impls in core. Fixes
   `crate::error::*` refs from both clusters and 100+ other call sites.

2. **`frankenterm-core-config-types`** — leaf crate containing `Config`,
   `McpClientConfig`, `PaneFilterConfig`, the connector subconfigs, etc.
   Bigger lift (`config.rs` is 1600+ LOC of mostly type definitions plus
   default impls).

3. **`frankenterm-core-policy-types`** — separable telemetry + redaction
   types (`Redactor`, `PolicySurface`, `PaneCapabilities`, the various
   `*Telemetry` snapshot structs that connectors compose into). The
   harder split: `policy.rs` mixes types with rule-evaluation logic.

4. **`frankenterm-core-cass-types` + `frankenterm-core-caut-types`** —
   error enums + small request/response types. Both files are <1000 LOC and
   look extraction-friendly.

After (1)+(2)+(3) land, **mcp/\* extraction becomes a clean tier-2 cut**:
the new crate depends on the four leaf-types crates and *not* on
`frankenterm-core`, so core can carry an optional dep on the new crate
without cycling. Same for connector_\*, after the four in-core importers
get rewritten to use the leaf-types crates.

## ADR

**Decision: PARK ft-t2d70.**

Closing the bead with this proposal as the deliverable rather than ship a
partial-cycle that won't compile. Re-open after the four prerequisite
leaf-type extractions land (filed as `ft-y0loj.5.A` through `ft-y0loj.5.D`
follow-ups, scoped against this proposal).

The fleet cycle (ft-y0loj.3) and mcp/connector cycle (ft-t2d70) are the same
phenomenon: `frankenterm-core` is an internal hub whose tier-2 leaves can't
move out until the hub's most-shared types ride a tier-1 leaf crate. The
right next bead is one of the four prerequisite types-extractions, not
another tier-2 attempt.

## Appendix — exact rewrite count

| File                       | `crate::*` cross-cluster refs |
| -------------------------- | ----------------------------- |
| `mcp.rs`                   | ~120                          |
| `mcp_tools.rs`             | ~80 (8484 LOC, biggest file)  |
| `mcp_missions.rs`          | ~25                           |
| `mcp_resources.rs`         | ~12                           |
| `mcp_middleware.rs`        | ~8                            |
| `mcp_proxy.rs`             | ~5                            |
| `mcp_types.rs`             | ~5                            |
| `mcp_bridge.rs`            | ~3                            |
| `mcp_client.rs`            | 4                             |
| `mcp_framework.rs`         | 1                             |
| `mcp_error.rs`             | 4                             |
| **mcp/\* total**           | **~270**                      |
| `connector_*` (14 files)   | ~150 (lighter than mcp/\*)    |

Combined ~420 mechanical sed-rewrites + feature-plumbing reconciliation +
prerequisite leaf-types extractions — roughly 4–6 sessions of focused work
once the prerequisites land.
