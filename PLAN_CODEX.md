# PLAN_CODEX.md — FrankenTerm (`ft`) — End-to-End System Plan (Codex)

*Created:* 2026-01-18  
*Primary spec:* `wezterm_automata_prompt_je.md` (JE; legacy filename)  
*Background:* `frankenterm_guide.md`, `wezterm_automation_guide_chatgpt.md`, `wezterm_guide_gemini.md`, `PLAN.md`  

---

## 0) What this plan is (and isn’t)

This document is a **single, comprehensive build plan** for `ft`: a Rust-first control plane for **observing and orchestrating WezTerm mux panes** that run AI coding agents (Claude Code, Codex CLI, Gemini CLI, and future agents).

It is deliberately:
- **Deterministic** (no timing-based “sendkeys” automation).
- **Observable** (every action is rooted in measured terminal state).
- **Operational** (daemon + DB + APIs + safety + tests).
- **Accretive** (each phase delivers real value; no speculative rewrites).

Non-goals:
- Not trying to replace the agents themselves.
- Not trying to build a general-purpose distributed scheduler on day 1.
- Not trying to fork/replace WezTerm unless the ROI is overwhelming (see vendoring strategy).

---

## 1) North Star

### 1.1 The one-sentence mission

Turn WezTerm’s mux into a **high-reliability “terminal hypervisor”** for agent swarms: *observe everything, understand key events, act safely and reliably, and expose a machine-optimized control surface for agents.*

### 1.2 The non-negotiables (from JE)

1. **Canonical WezTerm setup** (remote `wezterm-mux-server` per domain via systemd + linger; local reconnects).
2. **Capture ALL output** from ALL panes across ALL domains and persist to **SQLite + FTS5**.
3. **Detect tell-tale patterns** (compaction, usage limit warnings/reached, session end/resume IDs, auth prompts) and store structured extracts.
4. **Act via WezTerm APIs**, not brittle sendkeys:
   - Prefer WezTerm CLI and mux protocol.
   - Use Lua IPC / OSC user-vars for low-latency signaling when helpful.
5. **Workflows** are explicit, agent-specific, testable state machines:
   - `handle_usage_limits` (cc/cod/gmi)
   - `handle_compaction` (cc/cod/gmi)
   - Additional workflows evolve, but these are foundational.
6. `ft` must have **agent-first “robot mode”** (JSON/Markdown, token-efficient, quick-start when no args).
7. Integrate existing tooling:
   - `cass` (`/dp/coding_agent_session_search`)
   - `caut` (`/dp/coding_agent_usage_tracker`, CodexBar logic in Rust)
   - `fastmcp_rust` for MCP
   - `rich_rust` + `charmed_rust` for ergonomic UX (CLI + optional TUI)
   - `asupersync` + `fastapi_rust` where they add real leverage (sync + HTTP control plane)
8. Consider **WezTerm vendoring** (selective, transformation-based) and make an explicit recommendation.

---

## 2) User stories (the “why” translated into behaviors)

### 2.1 Human operator stories

1. “Show me what all agents are doing *right now* across all domains.”
2. “Search: where did the agent mention `panic` / `clippy` / `usage limit` last week?”
3. “This Codex pane hit limit; rotate accounts, re-auth, resume session, send `proceed.`”
4. “After compaction, automatically re-inject a context refresh prompt.”
5. “Bring a new remote host online: install matching WezTerm, set up mux server, standardize config.”
6. “I want a clean UI to browse panes and events when I’m debugging at 2am.”

### 2.2 Agent (robot-mode) stories

1. “List panes; pick the pane running Claude; fetch last 200 lines; decide next action.”
2. “Wait for a pattern; then send the next command; confirm output contains expected marker.”
3. “Search historical output for a prior decision / error; cite it back to the operator agent.”
4. “Execute a named workflow and get back structured results (what happened, what changed, what to do next).”

---

## 3) System overview (control plane + data plane)

### 3.1 High-level architecture

`ft` is a **local control plane** that speaks to WezTerm (local GUI + remote mux servers) and exposes:
- **Daemon**: continuous capture + detection + workflow orchestration
- **SQLite**: durable store + FTS5
- **CLI**: human + robot modes
- **MCP server** (`fastmcp_rust`): tool surface for other agents
- **HTTP API** (`fastapi_rust`, optional): dashboards, webhooks, remote control (if/when useful)
- **TUI** (`charmed_rust`, optional): interactive human debugging surface
- **Sync** (`asupersync`, optional): syncing configs/binaries/db snapshots across machines

### 3.2 The most important separation: “observe” vs “act”

To keep reliability high, we split the runtime into two conceptual loops:

1. **Observation loop**: capture output → persist → detect events  
2. **Action loop**: evaluate events → run workflows → emit actions → audit

The action loop *never* runs on stale assumptions; it always re-checks current pane state before sending input.

---

## 4) WezTerm integration: tiers, contracts, and reliability

### 4.1 Tiered integration (graceful fallback)

Tier 1 (required): **WezTerm CLI** (`wezterm cli`)
- `list --format json` as the authoritative “pane inventory”
- `get-text` to pull content
- `send-text` to inject input
- `spawn`, `split-pane`, etc. for lifecycle management

