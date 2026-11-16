# Reality Check vs Stated Vision (review pass)

**Scope:** compare README.md + AGENTS.md's stated vision (swarm-native
terminal platform, asupersync-native runtime, robot-mode + MCP control
surfaces) against what's actually shipped at HEAD as of 2026-04-26.
**Method:** read the docs, count things, run `br show` on the named
epics, spot-check call-site counts.

## TL;DR

The vision is largely **delivered**, with a few specific gaps:

- **What's working today:** the watch/capture/pattern/workflow/policy
  pipeline; robot-mode (15+ command families); search (FTS5 + optional
  semantic); web/SSE behind feature; distributed mode behind feature;
  MCP server (mcp/mcp-client features); 47k+ tests; 779k LOC of core.
- **What's regressed or out-of-date:** the docs stats (54 crates /
  483 modules) are pre-extraction and don't reflect the 10 new
  sub-crates landed under ft-y0loj.*. The `runtime_compat → runtime_async`
  rename is documented as "deprecated alias for one release" but is
  effectively the active name — only 3 files use the new name vs ~190
  still using the alias.
- **Previously not implemented, now superseded:** the original sweep found
  `ft robot checkpoint/context/work/fleet/profile` routed through
  `robot.not_implemented`. Current HEAD has native dispatch for all five
  families; `docs/robot-contracts/current-ntm-gap-dispatch.md` is the current
  source of truth and records an empty generic NTM-gap fallback set.

## Vision (as stated)

From `README.md:33-49` (TL;DR + Platform Model) and `AGENTS.md:209-236`
(Strategic Direction):

1. **Swarm-native terminal platform for 200+ AI agents**
2. **asupersync-native runtime** — Cx-aware, structured, cancel-correct
3. **Replacement-class** — not a wrapper around another terminal; a
   wezterm-fork mux runtime in its own right
4. **Robot Mode + MCP** as the dual machine-native control surfaces
5. **Capture → pattern → react → audit** event-driven pipeline
6. **Policy engine** with 21 subsystems, capability gates, redaction
7. **Mission orchestration** with prepare/commit/compensate transactions
8. **Tiered scrollback** for memory management on 200+ pane fleets
9. **Replay & forensics** for post-incident analysis

## Reality

### (1) Code volume and shape — close, slightly stale

| Claim                                           | README   | Actual at HEAD  | Verdict                      |
| ----------------------------------------------- | -------- | --------------- | ---------------------------- |
| Workspace crates                                | 54       | **64**          | Stale (+10 sub-crates added) |
| Vendored crates under `frankenterm/`            | 47       | 47              | ✓ accurate                   |
| `crates/frankenterm-core/src` Rust modules      | 483      | **427**         | Stale (-56, post-extraction) |
| Core-library Rust LOC                           | 790k+    | 779k            | ✓ within rounding            |
| Tests                                           | 45,000+  | 47,603 `#[test]`| ✓ conservative               |

The numbers were "last verified against the current checkout on
April 6, 2026" per README:18. Today is 2026-04-26 — they're 20 days
stale, and the ft-y0loj.* sub-crate extractions (10 of them) have
shifted both directions: workspace member count went UP (54 → 64) as
new sub-crates landed, but `crates/frankenterm-core/src` went DOWN
(483 → 427) as files moved out into the new sub-crates.

**Filed:** `ft-f1ec3` (P2 reality-check) — refresh stats + add the
ft-y0loj.* sub-crates to AGENTS.md's `Workspace Structure` tree (it
currently lists only the original 5 `crates/frankenterm-*/` members).

### (2) `runtime_compat → runtime_async` rename — partially done

`README.md:41` says the rename is "load-bearing — ~709 call sites
across 83 files in `crates/frankenterm-core/src/` alone route task
spawns, channels, sleeps, timeouts, and blocking work through it" and
points at the ft-xbnl0 epic for collapsing seams.

`AGENTS.md:229,263,299` document `runtime_async` as "the canonical async
API surface" and `runtime_compat` as "a deprecated module alias for one
release."

Reality at HEAD:

```
crates/frankenterm-core/src/   — 740 references to `runtime_compat`
                                — only 10 references to `runtime_async`
                                — 85 of 427 files still use `runtime_compat`
                                — only 3 files use `runtime_async`

workspace-wide                  — 190 files use `runtime_compat`
                                — ~3 files use `runtime_async`
```

The README is honest about the load-bearing nature ("709 call sites").
AGENTS.md's "deprecated alias for one release" framing is **optimistic**
— at the current migration pace (3 files in the new name across the
entire workspace), this is multi-month work, not a one-release flip.
The deprecated alias is doing 99% of the work right now.

