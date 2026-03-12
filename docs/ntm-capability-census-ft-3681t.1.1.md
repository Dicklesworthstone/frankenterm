# NTM (Named Tmux Manager) — Exhaustive Capability Census

> **Bead**: ft-3681t.1.1
> **Author**: PinkForge (claude-code, opus-4.6)
> **Date**: 2026-03-11
> **Source**: /dp/ntm (Go-based, ~60+ internal packages, 200+ CLI files)

---

## Executive Summary

NTM is a mature, feature-rich tmux session management platform for orchestrating multiple AI coding agents in parallel. It has **150+ CLI commands** organized into 20+ functional domains, sophisticated multi-agent coordination, robust safety guardrails, and rich machine APIs.

**Key assessment**: NTM solved *tmux-based orchestration well*. FrankenTerm should solve *agent-agnostic orchestration* — the principles transfer, but the implementation details differ significantly.

## Code-Grounded FrankenTerm Baseline (2026-03-11 delta)

This census needs to stay connected to the current FrankenTerm codebase, not
just the source NTM inventory. The highest-signal implementation anchors for
downstream convergence work are:

- `crates/frankenterm/src/main.rs`: monolithic CLI router for `watch`, `robot`,
  `search`, `send`, `workflow`, `session`, `snapshot`, `doctor`, and MCP entry
  paths. Track 4 robot-surface work will keep colliding here unless routing is
  decomposed.
- `crates/frankenterm-core/src/runtime.rs` +
  `crates/frankenterm-core/src/ingest.rs`: passive observation runtime,
  discovery loop, delta extraction, gap recording, and watcher orchestration.
- `crates/frankenterm-core/src/storage.rs`,
  `crates/frankenterm-core/src/search/*`,
  `crates/frankenterm-core/src/patterns.rs`,
  `crates/frankenterm-core/src/events.rs`: durable eventing, search, and
  detection backbone that already covers a large part of NTM's monitoring and
  analytics value.
- `crates/frankenterm-core/src/policy.rs`,
  `crates/frankenterm-core/src/approval.rs`,
  `crates/frankenterm-core/src/command_guard.rs`: mutation authorization,
  approval, and safety gates.
- `crates/frankenterm-core/src/robot_types.rs`,
  `crates/frankenterm-core/src/mcp.rs`,
  `crates/frankenterm-core/src/mcp_bridge.rs`: machine-facing control plane and
  MCP parity surfaces.
- `crates/frankenterm-core/src/wezterm.rs`,
  `crates/frankenterm-gui/src/native_bridge.rs`,
  `crates/frankenterm-mux-server/src/main.rs`,
  `crates/frankenterm-mux-server-impl/src/lib.rs`: current backend/mux
  compatibility seam and the split between CLI, GUI, and headless mux server.

The practical takeaway is that FrankenTerm already has strong building blocks
for NTM-style observability, automation, and machine control, but the remaining
gaps are concentrated in first-class swarm coordination, native mux/session
semantics, and a less monolithic command-routing surface.

## Source Implementation Anchors (/dp/ntm)

These are the upstream files that most directly justify the capability census
 and the preserve/upgrade/drop decisions below.

| Source Anchor | Responsibility | FrankenTerm Relevance |
|---|---|---|
| `cmd/ntm/main.go` | Thin binary entrypoint delegating to Cobra CLI | Confirms NTM itself is mostly a routing shell around internal orchestration packages. |
| `internal/cli/root.go` | Root Cobra command, global flags, startup phases, giant robot-flag surface | Shows the operator and machine API breadth FrankenTerm still needs to decompose more cleanly than NTM did. |
| `internal/cli/coordinator.go` + `internal/coordinator/coordinator.go` | Session coordinator startup, activity polling, state transitions, alerting | Core preserve target for swarm-state inference and alert generation. |
| `internal/watcher/file_reservation.go` + `internal/watcher/conflict.go` | TTL file locks, conflict detection, negotiation path | Direct precedent for native reservation/conflict semantics beyond external Agent Mail workflows. |
| `internal/approval/engine.go` | Approval tokens, SLB/two-person-rule, gated destructive flows | Preserve the approval semantics, but keep them policy-native inside FrankenTerm. |
| `internal/robot/robot.go` + robot handlers in `internal/cli/root.go` | Machine-readable status/search/assignment/mail/ensemble surfaces | Main evidence for the size of the NTM robot-control plane and why `ft robot` track work must stay contract-driven. |
| `internal/session/restore.go` + `internal/checkpoint/restore.go` | Checkpoint restore, pane recreation, context reinjection, git drift checks | Preserve the operator recovery workflow, but anchor it to ft snapshots/session restore instead of tmux. |
| `internal/assign/matcher.go` | Capability matrix, assignment strategies, blocker-aware work routing | Preserve the assignment model, upgrade it with FrankenTerm-native workflow, policy, and telemetry inputs. |