Tier 2 (strongly recommended): **Lua + OSC user-vars as a signaling lane**
- Use OSC 1337 `SetUserVar` from within panes to publish *structured events* (JSON) to WezTerm.
- Use `wezterm.on('user-var-changed', ...)` in `wezterm.lua` to forward these signals to `ft` (non-blocking via `wezterm.background_child_process`).
- This becomes the “low-latency interrupt line” (e.g., “agent is ready”, “workflow step completed”, “I’m at prompt”, “session id is X”).

Tier 3 (optional, high ROI only): **Selective vendoring of WezTerm crates**
- Goal: reduce subprocess overhead + enable “subscribe to output” instead of polling.
- Done as an *optional feature* with hard version checks and clear fallback to Tier 1.

### 4.2 Canonical WezTerm setup (enforced by `ft setup`)

We standardize (and optionally auto-install) the remote mux server approach:
- systemd user unit for `wezterm-mux-server`
- `loginctl enable-linger $USER`
- pinned WezTerm version parity across local + remotes
- no nested tmux/screen in panes (assumed)

`wa setup` should:
- Parse `~/.ssh/config`, and optionally scan shell rc files for `ssh` aliases.
- Offer an interactive selection (TUI) and/or robot-friendly JSON plan (“what will be installed where”).
- Perform remote bootstrap over SSH.

### 4.3 Pane identity & stability

Assumptions to design for:
- pane IDs are stable while a pane exists, but panes can be created/destroyed freely.
- domains may disconnect/reconnect; remotes may restart mux server.

Design:
- Model “pane instance” as `(domain_name, pane_id)` plus a **fingerprint**:
  - title + cwd + initial banner hashes
  - first-seen timestamp
- If pane_id disappears, treat as ended and close out any active agent session row.

### 4.4 Input injection correctness (no “blind typing”)

Actions always require a guard:
- If pane is in alt-screen (vim/less), default to **no automatic typing**.
- Rate-limit sends per pane.
- Workflow steps can require either:
  - explicit “ready” user-var signal, or
  - detected prompt marker, or
  - a stable pattern in the last N lines.

To send Ctrl-C:
- `send-text` should support sending byte 0x03 via Rust string `'\u{3}'` and `--no-paste` where applicable.

---

## 5) Data model: SQLite as the durable “world memory”

### 5.1 Storage goals

We need three classes of stored data:
1. **Raw transcript** (what happened)
2. **Indexed search** (fast recall)
3. **Structured facts** (session IDs, token usage, usage reset times, workflow results)

SQLite + WAL + FTS5 is the right default:
- portable, embeddable
- strong enough for our scale (100s of panes, long histories)
- excellent full text search and snippet/highlight

### 5.2 Tables (core set)

Minimum viable schema (expand as needed):
- `domains`: known domains and metadata
- `panes`: current + historical pane registry
- `captures`: append-only output chunks (delta-first)
- `captures_fts`: FTS5 virtual table over capture text
- `events`: detected events with extracted JSON
- `agent_sessions`: per-agent session timeline (start/end, model, token usage, resume IDs)
- `workflow_runs`: workflow executions + step tracing
- `accounts`: services + accounts + usage metadata (populated/updated via `caut`)
- `config`: persisted settings (poll intervals, retention policy, safety gates)

### 5.3 Capture strategy (guaranteeing “ALL output”)

We must avoid losing output when scrollback wraps. The robust approach:

1. **Polling baseline** (Tier 1):
   - Poll each pane at an adaptive interval.
   - Pull last N lines (or entire scrollback) and compute overlap with last-known tail.
   - Persist only the **delta** chunk.

2. **Safety margin**:
   - Set WezTerm scrollback high enough (e.g., 10k–100k lines) so wrap is unlikely between polls.
   - Adaptive polling speeds up on active panes; slows down on idle panes.

3. **Optional “high assurance mode”** (Tier 2 signaling):
   - Shell integration emits prompt boundaries and command boundaries via user-vars.
   - This gives explicit segmentation (“this output belongs to command X”).

4. **Optional “stream mode”** (Tier 3 vendoring):
   - Subscribe to pane output stream and append directly, bypassing polling.

### 5.4 FTS strategy (search that actually feels magical)

FTS details:
- Use `unicode61` tokenizer with separators tuned for paths (`/_-.`).
- Keep `pane_id`, `domain_id`, timestamps as UNINDEXED columns for filtering.
- Provide helpers for:
  - “search within pane”
  - “search across domain”
  - “search only errors / warnings” (via event join)

### 5.5 Retention, compaction, and DB hygiene

Log everything, but don’t let it rot:
- Configurable retention (days, max DB size, or “keep last N captures per pane”).
- Periodic `VACUUM` only via explicit command (avoid surprise long pauses).
- Prefer chunk-level deletion + FTS sync triggers.
- Optionally compress older chunks (zstd) if/when it pays off (store compressed blob + extracted text for FTS).

---

## 6) Pattern & extraction engine (fast, correct, testable)

### 6.1 What we’re detecting (first-class)

From JE’s examples (minimum set):
- Claude Code:
  - `Conversation compacted`
  - usage-limit warnings/reached (format varies; treat as pattern pack that evolves)
- Codex CLI:
  - usage warnings (25%/10%/5%)
  - usage limit reached line with retry timestamp
  - session end block: `Token usage: total=... input=... (+ ... cached) output=... (reasoning ...)`
  - resume hint: `codex resume <uuid>`
  - device auth prompt: device code `XXXX-YYYYY` + URL
