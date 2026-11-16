# Legacy Project Adoption Matrix

Date: 2026-03-11  
Primary traceability bead: `ft-3681t.1.5`  
Supplemental machine-readable artifact: `docs/design/legacy-project-adoption-matrix.json`

This dashboard answers a different question than
`docs/design/ntm-fcp-traceability-matrix.json`.

- `ntm-fcp-traceability-matrix.json` is capability-centric and focused on the
  current FrankenTerm control/data/policy plane.
- This artifact is source-project-centric and answers:
  legacy project -> extracted idea -> mapped `ft-*` bead(s) -> live status.

It exists because Rio already had an explicit recommendation-to-implementation
matrix, while Ghostty and Zellij only embedded bead mappings inside their
synthesis prose, and vendored WezTerm research was split across design + crate
vendoring work.

Status values in this dashboard are normalized from `.beads/issues.jsonl` as of
2026-03-11 because the local `br` database is currently unhealthy.

## Project Summary

| Source project | Legacy mining status | Primary source docs | Pre-existing live status matrix | Coverage before this artifact | Notes |
|---|---|---|---|---|---|
| Ghostty | Closed (`ft-3bja`, `ft-3bja.5`) | `docs/ghostty-analysis-synthesis.md`, `evidence/ghostty/*` | No | Partial | Source doc still uses legacy `wa-*` bead ids. |
| Zellij | Closed (`ft-okyhm`, `ft-2bai5`) | `docs/zellij-analysis-synthesis.md`, `docs/zellij-analysis.md`, `evidence/zellij/*` | No | Partial | Source doc still uses legacy `wa-*` bead ids. |
| Rio | Closed (`ft-34sko`, `ft-34sko.7`, `ft-34sko.8`) | `docs/rio-analysis-synthesis.md`, `docs/rio-implementation-validation-matrix.md`, `evidence/rio/*` | Yes | Strong | Rio was the only source with an explicit execution/status matrix. |
| WezTerm | Closed research + vendoring (`ft-wyr1`, `ft-od8xy`) | `docs/vendored-wezterm-design.md` | No | Partial | Foundation work is closed, but README still documents WezTerm as the current compatibility bridge. |

## Adoption Matrix