---

## Command Taxonomy (150+ commands)

### A. Session Lifecycle (8 commands)
| Command | Aliases | Purpose |
|---------|---------|---------|
| `create` | `cnt` | Create empty tmux session with N panes |
| `spawn` | `sat` | Create session and launch specific agent mix (--cc=N --cod=N --gmi=N) |
| `quick` | `qps` | Full project scaffolding + git init + spawn |
| `attach` | `rnt` | Attach to session (create if missing) |
| `list` | `lnt` | List all tmux sessions with agent counts |
| `view` | `vnt` | Unzoom, tile, and attach |
| `kill` | `knt` | Kill session (confirmation prompt unless -f) |
| `adopt` | | Adopt external tmux session for NTM management |

### B. Agent Management (4 commands)
| Command | Aliases | Purpose |
|---------|---------|---------|
| `add` | `ant` | Add more agents to running session |
| `send` | `bp` | Broadcast prompt to agents by type (--cc/--cod/--gmi/--all) |
| `interrupt` | `int` | Send Ctrl+C to all agent panes |
| `scale` | | Adjust agent count dynamically |

### C. Session Navigation & Display (6 commands)
`status`, `zoom`, `dashboard`, `palette`, `wait`, `activity`

### D. Output Capture & Analysis (8 commands)
`copy`, `save`, `errors`, `grep`, `extract`, `diff`, `changes`, `get-all-session-text`

### E. Monitoring, Health, Analytics (7 commands)
`health`, `watch`, `analytics`, `locks`, `activity`, `doctor`, `summary`

### F. Session Persistence & Recovery (6 commands)
`checkpoint save/list/show/delete`, `rollback`, `restore`

### G. Git Integration & Hooks (6 commands)
`repo sync`, `git`, `audit`, `hooks install/uninstall/run`

### H. Context & Memory Management (8 commands)
`rotate context` (+ `history/stats/clear/pending/confirm`), `cache`, `memory`

### I. Approval & Conflict Management (4 commands)
`approve list/deny/show/history`

### J. Workflow & Task Orchestration (8 commands)
`work` (+ `assign/show/claim`), `workflows`, `marching-orders`, `recipes`, `recipe show`

### K. Ensemble (Multi-Agent Reasoning) (8 commands, 11 strategies)
`ensemble` (+ `spawn/estimate/compare/presets/stop/suggest/synthesize`)
Strategies: manual, adversarial, consensus, creative, analytical, deliberative, prioritized, dialectical, meta-reasoning, voting, argumentation

### L. Policy, Safety & Access Control (6 commands)
`policy`, `policy show`, `safety`, `assign`, `locks force-release/renew`

### M. Notifications & Messaging (5 commands)
`mail send/inbox/read/ack`, `message inbox`

### N. Redaction, Privacy & Encryption (4 commands)
`redact`, `redact preview`, `scrub`, `privacy`

### O. Project & Template Management (6 commands)
`template`, `session-templates` (+ `show`), `profiles list/show`, `personas`

### P. Configuration & System (8 commands)
`config init/show`, `tutorial`, `deps`, `upgrade`, `bind`, `version`, `completion`

### Q. Advanced Diagnostics (10 commands)
`kernel list`, `beads`, `scan`, `bugs`, `cass` (+ `search/context`), `timeline`, `history`, `plugins`