- Gemini CLI:
  - usage limit reached
  - session summary including session id
  - model indicator: `Responding with gemini-...`

We treat these as **versioned rule packs**, not hardcoded one-offs.

### 6.2 Engine architecture (inspired by JE + dp patterns)

We want DCG-style performance discipline:
1. **Quick reject** (`memchr`/`memmem`): if none of the relevant keywords exist, do nothing.
2. **Multi-literal scan** (`aho-corasick`): fast detection of known anchors.
3. **Regex extraction** (`regex` / `fancy-regex` when lookarounds are worth it):
   - keep extraction regexes tight and anchored
   - use named captures for structured fields
4. **Post-processing**:
   - parse numbers (strip commas)
   - parse timestamps into canonical ISO-8601 + timezone
   - attach confidence scores

Key output of the engine:
- `Detection { rule_id, agent_type, event_type, severity, confidence, raw_context, extracted_json }`

### 6.3 Pattern packs (how we scale beyond the first 20 rules)

Define a “pack” system:
- `core.codex`, `core.claude_code`, `core.gemini`
- `core.wezterm` (mux/server diagnostics)
- `org.local` (user custom rules)

Each rule has:
- stable `rule_id` (`codex.session_end`, `claude.compaction`, …)
- match method: literal anchor(s) + optional regex extraction
- severity + suggested remediation

	This makes it possible to:
	- add rules without rewriting the engine
	- test rules in isolation
	- expose rules via CLI (`ft rules list`, `ft rules test`)

### 6.4 Structured parsing where regex is the wrong tool (ast-grep)

	We incorporate `ast-grep` in two **high-leverage** places:

	1. **Codebase understanding workflows** (not pane transcript matching):
	   - When an agent is stuck, `ft` can run (or instruct a pane to run) AST scans to locate:
	     - `TODO`, `FIXME`, unsafe patterns, known bug signatures
	     - “where is this function defined?” queries
	   - Results are summarized and optionally injected into agent panes as context.

2. **Rule development tooling**:
   - Use ast-grep to enforce “no brittle regex” in our own codebase (e.g., ensure patterns live in a dedicated module with tests).

This keeps ast-grep aligned to what it’s best at: **structure-aware matching**.

---

## 7) Workflow engine (deterministic automation as state machines)

### 7.1 Workflow design constraints

Every workflow must be:
- **Idempotent**: re-running doesn’t make a mess.
- **Recoverable**: can resume after a crash.
- **Audited**: every step recorded (what was observed, what was sent).
- **Guarded**: each step validates state before action.

### 7.2 Execution model

Workflows are explicit step machines:
- each step: `Observe → Decide → Act → Verify`
- step can `Continue`, `WaitFor(condition)`, `Retry(after)`, `Abort(reason)`, `Done(result)`
- a per-pane lock prevents interleaving workflows that would conflict

### 7.3 Foundational workflows

#### A) `handle_compaction` (cc/cod/gmi)

Trigger: compaction detected for that agent.

Steps:
1. Verify compaction completed (pattern stable for N polls OR explicit user-var).
2. Inject agent-appropriate prompt (JE’s preferred example for Claude):
   - “Reread AGENTS.md so it’s still fresh in your mind.”
3. Record workflow result (event handled, prompt sent, confirmation pattern seen if available).

#### B) `handle_usage_limits` (cc/cod/gmi)

Trigger: usage limit reached (or “critical warning” if configured).

Codex path (JE example):
1. Gracefully exit current session (Ctrl-C twice) and wait for session summary.
2. Parse token usage + resume session id.
3. Choose next account via `caut` (best remaining quota) and record selection.
4. Start `cod login --device-auth` and extract device code.
5. Run Playwright automation to complete device auth, using stored browser profile.
6. Resume session (`cod resume <id>`) and send `proceed.`
7. Persist:
   - token usage breakdown
   - resume id
   - account rotation event

Gemini path:
- `/quit`, parse Session ID from summary, re-auth via `/auth`, resume as supported.

Claude Code path:
- Detect/derive session identity (often indirect):
  - integrate `cass` to correlate recent sessions and derive session metadata
  - treat “session id” as external correlation key, not necessarily printed

### 7.4 “Workflow library” vs “workflow scripts”

We support **two layers**:
1. Compiled Rust workflows (fast, safe, testable).
2. Data-driven workflow descriptors for simple sequences (YAML/TOML):
   - still executed by the same guarded engine
   - used for “prompt injection sequences” and operator customization

---

## 8) Accounts, auth, and usage tracking (caut + Playwright)

### 8.1 `caut` as the source of truth

Use `caut` for:
- per-service usage %
- reset time prediction
- selecting “next best account”

	`ft` responsibilities:
	- store a local mirror in SQLite for history and offline behavior
	- reconcile `caut` data periodically

### 8.2 Playwright automation (device auth)

Constraints:
- must tolerate MFA / SSO realities
- must support “persistent browser profiles” per account (cookies, SSO sessions)
- must have timeouts and explicit failure modes

	Design:
	- store profiles under `~/.local/share/ft/browser_profiles/<service>/<account>/` (legacy: `~/.local/share/wa/...`)
	- implement “semi-automated fallback”:
	  - if Playwright can’t complete step (password/MFA), open interactive browser and prompt human (or agent) to finish once
	  - then persist profile for future headless runs