**Filed:** `ft-y378j` (P2 reality-check) — under ft-7iof6 epic. Either
soften the AGENTS.md framing to match reality ("the rename is in
progress; the alias remains active") or commit to a real migration
schedule.

### (3) Sub-crate split: net positive for the vision, undocumented

Ten new sub-crates have landed under `ft-y0loj.*` between 2026-04-25
and 2026-04-26:

| Sub-crate                              | Bead       | Status         |
| -------------------------------------- | ---------- | -------------- |
| `frankenterm-core-tantivy`             | ft-y0loj.1 | shipped        |
| `frankenterm-core-ars`                 | ft-y0loj.2 | shipped        |
| `frankenterm-core-fleet`               | ft-y0loj.3 | partial — fleet_dashboard only; rest blocked on cycles |
| `frankenterm-core-resource-types`      | ft-usvnt   | shipped        |
| `frankenterm-core-error-types`         | ft-g6sa8   | shipped        |
| `frankenterm-core-config-types`        | ft-otfxs   | shipped        |
| `frankenterm-core-policy-types`        | ft-0pykm   | shipped        |
| `frankenterm-core-replay-types`        | ft-j1qjt.1 | shipped        |
| `frankenterm-core-telemetry-types`     | ft-yf2am   | shipped        |
| `frankenterm-core-replay`              | ft-j1qjt   | **broken stub** — `lib.rs` declares 28 mods but only `lib.rs` exists on disk; filed `ft-lwa5q` |

This is a **net positive** for the original vision. The ft-y0loj parent
epic ("488-file / ~854K-line monolith — every consumer pays full
compile cost; feature-flag N×M matrix uncitable") is being chipped
down: 7760+ LOC of "type-only" code carved into leaf crates means the
GUI/fuzz/mux-server build graphs no longer pay for tantivy, ARS, fleet
dashboard, error_codes, tuning_config, policy types, replay decision
graph, or telemetry primitives. The cold-build measurement under
ft-l3tfo (committed 2026-04-26 in `docs/proposals/`) parked further
tier-3 cuts until prerequisite leaf-types extractions land — and three
of those prerequisites have now shipped.

**Zero of these crates are mentioned in either README.md or AGENTS.md.**
The architectural diagram in AGENTS.md:240-251 still shows
`frankenterm-core` as a monolith feeding the CLI; the workspace tree
in AGENTS.md:255-291 lists only the original 5 `crates/frankenterm-*/`
members. The README's bottom-line summary (line 1272) still says
"54 crates. 483 modules."

This finding is rolled into `ft-f1ec3` above.

### (4) Robot mode NTM-gap fallback — superseded by native dispatch

At the original review date, five robot families still returned
`robot.not_implemented`. Current HEAD has since graduated those surfaces to
native handlers; `docs/robot-contracts/current-ntm-gap-dispatch.md` now records
that no live Robot family routes through the generic NTM-gap fallback.

| Command            | Dispatch site                          | Status |
| ------------------ | -------------------------------------- | ------ |
| `ft robot checkpoint` | `crates/frankenterm/src/main.rs::handle_robot_checkpoint_command` | Native snapshot/session adapter; mutating rollback remains approval-gated |
| `ft robot context` | `crates/frankenterm/src/main.rs::robot_context_command_response` | Native SQLite `pane_contexts` / `context_rotations` adapter |
| `ft robot work` | `crates/frankenterm/src/main.rs::robot_work_command_response` | Native SQLite `work_claims` queue |
| `ft robot fleet` | `crates/frankenterm/src/main.rs::robot_fleet_command_response` | Native inventory reads plus fleet mutation receipts for scale/rebalance |
| `ft robot profile` | `crates/frankenterm-core/src/robot_profile_handler.rs` via CLI dispatch | Native read/validate/dry-run apply plus mux-backed non-dry-run apply |

The old `build_ntm_not_implemented_response` helper remains as a typed error
envelope utility and for regression tests, but the live graduated families no
longer use it as their dispatch target.

**Supersession note filed:** `ft-ssm7t` refreshed the stale review text and
state-machine module comments after the native handlers shipped.

### (5) Mux boundary — wezterm-fork stance is now codified

`AGENTS.md:230` and `README.md:47` both adopted the "wezterm-fork mux
runtime" framing in ft-zoxxq.3 (2026-04-26, today). The `MuxInterface`
trait (formerly `WeztermInterface`) is the canonical surface, with the
old name preserved as an alias under ft-zoxxq.1. Verified `ft-zoxxq`
is CLOSED in beads: "All 5 children (.1-.5) shipped: rename, relocate,
docs, CI guard, PROVENANCE audit."

The earlier "implementation boundary" framing has been retired. ✓

### (6) ft-xbnl0 finish-line program — closed, documented

The "goal-line closure" epic referenced from `README.md:41` and
`AGENTS.md:231-235` is **closed** (2026-04-23) per `br show ft-xbnl0`:
"All 5 child epics and blockers closed - goal-line program rollup
complete." The four `docs/ft-xbnl0-*.md` audit documents referenced
from the README all exist in-tree.

**No bead filed** — docs match code.

### (7) Replay subsystem — the open wound

The replay epic (ft-j1qjt) attempted a tier-1 leaf extraction, hit
cycle blockers, and partial-shipped: `frankenterm-core-replay-types`
landed cleanly with `replay_decision_graph` (1032 LOC), but the
broader `frankenterm-core-replay/` crate is in a half-extracted state
— `lib.rs` declares 28 `pub mod replay_*` submodules, but only `lib.rs`
exists on disk and the 28 `replay_*.rs` files sit untracked in
`crates/frankenterm-core/src/`. Result: 16 baseline `cannot find
replay_*` errors that have been filtered past in every recent
`cargo check`.

**Filed previously:** `ft-lwa5q` (P1 review) — cc7's `replay/` lane to
choose between hard-revert and partial-extract per the ft-y0loj.3
fleet template.

This is the only place where the documented vision and the actual
shipped state diverge by more than "stale numbers." Replay-and-forensics
is in the README's Why Use ft? table (line 62) but the replay sub-crate
that was supposed to carve it out cleanly is currently broken.

## Gaps between docs and code (summary)

| # | Gap                                              | Severity | Bead       |
| - | ------------------------------------------------ | -------- | ---------- |
| 1 | Workspace stats stale (54→64 / 483→427)          | low      | ft-f1ec3   |
| 2 | AGENTS.md workspace tree missing 10 sub-crates   | low      | ft-f1ec3   |
| 3 | runtime_compat alias still load-bearing         | medium   | ft-y378j   |
| 4 | frankenterm-core-replay broken stub              | medium   | ft-lwa5q   |

Three findings, three beads. Nothing P0 or P1 except the replay stub
(already escalated).

## Does the sub-crate split improve or hurt the vision?

**Improves it, materially.** The original vision (line README:16) is
"54 workspace crates. 483 core modules. 45,000+ tests. Purpose-built
for fleets of 200+ concurrent AI coding agents." The implicit promise
of that framing is *modularity*: a 200-agent fleet shouldn't pay for
every byte of every subsystem at every consumer site.

The 8 type/leaf crates (resource-types, error-types, config-types,
policy-types, replay-types, telemetry-types, plus the larger ars +
tantivy extractions) shrink the compile graph for downstream
consumers (GUI, fuzz, mux-server) by carving out ~13K+ LOC that was
previously paid for by every consumer of `frankenterm-core`. That's a
direct realization of the modularity promise.

The cycle-blocked extractions (fleet partial, mcp/connector full
PARK ADR, replay broken stub) are honest signals that the monolith
has internal coupling that doesn't yield to a `git mv` — it needs
prerequisite types-down extractions first. The audit trail for that
discovery is now in the proposals dir (`ft-l3tfo-cold-build-measurements.md`,
`ft-t2d70-mcp-connector-extraction-feasibility.md`,
`ft-t2d70-leaf-extraction-readme.md`), so the next agent picking this
up has the breadcrumbs.

The vision says "kubernetes for terminal-based AI agents" (README:37).
The kubernetes-shaped cleanup work — separating stateless types from
stateful managers, breaking cycles via leaf-types crates, parking what
can't move yet — is happening. Slowly, but visibly.

## Conclusion

The README's claim "the closest analogy is Kubernetes for terminal-based
AI agents" is **mostly delivered** at the level of feature surface
(robot mode, policy engine, mission tx, replay forensics, distributed
mode, web API, MCP). The architectural cleanup that *makes* it feel
kubernetes-shaped (types-down extractions, cycle-free leaf crates,
boundary-honest framing) is **in flight** under ft-y0loj.* and ft-7iof6,
with three filed reality-check beads to keep the docs honest.

The honest finish-line: vision largely delivered, docs lag the code by
about 20 days (the ft-y0loj.* extractions of 2026-04-25/26), one open
wound on the replay sub-crate, and a runtime-rename migration that's
been declared but barely started.

Filed beads: `ft-f1ec3` (workspace stats refresh), `ft-y378j`
(runtime_compat migration framing), `ft-lwa5q` (replay stub) earlier.