### R. Multi-Project Orchestration (5 commands)
`swarm` (+ `stop`), `coordinator`, `controller`, `worktrees`

## NTM Domain -> FrankenTerm Current-State Mapping

| NTM Domain | Closest FrankenTerm Surface(s) | Classification | Current Implementation Anchors | Code-Grounded Gap / Note |
|---|---|---|---|---|
| Session lifecycle | `ft watch`, `ft status`, `ft stop`, `ft snapshot *`, `ft session *`, standalone GUI/mux binaries | Partial parity | `crates/frankenterm/src/main.rs`, `crates/frankenterm-core/src/runtime.rs`, `crates/frankenterm-core/src/snapshot_engine.rs`, `crates/frankenterm-core/src/session_restore.rs`, `crates/frankenterm-mux-server/src/main.rs` | Lacks a first-class `create/spawn/adopt/attach` family comparable to NTM's session-oriented operator verbs. |
| Agent management | `ft send`, `ft robot send`, workflow handlers, pane reservations | Partial parity | `crates/frankenterm/src/main.rs`, `crates/frankenterm-core/src/policy.rs`, `crates/frankenterm-core/src/workflows/*`, `crates/frankenterm-core/src/storage.rs` | Strong per-pane mutation path, but no native type-targeted fanout or scale semantics equivalent to NTM's agent mix controls. |
| Session navigation and display | `ft status`, `ft show`, `ft tui`, GUI surfaces | Partial parity | `crates/frankenterm/src/main.rs`, `crates/frankenterm-core/src/tui.rs`, `crates/frankenterm-gui/src/main.rs` | Current coverage is more diagnostic than operational; NTM-style dashboard, palette, and session navigation UX is not yet the primary path. |
| Output capture and analysis | `ft get-text`, `ft search`, `ft robot get-text`, `ft robot search`, `ft events` | Superset | `crates/frankenterm-core/src/runtime.rs`, `crates/frankenterm-core/src/ingest.rs`, `crates/frankenterm-core/src/storage.rs`, `crates/frankenterm-core/src/search/*` | FrankenTerm already exceeds NTM here with durable deltas, explicit gaps, FTS, and semantic/hybrid search. |
| Monitoring, health, analytics | `ft events`, `ft triage`, `ft doctor`, metrics, audit surfaces | Superset | `crates/frankenterm-core/src/events.rs`, `crates/frankenterm-core/src/storage.rs`, `crates/frankenterm-core/src/diagnostic.rs`, `crates/frankenterm-core/src/metrics.rs` | Strong foundation exists; the remaining work is operator presentation and mission-level rollups, not raw telemetry plumbing. |
| Session persistence and recovery | `ft snapshot *`, `ft session *`, restore-on-watch startup | Partial parity | `crates/frankenterm-core/src/snapshot_engine.rs`, `crates/frankenterm-core/src/session_restore.rs`, `crates/frankenterm/src/main.rs` | Core persistence exists, but NTM-style operator-facing save/restore ergonomics are still uneven. |
| Git integration and hooks | repo hook installer, diagnostics, external automation | Drop / defer | `scripts/install-hooks.sh`, `.git/hooks/pre-commit`, selected CLI diagnostics | NTM-style git and hook orchestration is not a core FrankenTerm differentiator and should stay decoupled unless it directly serves swarm safety. |
| Context and memory management | `cass`, accounts, mission/planner scaffolding, search/replay surfaces | Partial parity | `crates/frankenterm-core/src/cass.rs`, `crates/frankenterm-core/src/accounts.rs`, `crates/frankenterm-core/src/plan.rs` | Pieces exist, but there is no single in-core context-rotation or memory-management workflow comparable to NTM. |
| Approval and conflict management | `ft approve`, policy decisions, reservations, Agent Mail integration outside core | Superset / gap | `crates/frankenterm-core/src/policy.rs`, `crates/frankenterm-core/src/approval.rs`, `crates/frankenterm-core/src/storage.rs` | FrankenTerm is stronger on policy and audit, but work claims and cross-agent conflict handling are still partly externalized. |
| Workflow and task orchestration | `ft workflow *`, tx and mission surfaces, Robot/MCP tools | Partial parity | `crates/frankenterm-core/src/workflows/*`, `crates/frankenterm-core/src/plan.rs`, `crates/frankenterm-core/src/mcp.rs`, `crates/frankenterm/src/main.rs` | Durable workflow substrate exists; mission-level assignment, claim, and rebalance semantics remain unfinished. |
| Ensemble reasoning | no first-class equivalent yet | Gap | `crates/frankenterm-core/src/plan.rs`, related mission orchestration modules | NTM's explicit ensemble strategies are not yet a native FrankenTerm feature family. |
| Policy, safety, access control | `ft send` dry-run and policy checks, approvals, audit chain, command guard | Superset | `crates/frankenterm-core/src/policy.rs`, `crates/frankenterm-core/src/approval.rs`, `crates/frankenterm-core/src/command_guard.rs`, `crates/frankenterm-core/src/policy_*` | This is already one of FrankenTerm's strongest convergence advantages over NTM. |
| Notifications and messaging | event notifications, desktop/email hooks, Agent Mail used alongside ft | Partial parity | `crates/frankenterm-core/src/notifications.rs`, `crates/frankenterm-core/src/desktop_notify.rs`, `crates/frankenterm-core/src/email_notify.rs` | Notification plumbing exists, but an in-core mailbox and threading surface is still external to FrankenTerm proper. |
| Redaction, privacy, encryption | redacted read/search paths, diagnostic redaction, policy guards | Superset | `crates/frankenterm-core/src/diagnostic_redaction.rs`, `crates/frankenterm-core/src/policy.rs`, `crates/frankenterm-core/src/storage.rs` | Secret-safe read surfaces are already a first-class design constraint. |
| Project and template management | config profiles, setup flows, session/template docs | Partial parity | `crates/frankenterm-core/src/config_profiles.rs`, `crates/frankenterm/src/main.rs`, `docs/*` | Some scaffolding exists, but NTM's template, persona, and project bootstrapping story is broader than current ft UX. |
| Configuration and system tooling | `ft config *`, `ft setup`, `ft doctor`, version/build metadata | Parity | `crates/frankenterm/src/main.rs`, `crates/frankenterm-core/src/config.rs`, `crates/frankenterm-core/src/logging.rs` | This area is already serviceable and mostly needs polish rather than new architecture. |
| Advanced diagnostics | `ft triage`, `ft diag bundle`, replay, forensics, and export surfaces | Superset | `crates/frankenterm-core/src/diagnostic.rs`, `crates/frankenterm-core/src/incident_bundle.rs`, `crates/frankenterm-core/src/forensic_export.rs` | FrankenTerm's evidence-first diagnostics path is broader than NTM's current CLI model. |
| Multi-project orchestration | distributed mode, headless mux server, future mission control | Gap | `crates/frankenterm-core/src/distributed.rs`, `crates/frankenterm-mux-server*`, `crates/frankenterm-core/src/plan.rs` | Early primitives exist, but there is no finished native equivalent to NTM's multi-project swarm and controller workflow yet. |