### 8.3 Credential handling

Principles:
- prefer SSO sessions + profiles over storing plaintext passwords
- if secrets must be stored, integrate OS keychain (future) and keep the DB free of secrets
- never print secrets in robot mode output

---

## 9) Interfaces (CLI, Robot, MCP, HTTP, TUI)

### 9.1 CLI modes

	`ft` has two “user personas”:
	- human operator: wants rich summaries, tables, panels
	- agent operator: wants minimal, structured output

	Implementation:
	- default human mode uses `rich_rust` for readable output
	- `ft robot ...` emits JSON/TOON, stable schemas

### 9.2 Robot mode contract (stable and token-efficient)

All robot commands return:
```json
{
  "ok": true,
  "data": { "...": "..." },
  "error": null,
  "hint": null,
  "elapsed_ms": 12,
  "version": "0.1.0"
}
```

	Key robot commands:
	- `ft robot state` (panes + inferred agent/state)
	- `ft robot get-text <pane_id> [--tail N] [--escapes]`
	- `ft robot send <pane_id> "<text>" [--wait-for "..."] [--timeout ...]`
	- `ft robot wait-for <pane_id> "<pattern>"`
	- `ft robot search "<fts query>" [--pane-id ...] [--since ...]`
	- `ft robot events [--unhandled] [--pane-id ...]`
	- `ft robot workflow <name> <pane_id> [--force]`
	- `ft robot quick-start` (when no args, same content but compressed)

### 9.3 MCP server (`fastmcp_rust`)

Expose tools/resources that mirror robot mode:
- Tools: `state`, `get_text`, `send`, `wait_for`, `search`, `events`, `workflow_run`, `accounts_list`, `accounts_refresh`
- Resources: `wa://panes`, `wa://events`, `wa://accounts`, `wa://workflows`

	Design choice:
	- MCP should be a thin transport; business logic lives in `frankenterm-core`.

### 9.4 HTTP API (`fastapi_rust`, optional but high leverage)

Why:
- dashboards, push notifications, webhooks
- remote “operator console” (humans or agents)

Keep it pragmatic:
- read-only endpoints first (`/panes`, `/events`, `/search`)
- authenticated control endpoints later (`/send`, `/workflow/run`)

### 9.5 TUI (`charmed_rust`, optional)

Use when it genuinely improves ergonomics:
- interactive pane picker
- event feed
- transcript viewer with search + highlight
- account rotation status

	The TUI should be a *front-end* over the same `frankenterm-core` APIs, not its own logic fork.

---

## 10) Sync & multi-machine pragmatics (`asupersync`)

We will inevitably have:
- local Mac control plane
- remote Linux domains where the work happens

	`asupersync` can be used for:
	1. **Shipping `ft` binaries** to remotes (when we want local execution there).
	2. **Syncing standard configs** (WezTerm unit files, templates, wrapper scripts).
	3. **Mirroring snapshots** of SQLite DB for backup / analysis (not hot replication initially).

Principle: sync is a *tool*, not the architecture. Start with simple copy + checksum + idempotent apply.

---

## 11) Performance engineering (budgets and mechanisms)

### 11.1 Budgets (what “fast enough” means)

Targets:
- Idle overhead: near-zero (quick reject avoids almost all work)
- Active pane detection latency: sub-second end-to-end
- Pattern detection: < 5ms per typical delta
- 100 panes: stable < 5–10% CPU on a modern laptop (adaptive polling)

### 11.2 Mechanisms

- Adaptive polling per pane (fast when active, slow when idle).
- Delta extraction via overlap detection (avoid dumping full scrollback repeatedly).
- Multi-pattern literal matching (Aho-Corasick).
- Avoid allocations on hot paths where possible (bytes-in, minimal copies).
- Batch DB writes with transactions (but never lose durability semantics).

### 11.3 Benchmarking

Add micro-benchmarks for:
- overlap/delta extraction
- pattern engine quick reject
- FTS query latency on representative DB size

---

## 12) Safety & guardrails (don’t become the thing we’re trying to control)

### 12.1 “Do no harm” principles

	`ft` must never:
	- spam input into the wrong pane
	- type into alt-screen apps (vim/less) unless explicitly enabled
	- inject destructive commands via automation

### 12.2 Command safety gate (leveraging dp guardrails)

Before sending text that looks like a command:
- run it through a local “denylist/allowlist” gate
- optionally integrate with `dcg`-style rules if installed
- require explicit operator enablement for any “dangerous class” sends

### 12.3 Audit trail

Every action is recorded:
- who initiated (human CLI / robot / MCP)
- pane and domain
- preconditions observed
- exact bytes sent (or a redacted version if sensitive)
- verification result

---

## 13) WezTerm vendoring strategy (explicit recommendation)

### 13.1 Decision framework

Vendoring is justified only if it materially improves:
- correctness (streaming output vs polling loss modes)
- latency (event subscription)
- capability (deep pane metadata not exposed in CLI)

### 13.2 Recommendation

Adopt **Selective Vendoring**, not a full fork:
- depend on a small set of WezTerm crates pinned to a commit
- keep it behind a feature flag (`--features vendored`)
- implement strict runtime version checks
- keep Tier 1 CLI fallback always available

