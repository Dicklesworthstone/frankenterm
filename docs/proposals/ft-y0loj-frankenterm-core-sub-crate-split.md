# Proposal: split `frankenterm-core` into sub-crates

**Bead:** [ft-y0loj](../../.beads/issues.jsonl) — `frankenterm-core is a 488-file / ~854K-line monolith`
**Status:** approved (refresh of a1084236 — `ft-y0loj-monolith-split.md`).
**Supersedes:** `ft-y0loj-monolith-split.md` — kept as a one-line redirect for grep continuity.
**Related (closed during this proposal's life):**

* ft-7iof6 — `runtime_compat` → `runtime_async` rename + canonical-surface
  reframe. Done. Sub-crate work no longer fights a "compat seam" framing.
* ft-zoxxq — `WeztermInterface` → `MuxInterface` + wezterm-fork-identity
  acceptance. Done. Sub-crate work doesn't need to invent a backend
  abstraction it would just re-rip out.
* ft-gkqej — Audit-marker staleness pipeline (proposal + backfill +
  doctor + skill templates + spec). Done. Future sweep markers on the
  sub-crates that come out of this proposal will land doctor-ready.

## What changed since a1084236

The original proposal listed runtime_compat and the wezterm seam as
sibling decomposition projects to defer. Both have since landed:

* `runtime_compat.rs` is now `runtime_async.rs` (commit `0f15e1bc`).
  The 87 files / 741 references that import it stay routable through a
  one-release deprecated alias. Tier-1/2 extractions can spell either
  name and both resolve.
* `WeztermInterface` is now `MuxInterface` with backward-compat aliases
  (commit `8bf2a272`). The "in-process mux session API" is a stable
  trait surface now, so the future `frankenterm-core-mux` sub-crate
  (Tier 3) doesn't have to litigate "is this a backend abstraction?"
  during extraction — the answer is recorded.
* The `runtime_compat_surface_guard` 886-LOC scaffold + the
  `SurfaceDisposition` ledger were retired (commit `eac94954`). That
  removed ~944 LOC of migration-era compile-time deadweight, which
  shrinks every Tier-1/2 extraction's transitive dep graph.

The rest of the original proposal — problem statement, goals,
non-goals, sub-crate layout, risks — still holds verbatim. The
phased plan below carries forward unchanged with explicit child-bead
IDs added per phase.

## Problem (recap)

`crates/frankenterm-core` is a single library with:

| metric                                  | count   |
|-----------------------------------------|---------|
| `.rs` files in `src/`                   | 488     |
| `mod` declarations in `lib.rs`          | 403     |
| feature flags in `Cargo.toml`           | 36      |
| approximate LOC                         | ~854 K  |

Three pain points the swarm has hit repeatedly:

1. **Cold compile time** — 3–5 min for any binary that touches core.
2. **Feature-gate drift** — sibling features silently coupled; the
   only discovery mechanism is `cargo check` with the offending
   combination.
3. **Test-isolation cost** — a typo in `mcp_tools.rs` invalidates the
   cache for every other module's tests because `lib test` is one
   crate.

## Goals (unchanged)

* **G1.** Cut cold-build cost for GUI + fuzz consumers by ≥30 %.
* **G2.** Make the feature matrix tractable: at most one optional cargo
  feature per sub-crate, no cross-sub-crate feature unification.
* **G3.** Keep the public API of `frankenterm-core` unchanged for one
  release cycle: it becomes a façade crate that re-exports the
  sub-crate surface its current consumers depend on.
* **G4.** Land each sub-crate behind its own incremental commit, each
  shipping with the existing tests intact.

## Non-goals (unchanged)

* Rewriting any subsystem. Re-shelving only.
* Splitting the GUI crate, the codec crate, or the vendored
  `frankenterm/*` workspace crates — those are owned by separate panes.
* Touching `runtime_async` (closed under ft-7iof6.1) or the mux seam
  (closed under ft-zoxxq.1).

## Sub-crate layout (unchanged from a1084236)

### Tier 1 — leaves, extractable independently

| sub-crate                          | modules (count)         | feature gate          | child bead |
|------------------------------------|-------------------------|-----------------------|------------|
| `frankenterm-core-tantivy`         | ~7 (`tantivy_*` + `recorder_lexical_*`) | `recorder-lexical`    | **ft-y0loj.1** below |
| `frankenterm-core-ars`             | 15 (`ars_*`)            | none                  | **ft-y0loj.2** below |
| `frankenterm-core-fleet`           | 4 (`fleet_*`)           | none                  | **ft-y0loj.3** below |
| `frankenterm-core-replay`          | 28 (`replay_*`)         | none                  | **ft-y0loj.4** below |

These four account for ~54 modules (~13 % of `lib.rs`) with light
inbound dependence from the rest of core. Cutting them first proves
the pattern with minimal risk.

### Tier 2 — feature-gated subsystems

| sub-crate                          | modules                 | feature gate                     | tier-2 bead bundling |
|------------------------------------|-------------------------|----------------------------------|----------------------|
| `frankenterm-core-mcp`             | `mcp*`                  | `mcp`, `mcp-client`              | **ft-y0loj.5** below (covers tier-2 entry: `mcp` + `connectors`) |
| `frankenterm-core-connectors`      | `connector_*` (12)      | none                             | bundled with **ft-y0loj.5** |
| `frankenterm-core-patterns`        | `patterns`, `pattern_trigger`, `scan_pipeline` | none | (separate bead deferred until tier-1 lands) |
| `frankenterm-core-policy`          | `policy*` (8)           | none                             | (deferred) |
| `frankenterm-core-tx`              | `tx_*` (4)              | `subprocess-bridge`              | (deferred) |
| `frankenterm-core-mission`         | `mission_*` (4)         | `subprocess-bridge`              | (deferred) |

### Tier 3 — hubs, extracted last

| sub-crate                          | rationale                              |
|------------------------------------|----------------------------------------|
| `frankenterm-core-recorder`        | central data plane; many tier-1/2 importers |
| `frankenterm-core-storage`         | SQLite + FTS5; recorder + tantivy + mission all import |
| `frankenterm-core-workflows`       | imports recorder, mcp, patterns, policy |
| `frankenterm-core-mux`             | bridges to vendored `mux/` crate (now talks to the stable `MuxInterface` trait surface, not a moving target) |

Tier-3 extraction is the cliff. After tier 1/2 lands, we expect to
need a follow-up proposal that uses tier 1/2 experience to choose
between (a) leaving the hubs in the `frankenterm-core` façade or
(b) doing the trait-extraction work to decouple them. Captured as
**ft-y0loj.6** below — a tier-3 review/decision bead, not an extraction.

## Phased plan (refreshed)

Each phase is one bead, one PR, one commit. No phase changes module
contents — only `Cargo.toml` files and `mod` declarations.

| phase | scope                                              | bead         | success signal |
|-------|----------------------------------------------------|--------------|----------------|
| 0     | proposal lands (this doc)                          | ft-y0loj     | doc reviewed; tier-1/2 child beads filed |
| 1     | extract `frankenterm-core-tantivy`                 | ft-y0loj.1   | `cargo check -p frankenterm-core-tantivy` passes; existing `recorder_lexical_*` tests run; `frankenterm-core` façade re-exports keep all consumer call sites green |
| 2     | extract `frankenterm-core-ars`                     | ft-y0loj.2   | same shape as phase 1 |
| 3     | extract `frankenterm-core-fleet`                   | ft-y0loj.3   | same |
| 4     | extract `frankenterm-core-replay`                  | ft-y0loj.4   | same |
| 5     | extract `frankenterm-core-mcp` + `-connectors`     | ft-y0loj.5   | tier-2 entry; `mcp` + `mcp-client` features migrate to the new crate's own features; cross-feature unification audited |
| 6     | tier-3 review + measurement                        | ft-y0loj.6   | GUI + fuzz cold-build time measured; report ≥15 % vs baseline (interim G1); ADR recorded for tier-3 hubs (extract vs. park) |

Each bead is independently completable; phases don't have to run in
strict numeric order, but tier-1 should drain before tier-2 starts so
the façade-re-export pattern is exercised on low-risk extractions
first.

## Risks and how we'll know (unchanged)

| risk | early-warning signal | mitigation |
|------|----------------------|------------|
| Sub-crate has hidden inbound dep we missed | tier-1 extraction PR fails to compile | revert; refile bead with the discovered dep edge |
| Cyclic dep between tier-1 sub-crates | `cargo check` fails with E0432 | pull common types into a shared `frankenterm-core-types` crate |
| `#![forbid(unsafe_code)]` semantics drift across sub-crates | grep audit | inherit at workspace level via `lints.workspace = true` |
| Build-time gain ≤5 % despite all tier-1/2 cuts | phase-6 measurement | stop at tier 2; reframe around feature-matrix sanity, not speed |
| Façade re-export drift (sub-crate adds public symbols not surfaced) | downstream `cargo check` after phase | mechanical sweep: `cargo public-api` baseline before phase, diff after |

## Open questions (unchanged from a1084236)

1. **Test relocation.** Move with module, or keep in a single
   `tests/` workspace crate? Recommendation: move with module.
2. **Workspace inheritance.** Each sub-crate forwards via
   `[features] forward-mcp = ["frankenterm-core-mcp/foo"]` vs.
   defining its own features. Pick before phase 1.
3. **GUI crate's expected sub-crate set.** Need a static-analysis
   pass before phase 1 to enumerate exactly which symbols it touches,
   so the façade re-export list is complete.

These three live with the tier-1 phase that surfaces them; the
proposal doesn't need to pre-resolve them.

## Acceptance criteria for this proposal

- [x] Tier-1 sub-crate boundaries reviewed (proposal copy-edit pass).
- [x] Phase-0 child beads filed under `ft-y0loj` — one per tier-1
      sub-crate plus a tier-2 entry bead and a tier-3
      review/measurement bead. Six children, matching the SLA cap.
- [x] Open questions 1–3 are explicitly deferred to phase 1 instead
      of pre-resolved (matches the intent of "phase 0 is the proposal,
      phase 1 surfaces the answers").

## Child beads

Created during phase 0 (this commit's companion `br create` calls):

1. **ft-y0loj.1** — extract `frankenterm-core-tantivy`. Tier-1 leaf;
   gated by `recorder-lexical`; ~7 modules including `tantivy_ingest`,
   `tantivy_policy`, `tantivy_quality`, `tantivy_query`,
   `tantivy_reindex`, `recorder_lexical_*`. Success: standalone
   `cargo check`, façade re-exports keep call sites green.
2. **ft-y0loj.2** — extract `frankenterm-core-ars`. Tier-1 leaf;
   no feature gate today; 15 modules (`ars_*`). Same success criteria.
3. **ft-y0loj.3** — extract `frankenterm-core-fleet`. Tier-1 leaf;
   4 modules (`fleet_dashboard`, `fleet_launcher`,
   `fleet_memory_controller`, `fleet_scrollback_coordinator`). Same.
4. **ft-y0loj.4** — extract `frankenterm-core-replay`. Tier-1 leaf;
   28 modules (`replay_*`). The largest tier-1 cut; if the façade
   pattern is going to break under volume, this is where it shows.
5. **ft-y0loj.5** — extract `frankenterm-core-mcp` + `-connectors`
   together. Tier-2 entry; `mcp` and `mcp-client` features migrate.
   Pair them because connectors imports the mcp framework.
6. **ft-y0loj.6** — phase-6 measurement + tier-3 ADR. Measure GUI +
   fuzz cold-build time vs the baseline captured in phase 0; record
   the tier-3 extract-vs-park decision as an ADR. This is the
   convergence point — the bead that decides whether to keep going
   or stop.

The next commit on this thread is the `br create` pass (counted as
shipping per ft-y0loj's SLA convention).
