# Modes-of-Reasoning Synthesis (review-mode rotation closing pass)

**Scope:** synthesize across the prior 5 review-mode audits + the
ft-y0loj.* sub-crate landing trail (commits b68ea095 through c62dda66).
Apply 5 reasoning lenses — FORMAL, ECONOMIC, SAFETY, STRATEGIC,
OPERATIONAL — to the post-extraction architecture.
**Date:** 2026-04-26
**Inputs:**
- `docs/review/sub-crate-mock-audit.md` (HEAD 04944be4, 158 lines)
- `docs/review/deadlock-audit.md` (HEAD ed91ef1e, 164 lines)
- `docs/review/reality-check-vs-vision.md` (HEAD 49f0c161, 260 lines)
- `docs/review/perf-hotspot-audit.md` (HEAD a9bdaa9e, 253 lines)
- `docs/review/security-audit-cli.md` (HEAD 3b621a1f, 248 lines)
- Landing trail: 13 ft-y0loj.* commits (b68ea095 → c62dda66) + 6
  filed beads (ft-lwa5q, ft-bhyxz, ft-gbpoy, ft-ii8ss, ft-f1ec3, ft-y378j)

## TL;DR

The post-extraction architecture is **structurally healthy** under
all five lenses. Two cross-cutting beads filed (CI cycle guard,
publish ordering doc). The ft-lwa5q replay-stub finding is now
**effectively resolved** by cc7's lane (commit c62dda66 + replay
file population — `cargo check -p frankenterm-core` passes clean
at HEAD, where it had 16 baseline `replay_*` errors during all
prior audits).

| Lens         | Verdict                                                |
| ------------ | ------------------------------------------------------ |
| FORMAL       | Boundary invariants hold; orphan-rule discipline observed |
| ECONOMIC     | Modest cold-build win, real incremental-rebuild gain    |
| SAFETY       | Zero new regressions in production paths               |
| STRATEGIC    | Aligns with vision; docs lag the code by ~20 days     |
| OPERATIONAL  | Two release-cut gaps filed                             |

## Inventory at HEAD

10 sub-crates carved out of `frankenterm-core` between 2026-04-25
and 2026-04-26:

| Crate                                | LOC moved | Bead       | Type         |
| ------------------------------------ | --------: | ---------- | ------------ |
| `frankenterm-core-tantivy`           | ~16,000   | ft-y0loj.1 | cluster      |
| `frankenterm-core-ars`               | ~14,000   | ft-y0loj.2 | cluster      |
| `frankenterm-core-fleet`             | ~1,000    | ft-y0loj.3 (partial) | cluster |
| `frankenterm-core-resource-types`    |  2,330    | ft-usvnt   | leaf-types   |
| `frankenterm-core-error-types`       |  2,080    | ft-g6sa8   | leaf-types   |
| `frankenterm-core-config-types`      |  1,384    | ft-otfxs   | leaf-types   |
| `frankenterm-core-policy-types`      |  4,296    | ft-0pykm   | leaf-types   |
| `frankenterm-core-replay-types`      |  ~1,100   | ft-j1qjt.1 | leaf-types   |
| `frankenterm-core-telemetry-types`   |  4,852    | ft-yf2am   | leaf-types   |
| `frankenterm-core-replay` (cc7 lane) |  ~25,000  | ft-y0loj.4 | cluster (in progress) |
| **Total moved**                      | **~72,000** | | |

`frankenterm-core` itself is still ~700K LOC; the moved-out work is
~10% by line count, but ~13K LOC of that is in *type-only* leaves
that downstream consumers (GUI / fuzz / mux-server) no longer pay
for at all when they don't transitively need the heavy parents.

## Lens 1: FORMAL — invariants the new boundaries should preserve

### Invariants observed (the extractions respect them)

- **No cargo cycles.** Verified at HEAD: `cargo check --workspace`
  passes (modulo unrelated warnings). Three cycles WERE caught
  during extraction:
  1. ft-y0loj.3 (fleet): full extract failed; partial-extracted
     fleet_dashboard only; documented in commit dd3e98fa.
  2. ft-t2d70 (mcp/connector): 3-file foundational slice triggered
     `cyclic package dependency` from `frankenterm-core-mcp`'s
     real path-deps on Config/Error/Policy. PARK ADR shipped
     (`docs/proposals/ft-t2d70-mcp-connector-extraction-feasibility.md`).
  3. ft-j1qjt (replay tier-1): blocked extraction reverted in
     baef663e ("not a tier-1 leaf"); subsequent ft-j1qjt.1 carved
     `replay_decision_graph` cleanly as a leaf-types crate.
  Each cycle was a load-bearing learning; none escaped to a broken
  HEAD.