Implementation posture:
- start with CLI first; prove product value
- add vendoring only after we have benchmarks showing clear wins

---

## 14) Proposed crate/workspace layout (Rust 2024, no unsafe)

	Workspace sketch (evolves as needed):
	- `crates/frankenterm-core` — domain model, capture engine, pattern engine, workflow engine, storage, safety
	- `crates/frankenterm` — CLI + robot mode + optional surfaces (TUI/MCP/web behind feature flags)
	- `fuzz/` — fuzz targets and harnesses

Keep `unsafe` forbidden; performance comes from algorithms and careful allocation discipline, not unsoundness.

---

## 15) Testing strategy (trust through verification)

### 15.1 Unit tests
- delta extraction correctness (overlap cases, wrap cases)
- pattern rules per agent (positive + negative fixtures)
- workflow step guards (prompt detection, alt-screen checks)
- DB schema migration tests (when we introduce migrations)

### 15.2 Integration tests
- simulate `wezterm cli list/get-text` outputs via fixtures
- run workflow engine against fake panes

	### 15.3 End-to-end tests (when WezTerm available)
	- spin up `wezterm-mux-server` locally
	- spawn dummy panes that emit known outputs
	- verify `ft` captures and detects events
	- verify workflow actions send expected bytes and stop safely

### 15.4 “Regression packs”
Each time a real-world pattern changes (agent output format drift), add:
- failing fixture from the field
- updated rule + tests

---

## 16) Implementation roadmap (accretive, pragmatic)

	### Phase 0: Scaffolding (1–2 days)
	- Rust workspace skeleton, `ft` CLI shell, `frankenterm-core` library
	- SQLite schema init + WAL + FTS5
	- `ft robot quick-start`

	### Phase 1: Observation MVP (week 1)
	- WezTerm CLI adapter: list panes, get text, send text
	- capture loop with adaptive polling + delta extraction
	- store captures + FTS
	- `ft robot state/get-text/search`

	### Phase 2: Detection MVP (week 2)
	- pattern engine packs for Codex + Gemini + Claude compaction
	- events table + event feed in robot mode
	- `ft robot events`

### Phase 3: Workflow MVP (week 3)
- workflow engine + per-pane locks + audit
- `handle_compaction` end-to-end (safe and deterministic)
- `handle_usage_limits` for **Codex** path (JE’s most concrete flow)

### Phase 4: Accounts + Auth (week 4–5)
- `caut` integration for usage selection
- Playwright device auth flow + persistent profiles
- robust failure modes + human fallback

### Phase 5: MCP + Operator UX (week 6)
- MCP server using `fastmcp_rust` mirroring robot commands
- `rich_rust` output polish for humans
- optional `charmed_rust` TUI for pane browsing

### Phase 6: Setup automation + sync (week 7)
- `wa setup` local + remote (systemd mux server)
- optional `asupersync` for distributing configs/binaries

### Phase 7: Vendoring spike (week 8+; only if warranted)
- prototype direct mux protocol access
- benchmark vs CLI polling
- decide to ship behind feature flag or defer

---

## 17) Open questions (explicit, so we resolve them early)

1. **Scrollback limits**: what default `scrollback_lines` do we standardize on to guarantee capture without bloat?
2. **Claude session identity**: what is the best stable “session key” to correlate with `cass`?
3. **Auth realities**: which services can be fully automated vs require “one-time human bootstrap”?
4. **Data volume**: do we need compression on day 1, or is retention enough?
5. **Vendoring ROI**: what concrete capability is blocked by CLI+Lua that justifies vendoring?

---

## 18) Definition of “done enough to ship”

	`ft` is “meaningfully real” when:
	- it continuously captures pane output and can search it with FTS
	- it detects at least the primary JE patterns reliably
	- it can run `handle_compaction` safely
	- it can run `handle_usage_limits` for Codex end-to-end at least once in the real world
	- it exposes robot mode + MCP so an agent can operate it without the UI

---

# Appendices (deep details for implementation)

## Appendix A: CLI / Robot / MCP surface (proposed)

### A.1 Top-level CLI commands

	Human-oriented (rich output by default via `rich_rust`):
	- `ft status` — panes + inferred agent state
	- `ft watch` — run daemon (foreground or background)
	- `ft events` — recent / unhandled events
	- `ft search` (alias: `ft query`) — full text search (FTS5)
	- `ft send` — send text / control codes to a pane (guarded)
	- `ft workflow` — run a workflow manually (guarded)
	- `ft rules` — list/test rule packs, show matching traces
	- `ft accounts` — list/refresh accounts, show rotation picks (delegates to `caut`)
	- `ft setup` — local/remote canonical WezTerm configuration
	- `ft doctor` — environment checks (wezterm presence, version parity, DB health)
	- `ft export` — export slices of history (JSONL/NDJSON), DB snapshot, or reports
	- `ft web` — start HTTP server (`fastapi_rust`) if enabled
	- `ft tui` — start interactive UI (`charmed_rust`) if enabled
	- `ft sync` — sync configs/binaries/db snapshots (`asupersync`) if enabled

	Agent-oriented (stable schemas, token-efficient):
	- `ft robot ...` — canonical agent interface

### A.2 Robot mode schemas