## Preserve / Upgrade / Drop Handoff Matrix

This matrix is the action-oriented handoff for downstream implementation beads.
It is intentionally keyed to upstream source anchors, current FrankenTerm
surfaces, and the primary tracks that should absorb each capability family.

| Capability Family | Decision | NTM Source Anchors | FrankenTerm Target Surface | Primary Downstream Tracks |
|---|---|---|---|---|
| Root CLI and robot routing | Upgrade | `internal/cli/root.go`, `internal/robot/robot.go` | `crates/frankenterm/src/main.rs`, future command-surface decomposition | `ft-3681t.4.*`, `ft-3681t.9.*` |
| Session coordinator, activity inference, alerts | Preserve then upgrade | `internal/cli/coordinator.go`, `internal/coordinator/coordinator.go` | `runtime.rs`, `ingest.rs`, `events.rs`, `metrics.rs`, `plan.rs` | `ft-3681t.3.*`, `ft-3681t.7.*` |
| File reservations, conflict handling, approvals | Preserve | `internal/watcher/file_reservation.go`, `internal/watcher/conflict.go`, `internal/approval/engine.go` | `policy.rs`, `approval.rs`, `storage.rs`, workflow locks, reservation-aware robot/MCP flows | `ft-3681t.3.*`, `ft-3681t.6.*` |
| Checkpoints, restore, context reinjection | Preserve then upgrade | `internal/session/restore.go`, `internal/checkpoint/restore.go` | `snapshot_engine.rs`, `session_restore.rs`, `restore_*`, `session_topology.rs` | `ft-3681t.2.*`, `ft-3681t.8.*` |
| Capability-based assignment and blocker-aware routing | Preserve then upgrade | `internal/assign/matcher.go` | `swarm_work_queue.rs`, `plan.rs`, mission/runtime scheduling surfaces | `ft-3681t.3.*`, `ft-1i2ge*` |
| Machine-readable operator APIs | Preserve envelope, upgrade internals | `internal/robot/robot.go`, robot handlers in `internal/cli/root.go` | `robot_types.rs`, `mcp.rs`, `mcp_bridge.rs`, `main.rs` | `ft-3681t.4.*`, `ft-3681t.8.*` |
| tmux-specific session mechanics and TUI shell | Drop implementation, keep semantics | tmux and Cobra/TUI command surfaces across `internal/cli/*` | native mux, GUI, headless mux server, `ft tui` | `ft-3681t.2.*`, `ft-1memj*` |

