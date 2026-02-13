# FrankenTerm (ft) — Master Implementation Plan

> **Vision**: Build the central nervous system for AI coding agent fleets—a tool so robust, intelligent, and ergonomic that it enables supersmart swarms of AI agents to coordinate seamlessly and solve humanity's hardest problems.

> **One-sentence mission**: Build ft into a high-reliability, swarm-native terminal platform for agent fleets: *observe everything, understand key events, act safely and reliably, and expose a machine-optimized control surface for agents.*

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [User Stories](#2-user-stories)
3. [Core Architecture](#3-core-architecture)
4. [WezTerm Integration Strategy](#4-wezterm-integration-strategy)
5. [Storage & Indexing Layer](#5-storage--indexing-layer)
6. [Pattern Detection Engine](#6-pattern-detection-engine)
7. [Workflow Automation System](#7-workflow-automation-system)
8. [Robot Mode: Agent-First Interface](#8-robot-mode-agent-first-interface)
9. [Browser Automation Layer](#9-browser-automation-layer)
10. [MCP Server Integration](#10-mcp-server-integration)
11. [External Tool Integration](#11-external-tool-integration)
12. [Setup & Configuration Automation](#12-setup--configuration-automation)
13. [Performance Engineering](#13-performance-engineering)
14. [Safety & Reliability](#14-safety--reliability)
15. [WezTerm Vendoring Strategy](#15-wezterm-vendoring-strategy)
16. [Implementation Phases](#16-implementation-phases)
17. [Project Structure](#17-project-structure)
18. [Observability & Diagnostics](#18-observability--diagnostics)
19. [Unified Configuration](#19-unified-configuration)
20. [Optional Distributed Mode](#20-optional-distributed-mode)
21. [Key Design Decisions](#21-key-design-decisions)
22. [Open Questions](#22-open-questions)
23. [Definition of Done](#23-definition-of-done)
24. [Appendices](#24-appendices)
    - [A: CLI / Robot / MCP Surface](#appendix-a-cli--robot--mcp-surface)
    - [B: SQLite Schema](#appendix-b-sqlite-schema)
    - [C: Initial Rule Packs](#appendix-c-initial-rule-packs)
    - [D: Workflow Specs](#appendix-d-workflow-specs)
    - [E: Shell Integration Snippets](#appendix-e-shell-integration-snippets)
    - [F: Library Integration Map](#appendix-f-library-integration-map)
    - [G: Testing Strategy](#appendix-g-testing-strategy)
    - [H: Vendoring Maintenance Plan](#appendix-h-vendoring-maintenance-plan)

---

## 1. Executive Summary

### 1.1 What We're Building

`frankenterm` (`ft`) is a high-performance Rust CLI and MCP server that provides:

1. **Perfect Observability**: Real-time capture of ALL terminal output across ALL panes in ALL supported runtime domains (current bridge: WezTerm)
2. **Intelligent Detection**: SIMD-accelerated pattern matching to identify agent events (compaction, usage limits, errors, session boundaries)
3. **Automated Response**: Workflow engine that reacts to detected events (quota-aware pause/resume, optional credential failover, context refresh, session resumption)
4. **Agent-First Control**: Robot mode and MCP interface optimized for AI agents controlling other AI agents
5. **Deep Integration**: Compatibility bridge hardening plus native ft runtime expansion for capabilities beyond current public APIs

### 1.2 Core Principles

| Principle | Rationale |
|-----------|-----------|
| **Never guess, always observe** | Every action predicated on observable terminal state |
| **Deterministic over probabilistic** | No timing heuristics; use actual state transitions |
| **Agent-first ergonomics** | Designed for AI agents to use, optimized for token efficiency |
| **Zero-copy hot paths** | Performance is critical—every Bash command goes through this |
| **Graceful degradation** | Failures should be handled, not propagated |
| **Single canonical setup** | One way to configure the active backend/runtime = simpler automation |

### 1.3 Success Metrics

- **Latency**: < 5ms for pattern detection on typical output
- **Reliability**: 99.99% uptime for watcher daemon
- **Detection Accuracy**: 100% for known patterns, < 0.1% false positives
- **Quota Handling**: < 5 seconds from limit detection to safe pause + next-step plan (resume-at-reset or failover)
- **Credential Failover (Optional)**: < 30 seconds when a preconfigured failover profile exists
- **Memory**: < 50MB RSS for watcher daemon with 100 panes

### 1.4 Non-Goals (Explicit Scope Boundaries)

- **Not replacing agents**: ft orchestrates agents, it doesn't replace Claude Code/Codex/Gemini
- **Not a general scheduler**: Day 1 is single-machine coordination; distributed scheduling comes later
- **Not freezing at compatibility bridge**: Keep bridge reliability high while ft evolves into a full runtime replacement

---

## 2. User Stories

Grounding the design in concrete use cases ensures we build what matters.

### 2.1 Human Operator Stories

| # | Story | Priority |
|---|-------|----------|
| H1 | "Show me what all agents are doing *right now* across all domains." | P0 |
| H2 | "Search: where did the agent mention `panic` / `clippy` / `usage limit` last week?" | P0 |
| H3 | "This Codex pane hit limit; rotate accounts, re-auth, resume session, send `proceed.`" | P0 |
| H4 | "After compaction, automatically re-inject a context refresh prompt." | P0 |
| H5 | "Bring a new remote host online: install supported backend runtime (currently WezTerm bridge), standardize config." | P1 |
| H6 | "I want a clean UI to browse panes and events when debugging at 2am." | P2 |
| H7 | "Export today's session logs for analysis or archival." | P2 |

### 2.2 Agent (Robot-Mode) Stories

| # | Story | Priority |
|---|-------|----------|
| A1 | "List panes; pick the pane running Claude; fetch last 200 lines; decide next action." | P0 |
| A2 | "Wait for a pattern; then send the next command; confirm output contains expected marker." | P0 |
| A3 | "Search historical output for a prior decision / error; cite it back to the operator agent." | P0 |
| A4 | "Execute a named workflow and get back structured results (what happened, what changed, what to do next)." | P0 |
| A5 | "Get quick-start context when invoked with no arguments." | P1 |
| A6 | "Reserve a pane for exclusive use during a multi-step operation." | P2 |

### 2.3 Story-to-Feature Mapping

```
H1, A1 → ft status / ft robot state
H2, A3 → ft search / ft robot search
H3     → handle_usage_limits workflow
H4     → handle_compaction workflow
H5     → ft setup
H6     → ft tui (charmed_rust)
H7     → ft export
A2     → ft robot send --wait-for
A4     → ft robot workflow
A5     → ft robot quick-start
A6     → pane locking mechanism
```

---

## 3. Core Architecture

### 3.1 System Overview

```
┌─────────────────────────────────────────────────────────────────────────────────────────┐
│                                    ft (FrankenTerm)                                      │
│                                                                                          │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐ │
│  │ Ingest Pipeline  │  │  Pattern Engine  │  │ Workflow Engine  │  │   Robot Mode     │ │
│  │ (Stream/Poll)    │  │ (AC+Regex+Gate)  │  │ (Durable FSM)    │  │   (CLI + MCP)    │ │
│  └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘ │
│           │                     │                     │                     │            │
│           │        ┌────────────────────────────────────────────────┐       │            │
│           └────────│         Event Bus (bounded channels + fanout)  │───────┘            │
│                    └────────────────────────────────────────────────┘                    │
│                                          │                                               │
│                              ┌───────────┴───────────┐                                   │
│                              │    Core Data Layer    │                                   │
│                              │  ┌─────────────────┐  │                                   │
│                              │  │ SQLite + FTS5   │  │                                   │
│                              │  │ (Segments+Events)│  │                                   │
│                              │  └─────────────────┘  │                                   │
│                              │  ┌─────────────────┐  │                                   │
│                              │  │ Event Log       │  │                                   │
│                              │  │ (Detect+Gaps)   │  │                                   │
│                              │  └─────────────────┘  │                                   │
│                              │  ┌─────────────────┐  │                                   │
│                              │  │ Pane Registry   │  │                                   │
│                              │  │ (Live State)    │  │                                   │
│                              │  └─────────────────┘  │                                   │
│                              └───────────────────────┘                                   │
│                                          │                                               │
│  ┌───────────────────────────────────────┼───────────────────────────────────────────┐   │
│  │                           WezTerm Interface Layer                                 │   │
│  │  ┌────────────┐  ┌────────────┐  ┌────────────┐  ┌────────────┐  ┌────────────┐  │   │
│  │  │ CLI Client │  │  Lua IPC   │  │ User Vars  │  │  OSC 1337  │  │ Vendored   │  │   │
│  │  │ (Primary)  │  │ (Signals)  │  │ (Signals)  │  │ (Signals)  │  │ (Optional) │  │   │
│  │  └────────────┘  └────────────┘  └────────────┘  └────────────┘  └────────────┘  │   │
│  └───────────────────────────────────────────────────────────────────────────────────┘   │
│                                          │                                               │
└──────────────────────────────────────────┼───────────────────────────────────────────────┘
                                           │
         ┌─────────────────────────────────┼─────────────────────────────────┐
         │                                 │                                 │
         ▼                                 ▼                                 ▼
┌─────────────────┐             ┌─────────────────┐             ┌─────────────────┐
│  Local WezTerm  │             │   Remote Mux    │             │   Remote Mux    │
│     (macOS)     │             │  (dev-server)   │             │   (staging)     │
│                 │             │                 │             │                 │
│ ┌─────┐ ┌─────┐ │             │ ┌─────┐ ┌─────┐ │             │ ┌─────┐ ┌─────┐ │
│ │ cc  │ │ cod │ │             │ │ cc  │ │ gmi │ │             │ │ cod │ │ cc  │ │
│ └─────┘ └─────┘ │             │ └─────┘ └─────┘ │             │ └─────┘ └─────┘ │
└─────────────────┘             └─────────────────┘             └─────────────────┘
```

### 3.2 Component Responsibilities

| Component | Responsibility | Key Properties |
|-----------|---------------|----------------|
| **Ingest Pipeline** | Discover panes, tail output deltas into bounded event bus | Per-pane cursors, backpressure, explicit gap events |
| **Event Bus** | Fan-out stream for deltas, detections, workflow signals | Bounded channels, multi-consumer, no blocking |
| **Pattern Engine** | Match output against patterns with state gating | SIMD-accelerated, sub-millisecond, state-aware |
| **Workflow Engine** | Execute durable automated responses to events | Persistent FSM, step logging, resumable after restart |
| **Robot Mode** | Agent-optimized CLI and MCP interface | JSON/Markdown output, token-efficient |
| **Storage Layer** | Persist segments, events, pane state | SQLite WAL mode, FTS5 search, async-safe |
| **WezTerm Interface** | Abstract all WezTerm interactions | CLI primary, optional deep integration |

### 3.3 The Critical Separation: Observe vs Act

To keep reliability high and prevent "blind automation", we split the runtime into two conceptual loops:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                       OBSERVATION LOOP (Passive)                         │
│                                                                          │
│   Poll Panes → Capture Output → Persist to DB → Detect Patterns → Emit  │
│       ↑                                                            │     │
│       └────────────────────────────────────────────────────────────┘     │
│                              (never modifies terminal state)             │
└─────────────────────────────────────────────────────────────────────────┘
                                      │
                                      │ Events
                                      ↓
┌─────────────────────────────────────────────────────────────────────────┐
│                        ACTION LOOP (Active)                              │
│                                                                          │
│   Receive Event → Re-check State → Validate Guards → Execute → Verify   │
│       ↑                                                            │     │
│       └────────────────────────────────────────────────────────────┘     │
│                    (always re-validates before acting)                   │
└─────────────────────────────────────────────────────────────────────────┘
```

**Critical Invariant**: The action loop *never* runs on stale assumptions; it always re-checks current pane state before sending any input.

### 3.4 Data Flow

```
1. Ingest discovers panes + schedules per-pane tailers (stream when possible, poll as fallback)
                ↓
2. Tailer emits PaneDelta {pane_id, seq, text, cursor, flags} into bounded Event Bus
                ↓
3. Storage writer persists PaneDelta segments (emits explicit GAP events on discontinuity)
                ↓
4. Pattern engine consumes PaneDelta and emits Detection events
                ↓
5. Workflow engine consumes Detection events + live pane state, executes durable workflows
                ↓
6. Robot/MCP can subscribe to deltas/events without re-polling WezTerm
                ↓
7. Workflows append actions + results to audit log and update pane state
```

**Key invariant**: If we can't guarantee contiguous output (scrollback rotated, cursor jump), we emit an explicit `GAP` event instead of silently losing data.

---

## 4. WezTerm Integration Strategy

### 4.1 Multi-Tier Interface Design

We implement THREE tiers of WezTerm integration, allowing graceful fallback:

#### Tier 1: CLI Interface (Primary, Always Available)

```rust
pub struct WeztermCliClient {
    wezterm_path: PathBuf,
    socket: Option<String>,
}

impl WeztermCliClient {
    /// List all panes with metadata
    pub async fn list_panes(&self) -> Result<Vec<PaneInfo>>;

    /// Get text from a pane
    pub async fn get_text(&self, pane_id: u64, escapes: bool) -> Result<String>;

    /// Send text to a pane
    pub async fn send_text(&self, pane_id: u64, text: &str, no_paste: bool) -> Result<()>;

    /// Spawn new pane/tab
    pub async fn spawn(&self, opts: SpawnOptions) -> Result<u64>;

    /// Split pane
    pub async fn split_pane(&self, pane_id: u64, direction: Direction) -> Result<u64>;

    /// Activate pane
    pub async fn activate_pane(&self, pane_id: u64) -> Result<()>;

    /// Get pane in direction
    pub async fn get_pane_direction(&self, pane_id: u64, dir: Direction) -> Result<Option<u64>>;
}
```

#### Tier 2: Lua IPC (Enhanced Signals)

> **DEPRECATED (v0.2.0)**: The `update-status` hook was removed due to performance issues.
> It fires at ~60Hz, causing continuous Lua interpreter invocations and IPC overhead.
> Alt-screen detection is now handled via escape sequence parsing (see `screen_state.rs`).
> Pane metadata is obtained via `wezterm cli list` only when needed.
> Only the `user-var-changed` hook is still used.

~~Install a Lua module in WezTerm that communicates via Unix socket:~~

```lua
-- ~/.wezterm.lua (snippet) — DEPRECATED: update-status removed in v0.2.0
local wa_socket = '/tmp/wa-ipc.sock'

-- DEPRECATED: This hook was removed in v0.2.0. Do not use.
-- wezterm.on('update-status', function(window, pane)
--   -- Send rich status updates to wa
--   local msg = wezterm.json_encode({
--     event = 'status_update',
--     pane_id = pane:pane_id(),
--     domain = pane:get_domain_name(),
--     cursor = pane:get_cursor_position(),
--     dimensions = pane:get_dimensions(),
--     title = pane:get_title(),
--     is_alt_screen = pane:is_alt_screen_active(),
--   })
--   -- Send via socket (non-blocking)
-- end)

-- STILL ACTIVE: user-var-changed forwarding
wezterm.on('user-var-changed', function(window, pane, name, value)
  if name:match('^wa%-') then
    -- Forward to wa daemon
  end
end)
```

#### Tier 3: Vendored Integration (Maximum Capability)

Direct access to WezTerm internals (see Section 14):

```rust
// When vendored WezTerm is available
pub trait VendoredWeztermExt {
    /// Direct access to pane scrollback buffer (zero-copy)
    fn scrollback_buffer(&self, pane_id: u64) -> &[Line];

    /// Register for real-time output events
    fn subscribe_output(&self, pane_id: u64) -> OutputStream;

    /// Inject text into display (not shell)
    fn inject_display(&self, pane_id: u64, text: &str);

    /// Access raw PTY
    fn pty_handle(&self, pane_id: u64) -> &PtyHandle;
}
```

### 4.2 Domain Management

```rust
#[derive(Debug, Clone)]
pub struct Domain {
    pub name: String,
    pub domain_type: DomainType,
    pub remote_address: Option<String>,
    pub username: Option<String>,
    pub mux_socket: Option<PathBuf>,
    pub status: DomainStatus,
}

#[derive(Debug, Clone, Copy)]
pub enum DomainType {
    Local,
    Ssh,
    Unix,
    Tls,
}

#[derive(Debug, Clone, Copy)]
pub enum DomainStatus {
    Connected,
    Connecting,
    Disconnected,
    Error,
}

impl Domain {
    /// Connect to domain's mux server
    pub async fn connect(&mut self) -> Result<()>;

    /// Get all panes in this domain
    pub async fn panes(&self) -> Result<Vec<PaneInfo>>;

    /// Spawn new pane in domain
    pub async fn spawn(&self, opts: SpawnOptions) -> Result<u64>;
}
```

### 4.3 Pane Identity & Stability

Pane IDs are stable while a pane exists, but panes can be created/destroyed freely, and domains may disconnect/reconnect.

#### Design Principles

1. **Pane Fingerprinting**: Model "pane instance" as `(domain_name, pane_id)` plus a **fingerprint**:
   - Title + CWD + initial banner hashes
   - First-seen timestamp
   - Content signature of first N lines

2. **Lifecycle Handling**:
   - If pane_id disappears, treat as ended and close out any active agent session row
   - If domain reconnects and pane_id reappears with matching fingerprint, consider it the same pane
   - Track pane "generations" for panes that get recycled

3. **Domain Resilience**:
   - Remote mux servers may restart
   - SSH tunnels may drop and reconnect
   - Design for eventual consistency, not instant state

### 4.4 Input Injection Correctness (No "Blind Typing")

Actions MUST require guards to prevent dangerous automation:

```rust
/// Guards that must pass before sending input to a pane
pub struct InputGuards {
    /// Pane must not be in alt-screen (vim, less, etc.)
    require_no_alt_screen: bool,
    /// Pane must show a shell prompt or specific marker
    require_prompt: Option<String>,
    /// Rate limit: max sends per minute
    rate_limit: u32,
    /// Require explicit "ready" user-var signal
    require_ready_signal: bool,
    /// Require stable pattern in last N lines
    require_stable_pattern: Option<(String, usize)>,
}

impl InputGuards {
    pub fn validate(&self, pane: &PaneState, text: &str) -> Result<()> {
        // 1. Alt-screen check
        if self.require_no_alt_screen && pane.is_alt_screen_active {
            return Err(GuardError::AltScreenActive {
                hint: "Pane is in vim/less/etc. Cannot send input automatically.",
            });
        }

        // 2. Prompt detection
        if let Some(ref marker) = self.require_prompt {
            if !pane.last_line.contains(marker) {
                return Err(GuardError::NoPrompt {
                    expected: marker.clone(),
                    actual: pane.last_line.clone(),
                });
            }
        }

        // 3. Rate limiting
        if pane.sends_last_minute >= self.rate_limit {
            return Err(GuardError::RateLimited {
                sends: pane.sends_last_minute,
                limit: self.rate_limit,
            });
        }

        // 4. Ready signal (from OSC user-var)
        if self.require_ready_signal && !pane.ready_signal_received {
            return Err(GuardError::NotReady {
                hint: "Waiting for ready signal from pane.",
            });
        }

        // 5. Stable pattern
        if let Some((ref pattern, lines)) = self.require_stable_pattern {
            let tail = pane.get_last_n_lines(lines);
            if !tail.contains(pattern) {
                return Err(GuardError::PatternNotStable {
                    pattern: pattern.clone(),
                    lines,
                });
            }
        }

        Ok(())
    }
}
```

#### Sending Control Characters

To send Ctrl-C safely:
```rust
// Send Ctrl-C: byte 0x03 via Rust string '\u{3}'
// Use --no-paste flag to prevent interpretation issues
client.send_text(pane_id, "\x03", SendOptions { no_paste: true }).await?;
```

### 4.5 Pane State Machine

```rust
#[derive(Debug, Clone)]
pub struct Pane {
    pub id: u64,
    pub domain: String,
    pub window_id: u64,
    pub tab_id: u64,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub size: PaneSize,
    pub agent: Option<AgentInfo>,
    pub state: PaneState,
}

#[derive(Debug, Clone)]
pub enum PaneState {
    /// Prompt is visible and safe to send input (via OSC 133 markers)
    PromptActive { prompt_id: u64, last_exit: Option<i32> },
    /// A command is running (known boundary via OSC 133 markers)
    CommandRunning { command_id: u64, started_at: Instant, cmdline: Option<String> },
    /// AI agent active
    AgentActive { agent: AgentType, started_at: Instant },
    /// Agent waiting for input
    AgentWaiting { agent: AgentType },
    /// Usage limit reached
    UsageLimitReached { agent: AgentType, resets_at: Option<DateTime<Utc>> },
    /// Compaction occurred
    Compacted { agent: AgentType },
    /// Alt screen active (vim, less, etc.) — NEVER send input here
    AltScreen,
    /// Ingest detected a discontinuity (scrollback truncation or cursor jump)
    OutputGap { last_seq: u64 },
    /// Unknown state (shell integration not installed or markers not received)
    Unknown,
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_type: AgentType,
    pub session_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub token_usage: Option<TokenUsage>,
}
```

### 4.6 Shell Integration for Deterministic State (OSC 133)

**Goal**: Replace prompt heuristics with explicit shell markers.

The "Deterministic over probabilistic" principle means we must NOT rely on `looks_like_prompt()` string matching. WezTerm supports shell integration via OSC 133 semantic zones, which provide:

- **Deterministic command boundaries** (no heuristics)
- **Exit status capture** (critical for workflow correctness)
- **Safer sending**: only inject when prompt is active and not in alt-screen
- **Better indexing**: attach output segments to command IDs

**Design**:

During `ft setup`, we install a small, idempotent shell snippet for bash/zsh/fish that emits:
- `prompt_start` / `prompt_end`
- `command_start` / `command_end(exit_status)`

```bash
# ~/.config/ft/shell-integration.bash (sourced by user's .bashrc)
__ft_prompt_start() { printf '\e]133;A\a'; }
__ft_command_start() { printf '\e]133;C\a'; }
__ft_command_end() { printf '\e]133;D;%s\a' "$?"; }
PROMPT_COMMAND='__ft_prompt_start'
trap '__ft_command_end' DEBUG  # Simplified; real impl is more robust
```

**How ingest uses it**:

```rust
impl IngestPipeline {
    fn parse_osc_markers(&mut self, pane_id: u64, text: &str) {
        // OSC 133;A = prompt start
        // OSC 133;C = command start
        // OSC 133;D;N = command end with exit code N
        for marker in extract_osc_133_markers(text) {
            match marker {
                Osc133::PromptStart => {
                    self.update_pane_state(pane_id, PaneState::PromptActive {
                        prompt_id: self.next_prompt_id(),
                        last_exit: self.last_exit_code(pane_id),
                    });
                }
                Osc133::CommandStart => {
                    self.update_pane_state(pane_id, PaneState::CommandRunning {
                        command_id: self.next_command_id(),
                        started_at: Instant::now(),
                        cmdline: None, // Can be captured via OSC 133;E
                    });
                }
                Osc133::CommandEnd(exit_code) => {
                    self.record_exit_code(pane_id, exit_code);
                }
            }
        }
    }
}
```

**Critical benefit**: `WaitCondition::PaneIdle` becomes "wait for OSC 133;A (prompt_start)" — deterministic, not string heuristics.

---

## 5. Storage & Indexing Layer

### 5.1 Storage Goals

We need three classes of stored data:

| Class | Purpose | Examples |
|-------|---------|----------|
| **Raw Transcript** | What happened (immutable history) | Complete pane output, timestamps |
| **Indexed Search** | Fast recall across all history | FTS5 virtual tables |
| **Structured Facts** | Extracted knowledge for workflows | Session IDs, token usage, reset times, workflow results |

SQLite + WAL + FTS5 is the right default because:
- Portable, embeddable, zero-config
- Strong enough for our scale (100s of panes, weeks of history)
- Excellent full-text search with snippet/highlight support
- Single-file portability for backups

### 5.2 SQLite Schema

```sql
-- Enable WAL mode for better concurrent access
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

-- ============================================================================
-- DOMAINS & PANES
-- ============================================================================

CREATE TABLE domains (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    domain_type TEXT NOT NULL CHECK (domain_type IN ('local', 'ssh', 'unix', 'tls')),
    remote_address TEXT,
    username TEXT,
    mux_socket TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_connected_at TEXT
);

CREATE TABLE panes (
    id INTEGER PRIMARY KEY,  -- WezTerm's pane_id
    domain_id INTEGER NOT NULL REFERENCES domains(id),
    window_id INTEGER NOT NULL,
    tab_id INTEGER NOT NULL,
    title TEXT,
    cwd TEXT,
    rows INTEGER NOT NULL,
    cols INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT NOT NULL,
    agent_type TEXT,  -- 'claude_code', 'codex', 'gemini', NULL
    agent_session_id TEXT,
    pane_state TEXT NOT NULL DEFAULT 'idle'
);

CREATE INDEX idx_panes_domain ON panes(domain_id);
CREATE INDEX idx_panes_agent ON panes(agent_type) WHERE agent_type IS NOT NULL;

-- ============================================================================
-- OUTPUT SEGMENTS (Core Data) — Ordered append-only stream per pane
-- ============================================================================
-- Why segments instead of snapshots?
-- - Cheap writes (small deltas, not full pane content)
-- - Natural timeline queries (seq is monotonic)
-- - Gap detection: if seq isn't contiguous, we have a discontinuity
-- - Better FTS behavior (smaller rows, less churn)

CREATE TABLE output_segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id INTEGER NOT NULL REFERENCES panes(id),
    seq INTEGER NOT NULL,                     -- Strictly increasing per pane
    captured_at INTEGER NOT NULL,             -- Unix epoch ms for faster range queries
    content TEXT NOT NULL,                    -- Small deltas, typically dozens of lines
    cursor_row INTEGER,                       -- Optional: cursor position at capture
    cursor_col INTEGER,
    flags INTEGER NOT NULL DEFAULT 0,         -- Bitflags: alt_screen, prompt_active, etc.
    command_id INTEGER,                       -- Links to command boundary (OSC 133)
    UNIQUE(pane_id, seq)
);

CREATE INDEX idx_segments_pane_seq ON output_segments(pane_id, seq);
CREATE INDEX idx_segments_pane_time ON output_segments(pane_id, captured_at);
CREATE INDEX idx_segments_command ON output_segments(command_id) WHERE command_id IS NOT NULL;

-- Explicitly record discontinuities in the output stream
-- This is how "Perfect Observability" degrades gracefully without silently losing data
CREATE TABLE output_gaps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id INTEGER NOT NULL REFERENCES panes(id),
    detected_at INTEGER NOT NULL,
    last_seq INTEGER NOT NULL,
    reason TEXT NOT NULL  -- 'scrollback_truncation', 'cursor_jump', 'pane_reset', etc.
);

CREATE INDEX idx_gaps_pane ON output_gaps(pane_id, detected_at);

-- FTS5 Virtual Table for full-text search
CREATE VIRTUAL TABLE output_fts USING fts5(
    content,
    pane_id UNINDEXED,
    captured_at UNINDEXED,
    content='output_segments',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 1 separators "_-/"'  -- Include / for paths
);

-- Triggers to keep FTS in sync
CREATE TRIGGER output_segments_ai AFTER INSERT ON output_segments BEGIN
    INSERT INTO output_fts(rowid, content, pane_id, captured_at)
    VALUES (new.id, new.content, new.pane_id, new.captured_at);
END;

CREATE TRIGGER output_segments_ad AFTER DELETE ON output_segments BEGIN
    INSERT INTO output_fts(output_fts, rowid, content, pane_id, captured_at)
    VALUES ('delete', old.id, old.content, old.pane_id, old.captured_at);
END;

CREATE TRIGGER output_segments_au AFTER UPDATE ON output_segments BEGIN
    INSERT INTO output_fts(output_fts, rowid, content, pane_id, captured_at)
    VALUES ('delete', old.id, old.content, old.pane_id, old.captured_at);
    INSERT INTO output_fts(rowid, content, pane_id, captured_at)
    VALUES (new.id, new.content, new.pane_id, new.captured_at);
END;

-- ============================================================================
-- EVENTS
-- ============================================================================

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id INTEGER NOT NULL REFERENCES panes(id),
    event_type TEXT NOT NULL,  -- 'usage_limit', 'compaction', 'session_end', etc.
    agent_type TEXT NOT NULL,
    severity TEXT NOT NULL DEFAULT 'info',  -- 'info', 'warning', 'error', 'critical'
    detected_at TEXT NOT NULL DEFAULT (datetime('now')),
    raw_match TEXT,            -- The text that triggered detection
    pattern_id TEXT,           -- Which pattern matched
    extracted_data TEXT,       -- JSON with extracted values
    handled_at TEXT,           -- When workflow responded
    action_taken TEXT,         -- What was done
    workflow_id TEXT           -- Which workflow handled it
);

CREATE INDEX idx_events_pane_type ON events(pane_id, event_type);
CREATE INDEX idx_events_unhandled ON events(handled_at) WHERE handled_at IS NULL;
CREATE INDEX idx_events_time ON events(detected_at);

-- ============================================================================
-- AGENT SESSIONS
-- ============================================================================

CREATE TABLE agent_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id INTEGER NOT NULL REFERENCES panes(id),
    agent_type TEXT NOT NULL,
    session_id TEXT,           -- Agent's session ID if available
    external_id TEXT,          -- Correlation with cass, etc.
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at TEXT,
    end_reason TEXT,           -- 'completed', 'limit_reached', 'error', 'manual'
    -- Token tracking
    total_tokens INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cached_tokens INTEGER,
    reasoning_tokens INTEGER,
    -- Model info
    model_name TEXT,
    -- Cost tracking
    estimated_cost_usd REAL
);

CREATE INDEX idx_sessions_pane ON agent_sessions(pane_id);
CREATE INDEX idx_sessions_external ON agent_sessions(external_id) WHERE external_id IS NOT NULL;

-- ============================================================================
-- ACCOUNTS (for usage limit rotation)
-- ============================================================================

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    service TEXT NOT NULL,     -- 'openai', 'anthropic', 'google'
    account_name TEXT NOT NULL,
    email TEXT,
    -- Usage tracking
    usage_type TEXT NOT NULL DEFAULT 'rate',  -- 'rate' or 'credits'
    usage_limit REAL,
    usage_current REAL,
    usage_resets_at TEXT,
    usage_percent_remaining REAL,
    last_usage_check TEXT,
    -- Credentials
    credentials_path TEXT,
    browser_profile TEXT,      -- For Playwright auth
    -- State
    is_active INTEGER NOT NULL DEFAULT 1,
    last_used_at TEXT,
    rotation_priority INTEGER NOT NULL DEFAULT 100,
    UNIQUE(service, account_name)
);

CREATE INDEX idx_accounts_service ON accounts(service, is_active);

-- ============================================================================
-- WORKFLOWS
-- ============================================================================

CREATE TABLE workflow_executions (
    id TEXT PRIMARY KEY,       -- UUID
    workflow_name TEXT NOT NULL,
    pane_id INTEGER NOT NULL REFERENCES panes(id),
    event_id INTEGER REFERENCES events(id),
    started_at TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at TEXT,
    status TEXT NOT NULL DEFAULT 'running',  -- 'running', 'completed', 'failed', 'cancelled'
    steps_completed INTEGER NOT NULL DEFAULT 0,
    steps_total INTEGER,
    current_step TEXT,
    error_message TEXT,
    result TEXT                -- JSON result data
);

CREATE INDEX idx_workflows_status ON workflow_executions(status);
CREATE INDEX idx_workflows_pane ON workflow_executions(pane_id);

-- Step-level audit for resumability and postmortems
-- This is CRITICAL for workflow durability: if daemon restarts mid-workflow,
-- we can resume from the last completed step instead of re-running from scratch
CREATE TABLE workflow_step_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id TEXT NOT NULL REFERENCES workflow_executions(id),
    step_index INTEGER NOT NULL,
    step_name TEXT NOT NULL,
    started_at INTEGER NOT NULL,       -- Unix epoch ms
    completed_at INTEGER,
    status TEXT NOT NULL,              -- 'running', 'completed', 'retry', 'failed', 'skipped'
    attempt INTEGER NOT NULL DEFAULT 1,
    input_snapshot TEXT,               -- JSON: pane state before step
    output_snapshot TEXT,              -- JSON: what changed after step
    detail TEXT                        -- Error message or notes
);

CREATE INDEX idx_step_log_execution ON workflow_step_log(execution_id, step_index);

-- ============================================================================
-- CONFIGURATION
-- ============================================================================

CREATE TABLE config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- ============================================================================
-- MAINTENANCE
-- ============================================================================

-- Retention policy: delete output older than configured days
CREATE TABLE maintenance_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    operation TEXT NOT NULL,
    records_affected INTEGER,
    executed_at TEXT NOT NULL DEFAULT (datetime('now')),
    duration_ms INTEGER
);
```

### 5.3 Storage Implementation

```rust
use rusqlite::{Connection, params};
use tokio::sync::mpsc;

// ============================================================================
// ASYNC-SAFE STORAGE ARCHITECTURE
// ============================================================================
// Why this design?
// - rusqlite::Connection is NOT Clone and NOT Send
// - Blocking SQLite calls in async workflows WILL block the Tokio runtime
// - Solution: single writer thread + read pool + async API via channels

/// Commands sent to the dedicated DB writer thread
enum DbCmd {
    AppendSegment(OutputSegment),
    RecordGap { pane_id: u64, last_seq: u64, reason: String },
    RecordEvent(Event),
    StepStarted { execution_id: String, step_index: usize, step_name: String },
    StepCompleted { execution_id: String, step_index: usize },
    StepFailed { execution_id: String, step_index: usize, reason: String },
    Shutdown,
}

/// Async-friendly handle; internally uses a dedicated DB thread + WAL.
/// Readers use a small pool of read-only connections.
#[derive(Clone)]
pub struct StorageHandle {
    write_tx: mpsc::Sender<DbCmd>,
    read_pool: deadpool_sqlite::Pool,
}

impl StorageHandle {
    pub async fn open(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(default_db_path);
        std::fs::create_dir_all(path.parent().unwrap())?;

        // Create bounded channel for write commands (backpressure!)
        let (write_tx, write_rx) = mpsc::channel(4096);

        // Spawn dedicated writer thread (no async runtime blocking)
        let db_path = path.clone();
        std::thread::spawn(move || db_writer_main(db_path, write_rx));

        // Create read pool (WAL mode allows concurrent readers)
        let cfg = deadpool_sqlite::Config::new(path);
        let read_pool = cfg.builder(deadpool_sqlite::Runtime::Tokio1)?
            .max_size(4)  // Small pool; reads are fast
            .build()?;

        Ok(Self { write_tx, read_pool })
    }

    /// Append a delta segment; writer enforces (pane_id, seq) monotonicity.
    pub async fn append_segment(&self, seg: OutputSegment) -> Result<()> {
        self.write_tx.send(DbCmd::AppendSegment(seg)).await?;
        Ok(())
    }

    /// Record an output gap (discontinuity detected).
    pub async fn record_gap(&self, pane_id: u64, last_seq: u64, reason: &str) -> Result<()> {
        self.write_tx.send(DbCmd::RecordGap {
            pane_id,
            last_seq,
            reason: reason.to_string(),
        }).await?;
        Ok(())
    }

    /// Full-text search across output (uses read pool, non-blocking)
    pub async fn search(&self, query: &str, opts: SearchOptions) -> Result<Vec<SearchResult>> {
        let query = query.to_string();
        let conn = self.read_pool.get().await?;

        conn.interact(move |conn| {
            let mut sql = String::from(
                "SELECT o.id, o.pane_id, o.captured_at,
                        snippet(output_fts, 0, '>>>', '<<<', '...', 64) as snippet,
                        highlight(output_fts, 0, '\x1b[1;33m', '\x1b[0m') as highlighted
                 FROM output_fts f
                 JOIN output_segments o ON f.rowid = o.id
                 WHERE output_fts MATCH ?1"
            );

            let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query)];

            if let Some(pane_id) = opts.pane_id {
                sql.push_str(" AND o.pane_id = ?2");
                params.push(Box::new(pane_id));
            }

            if let Some(since) = opts.since {
                sql.push_str(&format!(" AND o.captured_at >= ?{}", params.len() + 1));
                params.push(Box::new(since));
            }

            sql.push_str(" ORDER BY bm25(output_fts) LIMIT ?");  // Correct FTS5 ranking
            params.push(Box::new(opts.limit.unwrap_or(100) as i64));

            let mut stmt = conn.prepare(&sql)?;
            // ... execute and collect results
            Ok(vec![])  // Placeholder
        }).await?
    }

    // Workflow step logging for durability
    pub async fn step_started(&self, execution_id: &str, step_index: usize, step_name: &str) -> Result<()> {
        self.write_tx.send(DbCmd::StepStarted {
            execution_id: execution_id.to_string(),
            step_index,
            step_name: step_name.to_string(),
        }).await?;
        Ok(())
    }

    pub async fn step_completed(&self, execution_id: &str, step_index: usize) -> Result<()> {
        self.write_tx.send(DbCmd::StepCompleted {
            execution_id: execution_id.to_string(),
            step_index,
        }).await?;
        Ok(())
    }

    pub async fn step_failed(&self, execution_id: &str, step_index: usize, reason: &str) -> Result<()> {
        self.write_tx.send(DbCmd::StepFailed {
            execution_id: execution_id.to_string(),
            step_index,
            reason: reason.to_string(),
        }).await?;
        Ok(())
    }
}

/// Dedicated writer thread — serializes all writes, never blocks async runtime
fn db_writer_main(path: PathBuf, mut rx: mpsc::Receiver<DbCmd>) {
    let conn = Connection::open(&path).expect("Failed to open DB");

    // Configure for performance
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
    conn.pragma_update(None, "cache_size", "-64000").unwrap();   // 64MB
    conn.pragma_update(None, "mmap_size", "268435456").unwrap(); // 256MB

    // Initialize schema
    conn.execute_batch(SCHEMA).expect("Failed to init schema");

    // Track per-pane sequence numbers for monotonicity enforcement
    let mut pane_seqs: HashMap<u64, u64> = HashMap::new();

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DbCmd::AppendSegment(seg) => {
                // Enforce monotonic seq per pane
                let expected_seq = pane_seqs.get(&seg.pane_id).copied().unwrap_or(0) + 1;
                if seg.seq != expected_seq {
                    tracing::warn!(
                        "Seq gap detected for pane {}: expected {}, got {}",
                        seg.pane_id, expected_seq, seg.seq
                    );
                    // Insert gap record
                    conn.execute(
                        "INSERT INTO output_gaps (pane_id, detected_at, last_seq, reason)
                         VALUES (?1, ?2, ?3, 'seq_discontinuity')",
                        params![seg.pane_id, epoch_ms(), expected_seq - 1],
                    ).ok();
                }

                conn.execute(
                    "INSERT INTO output_segments
                     (pane_id, seq, captured_at, content, cursor_row, cursor_col, flags, command_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        seg.pane_id, seg.seq, seg.captured_at, seg.content,
                        seg.cursor_row, seg.cursor_col, seg.flags, seg.command_id
                    ],
                ).ok();

                pane_seqs.insert(seg.pane_id, seg.seq);
            }
            DbCmd::RecordGap { pane_id, last_seq, reason } => {
                conn.execute(
                    "INSERT INTO output_gaps (pane_id, detected_at, last_seq, reason)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![pane_id, epoch_ms(), last_seq, reason],
                ).ok();
            }
            DbCmd::RecordEvent(event) => {
                // ... insert event
            }
            DbCmd::StepStarted { execution_id, step_index, step_name } => {
                conn.execute(
                    "INSERT INTO workflow_step_log
                     (execution_id, step_index, step_name, started_at, status)
                     VALUES (?1, ?2, ?3, ?4, 'running')",
                    params![execution_id, step_index, step_name, epoch_ms()],
                ).ok();
            }
            DbCmd::StepCompleted { execution_id, step_index } => {
                conn.execute(
                    "UPDATE workflow_step_log SET completed_at = ?1, status = 'completed'
                     WHERE execution_id = ?2 AND step_index = ?3",
                    params![epoch_ms(), execution_id, step_index],
                ).ok();
            }
            DbCmd::StepFailed { execution_id, step_index, reason } => {
                conn.execute(
                    "UPDATE workflow_step_log SET completed_at = ?1, status = 'failed', detail = ?2
                     WHERE execution_id = ?3 AND step_index = ?4",
                    params![epoch_ms(), reason, execution_id, step_index],
                ).ok();
            }
            DbCmd::Shutdown => break,
        }
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
```

### 5.4 FTS Strategy (Search That Feels Magical)

FTS configuration details:
- Use `unicode61` tokenizer with separators tuned for paths (`/_-.:`)
- Keep `pane_id`, `domain_id`, timestamps as UNINDEXED columns for filtering
- Support compound queries with domain/pane scoping

```sql
-- FTS5 Virtual Table for full-text search (references output_segments)
CREATE VIRTUAL TABLE output_fts USING fts5(
    content,
    pane_id UNINDEXED,
    captured_at UNINDEXED,
    content='output_segments',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 1 separators "/_-.:@"'
);

-- IMPORTANT: Use bm25(output_fts) for ranking, NOT "ORDER BY rank"
-- Example query:
--   SELECT * FROM output_fts WHERE output_fts MATCH 'error' ORDER BY bm25(output_fts);
```

#### Search Helpers

- "search within pane" → `pane_id = ?`
- "search across domain" → JOIN on domain
- "search only errors / warnings" → JOIN on events table

### 5.5 Capture Strategy (Guaranteeing "ALL output")

We must avoid losing output when scrollback wraps. The robust approach:

1. **Polling baseline** (Tier 1):
   - Poll each pane at an adaptive interval
   - Pull last N lines (or entire scrollback) and compute overlap with last-known tail
   - Persist only the **delta** chunk

2. **Safety margin**:
   - Set WezTerm scrollback high enough (10k–100k lines) so wrap is unlikely between polls
   - Adaptive polling speeds up on active panes; slows down on idle panes

3. **Optional "high assurance mode"** (Tier 2 signaling):
   - Shell integration emits prompt boundaries and command boundaries via user-vars
   - This gives explicit segmentation ("this output belongs to command X")

4. **Optional "stream mode"** (Tier 3 vendoring):
   - Subscribe to pane output stream and append directly, bypassing polling

### 5.6 Incremental Capture Strategy

```rust
/// Efficiently capture only new content
pub struct IncrementalCapture {
    /// Last known line count per pane
    line_counts: HashMap<u64, usize>,
    /// Last content hash per pane (for change detection)
    hashes: HashMap<u64, u64>,
    /// Rolling window of recent lines (for overlap detection)
    recent_lines: HashMap<u64, VecDeque<String>>,
}

impl IncrementalCapture {
    const OVERLAP_WINDOW: usize = 10;  // Lines to keep for overlap detection

    pub fn extract_delta(&mut self, pane_id: u64, full_text: &str) -> Option<String> {
        let current_hash = fxhash::hash64(full_text);
        let current_lines: Vec<&str> = full_text.lines().collect();

        // Quick hash check - no change
        if self.hashes.get(&pane_id) == Some(&current_hash) {
            return None;
        }

        let delta = if let Some(recent) = self.recent_lines.get(&pane_id) {
            // Find where new content starts by matching overlap
            let overlap_point = self.find_overlap_point(&current_lines, recent);

            if let Some(start_idx) = overlap_point {
                // Only new lines
                current_lines[start_idx..].join("\n")
            } else {
                // No overlap found - content significantly changed
                full_text.to_string()
            }
        } else {
            // First capture
            full_text.to_string()
        };

        // Update state
        self.hashes.insert(pane_id, current_hash);
        self.line_counts.insert(pane_id, current_lines.len());

        // Update recent lines window
        let recent = self.recent_lines.entry(pane_id).or_default();
        recent.clear();
        for line in current_lines.iter().rev().take(Self::OVERLAP_WINDOW) {
            recent.push_front(line.to_string());
        }

        Some(delta)
    }

    fn find_overlap_point(&self, current: &[&str], recent: &VecDeque<String>) -> Option<usize> {
        // Try to find where the recent lines appear in current content
        if recent.is_empty() || current.is_empty() {
            return None;
        }

        // Look for the last line of recent in current
        let last_recent = recent.back()?;
        for (i, line) in current.iter().enumerate().rev() {
            if *line == last_recent {
                // Verify more lines match
                let matches = recent.iter().rev()
                    .zip(current[..=i].iter().rev())
                    .take_while(|(a, b)| a.as_str() == **b)
                    .count();

                if matches >= recent.len().min(3) {
                    return Some(i + 1);
                }
            }
        }

        None
    }
}
```

---

### 5.7 Retention, Compaction, and DB Hygiene

Log everything, but don't let it rot:
- Configurable retention (days, max DB size, or "keep last N captures per pane")
- Periodic `VACUUM` only via explicit command (avoid surprise long pauses)
- Prefer chunk-level deletion + FTS sync triggers
- Optionally compress older chunks (zstd) if/when it pays off

---

## 6. Pattern Detection Engine

### 6.1 Architecture

The pattern engine uses a tiered approach for maximum performance:

1. **Quick Reject**: Fast substring check using memchr/Aho-Corasick
2. **Literal Match**: Aho-Corasick automaton for known literal patterns
3. **Regex Match**: SIMD-accelerated regex for complex patterns
4. **Extraction**: Named capture groups for data extraction

```rust
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::Regex;
use memchr::memmem;

pub struct PatternEngine {
    /// Quick rejection keywords (if none present, skip all patterns)
    quick_reject: QuickReject,
    /// Aho-Corasick automaton for literal patterns
    literal_matcher: AhoCorasick,
    /// Pattern definitions indexed by literal match
    patterns: Vec<PatternDef>,
    /// Regex patterns for complex matches
    regex_patterns: Vec<CompiledRegexPattern>,
}

struct QuickReject {
    /// Keywords that must be present for any pattern to match
    keywords: Vec<&'static [u8]>,
    /// Precomputed finders for each keyword
    finders: Vec<memmem::Finder<'static>>,
}

impl QuickReject {
    fn might_match(&self, text: &[u8]) -> bool {
        // If ANY keyword is present, we need to run full matching
        self.finders.iter().any(|f| f.find(text).is_some())
    }
}

#[derive(Clone)]
struct PatternDef {
    id: &'static str,
    agent: AgentType,
    event: EventType,
    severity: Severity,
    /// Regex for extracting data (run after literal match confirms)
    extractor: Option<Regex>,
    /// How much context to include in raw_match
    context_before: usize,
    context_after: usize,
}

pub struct Detection {
    pub pattern_id: &'static str,
    pub agent: AgentType,
    pub event: EventType,
    pub severity: Severity,
    pub raw_match: String,
    pub position: usize,
    pub extracted: HashMap<String, String>,
    pub confidence: f32,
}

impl PatternEngine {
    pub fn new() -> Self {
        let mut literals = Vec::new();
        let mut patterns = Vec::new();

        // Build all patterns
        Self::add_claude_code_patterns(&mut literals, &mut patterns);
        Self::add_codex_patterns(&mut literals, &mut patterns);
        Self::add_gemini_patterns(&mut literals, &mut patterns);

        // Build quick reject keywords
        let quick_reject = QuickReject::new(&[
            b"limit", b"compacted", b"Token usage", b"session",
            b"limit reached", b"hit your", b"Heads up",
        ]);

        // Build Aho-Corasick
        let literal_matcher = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostFirst)
            .build(literals.iter().map(|p| p.literal))
            .expect("Failed to build pattern matcher");

        Self {
            quick_reject,
            literal_matcher,
            patterns,
            regex_patterns: Vec::new(),
        }
    }

    /// Detect all patterns in text
    pub fn detect(&self, text: &str) -> Vec<Detection> {
        let bytes = text.as_bytes();

        // Quick reject - if no keywords present, skip
        if !self.quick_reject.might_match(bytes) {
            return Vec::new();
        }

        let mut detections = Vec::new();

        // Run Aho-Corasick
        for mat in self.literal_matcher.find_iter(text) {
            let pattern = &self.patterns[mat.pattern().as_usize()];

            // Get context for extraction
            let ctx_start = mat.start().saturating_sub(pattern.context_before);
            let ctx_end = (mat.end() + pattern.context_after).min(text.len());
            let context = &text[ctx_start..ctx_end];

            // Extract data if extractor defined
            let extracted = if let Some(ref re) = pattern.extractor {
                Self::extract_captures(re, context)
            } else {
                HashMap::new()
            };

            detections.push(Detection {
                pattern_id: pattern.id,
                agent: pattern.agent,
                event: pattern.event,
                severity: pattern.severity,
                raw_match: context.to_string(),
                position: mat.start(),
                extracted,
                confidence: 1.0,
            });
        }

        // Sort by position
        detections.sort_by_key(|d| d.position);

        // Deduplicate overlapping detections
        Self::deduplicate(&mut detections);

        detections
    }

    fn extract_captures(re: &Regex, text: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();

        if let Some(caps) = re.captures(text) {
            for name in re.capture_names().flatten() {
                if let Some(m) = caps.name(name) {
                    result.insert(name.to_string(), m.as_str().to_string());
                }
            }
            // Also include numbered groups
            for (i, m) in caps.iter().enumerate().skip(1) {
                if let Some(m) = m {
                    result.entry(format!("group_{}", i))
                        .or_insert_with(|| m.as_str().to_string());
                }
            }
        }

        result
    }
}
```

### 6.2 Pattern Packs System

Patterns are organized into "packs" for scalability and maintainability:

```
core.codex         # Codex CLI patterns
core.claude_code   # Claude Code patterns
core.gemini        # Gemini CLI patterns
core.wezterm       # Compatibility backend bridge diagnostics (current: WezTerm mux/server)
org.local          # User custom rules
```

Each rule has:
- **stable `rule_id`**: `codex.session_end`, `claude.compaction`, `gemini.usage_reached`
- **match method**: literal anchor(s) + optional regex extraction
- **severity**: `info`, `warning`, `error`, `critical`
- **suggested remediation**: What workflow to trigger

This makes it possible to:
- Add rules without rewriting the engine
- Test rules in isolation
- Expose rules via CLI (`ft rules list`, `ft rules test`) and robot mode (`ft robot rules ...`)
- Version rule packs independently

#### 6.2.1 Explain-match trace (rules test)

`ft rules test` / `ft robot rules test` can optionally return a per-detection
`match_trace` payload intended for debugging. The trace is **bounded** and
**redacted** by design: it never includes raw input or secret values.

**Trace schema (v0, per detection):**

```
match_trace: {
  rule_id: string,              // stable rule id
  spans: [                      // where the match occurred (indices only)
    { start: u64, end: u64, kind: "anchor|regex|context", capture_keys?: [string] }
  ],
  extracted_keys: [string],     // keys present in `extracted` (values omitted)
  confidence?: f64,             // optional score (0..1)
  eval_path: [                  // evaluation path / subpattern hits
    { stage: string, matched: bool, hits?: [string] }
  ],
  bounds: {                     // boundedness + truncation metadata
    max_spans: u32,
    max_bytes: u32,
    truncated: bool,
    truncated_fields?: [string]
  },
  redaction: {                  // explicit safety contract
    enabled: bool,
    policy: string,             // e.g. "omit_values"
    redacted_fields?: [string]
  }
}
```

**Safety / boundedness defaults:**
- `max_spans`: 8 (cap per detection)
- `max_bytes`: 4096 (cap total trace JSON size)
- **No raw snippets**: spans are indices only; anchors/hits must be rule-defined labels,
  not input substrings.
- If truncation occurs, set `bounds.truncated=true` and list `truncated_fields`.

### 6.3 Agent-Specific Patterns

```rust
impl PatternEngine {
    fn add_claude_code_patterns(literals: &mut Vec<LiteralPattern>, patterns: &mut Vec<PatternDef>) {
        // Usage limit reached
        literals.push(LiteralPattern { literal: "You've hit your limit" });
        patterns.push(PatternDef {
            id: "cc.usage_limit",
            agent: AgentType::ClaudeCode,
            event: EventType::UsageLimitReached,
            severity: Severity::Warning,
            extractor: Some(Regex::new(
                r"resets\s+(?P<time>\d+[ap]m)\s+\((?P<timezone>[^)]+)\)"
            ).unwrap()),
            context_before: 50,
            context_after: 200,
        });

        // Compaction
        literals.push(LiteralPattern { literal: "Conversation compacted" });
        patterns.push(PatternDef {
            id: "cc.compaction",
            agent: AgentType::ClaudeCode,
            event: EventType::Compaction,
            severity: Severity::Info,
            extractor: None,
            context_before: 0,
            context_after: 100,
        });

        // Session banner (version detection)
        literals.push(LiteralPattern { literal: "Claude Code v" });
        patterns.push(PatternDef {
            id: "cc.session_start",
            agent: AgentType::ClaudeCode,
            event: EventType::SessionStart,
            severity: Severity::Info,
            extractor: Some(Regex::new(
                r"Claude Code v(?P<version>\d+\.\d+\.\d+).*?(?P<model>\w+\s*\d*\.?\d*)"
            ).unwrap()),
            context_before: 0,
            context_after: 200,
        });
    }

    fn add_codex_patterns(literals: &mut Vec<LiteralPattern>, patterns: &mut Vec<PatternDef>) {
        // Usage warnings (25%, 10%, 5%)
        for (pct, severity) in [(25, Severity::Info), (10, Severity::Warning), (5, Severity::Warning)] {
            literals.push(LiteralPattern {
                literal: Box::leak(format!("less than {}%", pct).into_boxed_str())
            });
            patterns.push(PatternDef {
                id: Box::leak(format!("codex.usage_warning_{}", pct).into_boxed_str()),
                agent: AgentType::Codex,
                event: EventType::UsageLimitWarning,
                severity,
                extractor: Some(Regex::new(r"(?P<remaining>\d+)% of your (?P<limit>\d+)h limit").unwrap()),
                context_before: 20,
                context_after: 100,
            });
        }

        // Usage limit reached
        literals.push(LiteralPattern { literal: "You've hit your usage limit" });
        patterns.push(PatternDef {
            id: "codex.usage_limit",
            agent: AgentType::Codex,
            event: EventType::UsageLimitReached,
            severity: Severity::Error,
            extractor: Some(Regex::new(
                r"try again at (?P<reset_time>[^.]+)"
            ).unwrap()),
            context_before: 0,
            context_after: 200,
        });

        // Session end with token usage
        literals.push(LiteralPattern { literal: "Token usage:" });
        patterns.push(PatternDef {
            id: "codex.session_end",
            agent: AgentType::Codex,
            event: EventType::SessionEnd,
            severity: Severity::Info,
            extractor: Some(Regex::new(
                r"total=(?P<total>[\d,]+)\s+input=(?P<input>[\d,]+)\s+\(\+\s*(?P<cached>[\d,]+)\s+cached\)\s+output=(?P<output>[\d,]+)(?:\s+\(reasoning\s+(?P<reasoning>[\d,]+)\))?"
            ).unwrap()),
            context_before: 0,
            context_after: 300,
        });

        // Resume session hint
        literals.push(LiteralPattern { literal: "codex resume" });
        patterns.push(PatternDef {
            id: "codex.resume_hint",
            agent: AgentType::Codex,
            event: EventType::SessionEnd,
            severity: Severity::Info,
            extractor: Some(Regex::new(
                r"codex resume (?P<session_id>[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"
            ).unwrap()),
            context_before: 0,
            context_after: 100,
        });

        // Device auth code
        literals.push(LiteralPattern { literal: "Enter this one-time code" });
        patterns.push(PatternDef {
            id: "codex.device_auth",
            agent: AgentType::Codex,
            event: EventType::AuthRequired,
            severity: Severity::Info,
            extractor: Some(Regex::new(r"(?P<code>[A-Z0-9]{4}-[A-Z0-9]{5})").unwrap()),
            context_before: 0,
            context_after: 50,
        });
    }

    fn add_gemini_patterns(literals: &mut Vec<LiteralPattern>, patterns: &mut Vec<PatternDef>) {
        // Usage limit
        literals.push(LiteralPattern { literal: "Usage limit reached for all Pro models" });
        patterns.push(PatternDef {
            id: "gemini.usage_limit",
            agent: AgentType::Gemini,
            event: EventType::UsageLimitReached,
            severity: Severity::Error,
            extractor: None,
            context_before: 0,
            context_after: 300,
        });

        // Session summary
        literals.push(LiteralPattern { literal: "Interaction Summary" });
        patterns.push(PatternDef {
            id: "gemini.session_end",
            agent: AgentType::Gemini,
            event: EventType::SessionEnd,
            severity: Severity::Info,
            extractor: Some(Regex::new(
                r"Session ID:\s+(?P<session_id>[0-9a-f-]+).*?Tool Calls:\s+(?P<tool_calls>\d+)"
            ).unwrap()),
            context_before: 0,
            context_after: 1000,
        });

        // Model indicator
        literals.push(LiteralPattern { literal: "Responding with gemini-" });
        patterns.push(PatternDef {
            id: "gemini.model_used",
            agent: AgentType::Gemini,
            event: EventType::ModelChange,
            severity: Severity::Info,
            extractor: Some(Regex::new(r"Responding with (?P<model>gemini-[^\s]+)").unwrap()),
            context_before: 0,
            context_after: 50,
        });
    }
}
```

### 6.4 ast-grep Integration

We incorporate `ast-grep` in two **high-leverage** places:

1. **Codebase understanding workflows** (not pane transcript matching):
   - When an agent is stuck, `ft` can run AST scans to locate:
     - `TODO`, `FIXME`, unsafe patterns, known bug signatures
     - "where is this function defined?" queries
   - Results are summarized and optionally injected into agent panes as context

2. **Rule development tooling**:
   - Use ast-grep to enforce "no brittle regex" in our own codebase
   - Ensure patterns live in a dedicated module with tests

This keeps ast-grep aligned to what it's best at: **structure-aware matching**, not line-based text matching.

### 6.5 Pattern Testing Framework

```rust
#[cfg(test)]
mod pattern_tests {
    use super::*;

    #[test]
    fn test_codex_token_usage() {
        let engine = PatternEngine::new();
        let text = r#"
Token usage: total=100,117 input=90,506 (+ 3,008,512 cached) output=9,611 (reasoning 7,168)
To continue this session, run codex resume 019bcea5-acb4-7370-a50d-8a2b59553cf6
"#;

        let detections = engine.detect(text);
        assert_eq!(detections.len(), 2);

        // Token usage detection
        let token_det = detections.iter().find(|d| d.pattern_id == "codex.session_end").unwrap();
        assert_eq!(token_det.extracted.get("total"), Some(&"100,117".to_string()));
        assert_eq!(token_det.extracted.get("cached"), Some(&"3,008,512".to_string()));
        assert_eq!(token_det.extracted.get("reasoning"), Some(&"7,168".to_string()));

        // Resume hint detection
        let resume_det = detections.iter().find(|d| d.pattern_id == "codex.resume_hint").unwrap();
        assert_eq!(
            resume_det.extracted.get("session_id"),
            Some(&"019bcea5-acb4-7370-a50d-8a2b59553cf6".to_string())
        );
    }

    #[test]
    fn test_claude_code_compaction() {
        let engine = PatternEngine::new();
        let text = "Conversation compacted · ctrl+o for history";

        let detections = engine.detect(text);
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].pattern_id, "cc.compaction");
        assert_eq!(detections[0].agent, AgentType::ClaudeCode);
        assert_eq!(detections[0].event, EventType::Compaction);
    }

    #[test]
    fn test_quick_reject_performance() {
        let engine = PatternEngine::new();

        // Text with no relevant keywords
        let boring_text = "ls -la\ndrwxr-xr-x 2 user user 4096 Jan 18 12:00 .\n";

        let start = std::time::Instant::now();
        for _ in 0..10000 {
            let _ = engine.detect(boring_text);
        }
        let elapsed = start.elapsed();

        // Should be very fast - quick reject should kick in
        assert!(elapsed.as_micros() < 10000, "Quick reject too slow: {:?}", elapsed);
    }
}
```

---

## 7. Workflow Automation System

### 7.1 Workflow Design Constraints

Every workflow must be:
- **Idempotent**: Re-running doesn't make a mess
- **Recoverable**: Can resume after a crash
- **Audited**: Every step recorded (what was observed, what was sent)
- **Guarded**: Each step validates state before action

### 7.2 Workflow Architecture

```rust
use async_trait::async_trait;

/// Context provided to workflows
pub struct WorkflowContext {
    pub client: WeztermClient,
    pub storage: Storage,
    pub pane_id: u64,
    pub pane: Pane,
    pub event: Event,
    pub detection: Detection,
    pub config: WorkflowConfig,
}

/// Result of a workflow step
pub enum StepResult {
    /// Step completed, continue to next
    Continue,
    /// Step completed, workflow done
    Done(String),
    /// Step failed, but recoverable
    Retry { after: Duration, reason: String },
    /// Step failed, abort workflow
    Abort(String),
    /// Need to wait for condition
    WaitFor { condition: WaitCondition, timeout: Duration },
}

/// Conditions to wait for
pub enum WaitCondition {
    /// Wait for pattern in pane output
    Pattern { pattern: String, pane_id: u64 },
    /// Wait for pane to be idle (shell prompt)
    PaneIdle { pane_id: u64 },
    /// Wait for external event
    External { key: String },
}

#[async_trait]
pub trait Workflow: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn handles(&self, detection: &Detection) -> bool;
    fn steps(&self) -> &[&'static str];

    async fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step: usize,
    ) -> Result<StepResult>;

    /// Called when workflow completes (success or failure)
    async fn cleanup(&self, ctx: &mut WorkflowContext, success: bool) -> Result<()> {
        Ok(())
    }
}

/// Workflow engine that manages execution
pub struct WorkflowEngine {
    workflows: Vec<Box<dyn Workflow>>,
    running: HashMap<String, WorkflowExecution>,
    config: WorkflowConfig,
}

struct WorkflowExecution {
    id: String,
    workflow_name: String,
    current_step: usize,
    started_at: Instant,
    pane_id: u64,
}

impl WorkflowEngine {
    pub fn new(config: WorkflowConfig) -> Self {
        let workflows: Vec<Box<dyn Workflow>> = vec![
            Box::new(HandleUsageLimits::new()),
            Box::new(HandleCompaction::new()),
            Box::new(HandleSessionEnd::new()),
            Box::new(HandleAuthRequired::new()),
        ];

        Self {
            workflows,
            running: HashMap::new(),
            config,
        }
    }

    /// Handle a detected event
    pub async fn handle_event(
        &mut self,
        client: &WeztermClient,
        storage: &mut Storage,
        pane: &Pane,
        event: &Event,
        detection: &Detection,
    ) -> Result<Option<String>> {
        // Find matching workflow
        let workflow = match self.workflows.iter().find(|w| w.handles(detection)) {
            Some(w) => w,
            None => return Ok(None),
        };

        // Check if already handling this pane
        if self.running.values().any(|e| e.pane_id == pane.id) {
            tracing::debug!("Workflow already running for pane {}", pane.id);
            return Ok(None);
        }

        // Create execution context
        let execution_id = uuid::Uuid::new_v4().to_string();
        let mut ctx = WorkflowContext {
            client: client.clone(),
            storage: storage.clone(),
            pane_id: pane.id,
            pane: pane.clone(),
            event: event.clone(),
            detection: detection.clone(),
            config: self.config.clone(),
        };

        // Record execution
        self.running.insert(execution_id.clone(), WorkflowExecution {
            id: execution_id.clone(),
            workflow_name: workflow.name().to_string(),
            current_step: 0,
            started_at: Instant::now(),
            pane_id: pane.id,
        });

        // Execute workflow
        let result = self.run_workflow(workflow.as_ref(), &mut ctx).await;

        // Cleanup
        self.running.remove(&execution_id);
        workflow.cleanup(&mut ctx, result.is_ok()).await?;

        result
    }

    async fn run_workflow(
        &self,
        workflow: &dyn Workflow,
        ctx: &mut WorkflowContext,
    ) -> Result<Option<String>> {
        let steps = workflow.steps();

        for (step_idx, step_name) in steps.iter().enumerate() {
            tracing::info!(
                "[{}] Step {}/{}: {}",
                workflow.name(),
                step_idx + 1,
                steps.len(),
                step_name
            );

            match workflow.execute_step(ctx, step_idx).await? {
                StepResult::Continue => continue,
                StepResult::Done(msg) => return Ok(Some(msg)),
                StepResult::Retry { after, reason } => {
                    tracing::warn!("Retry after {:?}: {}", after, reason);
                    tokio::time::sleep(after).await;
                    // Retry same step
                    return Box::pin(self.run_workflow(workflow, ctx)).await;
                }
                StepResult::Abort(reason) => {
                    return Err(anyhow::anyhow!("Workflow aborted: {}", reason));
                }
                StepResult::WaitFor { condition, timeout } => {
                    self.wait_for_condition(&condition, timeout, ctx).await?;
                }
            }
        }

        Ok(Some(format!("{} completed all steps", workflow.name())))
    }

    async fn wait_for_condition(
        &self,
        condition: &WaitCondition,
        timeout: Duration,
        ctx: &WorkflowContext,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let poll_interval = Duration::from_millis(100);

        while Instant::now() < deadline {
            match condition {
                WaitCondition::Pattern { pattern, pane_id } => {
                    let text = ctx.client.get_text(*pane_id, false).await?;
                    if text.contains(pattern) {
                        return Ok(());
                    }
                }
                WaitCondition::PaneIdle { pane_id } => {
                    // Check for shell prompt indicators
                    let text = ctx.client.get_text(*pane_id, false).await?;
                    if Self::looks_like_prompt(&text) {
                        return Ok(());
                    }
                }
                WaitCondition::External { key } => {
                    // Check external signal
                    // ...
                }
            }

            tokio::time::sleep(poll_interval).await;
        }

        Err(anyhow::anyhow!("Timeout waiting for condition"))
    }

    fn looks_like_prompt(text: &str) -> bool {
        let last_line = text.lines().last().unwrap_or("");
        // Common prompt patterns
        last_line.ends_with("$ ") ||
        last_line.ends_with("# ") ||
        last_line.ends_with("> ") ||
        last_line.contains("❯")
    }
}
```

### 7.3 Execution Model

Workflows are explicit step machines:
- Each step: `Observe → Decide → Act → Verify`
- Step can return: `Continue`, `WaitFor(condition)`, `Retry(after)`, `Abort(reason)`, `Done(result)`
- A per-pane lock prevents interleaving workflows that would conflict

### 7.4 "Workflow Library" vs "Workflow Scripts"

We support **two layers**:

1. **Compiled Rust workflows** (fast, safe, testable):
   - `HandleUsageLimits`
   - `HandleCompaction`
   - `HandleAuthRequired`
   - Complex logic with full type safety

2. **Data-driven workflow descriptors** (YAML/TOML):
   - For simple "prompt injection sequences" and operator customization
   - Still executed by the same guarded engine
   - Example: custom post-compaction prompt for specific projects

```yaml
# ~/.config/wa/workflows/custom_compaction.yaml
name: custom_compaction_prompt
trigger:
  event: session.compaction
  agent: claude_code
steps:
  - wait_stable: 2s
  - send: |
      Please re-read AGENTS.md and the project's README.md.
      Also review the current TODO list via `bd ready`.
  - wait_for: "❯"
```

### 7.5 Handle Usage Limits Workflow

```rust
pub struct HandleUsageLimits {
    browser_automation: BrowserAutomation,
}

impl HandleUsageLimits {
    pub fn new() -> Self {
        Self {
            browser_automation: BrowserAutomation::new(),
        }
    }
}

#[async_trait]
impl Workflow for HandleUsageLimits {
    fn name(&self) -> &'static str { "handle_usage_limits" }

    fn description(&self) -> &'static str {
        "Automatically rotate accounts when usage limit is reached"
    }

    fn handles(&self, detection: &Detection) -> bool {
        matches!(detection.event, EventType::UsageLimitReached)
    }

    fn steps(&self) -> &[&'static str] {
        &[
            "Exit current session gracefully",
            "Parse session info for resume",
            "Select next available account",
            "Perform account authentication",
            "Resume previous session",
            "Send continue prompt",
        ]
    }

    async fn execute_step(&self, ctx: &mut WorkflowContext, step: usize) -> Result<StepResult> {
        match step {
            0 => self.exit_session(ctx).await,
            1 => self.parse_session_info(ctx).await,
            2 => self.select_next_account(ctx).await,
            3 => self.perform_auth(ctx).await,
            4 => self.resume_session(ctx).await,
            5 => self.send_continue(ctx).await,
            _ => Ok(StepResult::Abort("Invalid step".to_string())),
        }
    }
}

impl HandleUsageLimits {
    async fn exit_session(&self, ctx: &mut WorkflowContext) -> Result<StepResult> {
        match ctx.detection.agent {
            AgentType::Codex => {
                // Send Ctrl-C twice
                ctx.client.send_text(ctx.pane_id, "\x03", true).await?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                ctx.client.send_text(ctx.pane_id, "\x03", true).await?;
            }
            AgentType::ClaudeCode => {
                // Send Ctrl-C
                ctx.client.send_text(ctx.pane_id, "\x03", true).await?;
            }
            AgentType::Gemini => {
                // Send /quit
                ctx.client.send_text(ctx.pane_id, "/quit\n", true).await?;
            }
        }

        // Wait for session to end
        Ok(StepResult::WaitFor {
            condition: WaitCondition::PaneIdle { pane_id: ctx.pane_id },
            timeout: Duration::from_secs(10),
        })
    }

    async fn parse_session_info(&self, ctx: &mut WorkflowContext) -> Result<StepResult> {
        let text = ctx.client.get_text(ctx.pane_id, false).await?;

        // Look for session ID and token usage
        let session_info = match ctx.detection.agent {
            AgentType::Codex => self.parse_codex_session(&text),
            AgentType::ClaudeCode => self.parse_claude_code_session(&text, ctx),
            AgentType::Gemini => self.parse_gemini_session(&text),
        };

        if let Some(info) = session_info {
            // Store for later steps
            ctx.storage.record_session_info(ctx.pane_id, &info)?;
            Ok(StepResult::Continue)
        } else {
            Ok(StepResult::Abort("Could not parse session info".to_string()))
        }
    }

    fn parse_codex_session(&self, text: &str) -> Option<SessionInfo> {
        // Extract session ID
        let session_re = Regex::new(
            r"codex resume ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"
        ).unwrap();

        let session_id = session_re.captures(text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())?;

        // Extract token usage
        let token_re = Regex::new(
            r"total=([\d,]+)\s+input=([\d,]+)\s+\(\+\s*([\d,]+)\s+cached\)\s+output=([\d,]+)"
        ).unwrap();

        let token_usage = token_re.captures(text).map(|c| TokenUsage {
            total: parse_number(c.get(1)),
            input: parse_number(c.get(2)),
            cached: parse_number(c.get(3)),
            output: parse_number(c.get(4)),
            reasoning: None,
        });

        Some(SessionInfo {
            agent: AgentType::Codex,
            session_id: Some(session_id),
            token_usage,
            resume_command: Some(format!("cod resume {}", session_id)),
        })
    }

    async fn select_next_account(&self, ctx: &mut WorkflowContext) -> Result<StepResult> {
        let service = match ctx.detection.agent {
            AgentType::Codex => "openai",
            AgentType::ClaudeCode => "anthropic",
            AgentType::Gemini => "google",
        };

        match ctx.storage.get_next_account(service)? {
            Some(account) => {
                // Store selected account
                ctx.config.set("selected_account", &account.account_name);
                tracing::info!("Selected account: {}", account.account_name);
                Ok(StepResult::Continue)
            }
            None => {
                Ok(StepResult::Abort(format!("No available {} accounts", service)))
            }
        }
    }

    async fn perform_auth(&self, ctx: &mut WorkflowContext) -> Result<StepResult> {
        let account_name = ctx.config.get("selected_account")
            .ok_or_else(|| anyhow::anyhow!("No account selected"))?;

        let account = ctx.storage.get_account(&account_name)?
            .ok_or_else(|| anyhow::anyhow!("Account not found"))?;

        match ctx.detection.agent {
            AgentType::Codex => {
                // Start device auth flow
                ctx.client.send_text(ctx.pane_id, "cod login --device-auth\n", true).await?;

                // Wait for device code
                tokio::time::sleep(Duration::from_secs(2)).await;
                let text = ctx.client.get_text(ctx.pane_id, false).await?;

                // Extract device code
                let code_re = Regex::new(r"([A-Z0-9]{4}-[A-Z0-9]{5})").unwrap();
                let device_code = code_re.captures(&text)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Device code not found"))?;

                // Perform browser automation
                self.browser_automation
                    .complete_openai_device_auth(device_code, &account)
                    .await?;

                // Wait for login confirmation
                Ok(StepResult::WaitFor {
                    condition: WaitCondition::Pattern {
                        pattern: "Successfully logged in".to_string(),
                        pane_id: ctx.pane_id,
                    },
                    timeout: Duration::from_secs(30),
                })
            }
            AgentType::ClaudeCode => {
                // Claude Code uses /login
                ctx.client.send_text(ctx.pane_id, "/login\n", true).await?;

                // Browser auth flow
                self.browser_automation
                    .complete_anthropic_auth(&account)
                    .await?;

                Ok(StepResult::WaitFor {
                    condition: WaitCondition::PaneIdle { pane_id: ctx.pane_id },
                    timeout: Duration::from_secs(30),
                })
            }
            AgentType::Gemini => {
                // Gemini uses /auth
                ctx.client.send_text(ctx.pane_id, "/auth\n", true).await?;

                self.browser_automation
                    .complete_google_auth(&account)
                    .await?;

                Ok(StepResult::WaitFor {
                    condition: WaitCondition::PaneIdle { pane_id: ctx.pane_id },
                    timeout: Duration::from_secs(30),
                })
            }
        }
    }

    async fn resume_session(&self, ctx: &mut WorkflowContext) -> Result<StepResult> {
        let session_info = ctx.storage.get_session_info(ctx.pane_id)?
            .ok_or_else(|| anyhow::anyhow!("No session info stored"))?;

        if let Some(resume_cmd) = session_info.resume_command {
            ctx.client.send_text(ctx.pane_id, &format!("{}\n", resume_cmd), true).await?;

            // Wait for session to resume
            Ok(StepResult::WaitFor {
                condition: WaitCondition::Pattern {
                    pattern: match ctx.detection.agent {
                        AgentType::Codex => "Welcome back".to_string(),
                        AgentType::ClaudeCode => "Claude Code".to_string(),
                        AgentType::Gemini => "Session restored".to_string(),
                    },
                    pane_id: ctx.pane_id,
                },
                timeout: Duration::from_secs(30),
            })
        } else {
            Ok(StepResult::Abort("No resume command available".to_string()))
        }
    }

    async fn send_continue(&self, ctx: &mut WorkflowContext) -> Result<StepResult> {
        // Brief pause for session to stabilize
        tokio::time::sleep(Duration::from_secs(1)).await;

        ctx.client.send_text(ctx.pane_id, "proceed.\n", true).await?;

        // Update account usage tracking
        let account_name = ctx.config.get("selected_account").unwrap();
        ctx.storage.mark_account_used(&account_name)?;

        Ok(StepResult::Done(format!(
            "Rotated to account {} and resumed session",
            account_name
        )))
    }
}
```

### 7.6 Handle Compaction Workflow

```rust
pub struct HandleCompaction;

#[async_trait]
impl Workflow for HandleCompaction {
    fn name(&self) -> &'static str { "handle_compaction" }

    fn description(&self) -> &'static str {
        "Re-inject critical context after conversation compaction"
    }

    fn handles(&self, detection: &Detection) -> bool {
        matches!(detection.event, EventType::Compaction)
    }

    fn steps(&self) -> &[&'static str] {
        &[
            "Wait for compaction to complete",
            "Send context refresh prompt",
        ]
    }

    async fn execute_step(&self, ctx: &mut WorkflowContext, step: usize) -> Result<StepResult> {
        match step {
            0 => {
                // Wait for compaction to finish
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(StepResult::Continue)
            }
            1 => {
                // Send agent-specific context refresh
                let prompt = match ctx.detection.agent {
                    AgentType::ClaudeCode => {
                        "Reread AGENTS.md so it's still fresh in your mind.\n"
                    }
                    AgentType::Codex => {
                        "Please re-read the AGENTS.md and any relevant context files.\n"
                    }
                    AgentType::Gemini => {
                        "Re-examine the AGENTS.md and project context.\n"
                    }
                };

                ctx.client.send_text(ctx.pane_id, prompt, true).await?;

                Ok(StepResult::Done(format!(
                    "Sent context refresh for {:?}",
                    ctx.detection.agent
                )))
            }
            _ => Ok(StepResult::Abort("Invalid step".to_string())),
        }
    }
}
```

---

## 8. Robot Mode: Agent-First Interface

### 8.1 Design Philosophy

Robot mode is designed for AI coding agents to control ft. Key principles:

1. **Explicit over implicit**: Every command clearly states what it does
2. **Structured output**: JSON for data, Markdown for reports
3. **Token-efficient**: Minimal verbosity, maximum information density
4. **Self-documenting**: Built-in help optimized for agent parsing
5. **Composable**: Small commands that combine well

### 8.2 Robot Mode Contract (Stable & Token-Efficient)

All robot commands return a consistent JSON envelope:

```json
{
  "ok": true,
  "data": { "...": "..." },
  "error": null,
  "hint": null,
  "elapsed_ms": 12,
  "version": "0.1.0",
  "now": "2026-01-18T02:24:00Z"
}
```

Error format:
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

### 8.3 CLI Structure

```rust
#[derive(Parser)]
#[command(name = "ft")]
#[command(about = "ft — FrankenTerm — swarm-native terminal platform for AI agent fleets")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Output format
    #[arg(long, global = true, default_value = "auto")]
    format: OutputFormat,

    /// Suppress non-essential output
    #[arg(long, short, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Continuous monitoring daemon
    Watch(WatchArgs),

    /// Agent-optimized interface
    Robot(RobotArgs),

    /// Query stored data
    Query(QueryArgs),

    /// Execute workflow
    Workflow(WorkflowArgs),

    /// Setup and configuration
    Setup(SetupArgs),

    /// Status and diagnostics
    Status(StatusArgs),
}

#[derive(Args)]
struct RobotArgs {
    #[command(subcommand)]
    command: RobotCommand,
}

#[derive(Subcommand)]
enum RobotCommand {
    /// Get state of all panes
    State {
        /// Filter by domain
        #[arg(long)]
        domain: Option<String>,
        /// Filter by agent type
        #[arg(long)]
        agent: Option<AgentType>,
    },

    /// Get text from pane
    GetText {
        pane_id: u64,
        /// Only last N lines
        #[arg(long)]
        tail: Option<usize>,
        /// Include ANSI escapes
        #[arg(long)]
        escapes: bool,
    },

    /// Send text to pane
    Send {
        pane_id: u64,
        text: String,
        /// Don't add newline
        #[arg(long)]
        no_newline: bool,
        /// Wait for pattern after sending
        #[arg(long)]
        wait_for: Option<String>,
        /// Timeout for wait
        #[arg(long, default_value = "30")]
        timeout_secs: u64,
    },

    /// Wait for pattern in pane
    WaitFor {
        pane_id: u64,
        pattern: String,
        #[arg(long, default_value = "30")]
        timeout_secs: u64,
    },

    /// Search output history
    Search {
        query: String,
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Get recent events
    Events {
        #[arg(long)]
        pane_id: Option<u64>,
        #[arg(long)]
        unhandled: bool,
        #[arg(long)]
        event_type: Option<String>,
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Trigger workflow manually
    Workflow {
        name: String,
        pane_id: u64,
        /// Force execution even if conditions don't match
        #[arg(long)]
        force: bool,
    },

    /// Get agent-optimized help
    Help {
        /// Show examples
        #[arg(long)]
        examples: bool,
    },
}
```

### 8.4 Robot Output Format

```rust
#[derive(Serialize)]
struct RobotOutput<T> {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RobotError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    /// Milliseconds taken
    elapsed_ms: u64,
}

#[derive(Serialize)]
struct RobotError {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

impl<T: Serialize> RobotOutput<T> {
    fn success(data: T, elapsed: Duration) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            hint: None,
            elapsed_ms: elapsed.as_millis() as u64,
        }
    }

    fn error(code: &'static str, message: impl Into<String>, elapsed: Duration) -> RobotOutput<()> {
        RobotOutput {
            success: false,
            data: None,
            error: Some(RobotError {
                code,
                message: message.into(),
                details: None,
            }),
            hint: None,
            elapsed_ms: elapsed.as_millis() as u64,
        }
    }

    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }

    fn to_markdown(&self) -> String {
        if self.success {
            format!(
                "## Success\n\n```json\n{}\n```\n\n*Completed in {}ms*",
                serde_json::to_string_pretty(&self.data).unwrap(),
                self.elapsed_ms
            )
        } else {
            let err = self.error.as_ref().unwrap();
            format!(
                "## Error: {}\n\n{}\n\n*Failed after {}ms*",
                err.code,
                err.message,
                self.elapsed_ms
            )
        }
    }
}
```

### 8.5 Quick-Start Help (No Arguments)

```rust
fn quick_start_help() -> String {
    r#"
╔══════════════════════════════════════════════════════════════════════════════╗
║                         ft — FrankenTerm                                     ║
║      Swarm-native terminal platform for AI agent fleets                      ║
╚══════════════════════════════════════════════════════════════════════════════╝

QUICK START
───────────
  ft watch            Start monitoring daemon (background)
  ft robot state      Get all panes as JSON
  ft robot help       Agent-optimized documentation

CORE WORKFLOWS
──────────────
  ft watch [--interval-ms 500]     Continuous monitoring + auto-handling
  ft robot state                   List all panes with metadata
  ft robot get-text <pane>         Read pane output
  ft robot send <pane> "text"      Send input to pane
  ft robot wait-for <pane> "pat"   Wait for pattern
  ft robot search "query"          Full-text search history

AGENT PATTERNS
──────────────
  # Check if agent hit limit
  ft robot events --unhandled --event-type usage_limit

  # Monitor specific agent
  ft robot state --agent codex

  # Send and wait for response
  ft robot send 3 "continue" --wait-for ">"

OUTPUT FORMATS
──────────────
  --format json       Machine-readable (default for robot)
  --format toon       Token-optimized object notation

For detailed help: ft robot help --examples
"#.to_string()
}
```

### 8.6 Robot Commands Implementation

```rust
async fn robot_state(args: StateArgs, storage: &Storage) -> Result<RobotOutput<Vec<PaneState>>> {
    let start = Instant::now();
    let client = WeztermClient::new()?;

    let mut panes = client.list_panes().await?;

    // Apply filters
    if let Some(ref domain) = args.domain {
        panes.retain(|p| p.domain == *domain);
    }

    // Enrich with stored state
    let states: Vec<PaneState> = panes.into_iter().map(|p| {
        let stored = storage.get_pane_state(p.pane_id).ok().flatten();
        PaneState {
            pane_id: p.pane_id,
            domain: p.domain,
            window_id: p.window_id,
            tab_id: p.tab_id,
            title: p.title,
            size: format!("{}x{}", p.size.cols, p.size.rows),
            cwd: p.cwd,
            agent: stored.as_ref().and_then(|s| s.agent_type.clone()),
            state: stored.map(|s| s.pane_state).unwrap_or_else(|| "unknown".to_string()),
            last_event: storage.get_last_event(p.pane_id).ok().flatten().map(|e| e.event_type),
        }
    }).collect();

    // Filter by agent if specified
    let states = if let Some(ref agent) = args.agent {
        states.into_iter().filter(|s| s.agent.as_ref() == Some(agent)).collect()
    } else {
        states
    };

    Ok(RobotOutput::success(states, start.elapsed()))
}

async fn robot_send(args: SendArgs, client: &WeztermClient) -> Result<RobotOutput<SendResult>> {
    let start = Instant::now();

    // Validate pane exists
    let panes = client.list_panes().await?;
    if !panes.iter().any(|p| p.pane_id == args.pane_id) {
        return Ok(RobotOutput::error(
            "PANE_NOT_FOUND",
            format!("Pane {} does not exist", args.pane_id),
            start.elapsed(),
        ));
    }

    // Prepare text
    let text = if args.no_newline {
        args.text.clone()
    } else {
        format!("{}\n", args.text)
    };

    // Send
    client.send_text(args.pane_id, &text, false).await?;

    // Wait if requested
    let matched = if let Some(ref pattern) = args.wait_for {
        let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
        loop {
            if Instant::now() > deadline {
                return Ok(RobotOutput::error(
                    "TIMEOUT",
                    format!("Pattern '{}' not found within {}s", pattern, args.timeout_secs),
                    start.elapsed(),
                ));
            }

            let output = client.get_text(args.pane_id, false).await?;
            if output.contains(pattern) {
                break true;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else {
        false
    };

    Ok(RobotOutput::success(SendResult {
        pane_id: args.pane_id,
        sent_bytes: text.len(),
        pattern_matched: if args.wait_for.is_some() { Some(matched) } else { None },
    }, start.elapsed()))
}
```

---

## 9. Browser Automation Layer

### 9.1 Credential Handling Principles

Before diving into Playwright, we need to establish credential handling principles:

- **Prefer SSO sessions + profiles** over storing plaintext passwords
- **Browser profiles** persist cookies, SSO sessions, reducing re-auth needs
- **OS keychain integration** (future) for sensitive data; DB stays secret-free
- **Never print secrets** in robot mode output
- **Semi-automated fallback**: If Playwright can't complete (MFA/password), open interactive browser for human completion, then persist profile

### 9.2 Playwright Integration

```rust
use playwright::{Playwright, Browser, Page, BrowserContext};

pub struct BrowserAutomation {
    playwright: Option<Playwright>,
    profiles_dir: PathBuf,
}

impl BrowserAutomation {
    pub fn new() -> Self {
        Self {
            playwright: None,
            profiles_dir: dirs::data_dir()
                .unwrap()
                .join("wa")
                .join("browser_profiles"),
        }
    }

    async fn get_playwright(&mut self) -> Result<&Playwright> {
        if self.playwright.is_none() {
            let pw = Playwright::initialize().await?;
            pw.prepare()?;  // Install browsers if needed
            self.playwright = Some(pw);
        }
        Ok(self.playwright.as_ref().unwrap())
    }

    async fn create_context(&mut self, account: &Account) -> Result<BrowserContext> {
        let pw = self.get_playwright().await?;
        let browser = pw.chromium().launcher().headless(false).launch().await?;

        // Use persistent profile if available
        let profile_path = self.profiles_dir.join(&account.service).join(&account.account_name);

        if profile_path.exists() {
            browser.context_builder()
                .storage_state_path(&profile_path)
                .build()
                .await
        } else {
            browser.context_builder().build().await
        }
    }

    pub async fn complete_openai_device_auth(
        &mut self,
        device_code: &str,
        account: &Account,
    ) -> Result<()> {
        let context = self.create_context(account).await?;
        let page = context.new_page().await?;

        // Navigate to device auth page
        page.goto_builder("https://auth.openai.com/codex/device")
            .goto()
            .await?;

        // Wait for and fill email
        page.wait_for_selector_builder("input[name='email']")
            .timeout(10000.0)
            .wait_for_selector()
            .await?;

        page.fill_builder("input[name='email']", &account.email.as_ref().unwrap_or(&account.account_name))
            .fill()
            .await?;

        page.click_builder("button[type='submit']").click().await?;

        // Handle authentication (password, OTP, or SSO)
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Check if we need password
        if page.query_selector("input[type='password']").await?.is_some() {
            // Would need secure credential storage
            return Err(anyhow::anyhow!("Password auth not yet supported"));
        }

        // Wait for device code input
        page.wait_for_selector_builder("input[name='user_code']")
            .timeout(30000.0)
            .wait_for_selector()
            .await?;

        // Enter device code
        page.fill_builder("input[name='user_code']", device_code)
            .fill()
            .await?;

        page.click_builder("button[type='submit']").click().await?;

        // Wait for success
        page.wait_for_selector_builder("text=Successfully")
            .timeout(15000.0)
            .wait_for_selector()
            .await?;

        // Save profile state
        let profile_path = self.profiles_dir
            .join(&account.service)
            .join(&account.account_name);
        std::fs::create_dir_all(&profile_path)?;
        context.storage_state_builder()
            .path(&profile_path.join("state.json"))
            .storage_state()
            .await?;

        Ok(())
    }

    pub async fn complete_anthropic_auth(&mut self, account: &Account) -> Result<()> {
        let context = self.create_context(account).await?;
        let page = context.new_page().await?;

        page.goto_builder("https://console.anthropic.com/login")
            .goto()
            .await?;

        // Similar flow for Anthropic...
        // Google OAuth or email/password

        todo!("Implement Anthropic auth flow")
    }

    pub async fn complete_google_auth(&mut self, account: &Account) -> Result<()> {
        let context = self.create_context(account).await?;
        let page = context.new_page().await?;

        page.goto_builder("https://accounts.google.com")
            .goto()
            .await?;

        // Google auth flow...

        todo!("Implement Google auth flow")
    }
}
```

---

## 10. MCP Server Integration

### 10.1 Using fastmcp_rust for MCP Interface

```rust
use fastmcp_rust::{McpServer, Tool, Resource, ToolResult};

pub struct WaMcpServer {
    watcher: Arc<RwLock<Watcher>>,
    storage: Arc<Storage>,
    client: WeztermClient,
}

impl WaMcpServer {
    pub async fn serve(self, transport: impl Transport) -> Result<()> {
        let server = McpServer::builder()
            .name("wezterm-automata")
            .version(env!("CARGO_PKG_VERSION"))
            .build();

        // Register tools
        server.register_tool(self.state_tool());
        server.register_tool(self.get_text_tool());
        server.register_tool(self.send_text_tool());
        server.register_tool(self.wait_for_tool());
        server.register_tool(self.search_tool());
        server.register_tool(self.workflow_tool());

        // Register resources
        server.register_resource(self.panes_resource());
        server.register_resource(self.events_resource());

        server.serve(transport).await
    }

    fn state_tool(&self) -> Tool {
        Tool::builder()
            .name("wa_state")
            .description("Get state of all WezTerm panes")
            .parameter("domain", "string", "Filter by domain name", false)
            .parameter("agent", "string", "Filter by agent type (claude_code, codex, gemini)", false)
            .handler(|params| async move {
                let domain = params.get("domain").and_then(|v| v.as_str());
                let agent = params.get("agent").and_then(|v| v.as_str());

                let client = WeztermClient::new()?;
                let panes = client.list_panes().await?;

                // Apply filters and return
                ToolResult::json(panes)
            })
            .build()
    }

    fn send_text_tool(&self) -> Tool {
        Tool::builder()
            .name("wa_send")
            .description("Send text to a WezTerm pane")
            .parameter("pane_id", "integer", "Target pane ID", true)
            .parameter("text", "string", "Text to send", true)
            .parameter("wait_for", "string", "Pattern to wait for after sending", false)
            .parameter("timeout_secs", "integer", "Timeout for wait (default: 30)", false)
            .handler(|params| async move {
                let pane_id: u64 = params.get("pane_id")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| anyhow::anyhow!("pane_id required"))?;
                let text = params.get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("text required"))?;

                let client = WeztermClient::new()?;
                client.send_text(pane_id, text, false).await?;

                // Handle wait_for if specified
                if let Some(pattern) = params.get("wait_for").and_then(|v| v.as_str()) {
                    let timeout = params.get("timeout_secs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(30);

                    // Wait for pattern...
                }

                ToolResult::success("Text sent successfully")
            })
            .build()
    }

    fn panes_resource(&self) -> Resource {
        Resource::builder()
            .uri("wa://panes")
            .name("WezTerm Panes")
            .description("Current state of all panes")
            .mime_type("application/json")
            .handler(|| async move {
                let client = WeztermClient::new()?;
                let panes = client.list_panes().await?;
                serde_json::to_string_pretty(&panes)
            })
            .build()
    }
}
```

---

## 11. External Tool Integration

### 11.1 CASS Integration (coding_agent_session_search)

```rust
pub mod cass {
    use std::process::Command;

    #[derive(Debug, Deserialize)]
    pub struct CassSession {
        pub session_id: String,
        pub agent: String,
        pub project_path: String,
        pub started_at: String,
        pub ended_at: Option<String>,
        pub messages: Vec<CassMessage>,
    }

    #[derive(Debug, Deserialize)]
    pub struct CassMessage {
        pub role: String,
        pub content: String,
        pub timestamp: String,
        pub token_count: Option<u64>,
    }

    /// Correlate wa session with cass data
    pub fn find_session(session_id: &str) -> Result<Option<CassSession>> {
        let output = Command::new("cass")
            .args(["query", "--session-id", session_id, "--format", "json"])
            .output()?;

        if !output.status.success() {
            return Ok(None);
        }

        let session: CassSession = serde_json::from_slice(&output.stdout)?;
        Ok(Some(session))
    }

    /// Search cass for sessions in a directory
    pub fn search_sessions(path: &str, agent: Option<&str>) -> Result<Vec<CassSession>> {
        let mut cmd = Command::new("cass");
        cmd.args(["search", "--path", path, "--format", "json"]);

        if let Some(a) = agent {
            cmd.args(["--agent", a]);
        }

        let output = cmd.output()?;
        let sessions: Vec<CassSession> = serde_json::from_slice(&output.stdout)?;
        Ok(sessions)
    }
}
```

### 11.2 CAUT Integration (coding_agent_usage_tracker)

```rust
pub mod caut {
    use std::process::Command;

    #[derive(Debug, Deserialize)]
    pub struct UsageInfo {
        pub service: String,
        pub limit_type: String,
        pub current: f64,
        pub max: f64,
        pub resets_at: Option<String>,
        pub percent_remaining: f64,
    }

    /// Get current usage for a service
    pub fn get_usage(service: &str) -> Result<UsageInfo> {
        let output = Command::new("caut")
            .args(["usage", "--service", service, "--format", "json"])
            .output()?;

        let usage: UsageInfo = serde_json::from_slice(&output.stdout)?;
        Ok(usage)
    }

    /// Refresh usage data from APIs
    pub async fn refresh_usage(service: &str) -> Result<UsageInfo> {
        let output = tokio::process::Command::new("caut")
            .args(["refresh", "--service", service, "--format", "json"])
            .output()
            .await?;

        let usage: UsageInfo = serde_json::from_slice(&output.stdout)?;
        Ok(usage)
    }
}
```

### 11.3 rich_rust Integration for Output

```rust
use rich_rust::{Console, Table, Panel, Style, Color};

pub fn render_pane_table(panes: &[PaneState]) -> String {
    let console = Console::new();

    let mut table = Table::new()
        .title("WezTerm Panes")
        .add_column("ID", Style::bold())
        .add_column("Domain", Style::new())
        .add_column("Title", Style::new())
        .add_column("Agent", Style::new())
        .add_column("State", Style::new());

    for pane in panes {
        let state_style = match pane.state.as_str() {
            "idle" => Style::fg(Color::Green),
            "usage_limit" => Style::fg(Color::Red).bold(),
            "compacted" => Style::fg(Color::Yellow),
            _ => Style::new(),
        };

        table = table.add_row(vec![
            pane.pane_id.to_string(),
            pane.domain.clone(),
            pane.title.clone().unwrap_or_default(),
            pane.agent.clone().unwrap_or_else(|| "-".to_string()),
            pane.state.clone(),
        ]);
    }

    console.render(&table)
}

pub fn render_event_panel(event: &Event) -> String {
    let console = Console::new();

    let title = format!("[{}] {}", event.severity, event.event_type);
    let content = format!(
        "Pane: {}\nAgent: {}\nDetected: {}\n\n{}",
        event.pane_id,
        event.agent_type,
        event.detected_at,
        event.raw_match
    );

    let style = match event.severity.as_str() {
        "critical" | "error" => Style::border(Color::Red),
        "warning" => Style::border(Color::Yellow),
        _ => Style::border(Color::Blue),
    };

    console.render(&Panel::new(content).title(&title).style(style))
}
```

---

## 12. Setup & Configuration Automation

### 12.1 SSH Config Parser

```rust
pub fn parse_ssh_config() -> Result<Vec<SshHost>> {
    let config_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("No home directory"))?
        .join(".ssh")
        .join("config");

    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&config_path)?;
    let mut hosts = Vec::new();
    let mut current: Option<SshHost> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() != 2 {
            continue;
        }

        let (key, value) = (parts[0].to_lowercase(), parts[1].trim());

        match key.as_str() {
            "host" => {
                if let Some(host) = current.take() {
                    if !host.name.contains('*') && !host.name.contains('?') {
                        hosts.push(host);
                    }
                }
                current = Some(SshHost {
                    name: value.to_string(),
                    hostname: value.to_string(),
                    ..Default::default()
                });
            }
            "hostname" => {
                if let Some(ref mut host) = current {
                    host.hostname = value.to_string();
                }
            }
            "user" => {
                if let Some(ref mut host) = current {
                    host.user = Some(value.to_string());
                }
            }
            "port" => {
                if let Some(ref mut host) = current {
                    host.port = value.parse().ok();
                }
            }
            "identityfile" => {
                if let Some(ref mut host) = current {
                    host.identity_file = Some(expand_tilde(value));
                }
            }
            _ => {}
        }
    }

    if let Some(host) = current {
        if !host.name.contains('*') && !host.name.contains('?') {
            hosts.push(host);
        }
    }

    Ok(hosts)
}
```

### 12.2 Automated Remote Setup

```rust
pub async fn setup_remote_host(host: &str, config: &SetupConfig) -> Result<SetupResult> {
    use tokio::process::Command;

    let mut result = SetupResult::default();

    // 1. Check if wezterm is installed
    let check = Command::new("ssh")
        .args([host, "which", "wezterm"])
        .output()
        .await?;

    if !check.status.success() {
        tracing::info!("Installing WezTerm on {}", host);

        // Detect package manager and install
        let install_script = r#"
            if command -v apt-get &> /dev/null; then
                curl -fsSL https://apt.fury.io/wez/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/wezterm-fury.gpg
                echo 'deb [signed-by=/usr/share/keyrings/wezterm-fury.gpg] https://apt.fury.io/wez/ * *' | sudo tee /etc/apt/sources.list.d/wezterm.list
                sudo apt-get update && sudo apt-get install -y wezterm
            elif command -v dnf &> /dev/null; then
                sudo dnf install -y wezterm
            else
                echo "Unsupported package manager" >&2
                exit 1
            fi
        "#;

        Command::new("ssh")
            .args([host, "bash", "-c", install_script])
            .status()
            .await?;

        result.wezterm_installed = true;
    }

    // 2. Create systemd service
    let service_unit = include_str!("../resources/wezterm-mux-server.service");

    Command::new("ssh")
        .args([host, "mkdir", "-p", "~/.config/systemd/user"])
        .status()
        .await?;

    let mut ssh = Command::new("ssh")
        .args([host, "cat", ">", "~/.config/systemd/user/wezterm-mux-server.service"])
        .stdin(std::process::Stdio::piped())
        .spawn()?;

    ssh.stdin.as_mut().unwrap()
        .write_all(service_unit.as_bytes())
        .await?;
    ssh.wait().await?;

    result.service_created = true;

    // 3. Enable and start service
    Command::new("ssh")
        .args([
            host,
            "systemctl", "--user", "daemon-reload", "&&",
            "systemctl", "--user", "enable", "--now", "wezterm-mux-server", "&&",
            "sudo", "loginctl", "enable-linger", "$USER",
        ])
        .status()
        .await?;

    result.service_enabled = true;

    // 4. Optionally install wa binary
    if config.install_wa {
        let wa_binary = std::env::current_exe()?;

        Command::new("scp")
            .args([wa_binary.to_str().unwrap(), &format!("{}:~/.local/bin/wa", host)])
            .status()
            .await?;

        result.wa_installed = true;
    }

    // 5. Verify
    let verify = Command::new("ssh")
        .args([host, "systemctl", "--user", "is-active", "wezterm-mux-server"])
        .output()
        .await?;

    result.verified = verify.status.success();

    Ok(result)
}
```

### 12.3 WezTerm Config Generator

```rust
pub fn generate_wezterm_config(hosts: &[SshHost], config: &WaConfig) -> String {
    let mut lua = String::from(r#"local wezterm = require 'wezterm'
local config = wezterm.config_builder()

-- SSH Domains with WezTerm multiplexing
config.ssh_domains = {
"#);

    for host in hosts {
        let user = host.user.as_deref().unwrap_or("$USER");
        lua.push_str(&format!(r#"  {{
    name = '{}',
    remote_address = '{}',
    username = '{}',
    multiplexing = 'WezTerm',
    assume_shell = 'Posix',
  }},
"#, host.name, host.hostname, user));
    }

    lua.push_str("}\n\n");

    // Add domain colors if configured
    if config.generate_colors {
        lua.push_str(&generate_domain_colors(hosts));
    }

    // Add wa integration
    lua.push_str(r#"
-- FrankenTerm (ft) compatibility-bridge integration
wezterm.on('user-var-changed', function(window, pane, name, value)
  if name:match('^wa%-') then
    wezterm.background_child_process {
      'wa', 'event', '--pane', tostring(pane:pane_id()),
      '--name', name, '--value', value
    }
  end
end)

return config
"#);

    lua
}
```

---

## 13. Performance Engineering

### 13.1 Adaptive Polling

```rust
pub struct AdaptivePoller {
    base_interval: Duration,
    min_interval: Duration,
    max_interval: Duration,
    current_interval: Duration,
    last_activity: HashMap<u64, Instant>,
    activity_decay: Duration,
}

impl AdaptivePoller {
    pub fn new(config: &PollerConfig) -> Self {
        Self {
            base_interval: config.base_interval,
            min_interval: config.min_interval,
            max_interval: config.max_interval,
            current_interval: config.base_interval,
            last_activity: HashMap::new(),
            activity_decay: Duration::from_secs(10),
        }
    }

    /// Record activity on a pane (new output detected)
    pub fn record_activity(&mut self, pane_id: u64) {
        self.last_activity.insert(pane_id, Instant::now());

        // Decrease interval to be more responsive
        self.current_interval = self.min_interval;
    }

    /// Get current polling interval
    pub fn interval(&self) -> Duration {
        self.current_interval
    }

    /// Update interval based on activity decay
    pub fn tick(&mut self) {
        let now = Instant::now();

        // Clean up old entries
        self.last_activity.retain(|_, t| now.duration_since(*t) < self.activity_decay * 3);

        // Check if any pane has recent activity
        let any_recent = self.last_activity.values()
            .any(|t| now.duration_since(*t) < self.activity_decay);

        if any_recent {
            // Keep responsive
            self.current_interval = self.min_interval;
        } else {
            // Gradually increase interval (exponential backoff)
            self.current_interval = (self.current_interval * 11 / 10).min(self.max_interval);
        }
    }

    /// Get per-pane priority (higher = poll first)
    pub fn pane_priority(&self, pane_id: u64) -> u32 {
        match self.last_activity.get(&pane_id) {
            Some(t) if t.elapsed() < Duration::from_secs(1) => 100,
            Some(t) if t.elapsed() < Duration::from_secs(5) => 50,
            Some(_) => 10,
            None => 1,
        }
    }
}
```

### 13.2 Parallel Pane Processing

```rust
use futures::stream::{self, StreamExt};
use tokio::sync::Semaphore;

pub struct ParallelProcessor {
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
}

impl ParallelProcessor {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
        }
    }

    pub async fn process_panes<F, Fut>(
        &self,
        panes: Vec<PaneInfo>,
        processor: F,
    ) -> Vec<Result<ProcessResult>>
    where
        F: Fn(PaneInfo) -> Fut + Send + Sync + Clone,
        Fut: Future<Output = Result<ProcessResult>> + Send,
    {
        stream::iter(panes)
            .map(|pane| {
                let sem = self.semaphore.clone();
                let proc = processor.clone();
                async move {
                    let _permit = sem.acquire().await.unwrap();
                    proc(pane).await
                }
            })
            .buffer_unordered(self.max_concurrent)
            .collect()
            .await
    }
}
```

### 13.3 Memory-Efficient Output Cache

### 13.4 Benchmarking Requirements

We need micro-benchmarks for:
- **Overlap/delta extraction**: Ensure correctness across wrap cases
- **Pattern engine quick reject**: < 1μs for typical non-matching text
- **FTS query latency**: < 50ms for common queries on representative DB size (100k captures)
- **Watcher loop overhead**: < 100μs per pane check when idle

```rust
#[bench]
fn bench_quick_reject_no_match(b: &mut Bencher) {
    let engine = PatternEngine::new();
    let boring = "ls -la\ndrwxr-xr-x 2 user user 4096 Jan 18 12:00 .\n".repeat(100);
    b.iter(|| {
        black_box(engine.detect(&boring));
    });
}
```

```rust
use lru::LruCache;
use std::num::NonZeroUsize;

pub struct OutputCache {
    /// Hash -> timestamp of last seen
    hash_cache: LruCache<u64, Instant>,
    /// Pane -> rolling hash of recent content
    rolling_hashes: HashMap<u64, RollingHash>,
}

struct RollingHash {
    hash: u64,
    line_count: usize,
    last_updated: Instant,
}

impl OutputCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            hash_cache: LruCache::new(NonZeroUsize::new(capacity).unwrap()),
            rolling_hashes: HashMap::new(),
        }
    }

    /// Check if content is new (returns true if we should process it)
    pub fn is_new(&mut self, pane_id: u64, content: &str) -> bool {
        let hash = fxhash::hash64(content);

        // Check LRU cache
        if self.hash_cache.contains(&hash) {
            return false;
        }

        // Check rolling hash
        if let Some(rolling) = self.rolling_hashes.get(&pane_id) {
            if rolling.hash == hash {
                return false;
            }
        }

        // It's new - update caches
        self.hash_cache.put(hash, Instant::now());
        self.rolling_hashes.insert(pane_id, RollingHash {
            hash,
            line_count: content.lines().count(),
            last_updated: Instant::now(),
        });

        true
    }

    /// Prune old entries
    pub fn prune(&mut self, max_age: Duration) {
        let now = Instant::now();
        self.rolling_hashes.retain(|_, v| now.duration_since(v.last_updated) < max_age);
    }
}
```

---

## 14. Safety & Reliability

### 14.1 "Do No Harm" Principles

`ft` must NEVER:
- Spam input into the wrong pane
- Type into alt-screen apps (vim/less) unless explicitly enabled
- Inject destructive commands via automation
- Expose credentials in logs or output

### 14.2 Command Safety Gate

Before sending text that looks like a command:
- Run it through a local "denylist/allowlist" gate
- Optionally integrate with `dcg`-style rules if installed
- Require explicit operator enablement for any "dangerous class" sends

### 14.3 Policy Engine (Capability-Based Validation)

The old approach (`text.contains("rm -rf")`) is both too weak (misses variants) and too strong (false positives in docs). A proper policy engine:

1. **Classifies actions** by kind (SendText, Spawn, BrowserAuth, etc.)
2. **Requires pane capabilities** (PromptActive, !AltScreen)
3. **Supports approval rules** (allowlist + require-approval for destructive actions)
4. **Redacts secrets** in audit logs

```rust
/// Action types with different safety requirements
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    SendText,        // Inject text into pane
    Spawn,           // Create new pane
    Split,           // Split existing pane
    Activate,        // Focus a pane
    BrowserAuth,     // Trigger browser automation
    WriteFile,       // Write to filesystem
    ExecuteWorkflow, // Run a named workflow
}

/// Policy rule evaluation result
#[derive(Debug)]
pub enum PolicyDecision {
    Allow,
    Deny(String),
    RequireApproval(String),
}

pub struct PolicyEngine {
    rate: RateLimiter,
    rules: Vec<PolicyRule>,
    redactor: Redactor,
}

impl PolicyEngine {
    /// Authorize an action against current pane state.
    /// Returns Ok(()) if allowed, Err with reason if denied.
    pub fn authorize(
        &mut self,
        action: ActionKind,
        pane: &Pane,
        text: Option<&str>,
    ) -> Result<()> {
        // 1. Rate limiting (per-pane, per-action-kind)
        self.rate.check(pane.id, action)?;

        // 2. Hard capability gates (deterministic, no heuristics)
        self.check_pane_capabilities(action, pane)?;

        // 3. Rule evaluation (allow/deny/require-approval)
        for rule in &self.rules {
            match rule.evaluate(action, pane, text) {
                PolicyDecision::Allow => continue,
                PolicyDecision::Deny(reason) => {
                    return Err(anyhow::anyhow!("Policy denied: {}", reason));
                }
                PolicyDecision::RequireApproval(reason) => {
                    // In robot mode, this is an error; in interactive mode, prompt user
                    return Err(anyhow::anyhow!("Requires approval: {}", reason));
                }
            }
        }

        Ok(())
    }

    fn check_pane_capabilities(&self, action: ActionKind, pane: &Pane) -> Result<()> {
        match action {
            ActionKind::SendText => {
                // SendText requires prompt active or agent waiting
                match &pane.state {
                    PaneState::PromptActive { .. } | PaneState::AgentWaiting { .. } => Ok(()),
                    PaneState::AltScreen => {
                        Err(anyhow::anyhow!("SendText blocked: pane is in alt-screen mode (vim/less)"))
                    }
                    PaneState::CommandRunning { .. } => {
                        Err(anyhow::anyhow!("SendText blocked: command is running"))
                    }
                    PaneState::OutputGap { .. } => {
                        Err(anyhow::anyhow!("SendText blocked: output gap detected, state uncertain"))
                    }
                    other => {
                        Err(anyhow::anyhow!("SendText blocked: pane state is {:?}", other))
                    }
                }
            }
            ActionKind::BrowserAuth => {
                // BrowserAuth is high-risk; require explicit pane state
                if matches!(pane.state, PaneState::UsageLimitReached { .. }) {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "BrowserAuth only allowed when pane is in UsageLimitReached state"
                    ))
                }
            }
            _ => Ok(()), // Other actions have fewer restrictions
        }
    }

    /// Redact secrets before logging
    pub fn redact_for_audit(&self, s: &str) -> String {
        self.redactor.redact(s)
    }
}

/// Secret redactor for audit logs
pub struct Redactor {
    patterns: Vec<Regex>,
}

impl Redactor {
    pub fn new() -> Self {
        Self {
            patterns: vec![
                Regex::new(r"(?i)(api[_-]?key|token|secret|password|credential)[=:]\s*\S+").unwrap(),
                Regex::new(r"sk-[a-zA-Z0-9]{20,}").unwrap(),  // OpenAI-style
                Regex::new(r"ghp_[a-zA-Z0-9]{36}").unwrap(),   // GitHub PAT
            ],
        }
    }

    pub fn redact(&self, s: &str) -> String {
        let mut result = s.to_string();
        for pat in &self.patterns {
            result = pat.replace_all(&result, "[REDACTED]").to_string();
        }
        result
    }
}
```

**Key improvements over string matching:**
- **Deterministic**: Uses actual pane state (via OSC 133), not string heuristics
- **Capability-based**: Actions require specific pane states, not pattern matching
- **Extensible**: Rules can be loaded from config, not hardcoded
- **Audit-safe**: Secrets are automatically redacted in logs

### 14.4 Graceful Degradation

```rust
pub struct ResilientClient {
    client: WeztermClient,
    retry_config: RetryConfig,
}

impl ResilientClient {
    pub async fn send_text_with_retry(
        &self,
        pane_id: u64,
        text: &str,
    ) -> Result<()> {
        let mut attempts = 0;
        let mut last_error = None;

        while attempts < self.retry_config.max_attempts {
            match self.client.send_text(pane_id, text, false).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    attempts += 1;
                    last_error = Some(e);

                    if attempts < self.retry_config.max_attempts {
                        let delay = self.retry_config.base_delay * 2u32.pow(attempts - 1);
                        tracing::warn!(
                            "Send failed (attempt {}), retrying in {:?}: {}",
                            attempts,
                            delay,
                            last_error.as_ref().unwrap()
                        );
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.unwrap())
    }

    pub async fn get_text_with_fallback(
        &self,
        pane_id: u64,
    ) -> Result<String> {
        // Try normal get
        match self.client.get_text(pane_id, false).await {
            Ok(text) => return Ok(text),
            Err(e) => {
                tracing::warn!("get_text failed, trying fallback: {}", e);
            }
        }

        // Fallback: try with different options
        // Maybe the pane changed IDs?
        let panes = self.client.list_panes().await?;

        // Find pane with similar properties
        // ...

        Err(anyhow::anyhow!("Could not get text from pane {}", pane_id))
    }
}
```

### 14.5 Audit Trail

Every action is recorded:
- **Who initiated**: Human CLI / Robot / MCP / Workflow
- **Pane and domain**: Target context
- **Preconditions observed**: What state was verified
- **Exact bytes sent**: Or redacted version if sensitive
- **Verification result**: What happened after

### 14.6 Audit Logging

```rust
use tracing::{info, warn, error, span, Level};

pub fn log_action(
    action: &str,
    pane_id: u64,
    input: Option<&str>,
    result: &Result<String>,
) {
    let span = span!(Level::INFO, "wa_action", action = action, pane_id = pane_id);
    let _guard = span.enter();

    match result {
        Ok(msg) => {
            info!(
                target: "wa::audit",
                input = input,
                result = %msg,
                "Action completed successfully"
            );
        }
        Err(e) => {
            error!(
                target: "wa::audit",
                input = input,
                error = %e,
                "Action failed"
            );
        }
    }
}
```

---

## 15. WezTerm Vendoring Strategy

### 15.1 Analysis & Recommendation

**Key Question**: Should we vendor WezTerm to go beyond public APIs?

**Analysis**:

| Approach | Pros | Cons |
|----------|------|------|
| **CLI Only** | Simple, stable, version-independent | Limited capabilities, subprocess overhead |
| **Lua IPC** | Event-driven, rich data | Requires user config modification |
| **Full Vendor** | Maximum capability, zero-copy | Maintenance burden, version tracking |
| **Selective Vendor** | Best of both worlds | Careful design required |

**Recommendation**: **Selective Vendoring with Transformation Layer**

We vendor specific WezTerm crates as dependencies, not the whole project:

```toml
[dependencies]
# Core WezTerm protocol types
wezterm-mux-proto = { git = "https://github.com/wez/wezterm", optional = true }
wezterm-term = { git = "https://github.com/wez/wezterm", optional = true }

[features]
default = []
vendored = ["wezterm-mux-proto", "wezterm-term"]
```

### 15.2 Selective Vendoring Implementation

```rust
#[cfg(feature = "vendored")]
mod vendored {
    use wezterm_mux_proto::*;
    use wezterm_term::*;

    /// Direct connection to mux server
    pub struct DirectMuxClient {
        stream: UnixStream,
    }

    impl DirectMuxClient {
        pub async fn connect(socket_path: &Path) -> Result<Self> {
            let stream = UnixStream::connect(socket_path).await?;
            Ok(Self { stream })
        }

        /// Get scrollback directly (zero-copy)
        pub async fn get_scrollback(&mut self, pane_id: u64) -> Result<&[Line]> {
            // Send GetPaneScrollback request
            // Parse response
            // Return reference to buffer
        }

        /// Subscribe to pane output events
        pub async fn subscribe_output(&mut self, pane_id: u64) -> Result<OutputStream> {
            // Set up event subscription
        }
    }
}

/// Unified client that uses best available method
pub struct UnifiedClient {
    #[cfg(feature = "vendored")]
    direct: Option<DirectMuxClient>,
    cli: WeztermCliClient,
}

impl UnifiedClient {
    pub async fn get_text(&self, pane_id: u64) -> Result<String> {
        #[cfg(feature = "vendored")]
        if let Some(ref direct) = self.direct {
            // Use direct access for better performance
            return direct.get_scrollback(pane_id)
                .map(|lines| lines.iter().map(|l| l.as_str()).collect());
        }

        // Fallback to CLI
        self.cli.get_text(pane_id, false).await
    }
}
```

### 15.3 Version Tracking

```rust
/// Check WezTerm version compatibility
pub fn check_version_compatibility() -> Result<VersionStatus> {
    let local_version = get_local_wezterm_version()?;
    let vendored_version = env!("WEZTERM_VENDORED_VERSION");

    if local_version == vendored_version {
        Ok(VersionStatus::Matched)
    } else if is_compatible(&local_version, vendored_version) {
        Ok(VersionStatus::Compatible {
            local: local_version,
            vendored: vendored_version.to_string(),
        })
    } else {
        Ok(VersionStatus::Incompatible {
            local: local_version,
            vendored: vendored_version.to_string(),
            recommendation: format!(
                "Update WezTerm to {} or rebuild wa with --features vendored",
                vendored_version
            ),
        })
    }
}
```

---

## 16. Implementation Phases

### Phase 1: Foundation (Weeks 1-2)
- [ ] Project setup with Cargo workspace
- [ ] Core compatibility-backend CLI client wrapper (current bridge: WezTerm)
- [ ] SQLite storage with FTS5
- [ ] Basic pattern engine (Claude Code, Codex)
- [ ] Watcher daemon with adaptive polling
- [ ] Basic robot mode (state, get-text, send)

### Phase 2: Workflows (Weeks 3-4)
- [ ] Workflow engine architecture
- [ ] Handle usage limits workflow (Codex first)
- [ ] Handle compaction workflow
- [ ] Browser automation skeleton
- [ ] Integration with caut for usage tracking

### Phase 3: Full Agent Support (Weeks 5-6)
- [ ] Complete Codex patterns and workflows
- [ ] Complete Claude Code patterns and workflows
- [ ] Complete Gemini patterns and workflows
- [ ] Full browser automation for all services
- [ ] Account rotation logic

### Phase 4: Polish & Integration (Weeks 7-8)
- [ ] MCP server with fastmcp_rust
- [ ] rich_rust for CLI output
- [ ] Setup automation (SSH config parsing, remote setup)
- [ ] CASS integration
- [ ] Comprehensive testing

### Phase 5: Advanced Features (Weeks 9-10)
- [ ] Selective WezTerm vendoring
- [ ] Real-time output streaming
- [ ] Multi-agent coordination features
- [ ] Performance optimization pass
- [ ] Documentation and skills.md

---

## 17. Project Structure

```
frankenterm/
├── Cargo.toml                    # Workspace manifest
├── crates/
│   ├── frankenterm/              # CLI binary (ft)
│   │   └── src/
│   │       ├── main.rs
│   │       └── mcp.rs            # MCP wiring (feature-gated)
│   └── frankenterm-core/         # Core library (capture/storage/patterns/workflows/policy)
│       └── src/
│           ├── config.rs
│           ├── ingest.rs
│           ├── patterns.rs
│           ├── workflows.rs
│           ├── policy.rs
│           ├── storage.rs
│           └── wezterm.rs
├── docs/                         # Architecture + operator docs
├── fixtures/                     # Test fixtures (E2E + corpus)
└── fuzz/                         # Fuzz targets
```

---

## 18. Observability & Diagnostics

"99.99% uptime" is not a property you wish into existence — you need a diagnostic surface.

### 18.1 Health Model

`ft status --health --format json` reports:

```json
{
  "wezterm": {
    "cli_ok": true,
    "mux_ok": true,
    "vendored_ok": false
  },
  "ingest": {
    "lag_max_ms": 142,
    "lag_avg_ms": 23,
    "panes_tracked": 47
  },
  "event_bus": {
    "delta_queue_depth": 12,
    "detection_queue_depth": 3
  },
  "storage": {
    "db_writable": true,
    "wal_pages": 1024,
    "last_checkpoint": "2026-01-18T10:00:00Z"
  },
  "workflows": {
    "active": 2,
    "oldest_running_ms": 5400
  }
}
```

### 18.2 Metrics

Expose optional Prometheus endpoint (`ft watch --metrics :9464`):

```
# Ingest metrics (note: metric name prefix is currently legacy `wa_`)
wa_ingest_deltas_total{domain="local",agent="codex"}
wa_ingest_gap_total{domain="dev-server"}
wa_ingest_lag_ms_bucket{le="10"}
wa_ingest_lag_ms_bucket{le="50"}
wa_ingest_lag_ms_bucket{le="100"}

# Pattern detection
wa_pattern_detections_total{pattern_id="codex_usage_limit",severity="warning"}
wa_pattern_engine_latency_ms_bucket{le="1"}
wa_pattern_engine_latency_ms_bucket{le="5"}

# Workflows
wa_workflow_runs_total{workflow="handle_usage_limits",status="completed"}
wa_workflow_step_latency_ms_bucket{workflow="handle_compaction",step="send_refresh"}

# Storage
wa_db_write_latency_ms_bucket{table="output_segments"}
wa_db_segments_total
wa_db_gaps_total

# Event bus
wa_queue_depth{queue="delta"}
wa_queue_depth{queue="detection"}
```

### 18.3 Diagnostic Bundle

`ft diag bundle --last 15m` produces a single tarball containing:

- **Config (redacted)**: ft.toml with secrets replaced
- **Version matrix**: wezterm version, ft version, enabled features, Rust version
- **Recent events**: Last N events + workflow logs
- **Gap summary**: List of output gaps with timestamps and reasons
- **Health snapshot**: Current health status
- **Optional output excerpts**: Anonymized samples around detections (opt-in)

```bash
$ ft diag bundle --last 15m --output /tmp/ft-diag.tar.gz
Created diagnostic bundle: /tmp/ft-diag.tar.gz (124 KB)
Contents:
  - config.toml.redacted
  - versions.json
  - events.jsonl (47 events)
  - workflows.jsonl (3 executions)
  - gaps.jsonl (2 gaps)
  - health.json
```

---

## 19. Unified Configuration

### 19.1 Config File: `ft.toml`

Primary config file: `~/.config/ft/ft.toml`

```toml
# ft.toml - Unified configuration for FrankenTerm

[general]
log_level = "info"
data_dir = "~/.local/share/ft"

[ingest]
poll_interval_ms = 200
backpressure_limit = 4096
gap_detection = true

[storage]
db_path = "~/.local/share/ft/ft.db"
retention_days = 30
checkpoint_interval_ms = 60000

[patterns]
# Pattern packs in priority order (later overrides earlier)
packs = ["builtin:core", "builtin:codex", "builtin:claude_code", "~/.config/ft/packs/custom.toml"]

[patterns.state_gating]
enabled = true
require_agent_match = true

[workflows]
enabled = ["handle_compaction", "handle_usage_limits"]
max_concurrent = 3

[workflows.handle_usage_limits]
enabled = true
# Default: pause + schedule resume. Set failover_profile to enable immediate rotation.
failover_profile = null  # or "backup_account"
pause_on_limit = true

[safety]
rate_limit_per_pane = 30
require_prompt_active = true
audit_redaction = true

[metrics]
enabled = false
bind = "127.0.0.1:9464"
```

### 19.2 Config Commands

```bash
ft config init                    # Create default config
ft config validate                # Schema + semantic checks
ft config show --effective        # Show config with env/CLI overrides applied
ft config set storage.retention_days 60  # Update single value
ft watch --reload-config          # Hot reload for non-destructive settings
```

### 19.3 Environment Overrides

Environment variables override config file values:

```bash
FT_LOG_LEVEL=debug ft watch
FT_STORAGE_DB_PATH=/tmp/test.db ft query "error"
FT_METRICS_ENABLED=true ft watch
```

### 19.4 WezTerm Config Patching (Idempotent)

Generated snippets are wrapped in markers for safe re-running:

```lua
-- ~/.wezterm.lua

-- FT-BEGIN (do not edit manually)
local ft_config = {
  domains = { ... },
  hooks = { ... },
}
-- FT-END

-- Your custom config continues here...
```

`ft setup patch` can safely re-run and update-in-place without duplicating hooks.

---

## 20. Optional Distributed Mode

### 20.1 Deployment Modes

**Mode A (Single-node)**: `ft` runs on the user workstation and connects to all WezTerm domains via CLI/mux.

**Mode B (Distributed)**: Lightweight `ft-agent` runs near each remote mux server.

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│   Workstation   │     │   dev-server    │     │    staging      │
│                 │     │                 │     │                 │
│  ft-aggregator  │◄────│    ft-agent     │     │    ft-agent     │
│  (central)      │     │   (local tail)  │     │   (local tail)  │
│                 │◄────┼─────────────────┼─────┤                 │
└─────────────────┘     └─────────────────┘     └─────────────────┘
```

### 20.2 Agent Mode

The `ft-agent` runs close to the mux server and streams:

- `PaneDelta` segments (small deltas, not full pane content)
- `Detection` events (pattern matches)
- `Gap` events (discontinuities)
- Minimal pane metadata

```bash
# On remote server
ft-agent --mux-socket /tmp/wezterm-mux --upstream ws://workstation:9465
```

### 20.3 Aggregator Mode

The workstation runs `ft` in aggregator mode:

```bash
ft watch --aggregate --bind 0.0.0.0:9465
```

This:
- Merges per-domain streams
- Persists segments/events to local SQLite
- Runs workflows (or delegates back to agents when pane-local actions needed)
- Serves the unified CLI/MCP interface

### 20.4 Benefits

- **Lower latency**: Agents capture locally, no round-trip for every poll
- **Fewer bytes**: Only deltas travel over the wire, not full pane content
- **Isolation**: One remote domain failing doesn't stall all ingestion
- **Fleet-ready**: Path to true multi-machine orchestration

---

## 21. Key Design Decisions

### 21.1 Why SQLite over Other Databases?

| Alternative | Reason Against |
|-------------|----------------|
| **PostgreSQL/MySQL** | Overkill for single-machine use; deployment complexity |
| **Redis** | No persistence guarantees; no FTS |
| **File-based** | Poor query performance; no FTS |
| **In-memory only** | Loses history on restart |

SQLite provides:
- Zero-config deployment
- Excellent FTS5 for search
- WAL mode for concurrent access
- Single-file portability

### 21.2 Why Aho-Corasick + Regex?

- **Aho-Corasick**: O(n) for matching multiple patterns simultaneously
- **Quick Reject**: Eliminates 99%+ of text before regex
- **Regex for extraction**: Named captures for structured data
- **SIMD acceleration**: Via regex crate's DFA

### 21.3 Why Not Direct WezTerm Modification?

- **Maintenance burden**: Staying in sync with upstream
- **User friction**: Would require custom WezTerm build
- **API stability**: CLI is stable; internals change

Selective vendoring gives us the best of both worlds when needed.

### 21.4 Why MCP + CLI?

- **MCP**: Optimal for AI agent integration (structured tools)
- **CLI**: Essential for shell scripts, human debugging, composability
- Both share the same core logic

### 21.5 Schema Source of Truth + Client Generation Strategy

**Decision:** Rust structs are the single source of truth. JSON Schema files are generated
from Rust and committed under `docs/json-schema/` for downstream consumers.

**Rationale:**
- Rust types already drive behavior and validation; duplicating schema logic invites drift.
- Generated schemas keep machine contracts in sync with code.
- Committed artifacts are easy to consume without Rust tooling.

**Client targets (v0.1):**
- **TypeScript**: Primary external consumer (tooling and agent integrations).
- **Rust**: Native use via the same structs (no extra generation needed).
- **Python**: Defer to Phase 2 unless a concrete integration requires it.

**Versioning policy:**
- Embed `schema_version` in the robot/MCP envelope.
- **Additive-only changes** keep the same `schema_version`.
- **Breaking changes** require a `schema_version` bump and a `ft` major version bump.
- CI will regenerate schemas and fail if committed artifacts drift.

---

## 22. Open Questions

Explicit questions to resolve early in implementation:

1. **Scrollback limits**: What default `scrollback_lines` do we standardize on to guarantee capture without bloat? (Recommendation: 50k lines)

2. **Claude session identity**: What is the best stable "session key" to correlate with `cass`? (Working hypothesis: use cwd + start time)

3. **Auth realities**: Which services can be fully automated vs require "one-time human bootstrap"? (Need to test with real accounts)

4. **Data volume**: Do we need compression on day 1, or is retention enough? (Start with retention; add compression if DB exceeds 1GB)

5. **Vendoring ROI**: What concrete capability is blocked by CLI+Lua that justifies vendoring? (Currently: real-time output streaming)

6. **Multi-machine coordination**: When do we need cross-machine `ft` instances to communicate? (Phase 2 scope)

7. **Agent-specific quirks**: What undocumented behaviors do Claude Code, Codex, and Gemini have that affect detection? (Need field testing)

---

## 23. Definition of Done

`ft` is "meaningfully real" when:

- [ ] It continuously captures pane output and can search it with FTS
- [ ] It detects at least the primary patterns reliably (compaction, usage limits for all 3 agents)
- [ ] It can run `handle_compaction` safely end-to-end
- [ ] It can run `handle_usage_limits` for Codex end-to-end at least once in the real world
- [ ] It exposes robot mode + MCP so an agent can operate it without the UI
- [ ] It has documentation sufficient for another developer to contribute
- [ ] It passes CI (lint, test, build) on every commit

**Shipping Milestone**: All checkboxes above = v0.1.0 release candidate.

---

## 24. Appendices

### Appendix A: CLI / Robot / MCP Surface

#### A.1 Top-Level CLI Commands

Human-oriented (rich output by default via `rich_rust`):

| Command | Description |
|---------|-------------|
| `ft status` | Panes + inferred agent state |
| `ft watch` | Run daemon (foreground or background) |
| `ft events` | Recent / unhandled events |
| `ft search` (alias: `ft query`) | Full text search (FTS5) |
| `ft send` | Send text / control codes to a pane (guarded) |
| `ft workflow` | Run a workflow manually (guarded) |
| `ft rules` | List/test rule packs, show matching traces |
| `ft accounts` | List/refresh accounts, show rotation picks |
| `ft setup` | Local/remote canonical backend-bridge configuration (current: WezTerm) |
| `ft doctor` | Environment checks (wezterm presence, version parity, DB health) |
| `ft export` | Export slices of history (JSONL/NDJSON), DB snapshot, or reports |
| `ft web` | Start HTTP server (`fastapi_rust`) if enabled |
| `ft tui` | Start interactive UI (`charmed_rust`) if enabled |
| `ft sync` | Sync configs/binaries/db snapshots (`asupersync`) if enabled |

Agent-oriented (stable schemas, token-efficient):

| Command | Description |
|---------|-------------|
| `ft robot ...` | Canonical agent interface |

#### A.2 Robot Mode Commands

```
ft robot state [--domain <name>] [--agent <type>]
ft robot get-text <pane_id> [--tail N] [--escapes]
ft robot send <pane_id> "<text>" [--no-newline] [--wait-for "<pat>"] [--timeout-secs N]
ft robot wait-for <pane_id> "<pat>" [--timeout-secs N]
ft robot search "<fts query>" [--pane-id <id>] [--since <iso8601>] [--limit N]
ft robot events [--unhandled] [--pane-id <id>] [--type <event>] [--limit N]
ft robot workflow <name> <pane_id> [--force]
ft robot accounts [--service <openai|anthropic|google>]
ft robot rules list [--pack <name>]
ft robot rules test "<text>" [--agent <type>]
ft robot quick-start
```

#### A.3 MCP Tools/Resources (via `fastmcp_rust`)

MCP mirrors robot mode: tools use `wa.*`, resources use `wa://...`. The goal is
token-efficient parity with `ft robot` and stable, versioned schemas.

Tool naming (short and obvious):

| Tool | Description |
|------|-------------|
| `wa.state` | Get current pane states |
| `wa.get_text` | Get text from pane |
| `wa.send` | Send text to pane |
| `wa.wait_for` | Wait for pattern |
| `wa.search` | FTS search |
| `wa.events` | Get events |
| `wa.workflow_run` | Execute workflow |
| `wa.accounts` | List accounts |
| `wa.accounts_refresh` | Refresh account usage |
| `wa.rules_list` | List detection rules |
| `wa.rules_test` | Test pattern matching |
| `wa.reservations` | List active reservations |
| `wa.reserve` | Create a reservation |
| `wa.release` | Release a reservation |

##### A.3.1 Response envelope (v1)

All MCP tools return the same envelope shape:

```json
{
  "ok": true,
  "data": { ... },
  "error": "human readable message (when ok=false)",
  "error_code": "WA-MCP-0001",
  "hint": "optional remediation hint",
  "elapsed_ms": 12,
  "version": "0.1.0",
  "now": 1700000000000,
  "mcp_version": "v1"
}
```

Notes:
- `data` matches the corresponding robot `data` schema under `docs/json-schema/`.
- `version` is the ft semver; `mcp_version` is the MCP surface version.
- Fields are stable; additions are backward-compatible only.

##### A.3.2 Error codes (stable)

All MCP errors use stable codes prefixed with `WA-MCP-`:

| Error code | Meaning | Robot equivalent |
|------------|---------|------------------|
| `WA-MCP-0001` | Invalid arguments | `robot.invalid_args` |
| `WA-MCP-0002` | Unknown tool/resource | `robot.unknown_subcommand` |
| `WA-MCP-0003` | Config error | `robot.config_error` |
| `WA-MCP-0004` | Backend bridge CLI error (current: WezTerm) | `robot.wezterm_error` |
| `WA-MCP-0005` | Storage error | `robot.storage_error` |
| `WA-MCP-0006` | Policy denied | `robot.policy_denied` |
| `WA-MCP-0007` | Pane not found | `robot.pane_not_found` |
| `WA-MCP-0008` | Workflow error | `robot.workflow_error` |
| `WA-MCP-0009` | Timeout | `robot.timeout` |
| `WA-MCP-0010` | Not implemented | `robot.not_implemented` |

##### A.3.3 Tool specs (v1)

Each tool maps 1:1 to `ft robot` with the same data schema:

- `wa.state`
  - Params: `{ domain?: string, agent?: string, pane_id?: u64 }`
  - Data: `docs/json-schema/wa-robot-state.json`

- `wa.get_text`
  - Params: `{ pane_id: u64, tail?: u64=50, escapes?: bool=false }`
  - Data: `docs/json-schema/wa-robot-get-text.json`

- `wa.send`
  - Params: `{ pane_id: u64, text: string, dry_run?: bool=false, wait_for?: string, timeout_secs?: u64=30, wait_for_regex?: bool=false }`
  - Data: `docs/json-schema/wa-robot-send.json`

- `wa.wait_for`
  - Params: `{ pane_id: u64, pattern: string, timeout_secs?: u64=30, tail?: u64=200, regex?: bool=false }`
  - Data: `docs/json-schema/wa-robot-wait-for.json`

- `wa.search`
  - Params: `{ query: string, limit?: u64=20, pane?: u64, since?: i64, snippets?: bool=false }`
  - Data: `docs/json-schema/wa-robot-search.json`

- `wa.events`
  - Params: `{ limit?: u64=20, pane?: u64, rule_id?: string, event_type?: string, unhandled?: bool=false, since?: i64, would_handle?: bool=false, dry_run?: bool=false }`
  - Data: `docs/json-schema/wa-robot-events.json`

- `wa.workflow_run`
  - Params: `{ name: string, pane_id: u64, force?: bool=false, dry_run?: bool=false }`
  - Data: `docs/json-schema/wa-robot-workflow-run.json`

- `wa.accounts`
  - Params: `{ service?: string }`
  - Data: `docs/json-schema/wa-robot-accounts.json`

- `wa.accounts_refresh`
  - Params: `{ service?: string }`
  - Data: `docs/json-schema/wa-robot-accounts-refresh.json`

- `wa.rules_list`
  - Params: `{ pack?: string }`
  - Data: `docs/json-schema/wa-robot-rules-list.json`

- `wa.rules_test`
  - Params: `{ text: string, agent?: string }`
  - Data: `docs/json-schema/wa-robot-rules-test.json`

- `wa.reservations`
  - Params: `{ pane_id?: u64 }`
  - Data: `docs/json-schema/wa-robot-reservations.json`

- `wa.reserve`
  - Params: `{ pane_id: u64, owner?: string, ttl_secs?: u64 }`
  - Data: `docs/json-schema/wa-robot-reserve.json`

- `wa.release`
  - Params: `{ reservation_id: string }`
  - Data: `docs/json-schema/wa-robot-release.json`

Resources:
- `wa://panes` — Current pane registry
- `wa://events` — Event feed
- `wa://accounts` — Account status
- `wa://workflows` — Available workflows
- `wa://rules` — Pattern rules
- `wa://reservations` — Active reservations snapshot

Resource semantics (v1):
- Read-only snapshots, no side effects.
- Default parameters mirror the tool defaults (e.g., `wa://events?limit=20`).
- Payloads mirror the corresponding tool `data` schemas for parity.

---

### Appendix B: SQLite Schema

See Section 5.2 for the complete schema. Key tables:

- `domains` — Known WezTerm domains
- `panes` — Current + historical pane registry
- `captures` — Append-only output chunks (delta-first)
- `captures_fts` — FTS5 virtual table
- `events` — Detected events with extracted JSON
- `agent_sessions` — Per-agent session timeline
- `workflow_runs` — Workflow executions + step tracing
- `accounts` — Service accounts + usage metadata
- `config` — Persisted settings

---

### Appendix C: Initial Rule Packs

#### C.1 Codex (`core.codex`)

| rule_id | anchors | extraction (named) | event_type |
|---------|---------|-------------------|------------|
| `codex.usage.warning_25` | `less than 25%` | `remaining`, `limit_hours` | `usage.warning` |
| `codex.usage.warning_10` | `less than 10%` | `remaining`, `limit_hours` | `usage.warning` |
| `codex.usage.warning_5` | `less than 5%` | `remaining`, `limit_hours` | `usage.warning` |
| `codex.usage.reached` | `You've hit your usage limit` | `try_again_at` | `usage.reached` |
| `codex.session.token_usage` | `Token usage:` | `total`, `input`, `cached`, `output`, `reasoning` | `session.summary` |
| `codex.session.resume_hint` | `codex resume` | `session_id` | `session.resume_hint` |
| `codex.auth.device_code_prompt` | `Enter this one-time code` | `code` | `auth.device_code` |

#### C.2 Claude Code (`core.claude_code`)

| rule_id | anchors | extraction | event_type |
|---------|---------|------------|------------|
| `claude.compaction` | `Conversation compacted` | none | `session.compaction` |
| `claude.banner` | `Claude Code v` | `version`, `model` | `session.start` |
| `claude.usage.warning` | (evolves) | (evolves) | `usage.warning` |
| `claude.usage.reached` | (evolves) | (evolves) | `usage.reached` |

#### C.3 Gemini (`core.gemini`)

| rule_id | anchors | extraction | event_type |
|---------|---------|------------|------------|
| `gemini.usage.reached` | `Usage limit reached for all Pro models` | none | `usage.reached` |
| `gemini.session.summary` | `Interaction Summary` | `session_id` | `session.summary` |
| `gemini.model.used` | `Responding with gemini-` | `model` | `session.model` |

---

### Appendix D: Workflow Specs

#### D.1 `handle_compaction` (All Agents)

**Trigger**: event `session.compaction` detected

**Steps**:
1. Acquire per-pane workflow lock
2. Re-read current pane tail (guard against false positives)
3. Guard: ensure trigger anchor is still present within last N lines
4. Wait stabilization window (2s) OR require explicit "compaction complete" marker
5. Send agent-specific prompt:
   - Claude Code: `"Reread AGENTS.md so it's still fresh in your mind.\n"`
   - Codex: `"Please re-read AGENTS.md and any key project context files.\n"`
   - Gemini: `"Please re-examine AGENTS.md and project context.\n"`
6. Verify: wait for agent prompt echo / UI marker
7. Mark event handled + record workflow result

**Failure Modes**:
- Pane disappeared → mark workflow cancelled
- Pane alt-screen detected → abort (unless explicitly allowed)

#### D.2 `handle_usage_limits` (Codex Path)

**Preconditions**:
- inferred agent = `codex`
- trigger event `usage.reached`
- **before every send**: pane is not `AltScreen`, no recent `OutputGap`, and `PolicyEngine` authorizes `SendText` / `BrowserAuth`

**Steps** (each step is **Observe → Act → Verify**):
1. Acquire per-pane workflow lock.
2. Exit Codex cleanly (avoid fixed sleeps):
   - Act: send Ctrl-C once (`\u{3}`).
   - Verify: wait for session summary + resume hint markers.
   - If not seen within a short grace window: Act: send Ctrl-C again.
   - Verify: continue waiting (bounded by overall timeout) until summary/resume markers appear.
3. Verify session summary/resume hint appears (bounded tail; timeout).
4. Parse from summary:
   - token usage stats (total/input/output/reasoning/cached where available)
   - resume session id
5. Refresh account usage (out-of-band):
   - `caut refresh --service openai --format json`
   - persist/update local accounts mirror
6. Select account:
   - highest `percent_remaining` above threshold (configurable)
   - tie-breaker: least recently used
   - if none above threshold **or failover disabled**, branch to **Safe Pause** (below)
7. Initiate device auth in the pane:
   - Act: send `cod login --device-auth\n`
   - Verify: device-code prompt appears
8. Parse device code (and URL if present) from the tail.
9. Playwright: complete device auth using the chosen account’s persistent profile.
10. Resume session:
    - Act: send `cod resume <session_id>\n`
    - Verify: “ready” marker / prompt appears
11. Continue:
    - Act: send `proceed.\n`
    - Verify: Codex begins responding (or agreed marker appears)
12. Persist:
    - mark event handled
    - store session info + account rotation record + workflow step logs

**Safe Pause (failover disabled / no eligible accounts)**:
- Do **not** attempt `cod login`.
- Persist a redacted next-step plan with:
  - try-again time (if provided by `usage.reached`)
  - resume session id (redacted/hashed)
  - recommended human actions
- Mark workflow `paused` so it does not reappear as “unhandled spam.”

**Failure Modes**:
- Device code not found → retry step 7 once; if still missing, **pause** with next-step plan.
- Playwright cannot proceed (MFA/unexpected wall) → open non-headless browser, request human completion, **pause** with recovery instructions.
- Resume fails (session id invalid / resume rejected) → surface error; **do not loop**.
- Pane becomes unsafe (AltScreen/OutputGap/CommandRunning) → abort or pause with reason.
- Policy denies send/auth → record denial and abort safely; never inject input.

---	

### Appendix E: Shell Integration Snippets

#### E.1 Minimal `wezterm.lua` Forwarding Lane

```lua
  -- Forward user-var events to ft daemon (legacy prefix `wa-` supported)
  wezterm.on('user-var-changed', function(window, pane, name, value)
    if name:match('^ft%-') or name:match('^wa%-') then
      wezterm.background_child_process {
      'ft', 'event', '--from-uservar',
      '--pane', tostring(pane:pane_id()),
      '--name', name,
      '--value', value
      }
    end
  end)
```

#### E.2 Emitting User-Vars from Shells/Agents

```bash
# Emit a user-var from within a pane
printf "\033]1337;SetUserVar=%s=%s\007" \
  ft_event \
  "$(printf '%s' '{"kind":"prompt","pane":"$WEZTERM_PANE"}' | base64)"
```

Use this to:
- Mark prompt boundaries
- Mark "agent ready"
- Mark workflow checkpoints

---

### Appendix F: Library Integration Map

| Library | Role in `ft` |
|---------|--------------|
| `cass` (`/dp/coding_agent_session_search`) | Correlation + session archaeology; used in status + workflows |
| `caut` (`/dp/coding_agent_usage_tracker`) | Usage truth + selection; used in accounts + `handle_usage_limits` |
| `rich_rust` | Human-first CLI output (tables/panels/highlight) |
| `charmed_rust` | Optional interactive TUI (pane picker, event feed, transcript viewer) |
| `fastmcp_rust` | MCP tool surface for agent control (mirrors robot mode) |
| `fastapi_rust` | Optional HTTP server for dashboards/webhooks (read-only first) |
| `asupersync` | Remote bootstrap/sync layer (configs, binaries, DB snapshots) |
| `playwright` | Automate device auth flows with persistent profiles |
| `ast-grep` | Structure-aware codebase scans in "unstick agent" workflows |

---

### Appendix G: Testing Strategy

#### G.1 Unit Tests
- Delta extraction correctness (overlap cases, wrap cases)
- Pattern rules per agent (positive + negative fixtures)
- Workflow step guards (prompt detection, alt-screen checks)
- DB schema migration tests
- Policy engine capability checks

#### G.2 Integration Tests
- Simulate `wezterm cli list/get-text` outputs via fixtures
- Run workflow engine against fake panes
- Test StorageHandle async operations

#### G.3 End-to-End Tests (when WezTerm available)
- Spin up `wezterm-mux-server` locally
- Spawn dummy panes that emit known outputs
- Verify `ft` captures and detects events
- Verify workflow actions send expected bytes and stop safely

#### G.4 Golden Corpus Regression Tests

**Directory structure**:
```
tests/corpus/
├── codex/
│   ├── usage_limit_v1.txt      # Captured pane output
│   ├── usage_limit_v1.expect.json  # Expected detections
│   ├── compaction_v1.txt
│   └── compaction_v1.expect.json
├── claude_code/
│   ├── session_end.txt
│   └── session_end.expect.json
└── gaps/
    ├── scrollback_truncation.txt
    └── scrollback_truncation.expect.json
```

**CI integration**:
```rust
#[test]
fn corpus_regression() {
    for entry in glob("tests/corpus/**/*.txt").unwrap() {
        let input = std::fs::read_to_string(&entry).unwrap();
        let expected_path = entry.with_extension("expect.json");
        let expected: Vec<Detection> = serde_json::from_str(
            &std::fs::read_to_string(&expected_path).unwrap()
        ).unwrap();

        let actual = pattern_engine.detect(&input);
        assert_eq!(actual, expected, "Corpus regression: {:?}", entry);
    }
}
```

#### G.5 Property-Based Tests (proptest)

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn segment_seq_is_monotonic(segments in prop::collection::vec(any::<OutputSegment>(), 1..100)) {
        let mut last_seq = 0;
        for seg in segments {
            storage.append_segment(seg.clone()).await.unwrap();
            assert!(seg.seq > last_seq || seg.pane_id != last_pane_id);
            last_seq = seg.seq;
        }
    }

    #[test]
    fn fts_finds_inserted_text(text in "[a-zA-Z0-9 ]{10,100}") {
        storage.append_segment(OutputSegment { content: text.clone(), .. }).await.unwrap();
        let results = storage.search(&text[..10], SearchOptions::default()).await.unwrap();
        assert!(!results.is_empty());
    }
}
```

#### G.6 Fuzzing

Use `cargo-fuzz` for:
- **Pattern pack parsing**: Malformed TOML shouldn't crash
- **Regex extractors**: Pathological regex shouldn't hang
- **OSC marker parsing**: Invalid escape sequences shouldn't panic
- **FTS queries**: Malformed queries shouldn't crash

```bash
# Setup
cargo install cargo-fuzz
mkdir fuzz/

# Fuzz targets
cargo fuzz add pattern_pack_parser
cargo fuzz add osc_marker_parser
cargo fuzz add fts_query

# Run
cargo +nightly fuzz run pattern_pack_parser -- -max_len=10000
```

#### G.7 Performance Budgets (Criterion)

```rust
use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};

fn bench_pattern_detection(c: &mut Criterion) {
    let engine = PatternEngine::new();
    let corpus = include_str!("../corpus/typical_pane_output.txt");

    c.bench_function("pattern_detection_typical", |b| {
        b.iter(|| engine.detect(black_box(corpus)))
    });

    // Enforce budget: p50 < 1ms, p99 < 5ms
    // CI fails if exceeded
}

fn bench_fts_query(c: &mut Criterion) {
    let storage = setup_test_db_with_100k_segments();

    c.bench_function("fts_query_common", |b| {
        b.iter(|| storage.search_sync("error", SearchOptions::default()))
    });

    // Budget: p50 < 10ms, p99 < 50ms
}

criterion_group!(benches, bench_pattern_detection, bench_fts_query);
criterion_main!(benches);
```

**CI enforcement**:
```yaml
# .github/workflows/bench.yml
- name: Run benchmarks
  run: cargo bench --bench perf -- --save-baseline main

- name: Check performance regression
  run: cargo bench --bench perf -- --baseline main --threshold 1.2
```

#### G.8 "Regression Packs"
Each time a real-world pattern changes (agent output format drift):
- Add failing fixture from the field
- Update rule + tests
- Ship as pack bump

---

### Appendix H: Vendoring Maintenance Plan

If we enable vendoring:

1. **Pin to commit hash**: Use git dependency in `Cargo.toml` or submodule
2. **Store vendored commit** in build metadata (so `ft doctor` can compare)
3. **Add CI job** that:
   - Builds `ft` with vendored feature
   - Runs minimal mux protocol smoke test
   - Reports if upstream changes broke compilation

**Default posture**: CLI-first; vendoring is an optimization lane, not the foundation.

---

## Summary

This plan outlines a comprehensive system for automating AI coding agent fleets through a swarm-native terminal platform with a current WezTerm compatibility bridge. The key innovations are:

1. **Deterministic Automation**: No timing heuristics; actions based on observed state
2. **Multi-Tier Integration**: CLI → Lua IPC → Vendored, with graceful fallback
3. **High-Performance Pattern Engine**: SIMD-accelerated, sub-millisecond detection
4. **Workflow State Machine**: Robust, recoverable, idempotent operations
5. **Agent-First Design**: Optimized for AI agents controlling AI agents

With this foundation, we can build the infrastructure needed for supersmart swarms of AI agents to coordinate and solve humanity's hardest problems.

---

*Plan created: 2026-01-18*
*Author: Claude Opus 4.5*