Robot output (always):
```json
{
  "ok": true,
  "data": {},
  "error": null,
  "hint": null,
  "elapsed_ms": 12,
  "version": "0.1.0",
  "now": "2026-01-18T02:24:00Z"
}
```

Robot error:
```json
{
	  "ok": false,
	  "data": null,
	  "error": {
	    "code": "robot.pane_not_found",
	    "message": "Pane 123 does not exist",
	    "details": { "pane_id": 123 }
	  },
	  "hint": "Run: ft robot state",
	  "elapsed_ms": 3,
	  "version": "0.1.0",
	  "now": "2026-01-18T02:24:00Z"
	}
	```

	Recommended robot commands (minimum):
	- `ft robot state [--domain <name>] [--agent <type>]`
	- `ft robot get-text <pane_id> [--tail N] [--escapes]`
	- `ft robot send <pane_id> "<text>" [--no-newline] [--wait-for "<pat>"] [--timeout-secs N]`
	- `ft robot wait-for <pane_id> "<pat>" [--timeout-secs N]`
	- `ft robot search "<fts query>" [--pane-id <id>] [--since <iso8601>] [--limit N]`
	- `ft robot events [--unhandled] [--pane-id <id>] [--type <event>] [--limit N]`
	- `ft robot workflow <name> <pane_id> [--force]`
	- `ft robot accounts [--service <openai|anthropic|google>]`
	- `ft robot rules list [--pack <name>]`
	- `ft robot rules test "<text>" [--agent <type>]` (returns match trace)

Explain-match trace (v0, per detection):
- bounded + redacted; no raw input snippets
- includes rule_id, spans (indices only), extracted_keys, optional confidence,
  eval_path (which subpatterns matched), and bounds/redaction metadata.

### A.3 MCP tools/resources (via `fastmcp_rust`)

Tool naming: keep short and obvious (agents hate surprises):
- Tools: `wa.state`, `wa.get_text`, `wa.send`, `wa.wait_for`, `wa.search`, `wa.events`, `wa.workflow_run`, `wa.accounts`, `wa.accounts_refresh`, `wa.rules_list`, `wa.rules_test`
- Resources: `wa://panes`, `wa://events`, `wa://accounts`, `wa://workflows`, `wa://rules`

Transport:
- start with stdio transport for local agent usage
- add TCP transport only if we need remote attachment

---

## Appendix B: SQLite schema draft (concrete)

Notes:
- WAL mode always.
- Keep tables append-heavy; avoid write amplification.
- Keep “facts” in dedicated tables so workflows don’t re-parse transcripts.

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS domains (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK (kind IN ('local','ssh','unix','tls')),
  remote_address TEXT,
  username TEXT,
  mux_socket TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  last_seen_at TEXT
);

CREATE TABLE IF NOT EXISTS panes (
  pane_id INTEGER PRIMARY KEY,
  domain_id INTEGER NOT NULL REFERENCES domains(id),
  window_id INTEGER NOT NULL,
  tab_id INTEGER NOT NULL,
  title TEXT,
  cwd_uri TEXT,
  rows INTEGER NOT NULL,
  cols INTEGER NOT NULL,
  first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
  last_seen_at TEXT NOT NULL,
  fingerprint TEXT,
  inferred_agent TEXT CHECK (inferred_agent IN ('claude_code','codex','gemini') OR inferred_agent IS NULL),
  inferred_state TEXT NOT NULL DEFAULT 'unknown'
);

CREATE INDEX IF NOT EXISTS idx_panes_domain ON panes(domain_id);
CREATE INDEX IF NOT EXISTS idx_panes_agent ON panes(inferred_agent) WHERE inferred_agent IS NOT NULL;