---

## Coordinator Architecture

### Core Coordinator
- `SessionCoordinator` tracks all agent states per session
- Polling-based monitoring (5s intervals, configurable)
- Parallel pane capture → activity velocity analysis → conflict detection → health assessment → event emission

### Agent State Model
States: `GENERATING` | `WAITING` | `THINKING` | `ERROR` | `STALLED` | `UNKNOWN`
- Pattern-based inference using 50+ regex patterns per agent type
- Velocity calculation (chars/sec)
- Hysteresis (2s stability before state transition, except ERROR which is immediate)

### Work Assignment
Three-phase: Capability Matching → Availability Check → Affinity Scoring
- Capability matrix: agent × task type (bug/feature/refactor/docs) with float scores
- Strategies: affinity, round-robin, least-loaded, optimal
- Weighted combination: 40% capability + 40% availability + 20% affinity

### Conflict Detection
- Monitors file reservations, detects 2+ agents on same file
- Triggers negotiation/notification, optional auto-resolution
- Logs conflicts to audit chain

---

## Session Semantics

### Naming
- Session: `{project}[__{label}]` (e.g., `myproject__frontend`)
- Pane: `{session}__{type}_{index}[_{variant}][{tags}]` (e.g., `myproject__cc_1_opus`)

### Persistence
- State snapshots: `~/.cache/ntm/sessions/{name}.json`
- Checkpoints: `~/.cache/ntm/checkpoints/{session}/{id}.json`
- Includes: git branch, commit, pane dimensions, agent types, creation time

### Recovery
1. Check if session exists in tmux
2. If panes present, restore from live panes
3. If missing, search for recent checkpoint
4. Offer restore or create fresh

---

## Safety Controls

### Approval Workflows
- Force-release file lock requires manual approval (24h timeout)
- SLB (Two-Person Rule) for critical operations
- Approval tokens with expiry

### File Reservations
- TTL-based locks (default 30m)
- Renew/force-release flows
- Agent Mail integration for notifications

### Alert System (20+ alert types)
`agent_stuck`, `agent_crashed`, `agent_error`, `disk_low`, `context_warning`, `rate_limit`, `mail_backlog`, `bead_stale`, `rotation_failed`, `file_conflict`, etc.

Lifecycle: Created → Refreshed → Resolved → Pruned (60min)

### Redaction & Privacy
- Pattern-based PII/secret detection
- Session log scrubbing before archival
- Optional encryption at rest

---

## Robot API

