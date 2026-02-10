# ft — FrankenTerm

<div align="center">
  <img src="ft_illustration.webp" alt="ft - Terminal Hypervisor for AI Agent Swarms">
</div>

<div align="center">

[![CI](https://github.com/Dicklesworthstone/frankenterm/actions/workflows/ci.yml/badge.svg)](https://github.com/Dicklesworthstone/frankenterm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://www.rust-lang.org/)

</div>

**The central nervous system for coordinating fleets of AI coding agents across WezTerm terminal sessions.**

<div align="center">
<h3>Quick Install</h3>

```bash
cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft
```

</div>

---

## TL;DR

**The Problem**: Running multiple AI coding agents (Claude Code, Codex CLI, Gemini CLI) across terminal panes is chaos. You can't see what they're doing, can't detect when they hit rate limits or need input, and can't coordinate their work without manual babysitting and fragile timing heuristics.

**The Solution**: `ft` transforms WezTerm into a **terminal hypervisor** — capturing all pane output in real-time, detecting agent state transitions through pattern matching, automatically executing workflows in response, and exposing a machine-optimized Robot Mode API for AI-to-AI orchestration.

### Why Use ft?

| Feature | What It Does |
|---------|--------------|
| **Perfect Observability** | Captures all terminal output across all panes with delta extraction (<50ms lag) |
| **Intelligent Detection** | Multi-agent pattern engine detects rate limits, errors, prompts, completions |
| **Event-Driven Automation** | Workflows trigger on patterns — no sleep loops or polling heuristics |
| **Robot Mode API** | JSON interface optimized for AI agents to control other AI agents |
| **Full-Text Search** | FTS5-powered search across all captured output with BM25 ranking |
| **Policy Engine** | Capability gates, rate limiting, audit trails for safe multi-agent control |

---

## Quick Example

```bash
# Start the ft watcher (observes all WezTerm panes)
$ ft watch

# See all active panes as JSON
$ ft robot state
{
  "ok": true,
  "data": {
    "panes": [
      {"pane_id": 0, "title": "claude-code", "domain": "local", "cwd": "/project"},
      {"pane_id": 1, "title": "codex", "domain": "local", "cwd": "/project"}
    ]
  }
}

# Compact TOON output (token-optimized)
$ ft robot --format toon state

# Token stats (printed to stderr so stdout stays data-only)
$ ft robot --format toon --stats state

# Get recent output from a specific pane
$ ft robot get-text 0 --tail 50

# Wait for a specific pattern (e.g., agent hitting rate limit)
$ ft robot wait-for 0 "core.codex:usage_reached" --timeout-secs 3600

# Search all captured output
$ ft robot search "error: compilation failed"

# Send input to a pane (with policy checks)
$ ft robot send 1 "/compact"

# View recent detection events
$ ft robot events --limit 10
```

---

## Design Philosophy

### 1. Passive-First Architecture

The observation loop (discovery, capture, pattern detection) has **no side effects**. It only reads and stores. The action loop (sending input, running workflows) is strictly separated with explicit policy gates.

### 2. Event-Driven, Not Time-Based

No `sleep(5)` loops hoping the agent is ready. Every wait is condition-based: wait for a pattern match, wait for pane idle, wait for an external signal. Deterministic, not probabilistic.

### 3. Delta Extraction Over Full Capture

Instead of repeatedly capturing entire scrollback buffers, `ft` uses 4KB overlap matching to extract only new content. Efficient storage, minimal latency, explicit gap markers for discontinuities.

### 4. Single-Writer Integrity

A watcher lock ensures only one watcher can write to the database. No corruption from concurrent mutations. Graceful fallback for read-only introspection.

### 5. Agent-First Interface

Robot Mode returns structured JSON with consistent schemas. Every response includes `ok`, `data`, `error`, `elapsed_ms`, and `version`. Designed for machines to parse, not humans to read.

## Safety Guarantees

- **Observe vs act split**: `ft watch` is read-only; mutating actions must pass the Policy Engine.
- **No silent gaps**: capture gaps are recorded explicitly and surfaced in events/diagnostics.
- **Policy-gated sending**: `ft send` and workflows enforce prompt/alt-screen checks, rate limits, and approvals.

## Secure Distributed Mode

Secure distributed mode is available as an optional feature-gated build and is
off by default.

```bash
# Build ft with distributed mode support
cargo build -p frankenterm --release --features distributed
```

Operator guidance:
- Keep `distributed.bind_addr` on loopback unless you explicitly need remote access.
- For non-loopback binds, enable TLS and use file/env token sources (avoid inline tokens).
- Use `ft doctor` (or `ft doctor --json`) to verify effective distributed security status.
- Follow `docs/distributed-security-spec.md` for setup, rotation, and troubleshooting.

---

## How ft Compares

| Feature | ft | tmux scripting | Manual monitoring |
|---------|-----|----------------|-------------------|
| Multi-pane capture | Full scrollback + delta | Capture-pane (snapshot) | One pane at a time |
| Pattern detection | <5ms, multi-agent | Manual grep | Human eyes |
| Event-driven waits | Built-in | Polling loops | Not possible |
| Full-text search | FTS5 with ranking | grep + manual | Not practical |
| Policy/safety | Capability gates | None | Trust |
| Robot Mode API | First-class JSON | Script parsing | N/A |

**When to use ft:**
- Running 2+ AI coding agents that need coordination
- Building automation that reacts to terminal output
- Debugging multi-agent workflows with full observability

**When ft might not be ideal:**
- Single terminal, single agent (overkill)
- Non-WezTerm terminal emulators (WezTerm-specific APIs)

---

## Installation

### From Source (Recommended)

```bash
# Clone and build
git clone https://github.com/Dicklesworthstone/frankenterm.git
cd frankenterm
cargo build --release

# Install to PATH
cp target/release/ft ~/.local/bin/
```

### Via Cargo

```bash
cargo install --git https://github.com/Dicklesworthstone/frankenterm.git ft
```

### Requirements

- **Rust 1.85+** (nightly required for Rust 2024 edition)
- **WezTerm** terminal emulator with CLI enabled
- **SQLite** (bundled via rusqlite)

---

## Quick Start

### 1. Run Setup (recommended)

```bash
# Guided setup (generates config snippets and shell hooks)
ft setup
```

### 2. Verify WezTerm CLI

```bash
# Should list your current panes
wezterm cli list
```

### 3. Start the Watcher

```bash
# Start observing all panes
ft watch

# Or run in foreground for debugging
ft -v watch --foreground
```

### 4. Check Status

```bash
# See what ft is observing
ft status

# Robot mode for JSON output
ft robot state
```

### 5. Search Captured Output

```bash
# Full-text search across all panes (alias: `ft query`)
ft search "error"
ft query "error"

# Events feed (recent detections)
ft events

# Annotate/triage events
ft events annotate 123 --note "Investigating"
ft events triage 123 --state investigating
ft events label 123 --add urgent

# Robot mode with structured results
ft robot search "compilation failed" --limit 20
```

### 6. React to Events

```bash
# Wait for an agent to hit its rate limit
ft robot wait-for 0 "core.codex:usage_reached"

# Then send a command to handle it
ft robot send 0 "/compact"
```

---

## Commands

### Watcher Management

```bash
ft watch                     # Start watcher in background
ft watch --foreground        # Run in foreground
ft watch --auto-handle       # Enable auto workflows
ft stop                      # Stop running watcher
```

### Pane Inspection

```bash
ft status                    # Overview of observed panes
ft show <pane_id>           # Detailed pane info
ft get-text <pane_id>       # Recent output from pane
```

### Pane Actions

```bash
ft send <pane_id> "<text>"                 # Send input (policy-gated)
ft send <pane_id> "<text>" --dry-run       # Preview without executing
ft send <pane_id> "<text>" --wait-for "ok" # Verify via wait-for
ft send <pane_id> "<text>" --no-paste --no-newline
```

### Search

```bash
ft search "<query>"          # Full-text search
ft search "<query>" --pane 0 # Scope to specific pane
ft search "<query>" --limit 50
```

### Explainability

```bash
ft why --list                # List available explanation templates
ft why deny.alt_screen       # Explain a common policy denial
```

### Workflows

```bash
ft workflow list                         # List available workflows
ft workflow run handle_usage_limits --pane 0
ft workflow run handle_usage_limits --pane 0 --dry-run
ft workflow status <execution_id> -v
```

### Rules

```bash
ft rules list                            # List detection rules
ft rules test "Usage limit reached"      # Test text against rules
ft rules show codex.usage_reached        # Show rule details
```

### Audit & Approvals

```bash
ft approve AB12CD34 --dry-run            # Check approval status
ft audit --limit 50 --pane 3             # Filter audit history
ft audit --decision deny                 # Only denied decisions
```

### Diagnostics

```bash
ft triage                               # Summarize issues (health/crashes/events)
ft diag bundle --output /tmp/ft-diag    # Collect diagnostic bundle
ft reproduce --kind crash               # Export latest crash bundle
```

### Robot Mode (JSON API)

Use `--format toon` for token-efficient output and `ft robot help` for the full command list.

```bash
ft robot state               # All panes as JSON
ft robot get-text <id> --tail 50      # Pane output as JSON
ft robot send <id> "<text>" # Send input (with policy)
ft robot send <id> "<text>" --dry-run  # Preview without executing
ft robot wait-for <id> <rule_id>       # Wait for pattern
ft robot search "<query>"   # Search with structured results
ft robot events             # Recent detection events
ft robot help               # List all robot commands
```

### MCP (Model Context Protocol)

```bash
# Build with MCP feature enabled
cargo build --release --features mcp

# Start MCP server over stdio
ft mcp serve
```

MCP mirrors robot mode. See `docs/mcp-api-spec.md` for the tool list and `docs/json-schema/` for response schemas.
For multi-agent operating procedures, see `docs/swarm-playbook.md`.

### Configuration

```bash
ft config show               # Display current config
ft config validate           # Check config syntax
ft config reload             # Hot-reload config (SIGHUP)
```

For the full command matrix (human + robot + MCP), see `docs/cli-reference.md`.

---

## Configuration

Configuration lives in `~/.config/ft/ft.toml`:

```toml
[general]
# Logging level: trace, debug, info, warn, error
log_level = "info"
# Output format: pretty (human) or json (machine)
log_format = "pretty"
# Data directory for database and locks
data_dir = "~/.local/share/ft"

[ingest]
# How often to poll panes for new content (milliseconds)
poll_interval_ms = 200
# Filter which panes to observe
[ingest.panes]
include = []  # Empty = all panes
exclude = ["*htop*", "*vim*"]  # Glob patterns

# Pane priority overrides (lower number = higher priority)
[ingest.priorities]
default_priority = 100

[[ingest.priorities.rules]]
id = "critical_codex"
priority = 10
title = "codex"

[[ingest.priorities.rules]]
id = "deprioritize_ssh"
priority = 200
domain = "SSH:*"

# Capture budgets (0 = unlimited)
[ingest.budgets]
max_captures_per_sec = 0
max_bytes_per_sec = 0

[storage]
# Write queue size for batched inserts
writer_queue_size = 100
# How long to retain captured output
retention_days = 30

[backup.scheduled]
# Enable scheduled backups
enabled = false
# Schedule: hourly, daily, weekly, or 5-field cron
schedule = "daily"
# Retention policy
retention_days = 30
max_backups = 10
# Optional destination root
destination = "~/.local/share/ft/backups"
# Optional tweaks
compress = false
metadata_only = false
# Notifications
notify_on_failure = true
notify_on_success = false

[sync]
# Feature gate
enabled = false
# Require confirmation for any write
require_confirmation = true
# Default overwrite policy
allow_overwrite = false
# Payload toggles (global defaults)
allow_binary = false
allow_config = true
allow_snapshots = true
# Optional allow/deny path globs
allow_paths = []
deny_paths = ["~/.local/share/ft/ft.db", "~/.local/share/ft/ft.db-wal", "~/.local/share/ft/ft.db-shm"]

[[sync.targets]]
name = "staging"
transport = "ssh"
endpoint = "ft@staging-host"
root = "~/.local/share/ft/sync"
default_direction = "push"

[patterns]
# Which detection packs to enable
packs = ["core"]
# Core pack detects: Claude Code, Codex, Gemini state transitions

[workflows]
# Enable automatic workflow execution on pattern matches
enabled = true
# Maximum concurrent workflows
concurrency = 10

[safety]
# Require approval for actions on new hosts
approve_new_hosts = true
# Redact sensitive patterns (API keys, tokens) in logs
redact_secrets = true
# Rate limits per action type
[safety.rate_limits]
send_text = { max_per_second = 2 }
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           WezTerm Multiplexer                           │
│   Pane 0 (Claude)    Pane 1 (Codex)    Pane 2 (Gemini)    Pane N...   │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                    wezterm cli list / get-text
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         Ingest Pipeline                                  │
│   Discovery → Delta Extraction → Fingerprinting → Observation Filter    │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         Storage Layer (SQLite + FTS5)                   │
│   output_segments │ events │ workflow_executions │ audit_actions        │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                    ┌──────────────┼──────────────┐
                    ▼              ▼              ▼
             ┌───────────┐  ┌───────────┐  ┌───────────┐
             │  Pattern  │  │   Event   │  │  Workflow │
             │  Engine   │  │    Bus    │  │  Engine   │
             │ (detect)  │  │ (fanout)  │  │ (execute) │
             └───────────┘  └───────────┘  └───────────┘
                    │              │              │
                    └──────────────┼──────────────┘
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         Policy Engine                                    │
│   Capability Gates │ Rate Limiting │ Audit Trail │ Approval Tokens      │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                 Robot Mode API + MCP (stdio)                             │
│   ft robot state │ get-text │ send │ wait-for │ search │ events        │
│   ft mcp serve (feature-gated, stdio transport)                          │
└─────────────────────────────────────────────────────────────────────────┘
```

For a deeper architecture writeup (OSC 133 prompt markers, gap semantics, library map), see `docs/architecture.md`.

### Data Flow

1. **Discovery**: Enumerate panes via `wezterm cli list`
2. **Capture**: Get output via `wezterm cli get-text`
3. **Delta**: Compare with previous capture using 4KB overlap matching
4. **Store**: Append new segments to SQLite with FTS5 indexing
5. **Detect**: Run pattern engine against new content
6. **Event**: Broadcast detections to event bus subscribers
7. **Workflow**: Execute registered workflows on matching events
8. **Policy**: Gate all actions through capability and rate limit checks
9. **API**: Expose everything via Robot Mode JSON interface

---

## Pattern Detection

`ft` detects state transitions across multiple AI coding agents:

| Agent | Pattern Examples |
|-------|------------------|
| **Codex** | `core.codex:usage_reached`, `core.codex:compaction_complete` |
| **Claude Code** | `core.claude:rate_limited`, `core.claude:approval_needed` |
| **Gemini** | `core.gemini:quota_exceeded`, `core.gemini:error` |
| **WezTerm** | `core.wezterm:pane_closed`, `core.wezterm:title_changed` |

### Pattern IDs

Every detection has a stable `rule_id` like `core.codex:usage_reached`. Use these in:
- `ft robot wait-for <pane_id> <rule_id>` — wait for specific condition
- Workflow triggers — automatically react to patterns
- Allowlists — suppress false positives

---

## Performance Benchmarks

Benchmarks live under `crates/frankenterm-core/benches/` and use Criterion. Each bench includes a short, human-readable budget and emits machine-readable artifacts under `target/criterion/`.

```bash
# Compile benches (fast sanity check)
cargo bench -p frankenterm-core --benches --no-run

# Run a specific bench
cargo bench -p frankenterm-core --bench pattern_detection
cargo bench -p frankenterm-core --bench delta_extraction
cargo bench -p frankenterm-core --bench fts_query
```

When a bench runs, it prints a `[BENCH] {...}` metadata line and writes:
- `target/criterion/ft-bench-meta.jsonl` (budgets + environment)
- `target/criterion/ft-bench-manifest-<bench>.json` (artifact manifest)

---

## Troubleshooting

For a step-by-step operator guide (triage → why → reproduce), see `docs/operator-playbook.md`.

### "wezterm cli list" returns empty

```bash
# Ensure WezTerm is running with multiplexer enabled
wezterm start --always-new-process
```

### Daemon won't start: "watcher lock held"

Another `ft` watcher is already running.

```bash
# Check for existing watcher
ft status

# Force stop if stuck
ft stop --force

# Or remove stale lock
rm ~/.local/share/ft/watcher.lock
```

### High memory usage

Delta extraction is failing; falling back to full captures.

```bash
# Check for gaps in capture
ft robot events --event-type gap

# Reduce poll interval
# In ft.toml:
[ingest]
poll_interval_ms = 500  # Slower polling
```

### Pattern not detecting

```bash
# Enable debug logging
ft -vv watch --foreground

# Test pattern manually
ft rules test "Usage limit reached. Try again later."
```

### Robot mode returns errors

```bash
# Check watcher is running
ft status

# Verify pane exists
wezterm cli list

# Check policy blocks
ft robot send 0 "test" --dry-run
```

---

## Limitations

### What ft Doesn't Do (Yet)

- **Non-WezTerm terminals**: Relies on WezTerm's CLI protocol. tmux/iTerm2 not supported.
- **Remote panes without SSH**: WezTerm SSH domains work; raw remote terminals don't.
- **GUI interaction**: Detects terminal output only, not graphical elements.
- **Multi-host federation**: Full fleet/federation orchestration is not implemented yet.

### Known Limitations

| Capability | Current State | Planned |
|------------|---------------|---------|
| Browser automation (OAuth) | Feature-gated, partial | v0.2 |
| MCP server integration | Feature-gated (stdio) | v0.2 |
| Web dashboard | Feature-gated (health-only) | v0.3 |
| Multi-host federation | Not started | v2.0 |

---

## FAQ

### Why "ft"?

**F**ranken**T**erm. Short, typeable, memorable.

### Is my terminal output stored permanently?

By default, output is retained for 30 days (configurable via `storage.retention_days`). Data is stored locally in SQLite at `~/.local/share/ft/ft.db`.

### Does ft send data anywhere?

No. Everything stays local. No telemetry, no cloud, no network calls except to WezTerm's local CLI.

### Can I use ft without running AI agents?

Yes. The pattern detection and search work for any terminal output. Useful for debugging, auditing, or building custom automation.

### How do I add custom patterns?

Edit `~/.config/ft/patterns.toml`:

```toml
[[patterns]]
id = "custom:my_error"
pattern = "FATAL ERROR:.*"
severity = "critical"
```

### What's the performance overhead?

- **CPU**: <1% during idle; brief spikes during pattern detection
- **Memory**: ~50MB for watcher with 100 panes
- **Disk**: ~10MB/day for typical multi-agent usage (compressed deltas)
- **Latency**: <50ms average capture lag

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT License. See [LICENSE](LICENSE) for details.

---

<div align="center">

**Built for the AI agent age. Observability without compromise.**

</div>