CREATE TABLE IF NOT EXISTS captures (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
  captured_at TEXT NOT NULL DEFAULT (datetime('now')),
  kind TEXT NOT NULL CHECK (kind IN ('snapshot','delta')),
  seq INTEGER NOT NULL,
  content TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  line_count INTEGER NOT NULL,
  byte_count INTEGER NOT NULL,
  UNIQUE(pane_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_captures_pane_time ON captures(pane_id, captured_at);
CREATE INDEX IF NOT EXISTS idx_captures_hash ON captures(content_hash);

CREATE VIRTUAL TABLE IF NOT EXISTS captures_fts USING fts5(
  content,
  pane_id UNINDEXED,
  captured_at UNINDEXED,
  seq UNINDEXED,
  content='captures',
  content_rowid='id',
  tokenize='unicode61 remove_diacritics 1 separators "/_-.:"'
);

CREATE TRIGGER IF NOT EXISTS captures_ai AFTER INSERT ON captures BEGIN
  INSERT INTO captures_fts(rowid, content, pane_id, captured_at, seq)
  VALUES (new.id, new.content, new.pane_id, new.captured_at, new.seq);
END;

CREATE TRIGGER IF NOT EXISTS captures_ad AFTER DELETE ON captures BEGIN
  INSERT INTO captures_fts(captures_fts, rowid, content, pane_id, captured_at, seq)
  VALUES ('delete', old.id, old.content, old.pane_id, old.captured_at, old.seq);
END;

CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
  detected_at TEXT NOT NULL DEFAULT (datetime('now')),
  agent TEXT NOT NULL CHECK (agent IN ('claude_code','codex','gemini')),
  event_type TEXT NOT NULL,
  severity TEXT NOT NULL CHECK (severity IN ('info','warning','error','critical')),
  rule_id TEXT NOT NULL,
  confidence REAL NOT NULL,
  raw_context TEXT,
  extracted_json TEXT,
  handled_at TEXT,
  workflow_run_id TEXT,
  action_summary TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_unhandled ON events(handled_at) WHERE handled_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_events_pane_time ON events(pane_id, detected_at);

CREATE TABLE IF NOT EXISTS agent_sessions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
  agent TEXT NOT NULL CHECK (agent IN ('claude_code','codex','gemini')),
  started_at TEXT NOT NULL DEFAULT (datetime('now')),
  ended_at TEXT,
  end_reason TEXT,
  session_id TEXT,
  resume_hint TEXT,
  model_name TEXT,
  total_tokens INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  cached_tokens INTEGER,
  reasoning_tokens INTEGER,
  external_ref TEXT
);

CREATE INDEX IF NOT EXISTS idx_agent_sessions_pane ON agent_sessions(pane_id, started_at);

CREATE TABLE IF NOT EXISTS workflow_runs (
  id TEXT PRIMARY KEY,
  workflow_name TEXT NOT NULL,
  pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
  started_at TEXT NOT NULL DEFAULT (datetime('now')),
  finished_at TEXT,
  status TEXT NOT NULL CHECK (status IN ('running','completed','failed','cancelled')),
  step INTEGER NOT NULL DEFAULT 0,
  step_name TEXT,
  last_error TEXT,
  result_json TEXT
);

CREATE TABLE IF NOT EXISTS accounts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  service TEXT NOT NULL CHECK (service IN ('openai','anthropic','google')),
  account_name TEXT NOT NULL,
  email TEXT,
  percent_remaining REAL,
  resets_at TEXT,
  last_checked_at TEXT,
  last_used_at TEXT,
  is_active INTEGER NOT NULL DEFAULT 1,
  browser_profile_path TEXT,
  UNIQUE(service, account_name)
);

CREATE INDEX IF NOT EXISTS idx_accounts_active ON accounts(service, is_active);
```

---

## Appendix C: Initial rule pack (v0)

Format (conceptual):
- `rule_id`: stable identifier
- `anchors`: literal strings for Aho-Corasick
- `extract`: optional regex with named captures
- `event_type`, `severity`, `agent`

Suggested minimum rules:

### C.1 Codex (`core.codex`)

| rule_id | anchors | extraction (named) | event_type |
|---|---|---|---|
| `codex.usage.warning_25` | `less than 25%` | `remaining`, `limit_hours` | `usage.warning` |
| `codex.usage.warning_10` | `less than 10%` | `remaining`, `limit_hours` | `usage.warning` |
| `codex.usage.warning_5` | `less than 5%` | `remaining`, `limit_hours` | `usage.warning` |
| `codex.usage.reached` | `You've hit your usage limit` | `try_again_at` | `usage.reached` |
| `codex.session.token_usage` | `Token usage:` | `total`, `input`, `cached`, `output`, `reasoning` | `session.summary` |
| `codex.session.resume_hint` | `codex resume` | `session_id` | `session.resume_hint` |
| `codex.auth.device_code_prompt` | `Enter this one-time code` | `code` | `auth.device_code` |

### C.2 Claude Code (`core.claude_code`)

| rule_id | anchors | extraction | event_type |
|---|---|---|---|
| `claude.compaction` | `Conversation compacted` | none | `session.compaction` |
| `claude.banner` | `Claude Code v` | `version` (optional), `model` (optional) | `session.start` |
| `claude.usage.warning` | (pack evolves) | (pack evolves) | `usage.warning` |
| `claude.usage.reached` | (pack evolves) | (pack evolves) | `usage.reached` |

### C.3 Gemini (`core.gemini`)

| rule_id | anchors | extraction | event_type |
|---|---|---|---|
| `gemini.usage.reached` | `Usage limit reached for all Pro models` | none | `usage.reached` |
| `gemini.session.summary` | `Interaction Summary` | `session_id` | `session.summary` |
| `gemini.model.used` | `Responding with gemini-` | `model` | `session.model` |

Rule development workflow:
1. capture raw snippet (fixture)
2. write rule + extraction
3. add positive + negative tests
4. ship as pack bump

---

## Appendix D: Workflow specs (step-by-step, with guards)

### D.1 `handle_compaction`

Trigger:
- event `session.compaction` for a pane whose inferred agent is one of `{claude_code,codex,gemini}`

Steps:
1. Acquire per-pane workflow lock.
2. Re-read current pane tail (guard against false positives / stale panes).
3. Guard: ensure the trigger anchor is still present within last N lines.
4. Wait a small stabilization window OR require an explicit “compaction complete” marker if the agent provides one.
5. Send prompt (agent-specific):
   - Claude Code: `Reread AGENTS.md so it's still fresh in your mind.\n`
   - Codex: `Please re-read AGENTS.md and any key project context files.\n`
   - Gemini: `Please re-examine AGENTS.md and project context.\n`
6. Verify:
   - if possible, wait for agent prompt echo / UI marker indicating receipt
7. Mark event handled + record `workflow_runs.result_json`.

Failure modes:
- pane disappeared → mark workflow cancelled
- pane alt-screen detected → abort (unless explicitly allowed)

### D.2 `handle_usage_limits` (Codex path)