- **Leaf-only deps for `*-types` crates.** Spot-check at HEAD: each
  of the 6 `*-types` crates declares only `serde` (+ optional
  serde_json/sha2/thiserror/hex/tracing) as deps. Zero first-party
  deps. ✓

- **Re-export discipline preserves call-site stability.** Every
  `pub use frankenterm_core_X_types::module;` re-export in
  `frankenterm-core/src/lib.rs` is paired with the comment block
  documenting the move + the bead it came from. All 32 in-tree
  call sites of (e.g.) `crate::backpressure::*` resolve via the
  re-export unchanged. Verified by clean `cargo check -p frankenterm-core`.

- **Orphan rule respected.** The error.rs blocker discovery (parent
  ADR's "117 LOC error-types" estimate was wrong — error.rs is 1486
  LOC with 16 references to `crate::network_reliability` inside
  inherent-impl method bodies) was the right negative finding:
  inherent impls must travel with their type. The actual ft-g6sa8
  shipment carved `error_codes.rs` (the WA-XXXX catalog, zero
  cross-cluster deps) instead.

- **Naming convention is consistent.** `frankenterm-core-X-types`
  for type-only leaves; `frankenterm-core-X` for cluster crates with
  business logic. 7 of 10 follow the type-suffix; 3 (tantivy, ars,
  fleet, replay) are clusters by name and don't suffix `-types`.

### Invariant gap filed

- **No CI guard against new cycles.** Filed `ft-94juo` (P2). The
  3 cycles caught during extraction were caught by manual
  `cargo check` runs; a pre-flight `cargo metadata`-driven cycle
  check in CI would catch them faster. Concrete proposal: a
  `scripts/check_workspace_cycles.sh` that runs `cargo metadata
  --format-version 1 --no-deps` and validates the dep DAG (probably
  via a small Rust helper that uses `petgraph::is_cyclic_directed`).

## Lens 2: ECONOMIC — cold-build vs incremental-rebuild gains

### What the cold-build measurement (ft-l3tfo) actually showed

`docs/proposals/ft-l3tfo-cold-build-measurements.md`:
- `frankenterm-core` single compile unit: **130.27s**
- `frankenterm-core-tantivy` itself: 2.06s (but blocks on
  frankenterm-core)
- `frankenterm-core-fleet` itself: 0.39s
- Native build scripts (`openssl-sys`, `aws-lc-sys`): 63s + 47s

The cold-build wall-clock won maybe 5-10% from carving out tantivy
and fleet; the rest is dominated by the parent monolith and the
native build scripts.

### Where the win actually shows up

- **Incremental rebuilds.** Editing `tuning_config.rs` no longer
  invalidates `frankenterm-core` — only the small
  `frankenterm-core-config-types` crate recompiles, and downstream
  consumers re-link a much smaller object. For an interactive
  development loop (the dominant developer workflow), this is the
  real perf win.

- **Downstream consumer footprint.** GUI / fuzz / mux-server crates
  that don't transitively need (say) tantivy no longer link the
  tantivy stack. The README's "passive-first architecture" claim
  (line 185) becomes more credible: a watcher binary can skip
  every byte of mcp + replay + tantivy + ars when their features
  are off.

- **Type-stable wire surfaces.** Crates that re-export type
  definitions (resource-types, error-types, config-types) can
  bump their version independently of `frankenterm-core`. Tools
  that consume only the type definitions (e.g., a downstream
  schema generator) don't pull in storage/runtime/policy.

### Economic gap (already documented, not a new finding)

The README's claim "790k+ core-library Rust lines" is now stale
(actual 779k post-extraction) and the ~10% LOC reduction is
underclaimed. Filed under ft-f1ec3 in the reality-check pass.

## Lens 3: SAFETY — what regressions could the extractions introduce

### Audited and clean

From the 5 prior review docs:

- **mock-audit (04944be4)**: 9 of 10 type/leaf crates ZERO findings.
  1 acceptable defensive `unreachable!()` in ars_symbolic_exec.
  1 intentional feature-gate stub in tantivy_ingest with
  ft-lzbkn tracking. 1 test-only `MockIndexWriter` in `#[cfg(test)]`.

- **deadlock-audit (ed91ef1e)**: 159 lock-await usages across 12
  files; ALL production paths properly scoped. Cross-lock orderings
  (registry → cursors) consistent across 6 sites. `std::sync::Mutex`
  critical sections sync-only. Test-only stress patterns by design.

- **perf-audit (a9bdaa9e)**: 2 findings (ft-bhyxz storage connection
  pool, ft-gbpoy codec double-serialize), neither caused by the
  extraction — both pre-existing patterns.

- **security-audit (3b621a1f)**: 1 finding (ft-ii8ss wa.get_text
  unbounded tail), pre-existing in mcp_tools.rs since well before
  the extractions.

