# Zellij Analysis Synthesis — Comparison Report and Improvement Roadmap

Bead: `wa-2bai5`

This report synthesizes the Zellij analysis set into an implementation-oriented roadmap for FrankenTerm.

Inputs (all required Zellij analysis beads):
- `evidence/zellij/INVENTORY.md` (`wa-okyhm`)
- `evidence/zellij/ipc-protocol.md` (`wa-vcjbi`)
- `evidence/zellij/session-management.md` (`wa-2apg5`)
- `evidence/zellij/wasm-plugins.md` (`wa-1pygr`)
- `evidence/zellij/layout-engine-analysis.md` (`wa-1xk9j`)
- `evidence/zellij/performance-analysis.md` (`wa-1pgzt`)

Cross-reference synthesis partner:
- `docs/ghostty-analysis-synthesis.md` (`wa-3bja.5`)

## Executive summary

Highest-value Zellij patterns for FrankenTerm:
1. Make compatibility and overload behavior explicit (protocol versioning + backpressure semantics).
2. Separate fast mutable session metadata from slower durable resurrection checkpoints.
3. Use stable logical identifiers (not positional guesses) for restore/swap/layout reconciliation.
4. Preserve causality across subsystems with completion tokens and cause-chain context.
5. Treat multi-client roles and capabilities as first-class policy entities.

## 1) Quick wins (low effort, high value)

### 1.1 Version local IPC endpoints and handshake compatibility
What Zellij does:
- Names local sockets under contract-versioned directories and prevents cross-version attach by default.

How FrankenTerm would implement it:
- Version local IPC namespace by protocol/schema version.
- Add a short capability handshake with explicit mismatch error payload.
- Define and test compatibility rules for additive vs breaking changes.

Estimated effort:
- 1 to 2 days

Related beads:
- `wa-1u9qw` (new, created from this synthesis)
- `wa-3kxe`
- `wa-dr6zv`
- `wa-3dfxb.13`

### 1.2 Make overload and fanout degradation explicit and observable
What Zellij does:
- Uses bounded queues and disconnect-on-overload semantics rather than unbounded growth.

How FrankenTerm would implement it:
- Keep bounded notification queues and explicit overflow policies (coalesce, drop, or disconnect).
- Emit first-class metrics and events for queue saturation and forced degradation.

Estimated effort:
- 0.5 to 2 days

Related beads:
- `wa-3cyp`
- `wa-x4rq`
- `wa-7o4f`
- `wa-9dp` (tiered update rates; renamed from `bd-9dp`)

### 1.3 Split live session index from resurrection checkpoints
What Zellij does:
- Separates live metadata (frequent updates) from resurrection artifacts (periodic durable snapshots).

How FrankenTerm would implement it:
- Keep a fast live session index for discovery/UI.
- Keep periodic checkpoint manifests with optional sidecar blobs for heavy payloads.
- Surface a distinct "dead but resurrectable" state in robot/session output.

Estimated effort:
- 2 to 4 days

Related beads:
- `wa-rsaf`
- `wa-3r5e`

### 1.4 Prefer logical slot identity over positional matching for restore/swap
What Zellij does:
- Tracks logical positions and reconciles panes in deterministic passes.

How FrankenTerm would implement it:
- Add stable logical slot IDs to topology/restore records.
- Use deterministic matching order (exact logical match first, fallback only when needed).

Estimated effort:
- 1 to 3 days

Related beads:
- `wa-rsaf`
- `wa-2dd4s.3`

### 1.5 Capability-gate extension actions before enabling broad plugin surfaces
What Zellij does:
- Maps plugin commands to explicit permission types and denies by default.

How FrankenTerm would implement it:
- Define capability classes for extension actions (read state, mutate pane, run command, network, filesystem).
- Log capability decisions in audit trail with request context.

Estimated effort:
- 2 to 4 days

Related beads:
- `wa-dr6zv`
- `wa-3kxe`

### 1.6 Bias scheduling for focused/active panes under pressure
What Zellij does:
- Prioritizes bounded safety mechanisms, but leaves room for focused-pane QoS improvements.

How FrankenTerm would implement it:
- Add focused-pane priority in coalesced drain/render/update loops.
- Track per-pane queue lag and schedule by recency + foreground state.

Estimated effort:
- 1 to 3 days

Related beads:
- `wa-3cyp`
- `wa-iehgn`

## 2) Strategic improvements (medium effort, high value)

### 2.1 Constraint-based pane geometry engine for tiled/floating/stacked layouts
Design sketch:
- Keep placement and interactive resize as separate phases.
- Use a constraint solver for resize with deterministic integer discretization.
- Maintain explicit floating z-order and pinning policy.

Migration path:
1. Introduce internal geometry model and solver behind feature flag.
2. Reuse existing topology APIs with compatibility adapters.
3. Expand to swap-layout selection by pane-count constraints.

Risk assessment:
- Medium: correctness risk under resize edge cases; test harness required.

New beads:
- No new bead required immediately; continue through `wa-2dd4s.2` and `wa-2dd4s.3`.