| Source | Idea / adopted pattern | Source artifacts | Primary `ft-*` beads (live status) | Adoption state | Notes |
|---|---|---|---|---|---|
| Ghostty | Coalesced wakeups and “drain then notify once” | `docs/ghostty-analysis-synthesis.md`, `evidence/ghostty/event-system.md` | `ft-x4rq` (closed), `ft-7o4f` (closed) | Implemented | The Ghostty recommendation is now represented in the runtime notification path and mux notification fanout work. |
| Ghostty | Split data-plane deltas from control-plane events | `docs/ghostty-analysis-synthesis.md`, `evidence/ghostty/io-pipeline.md`, `evidence/ghostty/event-system.md` | `ft-3dfxb.13` (closed), `ft-x4rq` (closed) | Partial | Native event hooks landed, but the repo still documents the WezTerm compatibility bridge as an active backend boundary rather than a fully replaced substrate. |
| Ghostty | Byte-budgeted memory, eviction, and reuse | `docs/ghostty-analysis-synthesis.md`, `evidence/ghostty/memory-architecture.md` | `ft-2ahu0` (closed), `ft-3r5e` (closed), `ft-8vla` (in_progress), `ft-3axa` (open) | Partial | Pressure tiers and eviction exist; mmap-backed scrollback and allocator specialization are still in flight. |
| Zellij | Versioned local IPC namespace and compatibility handshake | `docs/zellij-analysis-synthesis.md`, `evidence/zellij/ipc-protocol.md` | `ft-1u9qw` (closed) | Implemented | Zellij’s compatibility handshake recommendation is already closed as a dedicated implementation bead. |
| Zellij | Explicit overload/fanout degradation semantics | `docs/zellij-analysis-synthesis.md`, `evidence/zellij/performance-analysis.md` | `ft-x4rq` (closed), `ft-7o4f` (closed), `ft-9dp` (closed) | Implemented | The relevant coalescing, callback-outside-lock, and tiered-update work all closed. |
| Zellij | Separate live session index from resurrection checkpoints | `docs/zellij-analysis-synthesis.md`, `evidence/zellij/session-management.md` | `ft-rsaf` (closed), `ft-3r5e` (closed) | Implemented | Session persistence/restart safety is closed as an epic, with scrollback pressure mitigation closed separately. |
| Zellij | Stable logical slot identity for floating/swap layout reconciliation | `docs/zellij-analysis-synthesis.md`, `evidence/zellij/layout-engine-analysis.md` | `ft-2dd4s.2` (closed), `ft-2dd4s.3` (closed) | Implemented | The Zellij-inspired floating-pane and swap-layout slices are both closed. |
| Zellij | Cross-subsystem completion tokens and cause-chain context | `docs/zellij-analysis-synthesis.md` | `ft-33uf8` (closed) | Implemented | The action-completion token/cause-chain recommendation became its own closed bead. |
| Zellij | Watcher clients and per-client view-state model | `docs/zellij-analysis-synthesis.md` | `ft-3jewu` (closed) | Implemented | Multi-client watcher/view-state support is closed as a dedicated bead. |
| Zellij | Capability-gated extension actions and mediated side effects | `docs/zellij-analysis-synthesis.md`, `evidence/zellij/wasm-plugins.md` | `ft-dr6zv` (closed), `ft-3kxe` (closed) | Partial | Tool integration and fork hardening landed, but this is broader than a single finished extension-runtime surface. |
| Rio | Canonical wakeup/coalescing contract across ingest -> detect -> render | `docs/rio-analysis-synthesis.md`, `docs/rio-implementation-validation-matrix.md`, `evidence/rio/runtime-event-loop.md` | `ft-1u90p.5` (closed), `ft-1u90p.7` (closed) | Implemented | Rio’s wakeup/coalescing recommendation is already mapped and closed through the resize/reflow program. |
| Rio | Two-source damage model merge (terminal damage + UI damage) | `docs/rio-analysis-synthesis.md`, `docs/rio-implementation-validation-matrix.md`, `evidence/rio/rendering-pipeline.md` | `ft-1u90p.4` (closed), `ft-1u90p.7` (closed) | Implemented | Rio’s damage-merge recommendation already had a closed execution+validation path. |
| Rio | Sync-update guardrails and adaptive batch thresholds | `docs/rio-analysis-synthesis.md`, `docs/rio-implementation-validation-matrix.md`, `evidence/rio/performance-analysis.md` | `ft-1u90p.5` (closed), `ft-1u90p.7` (closed), `ft-283h4.4` (open) | Partial | Historical resize/reflow work is closed, but the active `io_uring` follow-on remains open. |
| Rio | Unified memory budget controller | `docs/rio-analysis-synthesis.md`, `docs/rio-implementation-validation-matrix.md` | `ft-1u90p.5` (closed), `ft-1u90p.6` (closed), `ft-1u90p.7` (closed) | Implemented | Rio’s memory-budget recommendation is the cleanest source-to-status mapping in the repo today. |
| Rio | Frame pacing policy tiers | `docs/rio-analysis-synthesis.md`, `docs/rio-implementation-validation-matrix.md`, `evidence/rio/runtime-event-loop.md` | `ft-1u90p.4` (closed), `ft-1u90p.8` (closed) | Implemented | Frame pacing, rollout, and operator validation all closed in the resize/reflow program. |
| Rio | Effective-config introspection and strict validation mode | `docs/rio-analysis-synthesis.md`, `docs/rio-implementation-validation-matrix.md`, `evidence/rio/config-platform.md` | `ft-1u90p.8` (closed), `ft-vv3h` (closed), `ft-x4bt` (closed) | Implemented | Rio’s config-surface recommendation is already backed by closed FrankenTerm work. |
| WezTerm | In-tree vendored crate substrate | `docs/vendored-wezterm-design.md`, `frankenterm/PROVENANCE.md` | `ft-od8xy` (closed) | Implemented | The crate vendoring phase is complete and is now reflected in the workspace layout under `frankenterm/*`. |
| WezTerm | Native event sink + IPC contract for high-fidelity pane events | `docs/vendored-wezterm-design.md` | `ft-wyr1` (closed), `ft-jgqs` (closed) | Partial | The contract and listener side are closed, but README and architecture docs still describe WezTerm as the current compatibility bridge rather than a fully retired backend boundary. |

## Crosswalk Notes

### `wa-*` to `ft-*` normalization

Ghostty and Zellij synthesis docs still use older `wa-*` bead identifiers in
their prose. This dashboard normalizes them to current `ft-*` ids for live
status tracking. The JSON companion preserves legacy aliases where useful.

### Why Rio looked “more complete” before this artifact

Rio already had:

- a recommendation-level synthesis in `docs/rio-analysis-synthesis.md`
- an implementation/validation matrix in
  `docs/rio-implementation-validation-matrix.md`

Ghostty and Zellij had good synthesis prose but no equivalent live status
dashboard.

### Current limitation that still cuts across the WezTerm rows

The repo’s current product reality is still:

- WezTerm integration is a migration bridge, not the product boundary
- complete backend independence is not finished

That is why some WezTerm- and Ghostty-derived rows remain `Partial` even though
their immediate mapping beads are closed.

## Legacy Source Sync Status

- Sync helper exists at `scripts/pull-legacy-repos.sh`.
- The script explicitly says it is intended to run daily via cron or launchd.
- No repo-local cron or launchd wiring was found during this pass.
- The script currently targets `/dp/ghostty`, `/dp/wezterm`, `/dp/zellij`, and
  `/dp/rio`, while this checkout also contains `legacy_ghostty`,
  `legacy_wezterm`, `legacy_zellij`, and `legacy_rio`.

Operationally, that means legacy-source refresh is still a manual helper path,
not a fully aligned or self-describing repo workflow.

## Suggested Maintenance Rules

1. Treat this file as the source-project slice of `ft-3681t.1.5`, not as a
   replacement for the capability-centric `ntm-fcp` matrix.
2. When a synthesis doc still uses `wa-*` ids, update this dashboard first with
   the normalized `ft-*` ids before rewriting the source doc.
3. For any future legacy-project mining track, add:
   - one project summary row above
   - one or more recommendation rows here
   - a matching JSON entry in `legacy-project-adoption-matrix.json`
4. Source live status from `.beads/issues.jsonl` until the local `br` database
   import path is healthy again.
5. If legacy-repo sync automation is wired up later, update both this note and
   the JSON companion so the refresh path stays auditable.