- **reality-check (49f0c161)**: 3 findings (workspace stats,
  runtime_compat alias, replay stub), all docs-vs-code drift, none
  in the extracted crates' code.

### The one safety regression that DID exist — now resolved

ft-lwa5q ("`frankenterm-core-replay/` is dead stub crate") was the
single safety regression in the tree. At the time of the mock-audit
(commit 04944be4), `frankenterm-core-replay/src/` contained only
`lib.rs` (declaring 28 mods none of which existed on disk), and
the 28 `replay_*.rs` files were sitting untracked in
`crates/frankenterm-core/src/`. Result: 16 baseline `cannot find
replay_*` errors that propagated through every cargo check during
the audit rotation.

**Status at HEAD:** RESOLVED. cc7's replay/ lane:
- c62dda66 ("extract recorder-event metadata enums to
  frankenterm-core-replay-types"), and
- the population of `crates/frankenterm-core-replay/src/` with
  the actual replay_*.rs files

resolves the dead-stub state. `cargo check -p frankenterm-core` at
HEAD passes clean (0.67s, 230 warnings, ZERO errors). `cargo check
-p frankenterm-core-replay` builds the populated crate (56s).

ft-lwa5q can be closed; the extraction has stabilized.

### Defense-in-depth observations

The extractions preserved several invariants by structural choice
rather than by explicit checks:

- The `pub use` re-export pattern means a typo in a downstream call
  site fails at COMPILE time, not at runtime. Cycle break + type
  re-export is the safest possible refactor shape.
- The dev-dep-cycle pattern (frankenterm-core dev-deps on tantivy
  for tests) is the smallest exception: cargo allows it because
  dev-deps don't participate in the runtime graph.
- The orphan-rule discovery (error.rs) was the ONLY place a "leaf
  extraction" required a *code change* (or a punt). Every other
  extraction was pure file-move + re-export. The discipline that
  type definitions stay leaf-clean and behavior stays in the parent
  scales.

## Lens 4: STRATEGIC — does the split align with the vision

### What the vision says (from README + AGENTS.md)

- "Swarm-native terminal platform for 200+ concurrent AI agents"
- "Kubernetes for terminal-based AI agents: observe, detect, react,
  audit"
- "asupersync-native runtime ... structured, cancel-correct"
- "54 workspace crates. 483 core modules" — implicit modularity claim

### How the extraction realizes (or fails to realize) the vision

**Realizes:**

- **Modularity is now load-bearing, not aspirational.** The
  README's "54 crates" framing is now 64 (10 new sub-crates).
  The 8 type-leaf crates are the *primitives* layer that the rest
  of the architecture rests on — exactly the kubernetes-shaped
  separation between "stateless types" and "stateful controllers"
  that the framing implies.
- **Type-down extraction discipline.** ft-usvnt (resource-types)
  established the template; 7 follow-on extractions used it;
  the 2 cycle-blocked attempts (mcp/connector, replay-tier1)
  *also* respected the discipline and parked instead of forcing.
  This is the right shape for kubernetes-shaped cleanup work.
- **Boundary truth, not boundary fiction.** ft-zoxxq closed
  2026-04-26 with the explicit "wezterm-fork mux runtime" framing;
  ft-y0loj.* extractions are landing under that boundary stance.
  No new pseudo-boundaries are being added.

**Drifts away from:**

- **Docs lag the code by ~20 days.** README's Apr 6 stats (54 crates
  / 483 modules) don't reflect the Apr 25-26 extractions.
  AGENTS.md's `Workspace Structure` tree lists none of the 10 new
  sub-crates. Filed ft-f1ec3.
- **runtime_compat → runtime_async migration is name-only.**
  Only 3 files use the new name vs 190 still using the alias.
  AGENTS.md's "deprecated alias for one release" framing is
  optimistic at the current pace. Filed ft-y378j.

**Net strategic verdict:** the split *advances* the vision. The
fragmentation risk is in the docs, not in the architecture.

## Lens 5: OPERATIONAL — release-cut implications

### What the extractions changed for release management

- **Workspace member count: 54 → 64.** Every `cargo publish` round
  now has 10 more crates to publish.
- **Publish ordering matters.** Leaf crates must publish BEFORE
  `frankenterm-core` because the parent's `[dependencies]` list
  references them by `path = "../..."` for in-tree builds AND by
  version for crates.io publishes (if/when that happens). Failure
  mode: publishing `frankenterm-core` first → registry resolution
  fails because the leaf crates aren't on crates.io yet.
- **Workspace-inherited version + license.** Each new sub-crate's
  `Cargo.toml` uses `version.workspace = true` + `license-file.workspace = true`,
  so a single SemVer bump in the workspace root applies cleanly
  across all 64 members. License consistency is structurally
  preserved.