### 2.2 Cross-subsystem logical completion protocol
Design sketch:
- Carry completion token through multi-step action paths.
- Define operation completion boundaries and timeout/error states.

Migration path:
1. Instrument `send` and workflow mutation paths first.
2. Add cause-chain propagation to audit/events.
3. Expand to recovery and automated orchestration actions.

Risk assessment:
- Medium: coordination complexity across runtime/workflow/policy boundaries.

New beads:
- `wa-33uf8` (new, created from this synthesis)

### 2.3 Multi-client role model with watcher semantics
Design sketch:
- Distinguish interactive vs watcher clients.
- Track per-client view state and optional mirrored mode.

Migration path:
1. Introduce role metadata and policy checks.
2. Expose per-client state in robot/session APIs.
3. Add role-based behavior in workflow and send paths.

Risk assessment:
- Medium: policy regressions if role checks are not exhaustive.

New beads:
- `wa-3jewu` (new, created from this synthesis)

### 2.4 Scoped extension runtime with strict capabilities and host mediation
Design sketch:
- Extension API stays narrow and schema-driven.
- Host mediates side effects; extensions request actions.

Migration path:
1. Start read-only extension capabilities.
2. Add explicit mutation capabilities with policy gate + audit.
3. Add quotas/backpressure for extension-host RPCs.

Risk assessment:
- Medium to high: security and resource-control surface area.

New beads:
- Covered by existing `wa-dr6zv` initially.

## 3) Future considerations (high effort or uncertain value)

### 3.1 Full WASI plugin runtime with hot reload and hard resource budgets
Why it is interesting:
- Strong isolation and language-agnostic extensibility.

What must change first:
- Stable extension command/event schema, policy integration, and budget enforcement primitives.

When it becomes worth doing:
- After core native event hooks and capability model are stable in production.

### 3.2 Protocol-level resume/replay semantics for advanced reconnect
Why it is interesting:
- Better resilience for long-running swarm sessions and transient client failures.

What must change first:
- Versioned protocol + cursor semantics + deterministic replay boundaries.

When it becomes worth doing:
- Once recorder/search tracks need stronger replay guarantees for operator workflows.

### 3.3 First-class public layout DSL and swap policy engine
Why it is interesting:
- Declarative layout control and reproducible automation.

What must change first:
- Internal layout model maturity plus safe validation/linting surface.

When it becomes worth doing:
- After floating + swap core primitives are stable and benchmarked.

## 4) Ghostty vs Zellij comparison notes (for `wa-3bja.5`)

### Convergent patterns (high confidence)
- Coalesced notifications and bounded fanout outperform naive per-event emission.
- Explicit memory and queue budgets are required at swarm scale.
- Backpressure semantics must be observable and deterministic.

### Divergent patterns (FrankenTerm-specific choices required)
- Event payload model:
  - Zellij favors render-frame output.
  - Ghostty analysis favors hot-path dirty/coalesced internal state propagation.
  - FrankenTerm should keep structured pane-addressed deltas for robot/search workflows.
- Concurrency structure:
  - Zellij is subsystem-thread and message-bus oriented.
  - Ghostty patterns emphasize tight coalesced event-loop style hot paths.
  - FrankenTerm should use hybrid: coalesced hot path plus explicit subsystem contracts.
- Memory representation:
  - Zellij emphasizes bounded line-based scrollback and queue controls.
  - Ghostty highlights packed-cell and side-table strategies.
  - FrankenTerm should prioritize budgeting/coalescing first, deep cell representation changes later.

### Unique innovations
- Zellij strengths:
  - Resurrection semantics and manifest-driven checkpointing.
  - Logical-position layout reconciliation and swap constraints.
  - Capability-mediated extension actions.
- Ghostty strengths:
  - Consumed-dirty semantics and coalesced wakeups in hot paths.
  - Memory layout discipline for high-throughput rendering workloads.

### Recommended best-fit synthesis
1. Adopt Ghostty-style coalescing and lock/backpressure invariants for hot-path throughput.
2. Adopt Zellij session/layout/capability patterns for correctness and operability.
3. Keep FrankenTerm-native structured event/delta contracts for robot and recorder use-cases.

## 5) Cross-epic mapping checklist (required)

- Session persistence: `wa-rsaf`
- Fork hardening: `wa-3kxe`
- Performance optimization: `wa-3cyp`
- asupersync migration: `wa-e34d9`
- Ultra-performance swarm scaling: `wa-iehgn`
- /dp integration: `wa-dr6zv`
- Tiered update rates: `wa-9dp` (formerly `bd-9dp`)
- Scrollback memory: `wa-3r5e`
- Ghostty synthesis cross-reference: `wa-3bja.5`

## 6) Follow-on beads created from this synthesis

- `wa-1u9qw` — Protocol-versioned local IPC namespace and compatibility handshake
- `wa-33uf8` — Cross-subsystem action completion tokens and cause-chain context
- `wa-3jewu` — Watcher clients and per-client view-state model for agent swarms