### State Inspection
`--robot-status`, `--robot-context`, `--robot-snapshot`, `--robot-tail`, `--robot-inspect-pane`, `--robot-files`, `--robot-metrics`, `--robot-health`, `--robot-dashboard`, `--robot-plan`, `--robot-graph`

### Agent Control
`--robot-send`, `--robot-ack`, `--robot-spawn`, `--robot-interrupt`, `--robot-assign`, `--robot-replay`, `--robot-dismiss-alert`

### Bead Management
`--robot-bead-create`, `--robot-bead-show`, `--robot-bead-claim`, `--robot-bead-close`

### Output Formats
JSON (default, full fidelity), TOON (token-efficient, ~40-60% smaller), TERSE (single-line minimal)

### Error Codes
`SESSION_NOT_FOUND`, `AGENT_OFFLINE`, `APPROVAL_REQUIRED`, `CONTEXT_HIGH`, `RATE_LIMITED`, `INSUFFICIENT_PERMISSIONS`

---

## Event Bus & Hooks

### Event Types
`agent.idle`, `agent.busy`, `agent.error`, `agent.recovered`, `conflict.detected`, `conflict.resolved`, `work.assigned`, `digest.sent`

### Git Hooks
pre-commit (UBS scan), pre-push, commit-msg, post-commit, post-checkout

### Subscriber Pattern
Events route to: notification subsystem, CASS indexer, health monitor, dashboard, Agent Mail

---

## Analytics & Diagnostics

### Metrics
Tokens per agent, time per task, error rate, output lines, context usage trend, compilation/test pass rate

### Health States
Green (all pass) → Yellow (warnings) → Red (failures) → Black (critical)

### Audit Trail
Who, What, When, Why, Result — stored in `~/.cache/ntm/audit/{session}.jsonl`

## Downstream Implementation Pressure Points

1. `crates/frankenterm/src/main.rs` is the current command-surface choke point.
   Any serious NTM-parity expansion in `ft robot`, mission control, or session
   verbs will keep colliding here until routing is decomposed.
2. `crates/frankenterm-core/src/wezterm.rs` and the
   `crates/frankenterm-mux-server*` split define the transition seam from
   compatibility backend to native mux ownership. Track 2 work should treat
   this boundary as a first-class migration target.
3. `runtime.rs` + `ingest.rs` + `storage.rs` + `patterns.rs` + `events.rs`
   already realize much of NTM's observability and health value. Downstream
   work should build on those subsystems rather than recreate parallel
   coordinator state stores.
4. Work claims, file coordination, and mailbox-style collaboration are still
   split between in-repo primitives and external Agent Mail/Beads workflows.
   Track 3, 4, and 6 convergence work needs an explicit decision on what
   becomes native to FrankenTerm versus what remains an adjacent operating tool.

---

## Assessment: Preserve / Improve / Drop

### PRESERVE (High Value)
1. **Coordinator Architecture** — Core orchestration logic, event-driven state transitions
2. **Pane Activity Detection** — 50+ regex patterns, velocity calculation, hysteresis
3. **File Reservation / Locking** — TTL locks, force-release with approval
4. **Approval Workflows** — Two-person rule, tokens with expiry
5. **Alert Generation** — 20+ types, lifecycle tracking
6. **Robot API Envelope** — Standardized JSON with error codes
7. **Event Bus** — Decoupled publish-subscribe

### IMPROVE (Adapt for FrankenTerm)
1. **Workflow Templating** — Add error recovery, sub-workflow composition
2. **Capability Matrix** — Add learning, resource constraints, dynamic rebalancing
3. **Ensemble Reasoning** — Better conflict resolution, consensus scoring
4. **Persona System** — Granular hierarchy, composition, dynamic switching

### DROP (Limited Relevance)
1. **Tmux Integration** — Study models, drop tmux calls
2. **Command Palette UI** — Extract logic, reimplement
3. **Git Hooks** — Orthogonal; defer to external automation
4. **Dashboard Visual Effects** — Cosmetic; focus on function first

### Porting Effort Estimate
~12-20 weeks (3-5 person-months) for full port of core systems

---

*END OF CENSUS*