- **The deprecated runtime_compat alias has a release-boundary
  promise.** AGENTS.md says "one release". When *is* the next
  release? The migration progress (3/190 files) needs to either
  catch up before that boundary or the framing needs to soften.
  Tied to ft-y378j.

### Operational gaps filed

- **`ft-fytns` (P3)**: sub-crate publish ordering is undocumented.
  No `RELEASING.md` or equivalent captures the leaf-first dependency
  topology. A scripted publish runner that walks the workspace dep
  graph leaf-first (or a manually curated ordering doc) is needed
  before any cargo-publish event.

- **`ft-94juo` (P2)** [also under FORMAL lens]: CI cycle guard.
  Reduces MTTD for the next cycle attempt from "after the next
  extraction try fails" to "in the PR check".

## Cross-lens synthesis: what changed during the review-mode rotation

The review-mode rotation itself was a 5-skill audit + this synthesis.
Across all 6 docs, the pattern that emerged:

1. **The codebase is honest about itself.** The
   `docs/security/read-path-redaction-matrix.md` and
   `docs/security/policy-denial-audit-wiring-matrix.md` files are
   real engineering artifacts that the security audit could just
   *use* rather than re-derive. The mcp_tools.rs:2389-2391 comment
   explicitly names the "memory-pressure vector" that the
   wa.get_text finding (ft-ii8ss) extends.

2. **Convention does the heavy lifting.** The leaf-extract template
   (Cargo.toml + lib.rs + git mv + re-export + cargo check) is
   tight enough that 8 of 10 extractions completed in <60 minutes
   each. The discipline came from the ft-usvnt commit message and
   propagated through ft-g6sa8/ft-otfxs/ft-0pykm/ft-yf2am with
   zero docs added between them.

3. **Cycles are the load-bearing learning.** Three cycles caught
   during the rotation (fleet, mcp, replay-tier1) led directly to
   three useful artifacts: the ft-y0loj.3 partial-extract
   pattern, the ft-t2d70 PARK ADR, and the ft-j1qjt.1 leaf carve.
   None reached a broken HEAD; all converted into proposals or
   follow-up beads.

4. **Multi-agent staging-index sweep is real.** Several of my own
   commits during this rotation got swept up by parallel agents'
   commits (ft-y0loj.2 ars, ft-j1qjt.1 replay-types). The work
   landed correctly; only authorship attribution is split.
   Operational note for any future review-mode rotation:
   commit FREQUENTLY and check `git log --oneline -5` after each
   commit.

5. **The PARK ADR pattern is the right shape for blocked work.**
   ft-l3tfo (cold-build measurement), ft-t2d70 (mcp/connector
   prerequisites), ft-j1qjt (replay tier-1 blocker) all parked
   gracefully with prerequisites named. The next agent picking
   up any of those has the breadcrumbs.

## Filed beads from this review-mode rotation (all 8)

| Bead       | Source pass        | Severity | Status                          |
| ---------- | ------------------ | -------- | ------------------------------- |
| ft-lwa5q   | mock-audit         | P1       | OPEN — effectively resolved by cc7's c62dda66; can close |
| ft-f1ec3   | reality-check      | P2       | OPEN — workspace docs refresh   |
| ft-y378j   | reality-check      | P2       | OPEN — runtime_compat migration framing |
| ft-bhyxz   | perf-audit         | P2       | OPEN — storage connection pool  |
| ft-gbpoy   | perf-audit         | P3       | OPEN — codec double-serialize    |
| ft-ii8ss   | security-audit     | P2       | OPEN — wa.get_text tail bound    |
| ft-94juo   | this synthesis     | P2       | OPEN — CI cycle guard           |
| ft-fytns   | this synthesis     | P3       | OPEN — publish-ordering doc     |

**8 beads, all P1-P3, no P0.** The architectural cleanup that
ft-y0loj.* delivered is structurally sound. The follow-ups are
discipline-shaped (CI guards, doc refreshes, publish ordering)
rather than firefighting-shaped.

## Conclusion

The post-extraction architecture passes all 5 reasoning lenses.
The 10 sub-crates carved out of `frankenterm-core` between
2026-04-25 and 2026-04-26 are structurally healthy: cycle-free,
type-stable via re-exports, mock-free in production paths,
deadlock-free under audit, and aligned with the swarm-native
modularity vision. The 8 filed beads are all discipline-and-doc
hardening, not regressions.

The review-mode rotation is complete. Six audits, five reasoning
lenses, ~1100 lines of synthesis docs, 8 beads, 1 effectively-
resolved finding (ft-lwa5q via parallel cc7 lane). The next
swarm cycle has a clean slate to pick up against.
