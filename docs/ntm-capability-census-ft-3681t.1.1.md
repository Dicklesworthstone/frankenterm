# NTM (Named Tmux Manager) — Exhaustive Capability Census

> **Bead**: ft-3681t.1.1
> **Author**: PinkForge (claude-code, opus-4.6)
> **Date**: 2026-03-11
> **Source**: /dp/ntm (Go-based, ~60+ internal packages, 200+ CLI files)

---

## Executive Summary

NTM is a mature, feature-rich tmux session management platform for orchestrating multiple AI coding agents in parallel. It has **150+ CLI commands** organized into 20+ functional domains, sophisticated multi-agent coordination, robust safety guardrails, and rich machine APIs.

**Key assessment**: NTM solved *tmux-based orchestration well*. FrankenTerm should solve *agent-agnostic orchestration* — the principles transfer, but the implementation details differ significantly.

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