Preconditions:
- inferred agent = `codex`
- trigger event `usage.reached` OR operator invoked workflow manually
- **before every send**: pane is not `AltScreen`, no recent `OutputGap`, and `PolicyEngine` authorizes `SendText` / `BrowserAuth`

Steps (each step is **Observe → Act → Verify**):
1. Acquire per-pane lock.
2. Exit Codex cleanly (avoid fixed sleeps):
   - Act: send Ctrl-C once (`\u{3}`).
   - Verify: wait for session summary / resume hint markers.
   - If not seen within a short grace window: Act: send Ctrl-C again.
   - Verify: continue waiting (bounded by overall timeout) until summary/resume markers appear.
3. Verify session summary/resume hint appears (bounded tail; timeout).
4. Parse:
   - token usage stats (total/input/output/reasoning/cached where available)
   - resume session id
5. Refresh account usage via `caut refresh --service openai --format json`.
6. Select account:
   - highest `percent_remaining` above threshold (configurable)
   - tie-breaker: least recently used
   - if none above threshold **or failover disabled**, branch to **Safe Pause** (below)
7. Initiate device auth:
   - Act: send `cod login --device-auth\n`
   - Verify: device-code prompt appears
8. Parse device code (and URL if present) from the tail.
9. Playwright:
   - open device auth URL
   - ensure logged into chosen account (profile)
   - submit device code
   - verify success marker
10. Resume:
   - Act: send `cod resume <session_id>\n`
   - Verify: “ready” marker / prompt appears
11. Continue:
   - Act: send `proceed.\n`
   - Verify: Codex begins responding (or agreed marker appears)
12. Persist:
   - event handled + action summary
   - session token usage + resume id in `agent_sessions`
   - account rotation record + step logs

Safe Pause (failover disabled / no eligible accounts):
- Do **not** attempt `cod login`.
- Persist a redacted next-step plan with try-again time (if provided), resume id (redacted/hashed), and recommended human actions.
- Mark workflow `paused` so it does not reappear as “unhandled spam.”

Failure modes + recoveries:
- device code not found → retry step 7 once; if still missing, **pause** with next-step plan
- Playwright cannot proceed due to MFA/unexpected wall → open non-headless browser and request human/agent completion; **pause** with recovery instructions
- resume fails (session id invalid / resume rejected) → surface error; **do not loop**
- pane becomes unsafe (AltScreen/OutputGap/CommandRunning) → abort or pause with reason
- policy denies send/auth → record denial and abort safely; never inject input

### D.3 `handle_usage_limits` (Gemini path)

This remains intentionally “spec-first” until we confirm Gemini resume semantics:
- treat session id as the primary continuity handle
- implement “store id, re-auth, restore if supported; otherwise restart with context injection”

### D.4 `handle_usage_limits` (Claude path)

Because Claude Code session IDs are not always printed:
- primary continuity is “continue in same pane” rather than “resume id”
- for correlation and accounting, use `cass`:
  - find the most recent session for the pane’s cwd
  - attach `external_ref` in `agent_sessions`
- usage-limit handling may be “rotate credentials and re-open agent”, but this needs concrete CLI evidence; implement behind a safety gate until verified.

---

## Appendix E: Recommended WezTerm + shell integration snippets

### E.1 Minimal `wezterm.lua` forwarding lane (non-blocking)

Concept:
- inside `wezterm.lua`, on `user-var-changed`, call `wezterm.background_child_process { 'wa', 'event', ... }`
	- `ft event` ingests the JSON and appends to DB as “signals”

### E.2 Emitting user-vars from shells/agents

	From within a pane, emit a user-var:
	```bash
	printf "\033]1337;SetUserVar=%s=%s\007" ft_event "$(printf '%s' '{"kind":"prompt","pane":"$WEZTERM_PANE"}' | base64)"
	```

Use this to:
- mark prompt boundaries
- mark “agent ready”
- mark workflow checkpoints

---

## Appendix F: “Use every library” map (where each one fits)

From JE:
- `cass` (`/dp/coding_agent_session_search`): correlation + session archaeology; used in status + workflows.
- `caut` (`/dp/coding_agent_usage_tracker`): usage truth + selection; used in `accounts` + `handle_usage_limits`.
- `rich_rust`: human-first CLI output (tables/panels/highlight).
- `charmed_rust`: optional interactive TUI (pane picker, event feed, transcript viewer).
- `fastmcp_rust`: MCP tool surface for agent control (mirrors robot mode).
- `fastapi_rust`: optional HTTP server for dashboards/webhooks (read-only first).
- `asupersync`: remote bootstrap/sync layer (configs, binaries, DB snapshots).
- `playwright`: automate device auth flows with persistent profiles.
- `ast-grep`: structure-aware codebase scans used in “unstick agent” workflows and rule hygiene tooling.

---

## Appendix G: Vendoring maintenance plan (if enabled)

	If we enable vendoring:
	- Pin to a WezTerm commit hash in `Cargo.toml` (git dependency) or via submodule.
	- Store the vendored commit in build metadata (so `ft doctor` can compare against `wezterm --version`).
	- Add a CI job that:
	  - builds `ft` with vendored feature
	  - runs a minimal mux protocol smoke test
	  - reports if upstream changes broke compilation

Default posture remains: **CLI-first**; vendoring is an optimization lane, not the foundation.
